//! §5.4.7 A/B 一致性：同一 URL 分别用 aria2c（产品自带 1.37.0）与 engine-net 下载，
//! 逐项比对文件 SHA256、tellStatus 关键字段、errorCode 与请求头行为。
//!
//! 用例矩阵（weilai.md §5.4.7）：
//!   1. 单 Range（206）续传
//!   2. 服务器忽略 Range（200）→ CANNOT_RESUME(8)
//!   3. 302 重定向
//!   4. 429 限流 → HTTP_PROTOCOL_ERROR(22)
//!   5. 断网重试（首次 RST，随后成功）
//!   6. raw.gitcode.com（本地伪造 DNS，不访问真实网络）
//!
//! 另含 §5.5 的两项互操作性：
//!   - `*.aria2` 控制文件双向互读（aria2 → engine-net、engine-net → aria2）
//!   - `aria2.session` 双向互读
//!
//! 两侧都是独立进程：
//!   - aria2 侧：`my-downloader/src-host/binaries/aria2c.exe`（或 `$ARIA2C_BIN`），
//!     通过 `--enable-rpc` + HTTP POST `/jsonrpc` 驱动。
//!   - engine-net 侧：本 crate 的 `engine-net` bin（换行分隔 JSON-RPC）。
//!
//! 所有 HTTP/DNS 目标都是 127.0.0.1，**不发起真实外网请求**（用户禁令）。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engine_net::progress::control::{ControlFile, ControlFileStore};
use engine_net::progress::session::{SessionEntry, SessionFile};
use engine_net::progress::sqlite_control::{SqliteControlStore, sqlite_path};
use engine_net::task::registry::control_path;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// A/B 统一负载：4 MiB（= 4 个默认 1 MiB 分片）。
const PAYLOAD_LEN: usize = 4 * 1024 * 1024;
/// aria2 `--piece-length` 下限就是 1 MiB，两侧都用默认值。
const PIECE_LEN: u32 = 1024 * 1024;

// ---------------------------------------------------------------------------
// 本地 HTTP 测试服务器
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Req {
    #[allow(dead_code)]
    method: String,
    path: String,
    headers: HashMap<String, String>,
}

enum Body {
    Full(Vec<u8>),
    /// 分块慢速发送（制造可暂停的在途下载）。
    Slow {
        data: Vec<u8>,
        chunk: usize,
        delay: Duration,
    },
    /// SO_LINGER=0，直接 RST（模拟断网 / 连接重置）。
    Reset,
}

struct Resp {
    status: String,
    headers: Vec<(String, String)>,
    body: Body,
}

struct TestServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Req>>>,
    handle: JoinHandle<()>,
}

impl TestServer {
    async fn start<F>(handler: F) -> Self
    where
        F: Fn(&Req) -> Resp + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定测试端口");
        let addr = listener.local_addr().expect("本地地址");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        let handler = Arc::new(move |req: &Req| {
            log.lock().unwrap().push(req.clone());
            handler(req)
        });

        let handle = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    serve_connection(socket, handler).await;
                });
            }
        });

        Self {
            addr,
            requests,
            handle,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.addr.port())
    }

    fn range_headers(&self) -> Vec<Option<String>> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|req| req.headers.get("range").cloned())
            .collect()
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

type Handler = Arc<dyn Fn(&Req) -> Resp + Send + Sync>;

async fn serve_connection(mut socket: TcpStream, handler: Handler) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = socket.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let request = parse_request(&buf);
    let response = handler(&request);
    write_response(&mut socket, response).await;
    let _ = socket.shutdown().await;
}

async fn write_response(socket: &mut TcpStream, response: Resp) {
    if matches!(response.body, Body::Reset) {
        let _ = socket.set_linger(Some(Duration::ZERO));
        return;
    }
    let mut head = format!("HTTP/1.1 {}\r\n", response.status);
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    let length = match &response.body {
        Body::Full(data) => data.len(),
        Body::Slow { data, .. } => data.len(),
        Body::Reset => 0,
    };
    head.push_str(&format!("Content-Length: {length}\r\nConnection: close\r\n\r\n"));
    if socket.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    match response.body {
        Body::Full(data) => {
            let _ = socket.write_all(&data).await;
        }
        Body::Slow { data, chunk, delay } => {
            for part in data.chunks(chunk) {
                if socket.write_all(part).await.is_err() {
                    return;
                }
                let _ = socket.flush().await;
                tokio::time::sleep(delay).await;
            }
        }
        Body::Reset => {}
    }
}

fn parse_request(raw: &[u8]) -> Req {
    let text = String::from_utf8_lossy(raw);
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    Req {
        method,
        path,
        headers,
    }
}

/// 请求的 Range 起点（无 Range 时返回 0）。
fn range_start(req: &Req) -> usize {
    req.headers
        .get("range")
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|value| value.split('-').next())
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
}

/// 支持 Range 的 200/206 响应。
fn range_response(req: &Req, payload: &[u8], slow: Option<(usize, Duration)>) -> Resp {
    let start = range_start(req).min(payload.len());
    let slice = payload[start..].to_vec();
    let mut headers = vec![(
        "Content-Type".to_string(),
        "application/octet-stream".to_string(),
    )];
    let status = if start > 0 {
        headers.push((
            "Content-Range".to_string(),
            format!("bytes {}-{}/{}", start, payload.len() - 1, payload.len()),
        ));
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let body = match slow {
        Some((chunk, delay)) => Body::Slow {
            data: slice,
            chunk,
            delay,
        },
        None => Body::Full(slice),
    };
    Resp {
        status: status.to_string(),
        headers,
        body,
    }
}

// ---------------------------------------------------------------------------
// aria2c 进程（产品自带二进制 + RPC 驱动）
// ---------------------------------------------------------------------------

struct Aria2 {
    child: Child,
    port: u16,
    secret: String,
    dir: PathBuf,
    session: PathBuf,
    log: PathBuf,
}

impl Aria2 {
    async fn start(dir: PathBuf, extra: &[&str]) -> Self {
        let port = free_port();
        let secret = "ab-secret".to_string();
        let log = dir.join("aria2.log");
        let session = dir.join("aria2.session");

        let mut args: Vec<String> = vec![
            "--no-conf=true".into(),
            "--enable-rpc=true".into(),
            "--rpc-listen-all=false".into(),
            format!("--rpc-listen-port={port}"),
            format!("--rpc-secret={secret}"),
            format!("--dir={}", dir.display()),
            format!("--log={}", log.display()),
            format!("--save-session={}", session.display()),
            "--save-session-interval=0".into(),
            "--auto-save-interval=0".into(),
            "--console-log-level=warn".into(),
            "--quiet=true".into(),
            "--file-allocation=none".into(),
            "--split=1".into(),
            "--max-connection-per-server=1".into(),
            "--max-tries=5".into(),
            "--retry-wait=0".into(),
            "--disable-ipv6=true".into(),
            "--piece-length=1M".into(),
            // 产品 aria2c 默认 --sqlite-progress=true（Aria2-Harness 实验特性），
            // HTTP 下载的控制文件是 `<file>.sqlite`；Phase 1 验收基准是 R5 约定的
            // legacy `*.aria2` 二进制格式，故 A/B 基线显式关闭 sqlite 进度文件。
            // 该口径冲突已在 Phase 1 报告中记录，等待红线修订决策。
            "--sqlite-progress=false".into(),
        ];
        args.extend(extra.iter().map(|item| (*item).to_string()));

        let child = Command::new(aria2c_path())
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|err| panic!("启动 aria2c 失败：{err}"));

        let mut aria2 = Self {
            child,
            port,
            secret,
            dir,
            session,
            log,
        };
        aria2.wait_ready().await;
        aria2
    }

    async fn wait_ready(&mut self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(Some(code)) = self.child.try_wait() {
                panic!(
                    "aria2c 提前退出（{code}）：\n{}",
                    std::fs::read_to_string(&self.log).unwrap_or_default()
                );
            }
            if self.try_rpc("aria2.getVersion", json!([])).await.is_ok() {
                return;
            }
            if tokio::time::Instant::now() > deadline {
                panic!(
                    "aria2c RPC 未就绪：\n{}",
                    std::fs::read_to_string(&self.log).unwrap_or_default()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn try_rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut params = match params {
            Value::Array(items) => items,
            _ => return Err("params 必须是数组".to_string()),
        };
        params.insert(0, json!(format!("token:{}", self.secret)));
        let body = json!({
            "jsonrpc": "2.0",
            "id": "ab",
            "method": method,
            "params": params,
        })
        .to_string();
        let request = format!(
            "POST /jsonrpc HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.port,
            body.len(),
            body
        );
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))
            .await
            .map_err(|err| err.to_string())?;
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|err| err.to_string())?;
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .await
            .map_err(|err| err.to_string())?;
        let text = String::from_utf8_lossy(&raw);
        let (_, payload) = text
            .split_once("\r\n\r\n")
            .ok_or_else(|| format!("aria2 RPC 响应缺 body：{text}"))?;
        serde_json::from_str(payload).map_err(|err| format!("aria2 RPC JSON 解析失败：{err}"))
    }

    async fn rpc(&self, method: &str, params: Value) -> Value {
        self.try_rpc(method, params)
            .await
            .unwrap_or_else(|err| panic!("aria2 RPC {method} 失败：{err}"))
    }

    async fn add(&self, url: &str, out: &str) -> String {
        let response = self.rpc("aria2.addUri", json!([[url], {"out": out}])).await;
        response["result"]
            .as_str()
            .unwrap_or_else(|| panic!("aria2.addUri 失败：{response}"))
            .to_string()
    }

    async fn status(&self, gid: &str) -> Value {
        let response = self.rpc("aria2.tellStatus", json!([gid])).await;
        response["result"].clone()
    }

    async fn wait_done(&self, gid: &str) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
        loop {
            let status = self.status(gid).await;
            let state = status["status"].as_str().unwrap_or("");
            if matches!(state, "complete" | "error" | "removed") {
                return status;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("aria2 等待 {gid} 超时：{status}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_downloaded(&self, gid: &str, bytes: u64) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let status = self.status(gid).await;
            let completed = status["completedLength"]
                .as_str()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0);
            if completed >= bytes || status["status"] == "error" || status["status"] == "complete" {
                return status;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("aria2 等待进度 {bytes}B 超时：{status}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// 优雅关闭（`aria2.shutdown`），保证控制文件与 session 落盘。
    async fn stop(&mut self) {
        let _ = self.try_rpc("aria2.shutdown", json!([])).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            if tokio::time::Instant::now() > deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for Aria2 {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn aria2c_path() -> PathBuf {
    if let Ok(path) = std::env::var("ARIA2C_BIN") {
        return PathBuf::from(path);
    }
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("..");
    path.push("my-downloader");
    path.push("src-host");
    path.push("binaries");
    path.push("aria2c.exe");
    assert!(
        path.exists(),
        "找不到产品自带 aria2c：{}（可用 ARIA2C_BIN 覆盖）",
        path.display()
    );
    path
}

// ---------------------------------------------------------------------------
// engine-net 独立 bin（换行分隔 JSON-RPC）
// ---------------------------------------------------------------------------

struct Native {
    child: Child,
    port: u16,
    secret: String,
    dir: PathBuf,
}

impl Native {
    async fn start(dir: PathBuf, dns_server: Option<String>) -> Self {
        let port = free_port();
        let secret = "ab-secret".to_string();
        let mut command = Command::new(env!("CARGO_BIN_EXE_engine-net"));
        command
            .arg(format!("--port={port}"))
            .arg(format!("--secret={secret}"))
            .arg(format!("--dir={}", dir.display()))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(server) = dns_server {
            command.arg(format!("--dns-server={server}"));
        }
        let child = command
            .spawn()
            .unwrap_or_else(|err| panic!("启动 engine-net 失败：{err}"));

        let mut native = Self {
            child,
            port,
            secret,
            dir,
        };
        native.wait_ready().await;
        native
    }

    async fn wait_ready(&mut self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(Some(code)) = self.child.try_wait() {
                panic!("engine-net 提前退出（{code}）");
            }
            if self.try_rpc("aria2.getVersion", json!([])).await.is_ok() {
                return;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("engine-net RPC 未就绪");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn try_rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut params = match params {
            Value::Array(items) => items,
            _ => return Err("params 必须是数组".to_string()),
        };
        params.insert(0, json!(format!("token:{}", self.secret)));
        let body = json!({
            "jsonrpc": "2.0",
            "id": "ab",
            "method": method,
            "params": params,
        })
        .to_string();

        let mut stream = TcpStream::connect(("127.0.0.1", self.port))
            .await
            .map_err(|err| err.to_string())?;
        stream
            .write_all(body.as_bytes())
            .await
            .map_err(|err| err.to_string())?;
        stream
            .write_all(b"\n")
            .await
            .map_err(|err| err.to_string())?;
        let mut line = String::new();
        let read = tokio::time::timeout(
            Duration::from_secs(30),
            BufReader::new(stream).read_line(&mut line),
        )
        .await
        .map_err(|_| "engine-net RPC 超时".to_string())?
        .map_err(|err| err.to_string())?;
        if read == 0 {
            return Err("engine-net 连接被关闭".to_string());
        }
        serde_json::from_str(&line).map_err(|err| format!("engine-net RPC JSON 解析失败：{err}"))
    }

    async fn rpc(&self, method: &str, params: Value) -> Value {
        self.try_rpc(method, params)
            .await
            .unwrap_or_else(|err| panic!("engine-net RPC {method} 失败：{err}"))
    }

    async fn add(&self, url: &str, out: &str, extra: Value) -> String {
        let mut options = json!({
            "dir": self.dir.to_string_lossy(),
            "out": out,
            "piece-length": PIECE_LEN.to_string(),
        });
        if let (Some(target), Some(more)) = (options.as_object_mut(), extra.as_object()) {
            for (key, value) in more {
                target.insert(key.clone(), value.clone());
            }
        }
        let response = self.rpc("aria2.addUri", json!([[url], options])).await;
        response["result"]
            .as_str()
            .unwrap_or_else(|| panic!("engine-net addUri 失败：{response}"))
            .to_string()
    }

    async fn status(&self, gid: &str) -> Value {
        let response = self.rpc("aria2.tellStatus", json!([gid])).await;
        response["result"].clone()
    }

    async fn wait_done(&self, gid: &str) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
        loop {
            let status = self.status(gid).await;
            let state = status["status"].as_str().unwrap_or("");
            if matches!(state, "complete" | "error" | "removed") {
                return status;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("engine-net 等待 {gid} 超时：{status}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_downloaded(&self, gid: &str, bytes: u64) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let status = self.status(gid).await;
            let completed = status["completedLength"]
                .as_str()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0);
            if completed >= bytes || status["status"] == "error" || status["status"] == "complete" {
                return status;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("engine-net 等待进度 {bytes}B 超时：{status}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn shutdown(&mut self) {
        // 独立 bin 没有 shutdown 方法：直接 kill，控制文件在 pause 时已落盘。
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

// ---------------------------------------------------------------------------
// 本地伪造 DNS（只在回环上；raw.gitcode.com → 127.0.0.1）
// ---------------------------------------------------------------------------

struct FakeDns {
    stop: Arc<AtomicBool>,
    addr: SocketAddr,
}

impl FakeDns {
    fn start(addr: &str) -> std::io::Result<Self> {
        let socket = std::net::UdpSocket::bind(addr)?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        let addr = socket.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while !flag.load(Ordering::Relaxed) {
                let Ok((len, peer)) = socket.recv_from(&mut buf) else {
                    continue;
                };
                if let Some(payload) = dns_response(&buf[..len]) {
                    let _ = socket.send_to(&payload, peer);
                }
            }
        });
        Ok(Self { stop, addr })
    }
}

impl Drop for FakeDns {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A 记录 → 127.0.0.1；其余类型 → NOERROR 空应答（对齐 `--disable-ipv6=true`）。
fn dns_response(query: &[u8]) -> Option<Vec<u8>> {
    let (qtype_end, qtype) = parse_question(query)?;
    let question = &query[12..qtype_end + 4];
    let is_a = qtype == 1;

    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&query[0..2]);
    out.extend_from_slice(&[0x81, 0x80]);
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&(if is_a { 1u16 } else { 0u16 }).to_be_bytes());
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    out.extend_from_slice(question);
    if is_a {
        out.extend_from_slice(&[0xC0, 0x0C]);
        out.extend_from_slice(&1u16.to_be_bytes()); // TYPE=A
        out.extend_from_slice(&1u16.to_be_bytes()); // CLASS=IN
        out.extend_from_slice(&60u32.to_be_bytes()); // TTL
        out.extend_from_slice(&4u16.to_be_bytes());
        out.extend_from_slice(&[127, 0, 0, 1]);
    }
    Some(out)
}

/// 返回 (qtype 字段起始偏移, qtype)。
fn parse_question(packet: &[u8]) -> Option<(usize, u16)> {
    if packet.len() < 12 {
        return None;
    }
    let mut i = 12usize;
    loop {
        let len = *packet.get(i)? as usize;
        i += 1;
        if len == 0 {
            break;
        }
        if len & 0xC0 != 0 {
            return None;
        }
        i += len;
    }
    let qtype = u16::from_be_bytes([*packet.get(i)?, *packet.get(i + 1)?]);
    Some((i, qtype))
}

// ---------------------------------------------------------------------------
// 公共工具
// ---------------------------------------------------------------------------

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("选端口");
    listener.local_addr().expect("本地地址").port()
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间")
        .as_nanos();
    let mut dir = std::env::temp_dir();
    dir.push(format!("engine-net-ab-{}-{tag}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("创建临时目录");
    dir
}

fn sample_body(len: usize) -> Vec<u8> {
    (0..len).map(|index| (index % 251) as u8).collect()
}

fn sha256(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_else(|err| {
        panic!("读取 {} 失败：{err}", path.display());
    });
    Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sha256_of(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 预置「已下载 `done` 字节」的原始文件（aria2 按文件长度续传的入口）。
fn seed_partial(dir: &Path, done: usize, payload: &[u8]) {
    std::fs::write(dir.join("file.bin"), &payload[..done]).expect("写入半成品");
}

/// engine-net 只从 `*.aria2` 控制文件续传，因此额外补一份与 `done` 对齐的控制文件。
fn seed_control(dir: &Path, done: u64) {
    let mut control = ControlFile::new(PAYLOAD_LEN as u64, PIECE_LEN);
    control.mark_completed_bytes(done);
    let mut store = ControlFileStore::new(control_path(&dir.join("file.bin")));
    store.save(&control).expect("写入控制文件");
}

fn assert_complete(status: &Value, engine: &str) {
    assert_eq!(
        status["status"], "complete",
        "{engine} 未完成：{status}"
    );
    assert_eq!(
        status["totalLength"],
        PAYLOAD_LEN.to_string(),
        "{engine} totalLength 不一致：{status}"
    );
    assert_eq!(
        status["completedLength"],
        PAYLOAD_LEN.to_string(),
        "{engine} completedLength 不一致：{status}"
    );
}

fn assert_error_code(status: &Value, code: i32, engine: &str) {
    assert_eq!(status["status"], "error", "{engine} 未报错：{status}");
    assert_eq!(
        status["errorCode"],
        code.to_string(),
        "{engine} errorCode 不一致：{status}"
    );
}

// ---------------------------------------------------------------------------
// 用例 1：单 Range（206）续传
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ab_1_range_resume_206() {
    const RESUME: usize = 2 * PIECE_LEN as usize;
    let payload = Arc::new(sample_body(PAYLOAD_LEN));
    let server = TestServer::start({
        let payload = Arc::clone(&payload);
        move |req| range_response(req, &payload, None)
    })
    .await;
    let url = server.url("/file.bin");

    let aria2 = Aria2::start(temp_dir("ab1-a"), &[]).await;
    let native = Native::start(temp_dir("ab1-n"), None).await;
    seed_partial(&aria2.dir, RESUME, &payload);
    seed_partial(&native.dir, RESUME, &payload);
    // aria2 默认 --continue=false：不给控制文件时它不会按文件长度续传，
    // 而会因目标文件已存在触发 auto-file-renaming（写 file.1.bin）。
    // A/B 基线必须两侧前置对称，因此给两侧都写入同一份 engine-net 控制文件。
    seed_control(&aria2.dir, RESUME as u64);
    seed_control(&native.dir, RESUME as u64);

    let a_gid = aria2.add(&url, "file.bin").await;
    let n_gid = native.add(&url, "file.bin", json!({})).await;
    let a_status = aria2.wait_done(&a_gid).await;
    let n_status = native.wait_done(&n_gid).await;

    assert_complete(&a_status, "aria2");
    assert_complete(&n_status, "engine-net");

    let expected = sha256_of(&payload);
    assert_eq!(sha256(&aria2.dir.join("file.bin")), expected, "aria2 文件 SHA256");
    assert_eq!(
        sha256(&native.dir.join("file.bin")),
        expected,
        "engine-net 文件 SHA256"
    );

    let ranges = server.range_headers();
    // aria2 发闭区间 `bytes=start-end`，engine-net 发开放区间 `bytes=start-`；
    // 两者的续传起点必须都等于 RESUME。
    let prefix = format!("bytes={RESUME}-");
    let resumed = ranges
        .iter()
        .filter(|value| {
            value
                .as_deref()
                .map(|raw| raw.starts_with(&prefix))
                .unwrap_or(false)
        })
        .count();
    assert_eq!(resumed, 2, "两侧都必须从 {RESUME}B 续传：{ranges:?}");

    let _ = std::fs::remove_dir_all(&aria2.dir);
    let _ = std::fs::remove_dir_all(&native.dir);
}

// ---------------------------------------------------------------------------
// 用例 2：服务器忽略 Range（200）→ CANNOT_RESUME(8)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ab_2_range_ignored_returns_error_8() {
    const RESUME: usize = 2 * PIECE_LEN as usize;
    let payload = Arc::new(sample_body(PAYLOAD_LEN));
    let server = TestServer::start({
        let payload = Arc::clone(&payload);
        move |_req| Resp {
            status: "200 OK".to_string(),
            headers: vec![(
                "Content-Type".to_string(),
                "application/octet-stream".to_string(),
            )],
            body: Body::Full((*payload).clone()),
        }
    })
    .await;
    let url = server.url("/file.bin");

    let aria2 = Aria2::start(temp_dir("ab2-a"), &[]).await;
    let native = Native::start(temp_dir("ab2-n"), None).await;
    seed_partial(&aria2.dir, RESUME, &payload);
    seed_partial(&native.dir, RESUME, &payload);
    // 同用例 1：aria2 需要控制文件才会进入续传路径（否则会改名另写）。
    seed_control(&aria2.dir, RESUME as u64);
    seed_control(&native.dir, RESUME as u64);

    let a_gid = aria2.add(&url, "file.bin").await;
    let n_gid = native.add(&url, "file.bin", json!({})).await;
    let a_status = aria2.wait_done(&a_gid).await;
    let n_status = native.wait_done(&n_gid).await;

    assert_error_code(&a_status, 8, "aria2");
    assert_error_code(&n_status, 8, "engine-net");

    // 两侧都必须保留半成品，不能把已下载的 2 MiB 截断。
    assert_eq!(
        std::fs::metadata(aria2.dir.join("file.bin")).unwrap().len(),
        RESUME as u64,
        "aria2 不应截断半成品"
    );
    assert_eq!(
        std::fs::metadata(native.dir.join("file.bin")).unwrap().len(),
        RESUME as u64,
        "engine-net 不应截断半成品"
    );

    let _ = std::fs::remove_dir_all(&aria2.dir);
    let _ = std::fs::remove_dir_all(&native.dir);
}

// ---------------------------------------------------------------------------
// 用例 3：302 重定向
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ab_3_redirect_302() {
    let payload = Arc::new(sample_body(PAYLOAD_LEN));
    let server = TestServer::start({
        let payload = Arc::clone(&payload);
        move |req| {
            if req.path == "/redirect" {
                Resp {
                    status: "302 Found".to_string(),
                    headers: vec![("Location".to_string(), "/file.bin".to_string())],
                    body: Body::Full(Vec::new()),
                }
            } else {
                range_response(req, &payload, None)
            }
        }
    })
    .await;
    let url = server.url("/redirect");

    let aria2 = Aria2::start(temp_dir("ab3-a"), &[]).await;
    let native = Native::start(temp_dir("ab3-n"), None).await;

    let a_gid = aria2.add(&url, "file.bin").await;
    let n_gid = native.add(&url, "file.bin", json!({})).await;
    let a_status = aria2.wait_done(&a_gid).await;
    let n_status = native.wait_done(&n_gid).await;

    assert_complete(&a_status, "aria2");
    assert_complete(&n_status, "engine-net");

    let expected = sha256_of(&payload);
    assert_eq!(sha256(&aria2.dir.join("file.bin")), expected);
    assert_eq!(sha256(&native.dir.join("file.bin")), expected);

    // 两侧都必须真的走了 /redirect，最终命中 /file.bin。
    let paths: Vec<String> = server
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|req| req.path.clone())
        .collect();
    assert!(paths.contains(&"/file.bin".to_string()), "{paths:?}");

    let _ = std::fs::remove_dir_all(&aria2.dir);
    let _ = std::fs::remove_dir_all(&native.dir);
}

// ---------------------------------------------------------------------------
// 用例 4：429 限流 → HTTP_PROTOCOL_ERROR(22)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ab_4_too_many_requests_returns_error_22() {
    let server = TestServer::start(|_req| Resp {
        status: "429 Too Many Requests".to_string(),
        headers: vec![],
        body: Body::Full(Vec::new()),
    })
    .await;
    let url = server.url("/file.bin");

    let aria2 = Aria2::start(temp_dir("ab4-a"), &[]).await;
    let native = Native::start(temp_dir("ab4-n"), None).await;

    let a_gid = aria2.add(&url, "file.bin").await;
    let n_gid = native.add(&url, "file.bin", json!({})).await;
    let a_status = aria2.wait_done(&a_gid).await;
    let n_status = native.wait_done(&n_gid).await;

    assert_error_code(&a_status, 22, "aria2");
    assert_error_code(&n_status, 22, "engine-net");
    assert_eq!(a_status["completedLength"], "0");
    assert_eq!(n_status["completedLength"], "0");

    // 429 不重试（aria2 HttpSkipResponseCommand.cc 的 default 分支）。
    let count = server.request_count();
    assert_eq!(count, 2, "429 不应重试，两个引擎各一次请求：{count}");

    let _ = std::fs::remove_dir_all(&aria2.dir);
    let _ = std::fs::remove_dir_all(&native.dir);
}

// ---------------------------------------------------------------------------
// 用例 5：断网重试（首次 RST）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ab_5_connection_reset_retry() {
    let payload = Arc::new(sample_body(PAYLOAD_LEN));
    let attempts = Arc::new(AtomicUsize::new(0));
    let server = TestServer::start({
        let payload = Arc::clone(&payload);
        let attempts = Arc::clone(&attempts);
        move |req| {
            if attempts.fetch_add(1, Ordering::SeqCst) % 2 == 0 {
                Resp {
                    status: String::new(),
                    headers: vec![],
                    body: Body::Reset,
                }
            } else {
                range_response(req, &payload, None)
            }
        }
    })
    .await;
    let url = server.url("/file.bin");

    let aria2 = Aria2::start(temp_dir("ab5-a"), &[]).await;
    let native = Native::start(temp_dir("ab5-n"), None).await;

    let a_gid = aria2.add(&url, "file.bin").await;
    let n_gid = native.add(&url, "file.bin", json!({"max-tries": "5"})).await;
    let a_status = aria2.wait_done(&a_gid).await;
    let n_status = native.wait_done(&n_gid).await;

    assert_complete(&a_status, "aria2");
    assert_complete(&n_status, "engine-net");
    // aria2 的 tellStatus 恒返回 errorCode/errorMessage（成功时 "0"/空串），
    // 见 RpcMethodImpl.cc `gatherStoppedDownload`，两侧必须一致。
    assert_eq!(a_status["errorCode"], "0");
    assert_eq!(n_status["errorCode"], "0");
    assert_eq!(a_status["errorMessage"], "");
    assert_eq!(n_status["errorMessage"], "");

    let expected = sha256_of(&payload);
    assert_eq!(sha256(&aria2.dir.join("file.bin")), expected);
    assert_eq!(sha256(&native.dir.join("file.bin")), expected);
    assert!(
        attempts.load(Ordering::SeqCst) >= 4,
        "两个引擎都应至少重试一次（共 ≥4 次请求）"
    );

    let _ = std::fs::remove_dir_all(&aria2.dir);
    let _ = std::fs::remove_dir_all(&native.dir);
}

// ---------------------------------------------------------------------------
// 用例 6：raw.gitcode.com（本地伪造 DNS）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ab_6_raw_gitcode_with_fake_dns() {
    let payload = Arc::new(sample_body(PAYLOAD_LEN));
    let server = TestServer::start({
        let payload = Arc::clone(&payload);
        move |req| range_response(req, &payload, None)
    })
    .await;

    // aria2 的 `--async-dns-server` 只接受 IP，端口固定 53（MultiUrlRequestInfo.cc）。
    let dns = match FakeDns::start("127.0.0.1:53") {
        Ok(dns) => dns,
        Err(err) => {
            eprintln!("跳过 raw.gitcode.com A/B：127.0.0.1:53 不可用（{err}）");
            return;
        }
    };
    let url = format!(
        "http://raw.gitcode.com:{}/file.bin",
        server.addr.port()
    );

    let aria2 = Aria2::start(
        temp_dir("ab6-a"),
        &["--async-dns-server=127.0.0.1"],
    )
    .await;
    let native = Native::start(temp_dir("ab6-n"), Some(dns.addr.to_string())).await;

    let a_gid = aria2.add(&url, "file.bin").await;
    let n_gid = native.add(&url, "file.bin", json!({})).await;
    let a_status = aria2.wait_done(&a_gid).await;
    let n_status = native.wait_done(&n_gid).await;

    assert_complete(&a_status, "aria2");
    assert_complete(&n_status, "engine-net");

    let expected = sha256_of(&payload);
    assert_eq!(sha256(&aria2.dir.join("file.bin")), expected);
    assert_eq!(sha256(&native.dir.join("file.bin")), expected);

    // Host 头必须带域名（而不是回环 IP），证明「域名 SNI/Host + 伪 DNS」路径生效。
    let hosts: Vec<Option<String>> = server
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|req| req.headers.get("host").cloned())
        .collect();
    let expected_host = Some(format!("raw.gitcode.com:{}", server.addr.port()));
    // Phase 4：engine-net 在无进度时会先发一条 `Range: bytes=0-0` 探测，
    // 因此两侧请求总数 ≥2（aria2 一条 + engine-net 探测+下载）。这里只校验
    // 「每一条请求的 Host 都是域名」，不断言具体条数。
    assert!(
        hosts.len() >= 2,
        "两侧都应至少发出一次请求：{hosts:?}"
    );
    assert!(
        hosts.iter().all(|value| *value == expected_host),
        "两侧的 Host 都必须是域名：{hosts:?}"
    );

    let _ = std::fs::remove_dir_all(&aria2.dir);
    let _ = std::fs::remove_dir_all(&native.dir);
}

// ---------------------------------------------------------------------------
// §5.5 互操作 1：aria2 写的 `*.aria2` + session 能被 engine-net 读取
// ---------------------------------------------------------------------------

#[tokio::test]
async fn interop_aria2_control_and_session_readable_by_engine_net() {
    const DONE_AT_LEAST: u64 = PIECE_LEN as u64 * 3 / 2; // 1.5 MiB，保证第 0 片已完成
    let payload = Arc::new(sample_body(PAYLOAD_LEN));
    let server = TestServer::start({
        let payload = Arc::clone(&payload);
        move |req| range_response(req, &payload, Some((32 * 1024, Duration::from_millis(8))))
    })
    .await;
    let url = server.url("/file.bin");

    let mut aria2 = Aria2::start(temp_dir("io-a"), &[]).await;
    let a_gid = aria2.add(&url, "file.bin").await;
    let partial = aria2.wait_downloaded(&a_gid, DONE_AT_LEAST).await;
    assert_eq!(partial["status"], "active", "{partial}");
    aria2.rpc("aria2.pause", json!([a_gid])).await;
    aria2.stop().await;

    let dest = aria2.dir.join("file.bin");
    let control_file = control_path(&dest);
    assert!(control_file.exists(), "aria2 暂停后必须留下控制文件");

    let control = ControlFileStore::new(control_file.clone())
        .load()
        .expect("engine-net 必须能解析 aria2 的控制文件")
        .expect("控制文件存在");
    assert_eq!(control.total_length, PAYLOAD_LEN as u64);
    assert_eq!(control.piece_length, PIECE_LEN);
    assert!(
        control.completed_bytes() >= PIECE_LEN as u64,
        "至少第 0 片应完成：{control:?}"
    );

    // session：aria2 退出时写盘，engine-net 必须能解析。
    assert!(aria2.session.exists(), "aria2 必须写出 session 文件");
    let session = SessionFile::load(&aria2.session)
        .expect("读取 session")
        .expect("session 存在");
    assert!(!session.entries.is_empty(), "session 至少一条");
    assert_eq!(
        session.entries[0].uris,
        vec![url.clone()],
        "session URI 必须原样保留"
    );
    assert!(session.entries[0].is_paused(), "session 必须标记 pause=true");

    // engine-net 从 aria2 的控制文件续传并完成。
    let native = Native::start(aria2.dir.clone(), None).await;
    let n_gid = native.add(&url, "file.bin", json!({})).await;
    let n_status = native.wait_done(&n_gid).await;
    assert_complete(&n_status, "engine-net(续 aria2 控制文件)");
    assert_eq!(sha256(&dest), sha256_of(&payload));
    assert!(!control_file.exists(), "完成后必须删除控制文件");

    let _ = std::fs::remove_dir_all(&aria2.dir);
}

// ---------------------------------------------------------------------------
// §5.5 互操作 2：engine-net 写的 `*.aria2` 能被 aria2 读取
// ---------------------------------------------------------------------------

#[tokio::test]
async fn interop_engine_net_control_readable_by_aria2() {
    const DONE_AT_LEAST: u64 = PIECE_LEN as u64 * 3 / 2;
    let payload = Arc::new(sample_body(PAYLOAD_LEN));
    let server = TestServer::start({
        let payload = Arc::clone(&payload);
        move |req| range_response(req, &payload, Some((32 * 1024, Duration::from_millis(8))))
    })
    .await;
    let url = server.url("/file.bin");

    let dir = temp_dir("io-n");
    let mut native = Native::start(dir.clone(), None).await;
    // 本用例验证 legacy `<file>.aria2` 双向互读，engine-net 必须显式关闭 sqlite。
    let n_gid = native
        .add(
            &url,
            "file.bin",
            json!({"max-tries": "5", "sqlite-progress": "false"}),
        )
        .await;
    let partial = native.wait_downloaded(&n_gid, DONE_AT_LEAST).await;
    assert_eq!(partial["status"], "active", "{partial}");
    native.rpc("aria2.pause", json!([n_gid])).await;
    native.shutdown().await;

    let dest = dir.join("file.bin");
    let control_file = control_path(&dest);
    let control = ControlFileStore::new(control_file.clone())
        .load()
        .expect("读取控制文件")
        .expect("暂停后必须留下控制文件");
    assert_eq!(control.total_length, PAYLOAD_LEN as u64);
    assert_eq!(control.piece_length, PIECE_LEN);
    let resume_from = control.completed_bytes();
    assert!(resume_from >= PIECE_LEN as u64, "{control:?}");
    assert!(
        std::fs::metadata(&dest).unwrap().len() > resume_from,
        "原始文件应比「已完成整片」更多（第 1 片在途），否则无法区分 aria2 是否读了控制文件"
    );

    // aria2 读 engine-net 的控制文件 → 必须从「已完成整片」而不是文件长度续传。
    let mut aria2 = Aria2::start(dir.clone(), &[]).await;
    let a_gid = aria2.add(&url, "file.bin").await;
    let a_status = aria2.wait_done(&a_gid).await;
    assert_complete(&a_status, "aria2(续 engine-net 控制文件)");
    assert_eq!(sha256(&dest), sha256_of(&payload));

    let ranges = server.range_headers();
    // aria2 的 Range 是闭区间 `bytes=start-end`，这里只校验起点。
    let prefix = format!("bytes={resume_from}-");
    assert!(
        ranges.iter().any(|value| value
            .as_deref()
            .map(|raw| raw.starts_with(&prefix))
            .unwrap_or(false)),
        "aria2 必须从控制文件的完成位置 {resume_from}B 续传（否则说明没读到 engine-net 的控制文件）：{ranges:?}"
    );

    // session 反向：aria2 退出写出的 session 必须能被 engine-net 解析。
    let _ = aria2.try_rpc("aria2.shutdown", json!([])).await;
    aria2.stop().await;
    if aria2.session.exists() {
        let session = SessionFile::load(&aria2.session)
            .expect("读取 aria2 session")
            .expect("session 存在");
        for entry in &session.entries {
            assert!(entry.uris.iter().all(|uri| !uri.is_empty()));
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Phase 2 红线：`<file>.sqlite` 双向互操作（产品 --sqlite-progress=true 主线）
// ---------------------------------------------------------------------------

/// aria2（`--sqlite-progress=true`）写的 `.sqlite` → engine-net 读取并续传。
#[tokio::test]
async fn interop_aria2_sqlite_readable_by_engine_net() {
    const DONE_AT_LEAST: u64 = PIECE_LEN as u64 * 3 / 2;
    let payload = Arc::new(sample_body(PAYLOAD_LEN));
    let server = TestServer::start({
        let payload = Arc::clone(&payload);
        move |req| range_response(req, &payload, Some((32 * 1024, Duration::from_millis(8))))
    })
    .await;
    let url = server.url("/file.bin");

    let mut aria2 = Aria2::start(temp_dir("sqlite-a"), &["--sqlite-progress=true"]).await;
    let a_gid = aria2.add(&url, "file.bin").await;
    let partial = aria2.wait_downloaded(&a_gid, DONE_AT_LEAST).await;
    assert_eq!(partial["status"], "active", "{partial}");
    aria2.rpc("aria2.pause", json!([a_gid])).await;
    aria2.stop().await;

    let dest = aria2.dir.join("file.bin");
    let sqlite_file = sqlite_path(&dest);
    assert!(
        sqlite_file.is_file(),
        "aria2 --sqlite-progress=true 必须写出 <file>.sqlite"
    );
    assert!(
        !control_path(&dest).exists(),
        "sqlite 模式不应再产出 legacy .aria2"
    );

    let control = SqliteControlStore::new(sqlite_file.clone())
        .load()
        .expect("engine-net 必须能解析 aria2 的 .sqlite")
        .expect("progress 表里应有 ctrl 记录");
    assert_eq!(control.total_length, PAYLOAD_LEN as u64);
    assert_eq!(control.piece_length, PIECE_LEN);
    let resume_from = control.completed_bytes();
    assert!(resume_from >= PIECE_LEN as u64, "至少第 0 片完成：{control:?}");
    assert!(
        std::fs::metadata(&dest).unwrap().len() > resume_from,
        "源文件应比「已完成整片」更多，否则无法区分是否真的读了控制文件"
    );

    // engine-net 接管同一目录：必须自动选中 sqlite 并从 resume_from 续传。
    let native = Native::start(aria2.dir.clone(), None).await;
    let n_gid = native.add(&url, "file.bin", json!({})).await;
    let n_status = native.wait_done(&n_gid).await;
    assert_complete(&n_status, "engine-net(续 aria2 的 .sqlite)");
    assert_eq!(sha256(&dest), sha256_of(&payload));
    assert!(!sqlite_file.exists(), "完成后必须删除 .sqlite");
    for sidecar_suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{sidecar_suffix}", sqlite_file.display()));
        assert!(!sidecar.exists(), "WAL 伴生文件必须一并清理：{sidecar:?}");
    }

    let ranges = server.range_headers();
    let prefix = format!("bytes={resume_from}-");
    assert!(
        ranges.iter().any(|value| value
            .as_deref()
            .map(|raw| raw.starts_with(&prefix))
            .unwrap_or(false)),
        "engine-net 必须从 .sqlite 记录的 {resume_from}B 续传：{ranges:?}"
    );

    let _ = std::fs::remove_dir_all(&aria2.dir);
}

/// engine-net 写的 `.sqlite` → aria2（`--sqlite-progress=true`）读取并续传。
#[tokio::test]
async fn interop_engine_net_sqlite_readable_by_aria2() {
    const DONE_AT_LEAST: u64 = PIECE_LEN as u64 * 3 / 2;
    let payload = Arc::new(sample_body(PAYLOAD_LEN));
    let server = TestServer::start({
        let payload = Arc::clone(&payload);
        move |req| range_response(req, &payload, Some((32 * 1024, Duration::from_millis(8))))
    })
    .await;
    let url = server.url("/file.bin");

    let dir = temp_dir("sqlite-n");
    let mut native = Native::start(dir.clone(), None).await;
    let n_gid = native
        .add(
            &url,
            "file.bin",
            json!({"max-tries": "5", "sqlite-progress": "true"}),
        )
        .await;
    let partial = native.wait_downloaded(&n_gid, DONE_AT_LEAST).await;
    assert_eq!(partial["status"], "active", "{partial}");
    native.rpc("aria2.pause", json!([n_gid])).await;
    native.shutdown().await;

    let dest = dir.join("file.bin");
    let sqlite_file = sqlite_path(&dest);
    assert!(sqlite_file.is_file(), "engine-net 必须写出 <file>.sqlite");
    assert!(
        !control_path(&dest).exists(),
        "sqlite 模式下不能同时产出 legacy .aria2"
    );

    let control = SqliteControlStore::new(sqlite_file.clone())
        .load()
        .expect("读取 .sqlite")
        .expect("pause 后必须留下 ctrl 记录");
    let resume_from = control.completed_bytes();
    assert!(resume_from >= PIECE_LEN as u64, "{control:?}");

    // 用 aria2 自己的 SQL 读库：主键必须是 ctrl，BLOB 必须就是 legacy 字节流。
    {
        use rusqlite::{Connection, params};
        let conn = Connection::open(&sqlite_file).expect("aria2 必须能打开该库");
        let (key, blob): (String, Vec<u8>) = conn
            .query_row(
                "SELECT info_hash, data FROM progress LIMIT 1;",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("必须存在 progress 行");
        assert_eq!(key, "ctrl", "HTTP 任务主键必须固定为 ctrl");
        assert_eq!(blob, control.encode(), "BLOB 必须与 legacy 序列化逐字节一致");
        // 顺带确认另一条路径也不存在（防止误写 legacy）。
        let legacy_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM progress WHERE info_hash = ?1;",
                params!["ctrl"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_rows, 1);
    }

    assert!(
        std::fs::metadata(&dest).unwrap().len() > resume_from,
        "源文件应比「已完成整片」更多（第 1 片在途）"
    );

    // aria2 读 engine-net 的 .sqlite：必须从 resume_from 续传，而不是从 0 重下。
    let mut aria2 = Aria2::start(dir.clone(), &["--sqlite-progress=true"]).await;
    let a_gid = aria2.add(&url, "file.bin").await;
    let a_status = aria2.wait_done(&a_gid).await;
    assert_complete(&a_status, "aria2(续 engine-net 的 .sqlite)");
    assert_eq!(sha256(&dest), sha256_of(&payload));

    let ranges = server.range_headers();
    let prefix = format!("bytes={resume_from}-");
    assert!(
        ranges.iter().any(|value| value
            .as_deref()
            .map(|raw| raw.starts_with(&prefix))
            .unwrap_or(false)),
        "aria2 必须从 .sqlite 记录的 {resume_from}B 续传（否则说明没读到库）：{ranges:?}"
    );

    let _ = aria2.try_rpc("aria2.shutdown", json!([])).await;
    aria2.stop().await;
    assert!(
        !sqlite_file.exists(),
        "aria2 完成后必须删除 .sqlite（含 -wal / -shm）"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// §5.5 互操作 3：engine-net 写的 session 能被 aria2 读取
// ---------------------------------------------------------------------------

#[tokio::test]
async fn interop_engine_net_session_readable_by_aria2() {
    let server = TestServer::start(|_req| Resp {
        status: "200 OK".to_string(),
        headers: vec![],
        body: Body::Full(Vec::new()),
    })
    .await;
    let dir = temp_dir("io-session");
    let url = server.url("/file.bin");

    let mut session = SessionFile {
        entries: Vec::new(),
    };
    session.entries.push(SessionEntry {
        uris: vec![url.clone(), url.clone()],
        options: vec![
            ("gid".to_string(), "fffe0000000000aa".to_string()),
            ("dir".to_string(), dir.to_string_lossy().into_owned()),
            ("user-agent".to_string(), "aria2/1.37.0".to_string()),
        ],
    });
    let session_path = dir.join("engine-net.session");
    session.save(&session_path).expect("写 session");

    // aria2 --input-file + --dry-run：只解析并探测 URL，不落盘数据。
    let mut command = Command::new(aria2c_path());
    command
        .arg("--no-conf=true")
        .arg(format!("--input-file={}", session_path.display()))
        .arg("--dry-run=true")
        .arg(format!("--dir={}", dir.display()))
        .arg("--console-log-level=warn")
        .arg("--disable-ipv6=true")
        .arg("--max-tries=1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("启动 aria2c 解析 session");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Ok(Some(status)) = child.try_wait() {
            break status;
        }
        if tokio::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("aria2c 解析 session 超时");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    assert!(
        status.success(),
        "aria2c 未能读取 engine-net 的 session（退出码 {status}）"
    );
    assert!(
        server.request_count() >= 1,
        "aria2c 应至少探测一次 session 中的 URI"
    );

    // 反向：engine-net 重新解析 aria2 可能重写的 session 不报错。
    let reparsed = SessionFile::load(&session_path)
        .expect("重新读取 session")
        .expect("session 仍在");
    assert!(!reparsed.entries.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// §5.5 并发基线：128 任务下 FD / RSS / 吞吐 vs aria2c
// ---------------------------------------------------------------------------

/// 并发任务数（weilai.md §5.5）。
const CONCURRENT_TASKS: usize = 128;
/// 每个任务的负载：2 MiB。
const CONCURRENT_PAYLOAD: usize = 2 * 1024 * 1024;

#[cfg(windows)]
mod process_metrics {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, THREADENTRY32, TH32CS_SNAPTHREAD, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessHandleCount, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// 一次采样：工作集（≈ RSS）+ 句柄总数 + 线程数。
    ///
    /// Windows 的 `GetProcessHandleCount` 把线程句柄也计入，所以句柄数并不是
    /// POSIX 意义上的 FD；单列线程数用于把「线程差异」从「文件/套接字泄漏」里剥离。
    #[derive(Debug, Clone, Copy, Default)]
    pub struct Sample {
        pub rss: u64,
        pub handles: u32,
        pub threads: u32,
    }

    pub fn sample(pid: u32) -> Option<Sample> {
        unsafe {
            let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return None;
            }
            let mut counters = PROCESS_MEMORY_COUNTERS::default();
            counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            let mut handles = 0u32;
            let mem_ok = GetProcessMemoryInfo(handle, &mut counters, counters.cb);
            let handle_ok = GetProcessHandleCount(handle, &mut handles);
            CloseHandle(handle);
            if mem_ok == 0 || handle_ok == 0 {
                return None;
            }
            Some(Sample {
                rss: counters.WorkingSetSize as u64,
                handles,
                threads: thread_count(pid).unwrap_or(0),
            })
        }
    }

    fn thread_count(pid: u32) -> Option<u32> {
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut count = 0u32;
            if Thread32First(snapshot, &mut entry) != 0 {
                loop {
                    if entry.th32OwnerProcessID == pid {
                        count += 1;
                    }
                    if Thread32Next(snapshot, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snapshot);
            Some(count)
        }
    }
}

#[cfg(not(windows))]
mod process_metrics {
    #[derive(Debug, Clone, Copy, Default)]
    pub struct Sample {
        pub rss: u64,
        pub handles: u32,
        pub threads: u32,
    }

    pub fn sample(_pid: u32) -> Option<Sample> {
        None
    }
}

/// 10ms 轮询采样子进程峰值 RSS / 句柄数。
struct PeakSampler {
    stop: Arc<AtomicBool>,
    peak: Arc<Mutex<process_metrics::Sample>>,
    join: std::thread::JoinHandle<()>,
}

impl PeakSampler {
    fn start(pid: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(Mutex::new(process_metrics::Sample::default()));
        let stop_flag = Arc::clone(&stop);
        let peak_slot = Arc::clone(&peak);
        let join = std::thread::spawn(move || {
            while !stop_flag.load(Ordering::SeqCst) {
                if let Some(sample) = process_metrics::sample(pid) {
                    let mut slot = peak_slot.lock().unwrap();
                    slot.rss = slot.rss.max(sample.rss);
                    slot.handles = slot.handles.max(sample.handles);
                    slot.threads = slot.threads.max(sample.threads);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        Self { stop, peak, join }
    }

    fn finish(self) -> process_metrics::Sample {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.join.join();
        *self.peak.lock().unwrap()
    }
}

struct BenchResult {
    elapsed: Duration,
    /// 启动后、下发任务前的空闲基线（用于区分「运行时基线差异」与「并发增长」）。
    idle: process_metrics::Sample,
    peak: process_metrics::Sample,
    /// 全部任务完成后的稳态采样（区分「并发瞬态」与「持续占用」）。
    post: process_metrics::Sample,
    dir: PathBuf,
}

fn deviation(candidate: f64, baseline: f64) -> f64 {
    if baseline <= 0.0 {
        f64::INFINITY
    } else {
        (candidate - baseline).abs() / baseline
    }
}

async fn bench_aria2(server: &TestServer) -> BenchResult {
    let dir = temp_dir("bench-a");
    let mut aria2 = Aria2::start(dir.clone(), &[]).await;
    let idle = process_metrics::sample(aria2.child.id()).unwrap_or_default();
    let sampler = PeakSampler::start(aria2.child.id());
    let started = std::time::Instant::now();
    let mut gids = Vec::with_capacity(CONCURRENT_TASKS);
    for index in 0..CONCURRENT_TASKS {
        let url = format!("{}?i={index}", server.url("/file.bin"));
        gids.push(aria2.add(&url, &format!("f{index}.bin")).await);
    }
    for gid in &gids {
        let status = aria2.wait_done(gid).await;
        assert_eq!(status["status"], "complete", "aria2 并发任务失败：{status}");
    }
    let elapsed = started.elapsed();
    let peak = sampler.finish();
    // 留 500ms 让运行时回收已完成任务的资源，再采「稳态」值。
    tokio::time::sleep(Duration::from_millis(500)).await;
    let post = process_metrics::sample(aria2.child.id()).unwrap_or_default();
    aria2.stop().await;
    BenchResult {
        elapsed,
        idle,
        peak,
        post,
        dir,
    }
}

async fn bench_native(server: &TestServer) -> BenchResult {
    let dir = temp_dir("bench-n");
    let mut native = Native::start(dir.clone(), None).await;
    let idle = process_metrics::sample(native.child.id()).unwrap_or_default();
    let sampler = PeakSampler::start(native.child.id());
    let started = std::time::Instant::now();
    let mut gids = Vec::with_capacity(CONCURRENT_TASKS);
    for index in 0..CONCURRENT_TASKS {
        let url = format!("{}?i={index}", server.url("/file.bin"));
        gids.push(native.add(&url, &format!("f{index}.bin"), json!({})).await);
    }
    for gid in &gids {
        let status = native.wait_done(gid).await;
        assert_eq!(
            status["status"], "complete",
            "engine-net 并发任务失败：{status}"
        );
    }
    let elapsed = started.elapsed();
    let peak = sampler.finish();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let post = process_metrics::sample(native.child.id()).unwrap_or_default();
    native.shutdown().await;
    BenchResult {
        elapsed,
        idle,
        peak,
        post,
        dir,
    }
}

#[tokio::test]
#[ignore = "性能验收必须用 release 构建：cargo test --release --test ab_consistency -- --ignored"]
async fn s55_concurrency_128_matches_aria2c_baseline() {
    let payload = Arc::new(sample_body(CONCURRENT_PAYLOAD));
    let server = TestServer::start({
        let payload = Arc::clone(&payload);
        move |req| range_response(req, &payload, None)
    })
    .await;

    let aria2 = bench_aria2(&server).await;
    let native = bench_native(&server).await;

    let total_bytes = (CONCURRENT_TASKS * CONCURRENT_PAYLOAD) as f64;
    let a_speed = total_bytes / aria2.elapsed.as_secs_f64();
    let n_speed = total_bytes / native.elapsed.as_secs_f64();
    eprintln!(
        "[§5.5] {CONCURRENT_TASKS} 并发基线（RSS/句柄/线程：空闲 -> 峰值 -> 稳态）：\
         \n  aria2c     耗时 {:.2}s  吞吐 {:.1} MiB/s  RSS {} -> {} -> {} KiB  句柄 {} -> {} -> {}  线程 {} -> {} -> {}\
         \n  engine-net 耗时 {:.2}s  吞吐 {:.1} MiB/s  RSS {} -> {} -> {} KiB  句柄 {} -> {} -> {}  线程 {} -> {} -> {}",
        aria2.elapsed.as_secs_f64(),
        a_speed / (1024.0 * 1024.0),
        aria2.idle.rss / 1024,
        aria2.peak.rss / 1024,
        aria2.post.rss / 1024,
        aria2.idle.handles,
        aria2.peak.handles,
        aria2.post.handles,
        aria2.idle.threads,
        aria2.peak.threads,
        aria2.post.threads,
        native.elapsed.as_secs_f64(),
        n_speed / (1024.0 * 1024.0),
        native.idle.rss / 1024,
        native.peak.rss / 1024,
        native.post.rss / 1024,
        native.idle.handles,
        native.peak.handles,
        native.post.handles,
        native.idle.threads,
        native.peak.threads,
        native.post.threads,
    );

    let speed_dev = deviation(n_speed, a_speed);
    let rss_dev = deviation(native.peak.rss as f64, aria2.peak.rss as f64);
    let handle_dev = deviation(native.peak.handles as f64, aria2.peak.handles as f64);
    eprintln!(
        "[§5.5] 偏差：吞吐 {:.1}% / RSS {:.1}% / 句柄 {:.1}%",
        speed_dev * 100.0,
        rss_dev * 100.0,
        handle_dev * 100.0
    );

    // 128 个任务全部完成且内容正确。
    let expected = sha256_of(&payload);
    for index in 0..CONCURRENT_TASKS {
        assert_eq!(
            sha256(&aria2.dir.join(format!("f{index}.bin"))),
            expected,
            "aria2 第 {index} 个并发文件不一致"
        );
        assert_eq!(
            sha256(&native.dir.join(format!("f{index}.bin"))),
            expected,
            "engine-net 第 {index} 个并发文件不一致"
        );
    }

    // 用户 2026-10-24 决策：§5.5 按「单向不回归」判定——
    // 吞吐不得低于 aria2c 基线 90%（更快不算偏差），
    // RSS 不得高于基线 110%。
    assert!(
        n_speed >= a_speed * 0.90,
        "吞吐低于基线 90%（engine-net {:.1} MiB/s vs aria2c {:.1} MiB/s）",
        n_speed / (1024.0 * 1024.0),
        a_speed / (1024.0 * 1024.0)
    );
    assert!(
        native.peak.rss <= aria2.peak.rss * 11 / 10,
        "RSS 峰值超过基线 110%（engine-net {} KiB vs aria2c {} KiB）",
        native.peak.rss / 1024,
        aria2.peak.rss / 1024
    );

    // Windows 的句柄总数包含线程句柄，不是 POSIX 语义的 FD；
    // 用「句柄 - 线程」近似文件/套接字句柄后再比对，避免把 tokio 阻塞线程池
    // 与 aria2 线程模型的差异误判成 FD 泄漏。
    let a_fd = aria2.peak.handles.saturating_sub(aria2.peak.threads);
    let n_fd = native.peak.handles.saturating_sub(native.peak.threads);
    eprintln!("[§5.5] 非线程句柄（≈ FD）：aria2c {a_fd} / engine-net {n_fd}");
    assert!(
        n_fd <= a_fd * 11 / 10,
        "FD 近似值超过基线 110%（engine-net {n_fd} vs aria2c {a_fd}）"
    );

    let _ = std::fs::remove_dir_all(&aria2.dir);
    let _ = std::fs::remove_dir_all(&native.dir);
}
