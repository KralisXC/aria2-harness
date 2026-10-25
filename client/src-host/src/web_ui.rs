//! Release 专用内置静态服务器（便携包前端加载）。
//!
//! 为什么不用 `file://` 或 wry 自定义协议：WebView2 在 `file://` 下会因
//! CORS / 同源策略限制 ES module 与 fetch；起一个只绑 `127.0.0.1` 的随机端口
//! 静态服务器，行为与开发期的 Vite 完全一致，前端零改动。
//!
//! 只在 Release 构建编译（`#[cfg(not(debug_assertions))]` 见 `lib.rs`）：
//! 开发期仍走 `http://localhost:1420` 的 Vite dev server（带 HMR）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

/// 单个请求头的读取上界（正常浏览器请求远小于此）。
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// 启动静态服务器并返回可直接交给 `WebViewBuilder::with_url` 的地址。
///
/// 绑定 `127.0.0.1:0`（内核分配空闲端口），彻底避免与 1420 / 17800 冲突。
/// 前端资源缺失时返回 `None`（调用方回退到 `DEV_URL` 并打错误日志）。
pub fn start() -> Option<String> {
    let root = crate::paths::web_root();
    let index = root.join("index.html");
    if !index.is_file() {
        tracing::error!("便携包缺少前端资源：{}", index.display());
        return None;
    }

    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(listener) => listener,
        Err(e) => {
            tracing::error!("内置静态服务器绑端口失败：{e}");
            return None;
        }
    };
    let port = match listener.local_addr() {
        Ok(addr) => addr.port(),
        Err(e) => {
            tracing::error!("读取静态服务器端口失败：{e}");
            return None;
        }
    };

    let thread_root = root.clone();
    std::thread::Builder::new()
        .name("harness-web-ui".to_string())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let root = thread_root.clone();
                        // 单连接单线程：本地静态资源量级很小，避免拖慢主循环。
                        let _ = std::thread::Builder::new()
                            .name("harness-web-ui-conn".to_string())
                            .spawn(move || {
                                let _ = handle(stream, &root);
                            });
                    }
                    Err(e) => tracing::debug!("静态服务器 accept 失败：{e}"),
                }
            }
        })
        .ok()?;

    tracing::info!(port, "内置静态服务器已启动（前端资源：{}）", root.display());
    Some(format!("http://127.0.0.1:{port}/"))
}

/// 处理一次 HTTP 请求：只支持 GET / HEAD。
fn handle(mut stream: TcpStream, root: &Path) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= MAX_HEADER_BYTES {
            break;
        }
    }

    let head = String::from_utf8_lossy(&buf);
    let mut parts = head.lines().next().unwrap_or_default().split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or("/");

    if method != "GET" && method != "HEAD" {
        return respond(&mut stream, 405, "text/plain; charset=utf-8", b"method not allowed", false, "no-cache");
    }

    let Some(path) = resolve(root, target) else {
        return respond(&mut stream, 404, "text/plain; charset=utf-8", b"not found", method == "HEAD", "no-cache");
    };
    match std::fs::read(&path) {
        Ok(body) => {
            let mime = mime_for(&path);
            respond(&mut stream, 200, mime, &body, method == "HEAD", cache_control(&path))
        }
        Err(_) => respond(&mut stream, 404, "text/plain; charset=utf-8", b"not found", method == "HEAD", "no-cache"),
    }
}

/// 把 URL 目标映射到 `root` 内的真实文件；越界（`..`）一律拒绝。
fn resolve(root: &Path, target: &str) -> Option<PathBuf> {
    let path = target.split(['?', '#']).next().unwrap_or("/");
    let decoded = percent_decode(path);
    let normalized = decoded.replace('\\', "/");
    let relative = normalized.trim_start_matches('/');
    let relative = if relative.is_empty() { "index.html" } else { relative };

    if relative.split('/').any(|seg| seg == "..") {
        return None;
    }
    let full = root.join(relative);
    if full.is_file() { Some(full) } else { None }
}

/// 最小百分号解码（Vite 产物文件名一般不含转义，兜底用）。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(value) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(value);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 常见前端资源类型（缺失时按二进制流下发）。
fn mime_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// 缓存策略：Vite 产物的 js/css 文件名带内容哈希，可长期 immutable 缓存；
/// `index.html` 及其它资源必须 no-cache，否则版本升级后仍会命中旧入口。
fn cache_control(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "js" | "mjs" | "css" | "woff" | "woff2" | "ttf" => {
            "public, max-age=31536000, immutable"
        }
        _ => "no-cache",
    }
}

/// 写回响应（`Connection: close`，每个连接只处理一次请求）。
fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    head_only: bool,
    cache: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: {cache}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    if !head_only {
        stream.write_all(body)?;
    }
    stream.flush()
}
