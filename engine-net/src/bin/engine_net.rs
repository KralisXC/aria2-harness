//! engine-net 独立进程形态（Phase 1：JSON-RPC 最小集，仅回环）。
//!
//! 协议：`127.0.0.1:6801`，换行分隔的 JSON-RPC 2.0（与 service 6800 同一套方法）。
//! 鉴权：`--secret` 必填；每个请求的 `params` 首项必须是 `token:<secret>`
//! （与 aria2 `--rpc-secret` 语义一致）。
//!
//! 用法：
//!   engine-net --port=6801 --secret=<RPC密钥> [--dir=<默认下载目录>]
//!
//! 说明：本进程不拥有 Job Object（由 service 侧守护），形态见 docs/c3/Q4-binary.md。

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use engine_net::dns::resolver::{DnsConfig, DnsResolver};
use engine_net::rpc::handle_value_guarded;
use engine_net::rpc::methods::Engine;

/// 单行上限：防止无换行输入把内存打爆。
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// 取 `--key=value` 形式的参数值。
fn arg_value(args: &[String], key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    args.iter().find_map(|a| a.strip_prefix(&prefix).map(str::to_owned))
}

#[tokio::main]
async fn main() {
    engine_net::log::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let port: u16 = arg_value(&args, "--port")
        .and_then(|v| v.parse().ok())
        .unwrap_or(6801);
    let secret = arg_value(&args, "--secret").unwrap_or_default();
    // 未显式给 `--dir` 时用绝对 cwd（引擎强制要求绝对路径，`.` 会被 RPC 拒绝）。
    let dir = arg_value(&args, "--dir").unwrap_or_else(|| {
        std::env::current_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string())
    });
    // `--dns-server=IP:PORT[,IP:PORT]`：A/B 与离线验证用（对齐 aria2 `--async-dns-server`）。
    let dns_servers: Vec<std::net::SocketAddr> = arg_value(&args, "--dns-server")
        .map(|value| {
            value
                .split(',')
                .filter_map(|item| item.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default();

    if secret.is_empty() {
        eprintln!("[engine-net] 缺少 --secret=<RPC密钥>，拒绝启动（红线：不允许无鉴权入口）");
        std::process::exit(2);
    }

    // 只绑回环：与 aria2c 的 --rpc-listen-all=false 保持一致。
    let listener = match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("[engine-net] 绑定 127.0.0.1:{port} 失败：{err}");
            std::process::exit(3);
        }
    };

    let resolver = match DnsResolver::new(&DnsConfig {
        servers: dns_servers,
        // 显式指定 DNS 时不做 hosts 兜底，保证与 aria2 `--async-dns-server` 同口径。
        use_hosts: arg_value(&args, "--dns-server").is_none(),
        ..DnsConfig::default()
    }) {
        Ok(resolver) => resolver,
        Err(err) => {
            eprintln!("[engine-net] DNS 解析器初始化失败：{err}");
            std::process::exit(3);
        }
    };
    let engine = Arc::new(Engine::new(resolver));
    engine.set_global_option("dir", &dir);
    let secret = Arc::new(secret);

    tracing::info!(
        "listening 127.0.0.1:{port} dir={dir} version={}",
        engine_net::version()
    );

    loop {
        match listener.accept().await {
            Ok((socket, peer)) => {
                let engine = Arc::clone(&engine);
                let secret = Arc::clone(&secret);
                tokio::spawn(async move {
                    serve(socket, engine, secret, peer.to_string()).await;
                });
            }
            Err(err) => tracing::warn!("accept 失败：{err}"),
        }
    }
}

async fn serve(socket: TcpStream, engine: Arc<Engine>, secret: Arc<String>, peer: String) {
    tracing::debug!("连接 {peer}");
    let (reader, mut writer) = socket.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        let read = tokio::time::timeout(Duration::from_secs(300), reader.read_line(&mut line)).await;
        match read {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {}
            Ok(Err(err)) => {
                tracing::warn!("{peer} 读取失败：{err}");
                break;
            }
            Err(_) => {
                tracing::warn!("{peer} 空闲超时，断开");
                break;
            }
        }

        let response = if line.len() > MAX_LINE_BYTES {
            json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32600, "message": "请求行超过 1MB 上限" }
            })
        } else {
            handle_line(&engine, &secret, line.trim())
        };

        let mut payload = response.to_string();
        payload.push('\n');
        if writer.write_all(payload.as_bytes()).await.is_err() {
            break;
        }
    }
}

fn handle_line(engine: &Engine, secret: &str, line: &str) -> Value {
    let mut request: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(err) => {
            return json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32700, "message": format!("JSON 解析失败：{err}") }
            });
        }
    };

    if !authorize(&mut request, secret) {
        return json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned().unwrap_or(Value::Null),
            "error": { "code": 1, "message": "Unauthorized：params 首项必须是 token:<secret>" }
        });
    }
    handle_value_guarded(engine, &request)
}

/// 校验并剥离 `token:<secret>`（与 aria2 一致，token 必须是首个参数）。
fn authorize(request: &mut Value, secret: &str) -> bool {
    let Some(params) = request.get_mut("params").and_then(Value::as_array_mut) else {
        return false;
    };
    let Some(Value::String(token)) = params.first() else {
        return false;
    };
    let Some(value) = token.strip_prefix("token:") else {
        return false;
    };
    if value != secret {
        return false;
    }
    params.remove(0);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_requires_token_first() {
        let mut request = json!({"jsonrpc":"2.0","id":"1","method":"aria2.getVersion","params":["token:s3"]});
        assert!(authorize(&mut request, "s3"));
        assert_eq!(request["params"], json!([]));

        let mut missing = json!({"jsonrpc":"2.0","id":"1","method":"aria2.getVersion","params":[]});
        assert!(!authorize(&mut missing, "s3"));

        let mut wrong = json!({"jsonrpc":"2.0","id":"1","method":"aria2.getVersion","params":["token:x"]});
        assert!(!authorize(&mut wrong, "s3"));
    }
}
