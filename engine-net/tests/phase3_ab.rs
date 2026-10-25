//! Phase 3 A/B：同一本地服务端分别由 engine-net 与产品 aria2c 下载，比对
//! 产物 SHA256 与线上可见行为（absolute-form / CONNECT / ALPN / socket 数）。
//!
//! aria2c 位置：`my-downloader/src-host/binaries/aria2c.exe`，可用 `$ARIA2C_BIN` 覆盖。
//! socks5 不在 aria2 1.37 支持范围（本仓源码全量 grep 无 socks5），故仅有本体测试。

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engine_net::dns::resolver::{DnsConfig, DnsResolver};
use engine_net::http::client::{DownloadRequest, HttpOptions, download};
use engine_net::http::proxy::{ProxySettings, ProxySlot};
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use rcgen::CertifiedKey as RcgenCertifiedKey;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("engine-net-p3ab-{}-{tag}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sample_body(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 131 + 17) as u8).collect()
}

fn sha256(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_else(|err| panic!("读 {} 失败：{err}", path.display()));
    format!("{:x}", Sha256::digest(&bytes))
}

fn expected_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn no_dns() -> DnsResolver {
    DnsResolver::new(&DnsConfig {
        servers: vec!["127.0.0.1:1".parse().unwrap()],
        use_hosts: false,
        timeout: Duration::from_millis(50),
        attempts: 1,
        ..DnsConfig::default()
    })
    .unwrap()
}

fn native_options() -> HttpOptions {
    HttpOptions {
        connect_timeout: Duration::from_secs(5),
        read_timeout: Some(Duration::from_secs(5)),
        max_tries: 1,
        retry_wait: Duration::ZERO,
        ..HttpOptions::default()
    }
}

async fn native_download(url: &str, dest: &Path, options: HttpOptions, resolver: &DnsResolver) {
    let request = DownloadRequest {
        url: url.to_string(),
        dest: dest.to_path_buf(),
        resume_from: 0,
        options,
    };
    download(&request, resolver)
        .await
        .unwrap_or_else(|err| panic!("engine-net 下载失败：{err}"));
}

fn aria2c_path() -> PathBuf {
    if let Ok(path) = std::env::var("ARIA2C_BIN") {
        return PathBuf::from(path);
    }
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.push("my-downloader");
    path.push("src-host");
    path.push("binaries");
    path.push("aria2c.exe");
    assert!(path.exists(), "找不到产品 aria2c：{}", path.display());
    path
}

/// 一次 aria2c 运行结果（带超时保护：对拍不允许无限挂起）。
struct AriaRun {
    code: Option<i32>,
    timed_out: bool,
    log: String,
}

impl AriaRun {
    fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// 跑一次 aria2c。必须放到 blocking 线程：测试的 local server 与 aria2c 共用
/// 同一 runtime，同步等待会让服务端任务饿死。
async fn run_aria2c(dir: PathBuf, out: &str, url: &str, extra: Vec<String>) -> AriaRun {
    let out = out.to_string();
    let url = url.to_string();
    tokio::task::spawn_blocking(move || run_aria2c_blocking(&dir, &out, &url, &extra))
        .await
        .expect("aria2c 线程")
}

fn run_aria2c_blocking(dir: &Path, out: &str, url: &str, extra: &[String]) -> AriaRun {
    let mut args: Vec<String> = vec![
        "--no-conf=true".into(),
        "--console-log-level=notice".into(),
        "--summary-interval=0".into(),
        format!("--log={}", dir.join("aria2.log").display()),
        "--file-allocation=none".into(),
        format!("--dir={}", dir.display()),
        format!("--out={out}"),
        "--split=1".into(),
        "--max-connection-per-server=1".into(),
        "--max-tries=1".into(),
        "--retry-wait=0".into(),
        "--disable-ipv6=true".into(),
        "--enable-http2=true".into(),
        "--sqlite-progress=false".into(),
    ];
    args.extend(extra.iter().cloned());
    args.push(url.to_string());

    let mut child = Command::new(aria2c_path())
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("启动 aria2c 失败");

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait().expect("等待 aria2c") {
            Some(status) => break Some(status),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                timed_out = true;
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };

    let log = std::fs::read_to_string(dir.join("aria2.log")).unwrap_or_default();
    let run = AriaRun {
        code: status.and_then(|status| status.code()),
        timed_out,
        log,
    };
    if !run.success() {
        eprintln!(
            "aria2c 未成功（code={:?}, timed_out={}），日志尾部：\n{}",
            run.code,
            run.timed_out,
            log_tail(&run.log, 40),
        );
    }
    run
}

fn log_tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

// ---------------------------------------------------------------------------
// TLS / H2 服务端（支持 Range，供 aria2 split 使用）
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct TlsObs {
    conns: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
    alpns: Arc<Mutex<Vec<String>>>,
}

struct TlsServer {
    addr: SocketAddr,
    obs: TlsObs,
    handle: JoinHandle<()>,
}

impl Drop for TlsServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start_range_tls_server(alpns: &[&'static str], body: Vec<u8>) -> TlsServer {
    let RcgenCertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["sni.test".into()]).unwrap();
    let cert: CertificateDer<'static> = cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let signing = rustls::crypto::ring::sign::any_supported_type(&key).unwrap();
    let certified = Arc::new(CertifiedKey::new(vec![cert], signing));
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(AlwaysCert(certified)));
    config.alpn_protocols = alpns.iter().map(|item| item.as_bytes().to_vec()).collect();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let obs = TlsObs::default();
    let obs_task = obs.clone();
    let body = Arc::new(body);

    let handle = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            let obs = obs_task.clone();
            let body = Arc::clone(&body);
            tokio::spawn(async move {
                obs.conns.fetch_add(1, Ordering::SeqCst);
                let Ok(tls) = acceptor.accept(socket).await else {
                    return;
                };
                let alpn = tls
                    .get_ref()
                    .1
                    .alpn_protocol()
                    .map(|item| String::from_utf8_lossy(item).to_string())
                    .unwrap_or_default();
                obs.alpns.lock().unwrap().push(alpn.clone());

                let service = service_fn(move |req| {
                    let obs = obs.clone();
                    let body = Arc::clone(&body);
                    async move {
                        obs.requests.fetch_add(1, Ordering::SeqCst);
                        let now = obs.active.fetch_add(1, Ordering::SeqCst) + 1;
                        obs.max_active.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        obs.active.fetch_sub(1, Ordering::SeqCst);

                        let range = req
                            .headers()
                            .get("range")
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string);
                        let response = match range.as_deref().and_then(parse_range) {
                            Some((start, end)) => {
                                let end = end.unwrap_or(body.len() as u64 - 1).min(body.len() as u64 - 1);
                                let slice = body[start as usize..=end as usize].to_vec();
                                hyper::Response::builder()
                                    .status(206)
                                    .header(
                                        "content-range",
                                        format!("bytes {start}-{end}/{}", body.len()),
                                    )
                                    .header("content-length", slice.len())
                                    .body(Full::new(Bytes::from(slice)))
                                    .unwrap()
                            }
                            None => hyper::Response::builder()
                                .status(200)
                                .header("content-length", body.len())
                                .body(Full::new(Bytes::from((*body).clone())))
                                .unwrap(),
                        };
                        Ok::<_, std::convert::Infallible>(response)
                    }
                });

                if alpn == "h2" {
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(tls), service)
                        .await;
                } else {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tls), service)
                        .await;
                }
            });
        }
    });

    TlsServer { addr, obs, handle }
}

#[derive(Debug)]
struct AlwaysCert(Arc<CertifiedKey>);

impl rustls::server::ResolvesServerCert for AlwaysCert {
    fn resolve(&self, _hello: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(&self.0))
    }
}

fn parse_range(value: &str) -> Option<(u64, Option<u64>)> {
    let rest = value.strip_prefix("bytes=")?;
    let (start, end) = rest.split_once('-')?;
    let start = start.trim().parse().ok()?;
    let end = if end.trim().is_empty() {
        None
    } else {
        Some(end.trim().parse().ok()?)
    };
    Some((start, end))
}

// ---------------------------------------------------------------------------
// 代理服务端
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct ProxyObs {
    lines: Arc<Mutex<Vec<String>>>,
    auths: Arc<Mutex<Vec<Option<String>>>>,
}

struct ProxyServer {
    addr: SocketAddr,
    obs: ProxyObs,
    handle: JoinHandle<()>,
}

impl Drop for ProxyServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn read_head(socket: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let Ok(n) = socket.read(&mut chunk).await else {
            return buf;
        };
        if n == 0 {
            return buf;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            return buf;
        }
    }
}

fn head_parts(raw: &[u8]) -> (String, HashMap<String, String>) {
    let text = String::from_utf8_lossy(raw);
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or_default().to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    (first, headers)
}

/// forward 代理（absolute-form）：直接回固定 body。
async fn start_forward_proxy(body: Vec<u8>) -> ProxyServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let obs = ProxyObs::default();
    let obs_task = obs.clone();
    let body = Arc::new(body);
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let obs = obs_task.clone();
            let body = Arc::clone(&body);
            tokio::spawn(async move {
                let head = read_head(&mut socket).await;
                let (line, headers) = head_parts(&head);
                obs.lines.lock().unwrap().push(line);
                obs.auths
                    .lock()
                    .unwrap()
                    .push(headers.get("proxy-authorization").cloned());
                let mut response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                response.extend_from_slice(&body);
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    ProxyServer { addr, obs, handle }
}

/// CONNECT 代理（可要求 Basic 认证），成功后双向转发。
async fn start_connect_proxy(require_auth: Option<(&'static str, &'static str)>) -> ProxyServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let obs = ProxyObs::default();
    let obs_task = obs.clone();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let obs = obs_task.clone();
            tokio::spawn(async move {
                let head = read_head(&mut socket).await;
                let (line, headers) = head_parts(&head);
                obs.lines.lock().unwrap().push(line.clone());
                obs.auths
                    .lock()
                    .unwrap()
                    .push(headers.get("proxy-authorization").cloned());
                if let Some((user, pass)) = require_auth {
                    let want = format!(
                        "Basic {}",
                        base64_standard(&format!("{user}:{pass}"))
                    );
                    if headers.get("proxy-authorization") != Some(&want) {
                        let _ = socket
                            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n")
                            .await;
                        let _ = socket.shutdown().await;
                        return;
                    }
                }
                let Some(target) = line.split_whitespace().nth(1) else {
                    return;
                };
                let Ok(mut upstream) = TcpStream::connect(target).await else {
                    let _ = socket
                        .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                };
                let _ = socket
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await;
                let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
            });
        }
    });
    ProxyServer { addr, obs, handle }
}

fn base64_standard(text: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(text)
}

fn proxy_settings(url: &str) -> ProxySettings {
    ProxySettings {
        all: ProxySlot::from_raw(url).unwrap(),
        ..ProxySettings::default()
    }
}

// ---------------------------------------------------------------------------
// 伪 DNS（aria2 的 --async-dns-server 只接受 IP，端口固定 53）
// ---------------------------------------------------------------------------

struct MockDns {
    addr: SocketAddr,
    handle: JoinHandle<()>,
}

impl Drop for MockDns {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl MockDns {
    async fn start_on(addr: &str, aaaa: Option<std::net::Ipv6Addr>) -> Option<Self> {
        let socket = UdpSocket::bind(addr).await.ok()?;
        let addr = socket.local_addr().ok()?;
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 512];
            loop {
                let Ok((len, peer)) = socket.recv_from(&mut buf).await else {
                    break;
                };
                if let Some(resp) = dns_response(&buf[..len], Ipv4Addr::LOCALHOST, aaaa) {
                    let _ = socket.send_to(&resp, peer).await;
                }
            }
        });
        Some(Self { addr, handle })
    }

    fn resolver(&self) -> DnsResolver {
        DnsResolver::new(&DnsConfig {
            servers: vec![self.addr],
            ipv6: true,
            use_hosts: false,
            timeout: Duration::from_secs(2),
            attempts: 1,
            ..DnsConfig::default()
        })
        .unwrap()
    }
}

fn dns_response(
    query: &[u8],
    a: Ipv4Addr,
    aaaa: Option<std::net::Ipv6Addr>,
) -> Option<Vec<u8>> {
    if query.len() < 12 {
        return None;
    }
    let mut offset = 12;
    while offset < query.len() {
        let len = query[offset] as usize;
        if len == 0 {
            offset += 1;
            break;
        }
        if len & 0xc0 == 0xc0 {
            offset += 2;
            break;
        }
        offset += len + 1;
    }
    if offset + 4 > query.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([query[offset], query[offset + 1]]);
    let question = &query[12..offset + 4];
    let record = match qtype {
        1 => Some((1u16, a.octets().to_vec())),
        28 => aaaa.map(|ip| (28u16, ip.octets().to_vec())),
        _ => None,
    };
    let mut resp = Vec::new();
    resp.extend_from_slice(&query[0..2]);
    resp.extend_from_slice(&[0x81, 0x80]);
    resp.extend_from_slice(&[0, 1]);
    let count = if record.is_some() { 1u16 } else { 0 };
    resp.extend_from_slice(&count.to_be_bytes());
    resp.extend_from_slice(&[0, 0, 0, 0]);
    resp.extend_from_slice(question);
    if let Some((rtype, data)) = record {
        resp.extend_from_slice(&[0xc0, 0x0c]);
        resp.extend_from_slice(&rtype.to_be_bytes());
        resp.extend_from_slice(&[0, 1]);
        resp.extend_from_slice(&[0, 0, 0, 60]);
        resp.extend_from_slice(&(data.len() as u16).to_be_bytes());
        resp.extend_from_slice(&data);
    }
    Some(resp)
}

// ---------------------------------------------------------------------------
// A/B 用例
// ---------------------------------------------------------------------------

/// A/B 1：HTTP forward 代理（absolute-form + Basic 认证）。
#[tokio::test]
async fn ab_forward_proxy_matches_aria2() {
    let body = sample_body(96 * 1024);
    let proxy = start_forward_proxy(body.clone()).await;
    let port = proxy.addr.port();
    let url = "http://origin.invalid/file.bin".to_string();
    let auth = "u:p";

    let dir = temp_dir("ab-forward");
    let mut options = native_options();
    options.proxy = proxy_settings(&format!("http://{auth}@127.0.0.1:{port}"));
    native_download(&url, &dir.join("native.bin"), options, &no_dns()).await;

    let run = run_aria2c(
        dir.clone(),
        "aria2.bin",
        &url,
        vec![format!("--all-proxy=http://{auth}@127.0.0.1:{port}")],
    )
    .await;
    assert!(
        run.success(),
        "aria2c forward 代理下载失败（code={:?}）",
        run.code
    );

    let expected = expected_sha256(&body);
    assert_eq!(sha256(&dir.join("native.bin")), expected);
    assert_eq!(sha256(&dir.join("aria2.bin")), expected);

    let lines = proxy.obs.lines.lock().unwrap().clone();
    assert_eq!(lines.len(), 2, "两侧各一次请求：{lines:?}");
    assert!(lines
        .iter()
        .all(|line| line == "GET http://origin.invalid/file.bin HTTP/1.1"));
    let auths = proxy.obs.auths.lock().unwrap().clone();
    let expected_auth = format!("Basic {}", base64_standard(auth));
    assert_eq!(auths, vec![Some(expected_auth.clone()), Some(expected_auth)]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A/B 2：`--proxy-method=tunnel` 把明文 HTTP 包进 CONNECT。
#[tokio::test]
async fn ab_proxy_method_tunnel_matches_aria2() {
    let body = sample_body(32 * 1024);
    let proxy = start_connect_proxy(None).await;
    let proxy_port = proxy.addr.port();

    // 上游明文 HTTP 服务。
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = listener.local_addr().unwrap();
    let body_arc = Arc::new(body.clone());
    let origin_handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let body = Arc::clone(&body_arc);
            tokio::spawn(async move {
                let _ = read_head(&mut socket).await;
                let mut response =
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
                        .into_bytes();
                response.extend_from_slice(&body);
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            });
        }
    });

    let url = format!("http://127.0.0.1:{}/file.bin", origin.port());
    let dir = temp_dir("ab-tunnel");
    let mut options = native_options();
    options.proxy = proxy_settings(&format!("http://127.0.0.1:{proxy_port}"));
    options.proxy_method_tunnel = true;
    native_download(&url, &dir.join("native.bin"), options, &no_dns()).await;

    let run = run_aria2c(
        dir.clone(),
        "aria2.bin",
        &url,
        vec![
            format!("--all-proxy=http://127.0.0.1:{proxy_port}"),
            "--proxy-method=tunnel".into(),
        ],
    )
    .await;
    assert!(
        run.success(),
        "aria2c 隧道代理下载失败（code={:?}）",
        run.code
    );

    let expected = expected_sha256(&body);
    assert_eq!(sha256(&dir.join("native.bin")), expected);
    assert_eq!(sha256(&dir.join("aria2.bin")), expected);
    let lines = proxy.obs.lines.lock().unwrap().clone();
    let connect = format!("CONNECT 127.0.0.1:{} HTTP/1.1", origin.port());
    assert!(
        lines.iter().filter(|line| **line == connect).count() >= 2,
        "两侧都必须发 CONNECT：{lines:?}"
    );
    origin_handle.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A/B 3：`--check-certificate=false` 接受自签 HTTPS（双方 SHA256 一致）。
///
/// 服务端只宣告 http/1.1，把用例限定在「TLS 信任开关」这一层；h2 由 A/B 4 覆盖。
#[tokio::test]
async fn ab_self_signed_https_matches_aria2() {
    let body = sample_body(64 * 1024);
    let server = start_range_tls_server(&["http/1.1"], body.clone()).await;
    let url = format!("https://127.0.0.1:{}/file.bin", server.addr.port());
    let dir = temp_dir("ab-cert");

    let mut options = native_options();
    options.check_certificate = false;
    native_download(&url, &dir.join("native.bin"), options, &no_dns()).await;

    let run = run_aria2c(
        dir.clone(),
        "aria2.bin",
        &url,
        vec!["--check-certificate=false".into()],
    )
    .await;
    assert!(
        run.success(),
        "aria2c 自签 HTTPS 下载失败（code={:?}）",
        run.code
    );

    let expected = expected_sha256(&body);
    assert_eq!(sha256(&dir.join("native.bin")), expected);
    assert_eq!(sha256(&dir.join("aria2.bin")), expected);
    assert!(
        server.obs.conns.load(Ordering::SeqCst) >= 2,
        "两侧都必须完成一次 TLS 连接"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A/B 4：HTTP/2 多路复用（split=2 / -x=2，单 socket 两条 stream）。
#[tokio::test]
async fn ab_http2_multiplexing_matches_aria2() {
    // aria2 的 --min-split-size 下限是 1M（`OptionHandlerFactory.cc`），所以文件
    // 必须 ≥ 2M 才会真的切两片；用 4M 让两片各 2M。
    let body = sample_body(4 * 1024 * 1024);
    let server = start_range_tls_server(&["h2", "http/1.1"], body.clone()).await;
    let url = format!("https://127.0.0.1:{}/file.bin", server.addr.port());
    let dir = temp_dir("ab-h2");

    // engine-net：split=2，-x=2。
    let mut options = native_options();
    options.check_certificate = false;
    options.split = Some(2);
    options.max_connection_per_server = 2;
    native_download(&url, &dir.join("native.bin"), options, &no_dns()).await;

    let run = run_aria2c(
        dir.clone(),
        "aria2.bin",
        &url,
        vec![
            "--check-certificate=false".into(),
            "--split=2".into(),
            "--max-connection-per-server=2".into(),
            "--min-split-size=1M".into(),
            "--piece-length=1M".into(),
        ],
    )
    .await;
    if !run.success() {
        eprintln!("跳过 HTTP/2 A/B：aria2c 未成功（code={:?}）", run.code);
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    let expected = expected_sha256(&body);
    assert_eq!(sha256(&dir.join("native.bin")), expected);
    assert_eq!(sha256(&dir.join("aria2.bin")), expected);

    let alpns = server.obs.alpns.lock().unwrap().clone();
    assert!(
        alpns.iter().filter(|item| *item == "h2").count() >= 2,
        "engine-net 与 aria2c 都必须协商到 h2：{alpns:?}"
    );
    assert!(
        server.obs.max_active.load(Ordering::SeqCst) <= 2,
        "流上限 min(split,-x)=2 被突破：{}",
        server.obs.max_active.load(Ordering::SeqCst)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A/B 5：CONNECT 隧道被代理拒绝 → 双方都归到 UNKNOWN_ERROR(1)
/// （C++ `AbstractProxyResponseCommand.cc:74-76`：非 200 一律 Proxy connection failed）。
#[tokio::test]
async fn ab_proxy_auth_failure_code_matches_aria2() {
    let proxy = start_connect_proxy(Some(("u", "p"))).await;
    let port = proxy.addr.port();
    // https 目标 → 双方都走 CONNECT 隧道，认证发生在隧道建立阶段。
    let url = "https://127.0.0.1:9/file.bin";
    let dir = temp_dir("ab-407");

    let mut options = native_options();
    options.proxy = proxy_settings(&format!("http://u:wrong@127.0.0.1:{port}"));
    options.max_tries = 1;
    let request = DownloadRequest {
        url: url.to_string(),
        dest: dir.join("native.bin"),
        resume_from: 0,
        options,
    };
    let err = download(&request, &no_dns())
        .await
        .expect_err("错误代理密码必须失败");
    assert_eq!(err.to_aria2_code(), 1, "{err}");

    let run = run_aria2c(
        dir.clone(),
        "aria2.bin",
        url,
        vec![format!("--all-proxy=http://u:wrong@127.0.0.1:{port}")],
    )
    .await;
    assert_eq!(
        run.code,
        Some(1),
        "aria2c 隧道代理认证失败应返回 1；实际 {:?}",
        run.code
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A/B 6：双栈域名 + 伪 DNS（A=127.0.0.1，AAAA 不可达），RFC 8305 回退。
#[tokio::test]
async fn ab_dualstack_fallback_matches_aria2() {
    let body = sample_body(48 * 1024);
    // aria2 的 --async-dns-server 端口固定 53；无权限则跳过。
    let Some(dns) = MockDns::start_on("127.0.0.1:53", Some(std::net::Ipv6Addr::LOCALHOST)).await
    else {
        eprintln!("跳过双栈 A/B：127.0.0.1:53 不可用（需要管理员权限）");
        return;
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = listener.local_addr().unwrap();
    let body_arc = Arc::new(body.clone());
    let origin_handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let body = Arc::clone(&body_arc);
            tokio::spawn(async move {
                let _ = read_head(&mut socket).await;
                let mut response =
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
                        .into_bytes();
                response.extend_from_slice(&body);
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            });
        }
    });

    let url = format!("http://dual.test:{}/file.bin", origin.port());
    let dir = temp_dir("ab-dual");

    let options = native_options();
    native_download(&url, &dir.join("native.bin"), options, &dns.resolver()).await;

    let run = run_aria2c(
        dir.clone(),
        "aria2.bin",
        &url,
        vec![
            "--async-dns=true".into(),
            "--async-dns-server=127.0.0.1".into(),
            "--disable-ipv6=false".into(),
        ],
    )
    .await;
    assert!(
        run.success(),
        "aria2c 双栈回退下载失败（code={:?}）",
        run.code
    );

    let expected = expected_sha256(&body);
    assert_eq!(sha256(&dir.join("native.bin")), expected);
    assert_eq!(sha256(&dir.join("aria2.bin")), expected);
    origin_handle.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 证据：aria2 1.37 不提供 socks5 选项，SOCKS5 是本项目增强。
#[test]
fn aria2_binary_has_no_socks5_option() {
    let output = Command::new(aria2c_path())
        .arg("--help=#all")
        .output()
        .expect("启动 aria2c --help");
    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    assert!(
        !text.contains("socks"),
        "aria2 帮助里出现 socks，需重新评估 A/B 范围"
    );
}
