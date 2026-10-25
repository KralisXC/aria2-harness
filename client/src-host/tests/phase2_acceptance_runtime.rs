//! Phase 2 运行时验收（**人工触发**，默认 `#[ignore]`）。
//!
//! 需要：管理员终端（High Mandatory）+ 空闲的 6800/6801。执行：
//! `cargo test --features native-http --test phase2_acceptance_runtime -- --ignored --nocapture`
//!
//! 覆盖 Phase2-acceptance.md §8 的机器可执行部分：
//! 1. 网络隔离：6800 / 6801 只允许 127.0.0.1 绑定；
//! 2. 回滚档 `--engine=aria2`：6800 由 aria2c 监听、不启 6801、参数表与 v0.9.4 一致；
//! 3. `--engine=auto`：HTTP→native、BT→aria2，`_engine` 正确、并发可用；
//! 4. 审计：`[route] gid=... engine=...` 落盘；
//! 5. 混合 A/B：aria2 与 native 下载同一负载，SHA256 一致；
//! 6. DB：真实 `data/aria2-harness.db` 迁移到 v2，`engine` 列存在。
//!
//! 所有网络目标均为 127.0.0.1，不发起真实外网请求。

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use aria2_harness_lib::rpc_client::Client;
use aria2_harness_lib::service_protocol::{ServiceResponse, decode_response, encode_request, ServiceRequest};

const PORT: u16 = 6800;
const INNER_PORT: u16 = 6801;
const PAYLOAD_LEN: usize = 2 * 1024 * 1024;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "需管理员权限并占用 6800/6801；用 -- --ignored --nocapture 手动执行"]
async fn phase2_runtime_acceptance() {
    let payload = sample_payload(PAYLOAD_LEN);
    let expected = sha256(&payload);

    // 1) 回滚档：HTTP 下载 + 端口/参数断言。
    let aria2_hash = run_http_stage("aria2", &payload, &expected).await;

    // 2) 原生档：同一负载，SHA256 必须一致（混合 A/B）。
    let native_hash = run_http_stage("native", &payload, &expected).await;
    assert_eq!(aria2_hash, native_hash, "A/B SHA256 不一致（aria2={aria2_hash} native={native_hash}）");
    println!("[A/B] aria2 与 native SHA256 一致：{aria2_hash}");

    // 3) auto：HTTP→native、BT→aria2，混合并发。
    run_auto_stage(&payload, &expected).await;

    // 4) DB schema v2 + engine 列。
    check_db().await;

    // 5) 审计日志落盘（Debug 构建不写 service.log，改为直接捕获 service 的 stdout）。
    check_route_log();

    let _ = std::fs::remove_dir_all(log_dir());
    println!("[完成] Phase 2 运行时验收脚本全部断言通过");
}

// ---------------------------------------------------------------------------
// 阶段 1/2：单引擎 HTTP 下载
// ---------------------------------------------------------------------------

async fn run_http_stage(mode: &str, payload: &[u8], expected: &str) -> String {
    let dir = temp_dir(&format!("http-{mode}"));
    let server = HttpServer::start(payload.to_vec(), false).await;
    let url = server.url("/file.bin");

    let svc = Svc::start(mode).await;
    let rpc = svc.connect().await;

    // 端口隔离断言。
    let rows = wait_listener(PORT).await;
    assert_all_loopback(&rows, PORT);
    assert_not_exposed(PORT).await;
    report_netstat(PORT);
    if mode == "aria2" {
        assert!(
            rows.iter().all(|(_, pid)| *pid == svc.engine_pid),
            "[{mode}] 6800 应由 aria2c 监听（回滚档无代理）：{rows:?}"
        );
        assert!(listeners(INNER_PORT).is_empty(), "[{mode}] 回滚档不应出现 6801");
        let cmd = cmdline(svc.engine_pid);
        println!("[rollback] aria2c 命令行：{cmd}");
        for flag in [
            format!("--rpc-listen-port={PORT}"),
            "--rpc-listen-all=false".to_string(),
            "--split=64".to_string(),
            "--max-connection-per-server=16".to_string(),
            "--file-allocation=falloc".to_string(),
            "--disk-cache=64M".to_string(),
        ] {
            assert!(cmd.contains(&flag), "[rollback] aria2c 命令行缺少 {flag}");
        }
        assert!(!cmd.contains(&format!("{INNER_PORT}")), "[rollback] 回滚档不应出现内部端口");
    } else {
        assert!(
            rows.iter().all(|(_, pid)| *pid == svc.pid),
            "[{mode}] 6800 应由 service（代理）监听：{rows:?}"
        );
        let inner = wait_listener(INNER_PORT).await;
        assert_all_loopback(&inner, INNER_PORT);
        assert!(
            inner.iter().all(|(_, pid)| *pid == svc.engine_pid),
            "[{mode}] 6801 应由 aria2c 监听：{inner:?}"
        );
    }
    println!("[{mode}] 监听 6800={:?} 6801={:?}", listeners(PORT), listeners(INNER_PORT));

    let gid = rpc
        .call("aria2.addUri", vec![json!([url]), json!({"dir": dir.to_string_lossy()})])
        .await
        .expect("addUri");
    let gid = gid.as_str().unwrap().to_string();
    let status = wait_done(&rpc, &gid, Duration::from_secs(120)).await;
    assert_eq!(status["status"], "complete", "[{mode}] 任务未完成：{status}");
    if mode == "native" {
        // 回滚档是纯 aria2c，没有代理层，因此不应有 `_engine`（保持 v0.9.4 原样）。
        assert_eq!(status["_engine"], "native", "[{mode}] _engine 字段不正确：{status}");
    }

    let dest = dir.join("file.bin");
    let digest = sha256_of_file(&dest);
    assert_eq!(digest, expected, "[{mode}] 文件 SHA256 不一致");

    // 控制文件必须清理（.sqlite / .aria2）。
    for suffix in [".sqlite", ".aria2"] {
        let path = PathBuf::from(format!("{}{suffix}", dest.display()));
        assert!(!path.exists(), "[{mode}] 完成后残留 {}", path.display());
    }

    rpc.call("aria2.forceRemove", vec![json!(gid)]).await.ok();
    svc.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
    digest
}

// ---------------------------------------------------------------------------
// 阶段 3：auto 混合路由 + 并发
// ---------------------------------------------------------------------------

async fn run_auto_stage(payload: &[u8], expected: &str) {
    let dir = temp_dir("auto");
    let server = HttpServer::start(payload.to_vec(), true).await;
    let url = server.url("/file.bin");
    let tracker_port = spawn_blackhole().await;

    let svc = Svc::start("auto").await;
    let rpc = svc.connect().await;

    // 先关掉 DHT / LPD，避免 BT 任务触碰真实网络（失败不阻塞验收）。
    if let Err(err) = rpc
        .call(
            "aria2.changeGlobalOption",
            vec![json!({"enable-dht": "false", "bt-enable-lpd": "false"})],
        )
        .await
    {
        println!("[auto][警告] changeGlobalOption 失败（DHT/LPD 可能仍开启）：{err:?}");
    }

    // HTTP 任务（慢速，保证观测窗口内有 active 状态）。
    let http_gid = rpc
        .call(
            "aria2.addUri",
            vec![json!([url]), json!({"dir": dir.to_string_lossy()})],
        )
        .await
        .expect("addUri")
        .as_str()
        .unwrap()
        .to_string();

    // BT 任务：本地 .torrent（tracker 指向黑洞端口，不产生真实外网流量）。
    let torrent = torrent_base64(
        &format!("http://127.0.0.1:{tracker_port}/announce"),
        "btmock.bin",
        4096,
        16384,
    );
    let bt_gid = rpc
        .call(
            "aria2.addTorrent",
            vec![
                json!(torrent),
                json!([]),
                json!({"dir": dir.to_string_lossy()}),
            ],
        )
        .await
        .expect("addTorrent")
        .as_str()
        .unwrap()
        .to_string();

    // 归属断言：HTTP→native，BT→aria2。
    let http_status = rpc.call("aria2.tellStatus", vec![json!(http_gid)]).await.unwrap();
    let bt_status = rpc.call("aria2.tellStatus", vec![json!(bt_gid)]).await.unwrap();
    assert_eq!(http_status["_engine"], "native", "HTTP 未路由到 native：{http_status}");
    assert_eq!(bt_status["_engine"], "aria2", "BT 未路由到 aria2：{bt_status}");
    println!("[auto] HTTP _engine={} BT _engine={}", http_status["_engine"], bt_status["_engine"]);

    // 并发：tellActive 必须同时包含两个 GID（HTTP 慢速仍在 active）。
    let active = rpc.call("aria2.tellActive", vec![]).await.unwrap();
    let gids: Vec<String> = active
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["gid"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(gids.contains(&http_gid), "tellActive 缺 HTTP 任务：{gids:?}");
    assert!(gids.contains(&bt_gid), "tellActive 缺 BT 任务：{gids:?}");
    let engines: Vec<&str> = active
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["_engine"].as_str().unwrap_or("?"))
        .collect();
    println!("[auto] tellActive {} 项，引擎={engines:?}", gids.len());

    // 端口隔离（auto：6800=service，6801=aria2c）。
    let rows = wait_listener(PORT).await;
    assert!(
        rows.iter().all(|(_, pid)| *pid == svc.pid),
        "[auto] 6800 应由 service 监听：{rows:?}"
    );
    assert_all_loopback(&rows, PORT);
    assert_not_exposed(PORT).await;
    let inner = wait_listener(INNER_PORT).await;
    assert!(
        inner.iter().all(|(_, pid)| *pid == svc.engine_pid),
        "[auto] 6801 应由 aria2c 监听：{inner:?}"
    );
    assert_all_loopback(&inner, INNER_PORT);
    report_netstat(PORT);
    report_netstat(INNER_PORT);

    // HTTP 完成 + SHA256。
    let final_status = wait_done(&rpc, &http_gid, Duration::from_secs(180)).await;
    assert_eq!(final_status["status"], "complete", "[auto] HTTP 未完成：{final_status}");
    let dest = dir.join("file.bin");
    assert_eq!(sha256_of_file(&dest), expected, "[auto] 文件 SHA256 不一致");

    rpc.call("aria2.forceRemove", vec![json!(bt_gid)]).await.ok();
    rpc.call("aria2.forceRemove", vec![json!(http_gid)]).await.ok();
    svc.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// service 生命周期（命名管道 + 6800 WS）
// ---------------------------------------------------------------------------

struct Svc {
    child: Child,
    pid: u32,
    pipe: String,
    secret: String,
    engine_pid: u32,
    log_path: PathBuf,
    stopped: bool,
}

impl Svc {
    async fn start(mode: &str) -> Self {
        let exe = env!("CARGO_BIN_EXE_aria2-service");
        let pipe = format!(r"\\.\pipe\aria2-service-c3-{}", std::process::id());
        // Debug 构建的 service 不写 service.log（`redirect_streams_to_log` 只对 Release 生效），
        // 因此这里直接把它的 stdout/stderr 落盘，用于采集 `[svc]` / `[route]` 审计行。
        let log_path = log_dir().join(format!(
            "service-{mode}-{}.log",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        let out = std::fs::File::create(&log_path).expect("创建 service 日志失败");
        let err = out.try_clone().expect("克隆日志句柄失败");
        let child = Command::new(exe)
            .arg(format!("--engine={mode}"))
            .env("ARIA2_SERVICE_PIPE", &pipe)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .unwrap_or_else(|err| panic!("启动 aria2-service 失败：{err}"));
        let pid = child.id();
        let resp = pipe_call(&pipe, &ServiceRequest::StartEngine { port: PORT }, Duration::from_secs(30)).await;
        assert!(resp.ok, "[{mode}] start_engine 失败：{:?}", resp.error);
        let secret = resp.rpc_secret.expect("start_engine 必须回传密钥");
        let engine_pid = resp.engine_pid.expect("start_engine 必须回传引擎 PID");
        println!("[{mode}] service pid={pid} engine pid={engine_pid} log={}", log_path.display());
        wait_listener(PORT).await;
        Self { child, pid, pipe, secret, engine_pid, log_path, stopped: false }
    }

    async fn connect(&self) -> Client {
        let client = Client::start(format!("ws://127.0.0.1:{PORT}/jsonrpc"), self.secret.clone());
        let deadline = Instant::now() + Duration::from_secs(30);
        while !client.is_connected() {
            assert!(Instant::now() < deadline, "WS 6800 连接超时");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        client
    }

    async fn stop(mut self) {
        let _ = pipe_call(&self.pipe, &ServiceRequest::Shutdown, Duration::from_secs(10)).await;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(100)).await
                }
                _ => {
                    let _ = self.child.kill();
                    break;
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while !listeners(PORT).is_empty() || !listeners(INNER_PORT).is_empty() {
            assert!(Instant::now() < deadline, "端口未释放");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        println!("[stop] service pid={} 已退出，日志 {}", self.pid, self.log_path.display());
        self.stopped = true;
    }
}

/// panic 兜底：测试中途失败时必须连同子进程树一起清掉，避免残留 aria2c 占用 6800。
impl Drop for Svc {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        let _ = Command::new("taskkill")
            .args(["/PID", &self.pid.to_string(), "/T", "/F"])
            .output();
        let _ = self.child.wait();
    }
}

async fn pipe_call(pipe: &str, req: &ServiceRequest, timeout: Duration) -> ServiceResponse {
    let deadline = Instant::now() + timeout;
    loop {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(pipe) {
            Ok(mut client) => {
                client
                    .write_all(encode_request(req).unwrap().as_bytes())
                    .await
                    .expect("写管道失败");
                let mut reader = BufReader::new(client);
                let mut line = String::new();
                reader.read_line(&mut line).await.expect("读管道失败");
                return decode_response(&line).expect("响应解析失败");
            }
            Err(err) => {
                assert!(Instant::now() < deadline, "连接命名管道 {pipe} 超时：{err}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

async fn wait_listener(port: u16) -> Vec<(String, u32)> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let rows = listeners(port);
        if !rows.is_empty() && probe_loopback(port) {
            return rows;
        }
        assert!(
            Instant::now() < deadline,
            "{port} 未开始监听（Get-NetTCPConnection 与 TCP 探针均失败）"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 回环连接探针：本机 TCP 表枚举异常时仍能证明 socket 真的在 accept。
fn probe_loopback(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(300),
    )
    .is_ok()
}

async fn wait_done(rpc: &Client, gid: &str, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        let status = rpc.call("aria2.tellStatus", vec![json!(gid)]).await.unwrap();
        let state = status["status"].as_str().unwrap_or_default().to_string();
        if matches!(state.as_str(), "complete" | "error" | "removed") {
            return status;
        }
        assert!(Instant::now() < deadline, "任务 {gid} 超时未结束：{status}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ---------------------------------------------------------------------------
// 进程 / 端口探测
// ---------------------------------------------------------------------------

/// 读取本机某端口的 LISTEN 行（`LocalAddress, OwningProcess`）。
///
/// **不用 netstat**：本机（Windows 10/11 某些补丁 + 特定 WFP 驱动组合）`netstat -ano`
/// 枚举不到部分回环监听 socket，实测 `netstat | findstr :6800` 为空但
/// `Invoke-WebRequest http://127.0.0.1:6800/jsonrpc` 返回 200。`Get-NetTCPConnection`
/// 能看到该行（`LocalAddress=127.0.0.1, RemotePort=0`，但 `State` 为空串），
/// 因此这里**按 `RemotePort == 0` 判定监听**，不依赖 `State`。
fn listeners(port: u16) -> Vec<(String, u32)> {
    let script = format!(
        "Get-NetTCPConnection -LocalPort {port} -ErrorAction SilentlyContinue | \
         Where-Object {{ $_.RemotePort -eq 0 }} | \
         ForEach-Object {{ \"{{0}}|{{1}}\" -f $_.LocalAddress, $_.OwningProcess }}"
    );
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .expect("powershell");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut rows = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some((addr, pid)) = line.rsplit_once('|') else { continue };
        if addr.is_empty() || pid.is_empty() {
            continue;
        }
        rows.push((addr.to_string(), pid.parse().unwrap_or(0)));
    }
    rows.sort();
    rows.dedup();
    rows
}

/// 断言监听地址只能是回环 IPv4（`127.0.0.1`），禁止 `0.0.0.0` / `[::]` / 具体网卡地址。
fn assert_all_loopback(rows: &[(String, u32)], port: u16) {
    assert!(!rows.is_empty(), "{port} 无监听行");
    for (local, _) in rows {
        assert_eq!(
            local, "127.0.0.1",
            "{port} 出现非回环绑定：{local}（rows={rows:?}）"
        );
    }
}

/// 外部可达性探针：用本机**非回环** IPv4 主动连该端口，必须被拒绝。
async fn assert_not_exposed(port: u16) {
    let Some(ip) = local_non_loopback_ipv4() else {
        println!("[隔离] 未取得非回环 IPv4，跳过外部连接探针（仅依赖 LocalAddress 断言）");
        return;
    };
    let addr = std::net::SocketAddr::from((ip, port));
    let result =
        std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500));
    assert!(
        result.is_err(),
        "{port} 可经非回环地址 {ip} 连接 —— 服务对外暴露"
    );
    println!(
        "[隔离] 非回环 {ip}:{port} 连接被拒绝：{}",
        result.err().unwrap()
    );
}

/// 取本机出口网卡的 IPv4（UDP `connect` 不发包，仅让内核选路）。
fn local_non_loopback_ipv4() -> Option<std::net::Ipv4Addr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("223.5.5.5:80").ok()?;
    match sock.local_addr().ok()? {
        std::net::SocketAddr::V4(v4) if !v4.ip().is_loopback() => Some(*v4.ip()),
        _ => None,
    }
}

/// 原样执行用户要求的 `netstat -ano | findstr :<port>` 并记录输出。
///
/// 本机 `netstat` 枚举不到回环监听（见 [`listeners`] 注释），输出为空时按实说明，
/// 不以“空”误判为未监听。
fn report_netstat(port: u16) {
    let out = Command::new("netstat").args(["-ano"]).output().expect("netstat");
    let text = String::from_utf8_lossy(&out.stdout);
    let needle = format!(":{port}");
    let hits: Vec<&str> = text
        .lines()
        .filter(|line| line.split_whitespace().any(|col| col.ends_with(&needle)))
        .collect();
    if hits.is_empty() {
        println!(
            "[netstat] :{port} → 空（本机 netstat 枚举不到回环监听；以 Get-NetTCPConnection + TCP 探针为等价证据）"
        );
    } else {
        for line in hits {
            println!("[netstat] {line}");
        }
    }
}

fn cmdline(pid: u32) -> String {
    let script = format!("(Get-CimInstance Win32_Process -Filter \"ProcessId={pid}\").CommandLine");
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .expect("powershell");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ---------------------------------------------------------------------------
// 本地 HTTP 服务器（支持 Range，可选慢速）
// ---------------------------------------------------------------------------

struct HttpServer {
    port: u16,
    task: tokio::task::JoinHandle<()>,
    hits: Arc<StdMutex<usize>>,
}

impl HttpServer {
    async fn start(payload: Vec<u8>, slow: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let payload = Arc::new(payload);
        let hits = Arc::new(StdMutex::new(0usize));
        let counter = Arc::clone(&hits);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let payload = Arc::clone(&payload);
                let counter = Arc::clone(&counter);
                tokio::spawn(async move {
                    *counter.lock().unwrap() += 1;
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&buf).to_string();
                    let start = text
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase().strip_prefix("range: bytes=").map(|v| {
                                v.split('-')
                                    .next()
                                    .and_then(|s| s.parse::<usize>().ok())
                                    .unwrap_or(0)
                            })
                        })
                        .unwrap_or(0)
                        .min(payload.len());
                    let body = &payload[start..];
                    let head = if start > 0 {
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                            body.len(), start, payload.len() - 1, payload.len()
                        )
                    } else {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                    };
                    if sock.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    if slow {
                        for part in body.chunks(32 * 1024) {
                            if sock.write_all(part).await.is_err() {
                                return;
                            }
                            let _ = sock.flush().await;
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    } else {
                        let _ = sock.write_all(body).await;
                    }
                    let _ = sock.shutdown().await;
                });
            }
        });
        Self { port, task, hits }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.task.abort();
        println!("[http] 请求数={}", self.hits.lock().unwrap());
    }
}

async fn spawn_blackhole() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { break };
            tokio::spawn(async move {
                let _sock = sock;
                tokio::time::sleep(Duration::from_secs(600)).await;
            });
        }
    });
    port
}

// ---------------------------------------------------------------------------
// DB / 审计日志
// ---------------------------------------------------------------------------

async fn check_db() {
    let path = aria2_harness_lib::db::db_path();
    // 真实库上跑一次迁移（与 UI 首次启动等价）。
    let _db = aria2_harness_lib::db::Db::open(&path).expect("打开/迁移真实 DB 失败");
    let conn = rusqlite::Connection::open(&path).expect("重开 DB");
    let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
    let has_engine: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'engine'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, 2, "schema 版本应为 2");
    assert_eq!(has_engine, 1, "tasks 表缺少 engine 列");
    let mut stmt = conn.prepare("SELECT engine, COUNT(*) FROM tasks GROUP BY engine").unwrap();
    let rows: Vec<(String, i64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    println!("[db] path={path} user_version={version} engine 列存在，分布={rows:?}");

    // 写路径实证：独立临时库（同一套 `Db` / SQL，不污染真实库），确认 engine=native 真的落盘。
    let smoke = temp_dir("db").join("smoke.db");
    let db = aria2_harness_lib::db::Db::open(smoke.to_str().unwrap()).expect("打开临时库");
    let rec = aria2_harness_lib::db::TaskRecord::minimal(
        "fffe0000000000aa",
        "complete",
        "http://127.0.0.1/smoke.bin",
        "C:/tmp",
    );
    assert_eq!(rec.engine, "native", "fffe 前缀 GID 应回落为 native");
    db.upsert_task(rec).await.expect("写临时库失败");
    let conn = rusqlite::Connection::open(&smoke).unwrap();
    let (engine, status): (String, String) = conn
        .query_row(
            "SELECT engine, status FROM tasks WHERE gid = ?1",
            ["fffe0000000000aa"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(engine, "native", "engine 列写入不正确");
    println!("[db-write] 临时库 path={} engine={engine} status={status}", smoke.display());
}

/// 审计证据来源：每个阶段 service 的 stdout/stderr 落盘文件。
fn check_route_log() {
    let dir = log_dir();
    let mut lines: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("读取日志目录失败") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        lines.extend(
            text.lines()
                .filter(|line| line.contains("[route]"))
                .map(str::to_string),
        );
    }
    println!("[audit] 日志目录 {}，[route] 行数={}", dir.display(), lines.len());
    // `--engine=aria2` 回滚档不启动 Router（直连 6800 的 aria2c），因此天然没有 [route] 行。
    // 审计只覆盖 native / auto 两个代理档：native 1 行（HTTP）+ auto 2 行（HTTP/BT）。
    assert!(lines.len() >= 3, "审计日志不足：{} 行", lines.len());
    assert!(
        lines.iter().any(|l| l.contains("engine=native")),
        "缺少 native 审计行"
    );
    assert!(
        lines.iter().any(|l| l.contains("engine=aria2")),
        "缺少 aria2 审计行"
    );
    for line in &lines {
        assert!(line.contains("gid="), "审计行缺 gid：{line}");
        assert!(line.contains("engine="), "审计行缺 engine：{line}");
        assert!(line.contains("at="), "审计行缺时间戳：{line}");
        println!("[audit] {line}");
    }
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("c3-phase2-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// service 日志目录（只建不清，供三个阶段累积审计行）。
fn log_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("c3-phase2-logs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sample_payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i.wrapping_mul(31).wrapping_add(7) % 251) as u8).collect()
}

fn sha256(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn sha256_of_file(path: &PathBuf) -> String {
    let data = std::fs::read(path).unwrap_or_else(|err| panic!("读取 {} 失败：{err}", path.display()));
    sha256(&data)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 最小 .torrent（单文件、单 piece 的 20 字节零占位；无 peer 时不会校验）。
fn torrent_base64(announce: &str, name: &str, length: usize, piece_len: usize) -> String {
    fn bstr(out: &mut Vec<u8>, value: &str) {
        out.extend_from_slice(format!("{}:", value.len()).as_bytes());
        out.extend_from_slice(value.as_bytes());
    }
    fn bint(out: &mut Vec<u8>, value: usize) {
        out.extend_from_slice(format!("i{value}e").as_bytes());
    }

    let pieces = vec![0u8; 20];
    let mut info = Vec::new();
    info.push(b'd');
    bstr(&mut info, "length");
    bint(&mut info, length);
    bstr(&mut info, "name");
    bstr(&mut info, name);
    bstr(&mut info, "piece length");
    bint(&mut info, piece_len);
    bstr(&mut info, "pieces");
    info.extend_from_slice(format!("{}:", pieces.len()).as_bytes());
    info.extend_from_slice(&pieces);
    info.push(b'e');

    let mut torrent = Vec::new();
    torrent.push(b'd');
    bstr(&mut torrent, "announce");
    bstr(&mut torrent, announce);
    bstr(&mut torrent, "info");
    torrent.extend_from_slice(&info);
    torrent.push(b'e');

    base64(&torrent)
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
    }
    out
}
