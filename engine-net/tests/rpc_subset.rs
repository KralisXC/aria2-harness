//! 5.4.5 RPC 最小子集：addUri / tellStatus / pause / unpause / remove /
//! getVersion / getGlobalOption / changeGlobalOption / system.multicall。
//!
//! 全部走本地 TCP 测试服务器，不访问外网。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engine_net::dns::resolver::{DnsConfig, DnsResolver};
use engine_net::progress::control::ControlFileStore;
use engine_net::rpc::handle_json;
use engine_net::rpc::methods::Engine;
use engine_net::task::registry::{TaskStatus, control_path};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

#[derive(Debug, Clone)]
struct Req {
    path: String,
    headers: HashMap<String, String>,
}

enum Body {
    Full(Vec<u8>),
    /// 分块慢速发送（用于制造可暂停的在途下载）。
    Slow {
        data: Vec<u8>,
        chunk: usize,
        delay: Duration,
    },
    /// 立即 RST（模拟断网 / 连接重置）。
    Reset,
}

struct Resp {
    status: String,
    headers: Vec<(String, String)>,
    body: Body,
}

struct TestServer {
    addr: SocketAddr,
    handle: JoinHandle<()>,
}

impl TestServer {
    async fn start<F>(handler: F) -> Self
    where
        F: Fn(&Req) -> Resp + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定测试端口");
        let addr = listener.local_addr().expect("本地地址");
        let handler = Arc::new(handler);

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

        Self { addr, handle }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn serve_connection<F>(mut socket: TcpStream, handler: Arc<F>)
where
    F: Fn(&Req) -> Resp + Send + Sync,
{
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
        // SO_LINGER=0 → 直接 RST，模拟中间设备断连。
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
    let path = first.split_whitespace().nth(1).unwrap_or("/").to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    Req { path, headers }
}

fn resolver() -> DnsResolver {
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

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间")
        .as_nanos();
    let mut dir = std::env::temp_dir();
    dir.push(format!("engine-net-rpc-{}-{tag}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("创建临时目录");
    dir
}

fn call(engine: &Engine, method: &str, params: Value) -> Value {
    let line = json!({
        "jsonrpc": "2.0",
        "id": "1",
        "method": method,
        "params": params,
    })
    .to_string();
    serde_json::from_str(&handle_json(engine, &line)).expect("RPC 响应必须是 JSON")
}

async fn wait_for<F>(engine: &Engine, gid: &str, mut done: F) -> Value
where
    F: FnMut(&Value) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let response = call(engine, "aria2.tellStatus", json!([gid]));
        let result = response["result"].clone();
        if done(&result) {
            return result;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("等待 tellStatus 超时：{response}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn sample_body(len: usize) -> Vec<u8> {
    (0..len).map(|index| (index % 251) as u8).collect()
}

#[tokio::test]
async fn add_pause_unpause_remove_roundtrip() {
    // aria2 `--piece-length` 下限是 1 MiB（Phase 2 偏差 1 已对齐），
    // 任务体量取 4 MiB 才能覆盖多分片续传。
    const LEN: usize = 4 * 1024 * 1024;
    let data = Arc::new(sample_body(LEN));
    let requests: Arc<Mutex<Vec<Req>>> = Arc::new(Mutex::new(Vec::new()));
    let records = Arc::clone(&requests);
    let payload = Arc::clone(&data);

    let server = TestServer::start(move |req| {
        records.lock().unwrap().push(req.clone());
        if req.path != "/file.bin" {
            return Resp {
                status: "404 Not Found".to_string(),
                headers: vec![],
                body: Body::Full(Vec::new()),
            };
        }
        let start = req
            .headers
            .get("range")
            .and_then(|value| value.strip_prefix("bytes="))
            .and_then(|value| value.split('-').next())
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0)
            .min(payload.len());
        let slice = payload[start..].to_vec();
        let mut headers = vec![("Content-Type".to_string(), "application/octet-stream".to_string())];
        let status = if start > 0 {
            headers.push((
                "Content-Range".to_string(),
                format!("bytes {}-{}/{}", start, payload.len() - 1, payload.len()),
            ));
            "206 Partial Content"
        } else {
            "200 OK"
        };
        Resp {
            status: status.to_string(),
            headers,
            body: Body::Slow {
                data: slice,
                chunk: 32 * 1024,
                delay: Duration::from_millis(8),
            },
        }
    })
    .await;

    let dir = temp_dir("roundtrip");
    let engine = Engine::new(resolver());
    let url = format!("http://127.0.0.1:{}/file.bin", server.addr.port());

    let added = call(
        &engine,
        "aria2.addUri",
        json!([[
            url
        ], {
            "dir": dir.to_string_lossy(),
            "out": "file.bin",
            "piece-length": "1048576",
            // 本用例校验 legacy `.aria2` 路径，显式关闭 sqlite（默认已为 true）。
            "sqlite-progress": "false",
            "header": ["X-Test: 1"],
            "referer": "http://ref/",
            "user-agent": "my-ua",
        }]),
    );
    let gid = added["result"].as_str().expect("addUri 返回 GID").to_string();
    assert_eq!(gid.len(), 16);
    assert!(gid.starts_with("fffe"), "GID 必须使用 fffe 命名空间：{gid}");

    let partial = wait_for(&engine, &gid, |status| {
        status["status"] == "active"
            && status["completedLength"]
                .as_str()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0)
                >= 1024 * 1024 * 3 / 2
    })
    .await;
    assert_eq!(partial["totalLength"], LEN.to_string());
    assert_eq!(
        partial["files"][0]["path"],
        dir.join("file.bin").to_string_lossy().as_ref()
    );

    // 暂停：返回 GID、状态变 paused、控制文件落盘。
    let paused = call(&engine, "aria2.pause", json!([gid]));
    assert_eq!(paused["result"], gid);
    let status = call(&engine, "aria2.tellStatus", json!([gid]));
    assert_eq!(status["result"]["status"], "paused");

    let control_path = control_path(&dir.join("file.bin"));
    let control = ControlFileStore::new(control_path.clone())
        .load()
        .expect("读取控制文件")
        .expect("暂停后必须写控制文件");
    assert_eq!(control.total_length, LEN as u64);
    assert!(control.completed_bytes() >= 1024 * 1024);
    assert_eq!(control.piece_length, 1024 * 1024);

    // 恢复：从控制文件的已完成位图续传。
    let resumed = call(&engine, "aria2.unpause", json!([gid]));
    assert_eq!(resumed["result"], gid);

    let completed = wait_for(&engine, &gid, |status| status["status"] == "complete").await;
    assert_eq!(completed["totalLength"], LEN.to_string());
    assert_eq!(completed["completedLength"], LEN.to_string());
    assert_eq!(std::fs::read(dir.join("file.bin")).unwrap(), *data);
    assert!(!control_path.exists(), "完成后必须删除控制文件");

    {
        let requests = requests.lock().unwrap();
        assert!(requests.len() >= 2, "应有首次请求 + 续传请求");
        assert!(requests.iter().all(|req| req.headers.get("x-test") == Some(&"1".to_string())));
        assert_eq!(requests[0].headers.get("referer"), Some(&"http://ref/".to_string()));
        assert_eq!(requests[0].headers.get("user-agent"), Some(&"my-ua".to_string()));
        assert!(
            requests.iter().any(|req| req.headers.get("range").is_some()),
            "恢复必须带 Range 头"
        );
    }

    // remove：状态 removed，控制文件不存在。
    let removed = call(&engine, "aria2.remove", json!([gid]));
    assert_eq!(removed["result"], gid);
    let status = call(&engine, "aria2.tellStatus", json!([gid]));
    assert_eq!(status["result"]["status"], "removed");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn error_status_reports_aria2_error_code() {
    let server = TestServer::start(|_| Resp {
        status: "404 Not Found".to_string(),
        headers: vec![],
        body: Body::Full(Vec::new()),
    })
    .await;

    let dir = temp_dir("404");
    let engine = Engine::new(resolver());
    let url = format!("http://127.0.0.1:{}/missing", server.addr.port());

    let added = call(
        &engine,
        "aria2.addUri",
        json!([[url], {"dir": dir.to_string_lossy(), "out": "missing.bin"}]),
    );
    let gid = added["result"].as_str().unwrap().to_string();

    let failed = wait_for(&engine, &gid, |status| status["status"] == "error").await;
    assert_eq!(failed["errorCode"], "3", "404 应映射 RESOURCE_NOT_FOUND");
    assert!(
        failed["errorMessage"]
            .as_str()
            .is_some_and(|message| !message.is_empty())
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn version_global_options_multicall_and_keys_filter() {
    let engine = Engine::new(resolver());

    let version = call(&engine, "aria2.getVersion", json!([]));
    assert_eq!(version["result"]["version"], "1.37.0");
    assert_eq!(version["result"]["_engine"], "native");
    let features = version["result"]["enabledFeatures"].as_array().unwrap();
    assert!(features.iter().any(|value| value == "native-http"));

    let globals = call(&engine, "aria2.getGlobalOption", json!([]));
    // v1.0-beta3：默认下载目录必须是绝对路径（服务可能以 LocalSystem 运行，
    // 相对 `.` 会落到 systemprofile\Downloads）。
    let default_dir = globals["result"]["dir"].as_str().expect("dir 必须是字符串");
    assert!(
        std::path::Path::new(default_dir).is_absolute(),
        "默认 dir 必须是绝对路径，实际为 {default_dir}"
    );
    assert_eq!(globals["result"]["check-certificate"], "true");

    let changed = call(
        &engine,
        "aria2.changeGlobalOption",
        json!([{"dir": "C:/tmp", "check-certificate": "false"}]),
    );
    assert_eq!(changed["result"], "OK");
    let globals = call(&engine, "aria2.getGlobalOption", json!([]));
    assert_eq!(globals["result"]["dir"], "C:/tmp");
    assert_eq!(globals["result"]["check-certificate"], "false");

    let multicall = call(
        &engine,
        "system.multicall",
        json!([[
            {"methodName": "aria2.getVersion", "params": []},
            {"methodName": "aria2.tellStatus", "params": ["fffeffffffffffff"]},
        ]]),
    );
    let results = multicall["result"].as_array().expect("multicall 返回数组");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0][0]["version"], "1.37.0");
    assert_eq!(results[1]["code"], 1, "不存在的 GID 用业务错误码 1");

    // keys 过滤：只返回请求的字段。
    let missing = call(&engine, "aria2.tellStatus", json!(["fffeffffffffffff", ["gid"]]));
    assert_eq!(missing["error"]["code"], 1);
}

#[tokio::test]
async fn retries_after_connection_reset() {
    let data = Arc::new(sample_body(64 * 1024));
    let payload = Arc::clone(&data);
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);

    let server = TestServer::start(move |_req| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            return Resp {
                status: String::new(),
                headers: vec![],
                body: Body::Reset,
            };
        }
        Resp {
            status: "200 OK".to_string(),
            headers: vec![],
            body: Body::Full((*payload).clone()),
        }
    })
    .await;

    let dir = temp_dir("retry");
    let engine = Engine::new(resolver());
    let url = format!("http://127.0.0.1:{}/retry.bin", server.addr.port());
    let added = call(
        &engine,
        "aria2.addUri",
        json!([[url], {"dir": dir.to_string_lossy(), "out": "retry.bin", "max-tries": "5"}]),
    );
    let gid = added["result"].as_str().unwrap().to_string();

    let done = wait_for(&engine, &gid, |status| {
        status["status"] == "complete" || status["status"] == "error"
    })
    .await;
    assert_eq!(done["status"], "complete", "首次 RST 后应自动重试成功：{done}");
    assert_eq!(done["errorCode"], "0");
    assert_eq!(done["errorMessage"], "");
    assert!(attempts.load(Ordering::SeqCst) >= 2, "应至少发起两次连接");
    assert_eq!(std::fs::read(dir.join("retry.bin")).unwrap(), *data);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn tellstatus_keys_filter_only_returns_requested_fields() {
    let payload = Arc::new(sample_body(4096));
    let server = TestServer::start(move |req| {
        if req.path != "/f" {
            return Resp {
                status: "404 Not Found".to_string(),
                headers: vec![],
                body: Body::Full(Vec::new()),
            };
        }
        let start = req
            .headers
            .get("range")
            .and_then(|value| value.strip_prefix("bytes="))
            .and_then(|value| value.split('-').next())
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0)
            .min(payload.len());
        Resp {
            status: "200 OK".to_string(),
            headers: vec![],
            body: Body::Full(payload[start..].to_vec()),
        }
    })
    .await;

    let dir = temp_dir("keys");
    let engine = Engine::new(resolver());
    let url = format!("http://127.0.0.1:{}/f", server.addr.port());
    let added = call(
        &engine,
        "aria2.addUri",
        json!([[url], {"dir": dir.to_string_lossy(), "out": "f.bin"}]),
    );
    let gid = added["result"].as_str().unwrap().to_string();
    wait_for(&engine, &gid, |status| status["status"] == "complete").await;

    let filtered = call(&engine, "aria2.tellStatus", json!([gid, ["gid", "status"]]));
    let object = filtered["result"].as_object().expect("对象");
    assert_eq!(object.len(), 2);
    assert_eq!(object["gid"], gid);
    assert_eq!(object["status"], "complete");

    // 状态枚举与 aria2 字面量一致。
    assert_eq!(TaskStatus::Complete.as_aria2(), "complete");
    let _ = std::fs::remove_dir_all(&dir);
}

/// v1.0-beta3：下载过程中必须推送 `aria2.onDownloadProgress`，
/// 且长度/速度/连接数等数值字段序列化后全部是**字符串**（防前端 NaN / 0）。
#[tokio::test]
async fn progress_events_carry_string_fields() {
    use engine_net::task::event::TaskEventKind;

    let payload = Arc::new(sample_body(4 * 1024 * 1024));
    let body = Arc::clone(&payload);
    let server = TestServer::start(move |_req| Resp {
        status: "200 OK".to_string(),
        headers: vec![],
        body: Body::Slow {
            data: (*body).clone(),
            chunk: 512 * 1024,
            delay: Duration::from_millis(150),
        },
    })
    .await;

    let dir = temp_dir("progress-event");
    let engine = Engine::new(resolver());
    let mut events = engine.subscribe_events();
    let url = format!("http://127.0.0.1:{}/p.bin", server.addr.port());
    let added = call(
        &engine,
        "aria2.addUri",
        json!([[url], {"dir": dir.to_string_lossy(), "out": "p.bin"}]),
    );
    let gid = added["result"].as_str().unwrap().to_string();

    let mut snapshot = None;
    for _ in 0..10 {
        if let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(800), events.recv()).await
        {
            if event.kind == TaskEventKind::Progress {
                snapshot = event.progress;
                break;
            }
        }
    }
    let snapshot = snapshot.expect("下载过程中必须收到 onDownloadProgress");
    let value = engine_net::rpc::types::status_to_value(&snapshot);
    for key in [
        "totalLength",
        "completedLength",
        "downloadSpeed",
        "connections",
        "pieceLength",
        "errorCode",
    ] {
        assert!(value[key].is_string(), "{key} 必须是字符串：{value}");
    }
    assert_eq!(value["gid"], gid);
    assert_eq!(value["totalLength"], (4 * 1024 * 1024).to_string());

    let _ = engine.call("aria2.forceRemove", &[json!(gid)]);
    let _ = std::fs::remove_dir_all(&dir);
}
