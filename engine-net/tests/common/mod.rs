//! Phase 4 测试公共设施：本地 Range 服务器、RPC 调用与临时目录。
//!
//! 服务器是裸 TCP + HTTP/1.1（一次请求一条连接），这样才能精确统计并发连接数、
//! 记录每个非探测 Range 的区间，并按需延迟响应头/慢速发 body。

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engine_net::dns::resolver::{DnsConfig, DnsResolver};
use engine_net::rpc::handle_json;
use engine_net::rpc::methods::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// 数据与摘要
// ---------------------------------------------------------------------------

/// 确定性伪随机内容（LCG），保证 SHA 比对可复现且不是全零。
pub fn make_data(len: usize) -> Arc<Vec<u8>> {
    let mut state: u64 = 0x5DEE_CE66_D1CE_0001;
    let mut data = Vec::with_capacity(len);
    for _ in 0..len {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        data.push((state >> 33) as u8);
    }
    Arc::new(data)
}

pub fn sha256(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn file_sha256(path: &std::path::Path) -> String {
    let data = std::fs::read(path).expect("读取目标文件");
    sha256(&data)
}

// ---------------------------------------------------------------------------
// 本地服务器
// ---------------------------------------------------------------------------

pub struct Req {
    pub path: String,
    /// `Range: bytes=start-end`（end 为 None 表示开放式）。
    pub range: Option<(u64, Option<u64>)>,
}

pub struct Plan {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// 发响应头之前的延迟（制造「等响应头僵死」）。
    pub delay: Duration,
    /// 0 = 一次写完；否则按该块大小慢速发送。
    pub chunk: usize,
    pub chunk_delay: Duration,
    /// 永不响应（等待客户端放弃）。
    pub hang: bool,
}

impl Plan {
    pub fn range(data: &[u8], range: Option<(u64, Option<u64>)>) -> Self {
        let total = data.len() as u64;
        let Some((start, end)) = range else {
            return Self::full(data);
        };
        if start >= total {
            return Self::unsatisfiable(total);
        }
        let end = end.unwrap_or(total - 1).min(total - 1);
        if end < start {
            return Self::unsatisfiable(total);
        }
        Self {
            status: 206,
            headers: vec![(
                "Content-Range".to_string(),
                format!("bytes {start}-{end}/{total}"),
            )],
            body: data[start as usize..=end as usize].to_vec(),
            delay: Duration::ZERO,
            chunk: 0,
            chunk_delay: Duration::ZERO,
            hang: false,
        }
    }

    pub fn full(data: &[u8]) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: data.to_vec(),
            delay: Duration::ZERO,
            chunk: 0,
            chunk_delay: Duration::ZERO,
            hang: false,
        }
    }

    pub fn unsatisfiable(total: u64) -> Self {
        Self {
            status: 416,
            headers: vec![("Content-Range".to_string(), format!("bytes */{total}"))],
            body: Vec::new(),
            delay: Duration::ZERO,
            chunk: 0,
            chunk_delay: Duration::ZERO,
            hang: false,
        }
    }

    pub fn hang() -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: Vec::new(),
            delay: Duration::ZERO,
            chunk: 0,
            chunk_delay: Duration::ZERO,
            hang: true,
        }
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    pub fn with_slow(mut self, chunk: usize, chunk_delay: Duration) -> Self {
        self.chunk = chunk;
        self.chunk_delay = chunk_delay;
        self
    }
}

#[derive(Default)]
pub struct Metrics {
    active: AtomicUsize,
    max_active: AtomicUsize,
    total_requests: AtomicUsize,
    pieces: Mutex<Vec<(u64, u64)>>,
}

impl Metrics {
    pub fn max_active(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }

    pub fn total_requests(&self) -> usize {
        self.total_requests.load(Ordering::SeqCst)
    }

    /// 非探测请求的区间（探测是 `bytes=0-0`，单独排除）。
    pub fn pieces(&self) -> Vec<(u64, u64)> {
        self.pieces.lock().unwrap().clone()
    }

    pub fn clear_pieces(&self) {
        self.pieces.lock().unwrap().clear();
    }

    pub fn piece_requests(&self, start: u64) -> usize {
        self.pieces()
            .iter()
            .filter(|(piece_start, _)| *piece_start == start)
            .count()
    }

    fn record(&self, start: u64, end: u64) {
        self.pieces.lock().unwrap().push((start, end));
    }
}

type Behavior = Arc<dyn Fn(&Req, &Arc<Metrics>, &[u8]) -> Plan + Send + Sync>;

pub struct Server {
    pub addr: SocketAddr,
    pub metrics: Arc<Metrics>,
    handle: JoinHandle<()>,
}

impl Server {
    /// `behavior` 拿到的第三个参数是完整文件内容（用于按 Range 切片）。
    pub async fn start<F>(data: Arc<Vec<u8>>, behavior: F) -> Self
    where
        F: Fn(&Req, &Arc<Metrics>, &[u8]) -> Plan + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定测试端口");
        let addr = listener.local_addr().expect("本地地址");
        let behavior: Behavior = Arc::new(behavior);
        let metrics = Arc::new(Metrics::default());

        let shared = Arc::clone(&metrics);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let data = Arc::clone(&data);
                let metrics = Arc::clone(&shared);
                let behavior = Arc::clone(&behavior);
                tokio::spawn(async move {
                    serve_connection(socket, data, metrics, behavior).await;
                });
            }
        });

        Self {
            addr,
            metrics,
            handle,
        }
    }

    pub fn url(&self, name: &str) -> String {
        format!("http://{}/{name}", self.addr)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn serve_connection(
    mut socket: TcpStream,
    data: Arc<Vec<u8>>,
    metrics: Arc<Metrics>,
    behavior: Behavior,
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        match socket.read(&mut chunk).await {
            Ok(0) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return,
        }
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            return;
        }
    }

    let req = parse_request(&buf);
    metrics.total_requests.fetch_add(1, Ordering::SeqCst);

    // 探测（`bytes=0-0`）不计入并发：它总是在调度开始前完成，计数会干扰
    // 「split=1 必须串行」这类断言。
    let is_probe = req.range == Some((0, Some(0)));
    let counted = if is_probe {
        false
    } else {
        let current = metrics.active.fetch_add(1, Ordering::SeqCst) + 1;
        metrics.max_active.fetch_max(current, Ordering::SeqCst);
        true
    };

    if let Some((start, end)) = req.range {
        if !is_probe {
            let last = data.len().saturating_sub(1) as u64;
            metrics.record(start, end.unwrap_or(last).min(last));
        }
    }

    let plan = behavior(&req, &metrics, &data);
    if plan.hang {
        // 不写任何字节：等客户端因 `--timeout` 放弃后连接关闭。
        let mut discard = [0u8; 8];
        let _ = socket.read(&mut discard).await;
        if counted {
            metrics.active.fetch_sub(1, Ordering::SeqCst);
        }
        return;
    }

    if !plan.delay.is_zero() {
        tokio::time::sleep(plan.delay).await;
    }

    let mut head = format!("HTTP/1.1 {}\r\n", status_line(plan.status));
    head.push_str(&format!("Content-Length: {}\r\n", plan.body.len()));
    head.push_str("Connection: close\r\n");
    for (name, value) in &plan.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");

    let written = if socket.write_all(head.as_bytes()).await.is_err() {
        false
    } else if plan.chunk == 0 {
        socket.write_all(&plan.body).await.is_ok()
    } else {
        let mut ok = true;
        let mut first = true;
        for part in plan.body.chunks(plan.chunk) {
            // 延迟放在两次写之间：写完最后一块立即归还并发计数，避免下一条
            // 连接因「尾部 sleep」被误计成并发。
            if !first && !plan.chunk_delay.is_zero() {
                tokio::time::sleep(plan.chunk_delay).await;
            }
            first = false;
            if socket.write_all(part).await.is_err() {
                ok = false;
                break;
            }
        }
        ok
    };
    if written {
        let _ = socket.flush().await;
        let _ = socket.shutdown().await;
    }
    if counted {
        metrics.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn status_line(status: u16) -> &'static str {
    match status {
        200 => "200 OK",
        206 => "206 Partial Content",
        416 => "416 Range Not Satisfiable",
        other => {
            let _ = other;
            "500 Internal Server Error"
        }
    }
}

fn parse_request(raw: &[u8]) -> Req {
    let text = String::from_utf8_lossy(raw);
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let path = first.split_whitespace().nth(1).unwrap_or("/").to_string();
    let mut range = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("range") {
            range = parse_range(value.trim());
        }
    }
    Req { path, range }
}

fn parse_range(value: &str) -> Option<(u64, Option<u64>)> {
    let rest = value.strip_prefix("bytes=")?;
    let (start, end) = rest.split_once('-')?;
    let start = start.trim().parse::<u64>().ok()?;
    let end = if end.trim().is_empty() {
        None
    } else {
        Some(end.trim().parse::<u64>().ok()?)
    };
    Some((start, end))
}

// ---------------------------------------------------------------------------
// RPC 与临时目录
// ---------------------------------------------------------------------------

pub fn resolver() -> DnsResolver {
    DnsResolver::new(&DnsConfig {
        // 字面 IP 直通，不会真的查这个不可达地址。
        servers: vec!["127.0.0.1:1".parse().unwrap()],
        use_hosts: false,
        timeout: Duration::from_millis(50),
        attempts: 1,
        ..DnsConfig::default()
    })
    .expect("构造 resolver")
}

pub fn engine() -> Arc<Engine> {
    Arc::new(Engine::new(resolver()))
}

pub fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间")
        .as_nanos();
    let mut dir = std::env::temp_dir();
    dir.push(format!("engine-net-p4-{}-{tag}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("创建临时目录");
    dir
}

pub fn call(engine: &Engine, method: &str, params: Value) -> Value {
    let line = json!({
        "jsonrpc": "2.0",
        "id": "1",
        "method": method,
        "params": params,
    })
    .to_string();
    serde_json::from_str(&handle_json(engine, &line)).expect("RPC 响应必须是 JSON")
}

/// `addUri` 并返回 GID；失败时打印 RPC 错误。
pub fn add_uri(
    engine: &Engine,
    url: &str,
    dir: &std::path::Path,
    out: &str,
    options: Value,
) -> String {
    let mut options = match options {
        Value::Object(map) => map,
        _ => panic!("options 必须是对象"),
    };
    options.insert("dir".into(), json!(dir.display().to_string()));
    options.insert("out".into(), json!(out));
    let response = call(engine, "aria2.addUri", json!([[url], options]));
    response["result"]
        .as_str()
        .unwrap_or_else(|| panic!("addUri 失败：{response}"))
        .to_string()
}

/// 等任务进入终态（complete/error/removed），返回最终 tellStatus。
pub async fn wait_terminal(engine: &Engine, gid: &str, timeout: Duration) -> Value {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let response = call(engine, "aria2.tellStatus", json!([gid]));
        let result = response["result"].clone();
        let status = result["status"].as_str().unwrap_or_default();
        if matches!(status, "complete" | "error" | "removed") {
            return result;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("任务 {gid} 未在 {timeout:?} 内结束，最后状态：{result}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// 等 `tellStatus.completedLength` 达到阈值（用于中途暂停）。
pub async fn wait_progress(engine: &Engine, gid: &str, at_least: u64, timeout: Duration) -> u64 {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let response = call(engine, "aria2.tellStatus", json!([gid, ["completedLength"]]));
        let completed: u64 = response["result"]["completedLength"]
            .as_str()
            .and_then(|text| text.parse().ok())
            .unwrap_or(0);
        if completed >= at_least {
            return completed;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("进度未达到 {at_least}（当前 {completed}）");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub fn error_code(status: &Value) -> Option<i64> {
    status["errorCode"].as_str().and_then(|text| text.parse().ok())
}

/// 断言 Range 区间互不重叠（同一区间允许多次出现：抢占后的整片重下）。
pub fn assert_no_overlap(ranges: &[(u64, u64)]) {
    let mut sorted = ranges.to_vec();
    sorted.sort();
    sorted.dedup();
    for pair in sorted.windows(2) {
        assert!(
            pair[0].1 < pair[1].0,
            "Range 重叠：{:?} 与 {:?}",
            pair[0],
            pair[1]
        );
    }
}

/// 断言已去重区间恰好覆盖 `[0, total)` 且不重不漏。
pub fn assert_exact_cover(ranges: &[(u64, u64)], total: u64) {
    let mut sorted = ranges.to_vec();
    sorted.sort();
    sorted.dedup();
    let mut cursor = 0;
    for (start, end) in sorted {
        assert_eq!(start, cursor, "覆盖出现空洞或重叠：{start} != {cursor}");
        cursor = end + 1;
    }
    assert_eq!(cursor, total, "覆盖未到文件末尾");
}

/// 一次性开关：用于「第一次请求才延迟/才失败」的行为。
pub fn once_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}
