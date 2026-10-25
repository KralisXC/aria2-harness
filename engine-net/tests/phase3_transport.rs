//! Phase 3 传输层集成测试（全部本地回环，不访问外网）。
//!
//! 覆盖：
//! - TLS：ALPN h2/http1.1 协商、`--check-certificate=false` 接受自签、
//!   校验开启时拒绝自签、SNI 在 TCP 连 IP 时仍发送域名（Q9）；
//! - HTTP/2：同 authority 复用单 socket、并发 stream ≤ min(split,-x)、
//!   服务端只给 http/1.1 时回退、`enable-http2=false` 时 ALPN 只宣告 http/1.1；
//! - 代理：HTTP forward（absolute-form + Basic）、`--proxy-method=tunnel`、
//!   CONNECT 隧道（隧道内 ALPN 仅 http/1.1，Q6 I5）、SOCKS5（认证/错误密码）、
//!   407 / SOCKS5 认证失败 → 错误码 33、`--no-proxy` 直连；
//! - 双栈：伪 DNS 返回 A+AAAA，IPv6 不可达时 Happy Eyeballs 回退 IPv4。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use engine_net::dns::resolver::{DnsConfig, DnsResolver};
use engine_net::error::NetworkError;
use engine_net::http::client::{DownloadRequest, HttpOptions, download};
use engine_net::http::h2::{H2Pool, MuxKey, Pooled};
use engine_net::http::proxy::{ProxySettings, ProxySlot};
use engine_net::http::tls::{TlsOptions, client_config, server_name};
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use rcgen::CertifiedKey as RcgenCertifiedKey;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// 公共夹具
// ---------------------------------------------------------------------------

fn temp_path(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "engine-net-p3-{}-{tag}-{nanos}.bin",
        std::process::id()
    ))
}

/// 字面 IP 直通，不真的查询这个不可达的 DNS。
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

fn options() -> HttpOptions {
    HttpOptions {
        connect_timeout: Duration::from_secs(3),
        read_timeout: Some(Duration::from_secs(3)),
        max_tries: 1,
        retry_wait: Duration::ZERO,
        ..HttpOptions::default()
    }
}

fn request(url: String, dest: PathBuf, options: HttpOptions) -> DownloadRequest {
    DownloadRequest {
        url,
        dest,
        resume_from: 0,
        options,
    }
}

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 37 % 251) as u8).collect()
}

/// 自签证书（DNS SAN: sni.test），返回 DER。
fn self_signed() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let RcgenCertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["sni.test".into()]).unwrap();
    (
        cert.der().clone(),
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der())),
    )
}

#[derive(Clone, Default)]
struct TlsObs {
    conns: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
    alpns: Arc<Mutex<Vec<String>>>,
    sni: Arc<Mutex<Vec<String>>>,
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

/// 记录 SNI 的证书解析器（`ResolvesServerCert`）。
#[derive(Debug)]
struct SniRecorder {
    cert: Arc<CertifiedKey>,
    sni: Arc<Mutex<Vec<String>>>,
}

impl rustls::server::ResolvesServerCert for SniRecorder {
    fn resolve(&self, hello: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        if let Some(name) = hello.server_name() {
            self.sni.lock().unwrap().push(name.to_string());
        }
        Some(Arc::clone(&self.cert))
    }
}

/// 启一个 TLS 服务端；`alpns` 决定宣告哪些协议，`delay` 模拟慢响应。
async fn start_tls_server(
    alpns: &[&'static str],
    body: Vec<u8>,
    delay: Duration,
) -> TlsServer {
    let (cert, key) = self_signed();
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key).unwrap();
    let certified = Arc::new(CertifiedKey::new(vec![cert], signing_key));
    let obs = TlsObs::default();
    let resolver = Arc::new(SniRecorder {
        cert: certified,
        sni: Arc::clone(&obs.sni),
    });
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    config.alpn_protocols = alpns.iter().map(|item| item.as_bytes().to_vec()).collect();
    let config = Arc::new(config);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
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
                    .unwrap_or_else(|| "none".to_string());
                obs.alpns.lock().unwrap().push(alpn.clone());

                let service = service_fn(move |_req| {
                    let obs = obs.clone();
                    let body = Arc::clone(&body);
                    async move {
                        obs.requests.fetch_add(1, Ordering::SeqCst);
                        let now = obs.active.fetch_add(1, Ordering::SeqCst) + 1;
                        obs.max_active.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(delay).await;
                        obs.active.fetch_sub(1, Ordering::SeqCst);
                        Ok::<_, std::convert::Infallible>(
                            hyper::Response::builder()
                                .status(200)
                                .header("content-length", body.len())
                                .body(Full::new(Bytes::from((*body).clone())))
                                .unwrap(),
                        )
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

/// 强制要求客户端证书（`--certificate` 场景）。任意证书都接受，只验证握手存在性。
#[derive(Debug)]
struct AcceptAnyClient(Arc<rustls::crypto::CryptoProvider>);

impl rustls::server::danger::ClientCertVerifier for AcceptAnyClient {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        Ok(rustls::server::danger::ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0
            .signature_verification_algorithms
            .supported_schemes()
    }
}

async fn start_mtls_server(body: Vec<u8>) -> TlsServer {
    let (cert, key) = self_signed();
    let signing = rustls::crypto::ring::sign::any_supported_type(&key).unwrap();
    let certified = Arc::new(CertifiedKey::new(vec![cert], signing));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let resolver = Arc::new(SniRecorder {
        cert: certified,
        sni: Arc::new(Mutex::new(Vec::new())),
    });
    let mut config = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(Arc::new(AcceptAnyClient(provider)))
        .with_cert_resolver(resolver);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
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
                let service = service_fn(move |_req| {
                    let obs = obs.clone();
                    let body = Arc::clone(&body);
                    async move {
                        obs.requests.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, std::convert::Infallible>(
                            hyper::Response::builder()
                                .status(200)
                                .header("content-length", body.len())
                                .body(Full::new(Bytes::from((*body).clone())))
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            });
        }
    });
    TlsServer { addr, obs, handle }
}

#[tokio::test]
async fn tls_h2_download_reports_http2() {
    let body = payload(4096);
    let server = start_tls_server(&["h2", "http/1.1"], body.clone(), Duration::ZERO).await;
    let dest = temp_path("h2");

    let mut opts = options();
    opts.check_certificate = false;
    let url = format!("https://127.0.0.1:{}/file.bin", server.addr.port());
    let report = download(&request(url, dest.clone(), opts), &no_dns())
        .await
        .expect("h2 下载应成功");

    assert!(report.http2, "ALPN 协商 h2 后必须走 HTTP/2 stream");
    assert!(!report.via_proxy);
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert_eq!(server.obs.alpns.lock().unwrap().as_slice(), ["h2"]);
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn tls_http1_only_server_falls_back_without_reconnect() {
    let body = payload(2048);
    let server = start_tls_server(&["http/1.1"], body.clone(), Duration::ZERO).await;
    let dest = temp_path("http1");

    let mut opts = options();
    opts.check_certificate = false;
    let url = format!("https://127.0.0.1:{}/file.bin", server.addr.port());
    let report = download(&request(url, dest.clone(), opts), &no_dns())
        .await
        .expect("http/1.1 下载应成功");

    assert!(!report.http2);
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    // 只握手一次：回退复用已协商 http/1.1 的那条连接（Q6 I4）。
    assert_eq!(server.obs.conns.load(Ordering::SeqCst), 1);
    assert_eq!(server.obs.alpns.lock().unwrap().as_slice(), ["http/1.1"]);
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn enable_http2_false_declares_only_http11() {
    let server = start_tls_server(&["h2", "http/1.1"], payload(512), Duration::ZERO).await;
    let dest = temp_path("no-h2");

    let mut opts = options();
    opts.check_certificate = false;
    opts.enable_http2 = false;
    let url = format!("https://127.0.0.1:{}/file.bin", server.addr.port());
    let report = download(&request(url, dest.clone(), opts), &no_dns())
        .await
        .expect("强制 http/1.1 应成功");

    assert!(!report.http2);
    assert_eq!(server.obs.alpns.lock().unwrap().as_slice(), ["http/1.1"]);
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn check_certificate_toggle_matches_aria2_semantics() {
    let server = start_tls_server(&["http/1.1"], payload(128), Duration::ZERO).await;
    let url = format!("https://127.0.0.1:{}/file.bin", server.addr.port());

    let mut strict = options();
    strict.check_certificate = true;
    let err = download(&request(url.clone(), temp_path("strict"), strict), &no_dns())
        .await
        .expect_err("默认必须校验证书并拒绝自签");
    assert!(matches!(err, NetworkError::CertError(_)), "{err}");

    let mut lax = options();
    lax.check_certificate = false;
    lax.enable_http2 = false;
    let dest = temp_path("lax");
    download(&request(url, dest.clone(), lax), &no_dns())
        .await
        .expect("--check-certificate=false 应放行自签");
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn client_certificate_satisfies_server_requirement() {
    let body = payload(256);
    let server = start_mtls_server(body.clone()).await;
    let url = format!("https://127.0.0.1:{}/file.bin", server.addr.port());

    // 不带客户端证书：服务端强制要求，握手必须失败。失败在 TLS 1.3 下可能由
    // 服务端的 certificate_required alert 触发，经 hyper 报成传输错误（不是
    // 本机校验失败），所以这里只断言"失败"，不锁定错误分类。
    let mut bare = options();
    bare.check_certificate = false;
    bare.enable_http2 = false;
    let err = download(&request(url.clone(), temp_path("mtls-bare"), bare), &no_dns())
        .await
        .expect_err("服务端要求客户端证书时必须失败");
    assert!(err.to_aria2_code() != 0, "{err}");

    // 带客户端证书（证书与私钥同一 PEM，等价 `--certificate` 缺省私钥）。
    let client = rcgen::generate_simple_self_signed(vec!["client.test".into()]).unwrap();
    let pem_path = temp_path("mtls-client.pem");
    std::fs::write(
        &pem_path,
        format!(
            "{}{}",
            client.cert.pem(),
            client.signing_key.serialize_pem()
        ),
    )
    .unwrap();

    let mut with_cert = options();
    with_cert.check_certificate = false;
    with_cert.enable_http2 = false;
    with_cert.client_certificate = Some(pem_path.clone());
    let dest = temp_path("mtls");
    download(&request(url, dest.clone(), with_cert), &no_dns())
        .await
        .expect("--certificate 应完成双向 TLS");
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    let _ = std::fs::remove_file(&dest);
    let _ = std::fs::remove_file(&pem_path);
}

#[tokio::test]
async fn sni_uses_domain_while_tcp_connects_to_resolved_ip() {
    let dns = MockDns::start(MockDnsAnswer::A(Ipv4Addr::LOCALHOST), None).await;
    let server = start_tls_server(&["http/1.1"], payload(256), Duration::ZERO).await;
    let resolver = dns.resolver(false);
    let dest = temp_path("sni");

    let mut opts = options();
    opts.check_certificate = false;
    let url = format!("https://sni.test:{}/file.bin", server.addr.port());
    download(&request(url, dest.clone(), opts), &resolver)
        .await
        .expect("域名 + 伪 DNS 下载应成功");

    assert_eq!(
        server.obs.sni.lock().unwrap().as_slice(),
        ["sni.test"],
        "SNI 必须是域名，而不是 TCP 层解析出的 IP"
    );
    let _ = std::fs::remove_file(&dest);
}

// ---------------------------------------------------------------------------
// HTTP/2 多路复用与流上限
// ---------------------------------------------------------------------------

#[tokio::test]
async fn h2_pool_reuses_one_socket_and_caps_concurrency() {
    let server = start_tls_server(&["h2"], payload(64), Duration::from_millis(120)).await;
    let tls = client_config(&TlsOptions {
        check_certificate: false,
        enable_http2: true,
        ..TlsOptions::default()
    })
    .unwrap();

    let pool = H2Pool::new();
    let authority = format!("127.0.0.1:{}", server.addr.port());
    let key = MuxKey::new("https", &authority, "cc=false", "", 2);

    let connect = || {
        let tls = Arc::clone(&tls);
        let addr = server.addr;
        async move {
            let name = server_name("127.0.0.1")?;
            let stream = tokio_rustls::TlsConnector::from(tls)
                .connect(name, TcpStream::connect(addr).await?)
                .await
                .map_err(|err| NetworkError::CertError(err.to_string()))?;
            Ok(stream)
        }
    };

    let first = pool.acquire(key.clone(), connect).await.unwrap();
    let Pooled::H2(mux) = first else {
        panic!("ALPN 必须是 h2")
    };
    let second = pool.acquire(key.clone(), connect).await.unwrap();
    let Pooled::H2(same) = second else {
        panic!("第二次 acquire 必须复用 h2 连接")
    };
    assert!(Arc::ptr_eq(&mux, &same), "同 authority 必须复用同一个 mux");
    assert_eq!(mux.max_streams(), 2);

    let mut handles = Vec::new();
    for _ in 0..4 {
        let mux = Arc::clone(&mux);
        let uri: hyper::Uri = format!("https://{authority}/file.bin").parse().unwrap();
        handles.push(tokio::spawn(async move {
            let req = hyper::Request::builder()
                .uri(uri)
                .body(http_body_util::Empty::<Bytes>::new())
                .unwrap();
            let (_response, lease) = mux.send(req).await.unwrap();
            drop(lease);
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }

    let stats = pool.stats();
    assert_eq!(stats.muxes, 1, "4 条 stream 只能占用 1 条 socket（Q6 I1）");
    assert_eq!(stats.total_streams, 4);
    assert_eq!(stats.active_streams, 0);
    assert_eq!(
        server.obs.conns.load(Ordering::SeqCst),
        1,
        "服务端只能看到 1 条 TLS 连接"
    );
    assert_eq!(server.obs.requests.load(Ordering::SeqCst), 4);
    assert!(
        server.obs.max_active.load(Ordering::SeqCst) <= 2,
        "并发流不得超过 min(split,-x)=2，实际 {}",
        server.obs.max_active.load(Ordering::SeqCst)
    );
    assert_eq!(server.obs.alpns.lock().unwrap().as_slice(), ["h2"]);
}

// ---------------------------------------------------------------------------
// 代理
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

fn parse_head(raw: &[u8]) -> (String, HashMap<String, String>) {
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

fn expected_basic(user: &str, pass: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
    )
}

/// HTTP forward 代理：记录 request-target 与 Proxy-Authorization，直接回 body。
async fn start_forward_proxy(
    require_auth: Option<(&'static str, &'static str)>,
    body: Vec<u8>,
    status: u16,
) -> ProxyServer {
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
                let (line, headers) = parse_head(&head);
                obs.lines.lock().unwrap().push(line);
                obs.auths
                    .lock()
                    .unwrap()
                    .push(headers.get("proxy-authorization").cloned());

                let authorized = match require_auth {
                    Some((user, pass)) => {
                        headers.get("proxy-authorization") == Some(&expected_basic(user, pass))
                    }
                    None => true,
                };
                if !authorized {
                    let _ = socket
                        .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    let _ = socket.shutdown().await;
                    return;
                }
                let mut response = format!(
                    "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\n\r\n",
                    if status == 200 { "OK" } else { "Error" },
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

/// HTTP CONNECT 代理：记录 CONNECT 目标，然后与上游做双向转发。
async fn start_connect_proxy(
    require_auth: Option<(&'static str, &'static str)>,
) -> ProxyServer {
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
                let (line, headers) = parse_head(&head);
                obs.lines.lock().unwrap().push(line.clone());
                obs.auths
                    .lock()
                    .unwrap()
                    .push(headers.get("proxy-authorization").cloned());

                let authorized = match require_auth {
                    Some((user, pass)) => {
                        headers.get("proxy-authorization") == Some(&expected_basic(user, pass))
                    }
                    None => true,
                };
                if !authorized {
                    let _ = socket
                        .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
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

/// SOCKS5：支持无认证与用户名/密码认证，CONNECT 后双向转发。
async fn start_socks5_proxy(user: Option<(&'static str, &'static str)>) -> ProxyServer {
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
                let mut buf = [0u8; 2];
                if socket.read_exact(&mut buf).await.is_err() || buf[0] != 5 {
                    return;
                }
                let count = buf[1] as usize;
                let mut methods = vec![0u8; count];
                if socket.read_exact(&mut methods).await.is_err() {
                    return;
                }
                let chosen = if user.is_some() {
                    if !methods.contains(&2) {
                        let _ = socket.write_all(&[5, 0xff]).await;
                        return;
                    }
                    2u8
                } else if methods.contains(&0) {
                    0
                } else {
                    let _ = socket.write_all(&[5, 0xff]).await;
                    return;
                };
                let _ = socket.write_all(&[5, chosen]).await;
                if chosen == 2 {
                    let mut auth = [0u8; 2];
                    if socket.read_exact(&mut auth).await.is_err() || auth[0] != 1 {
                        return;
                    }
                    let (want_user, want_pass) = user.unwrap();
                    let mut got_user = vec![0u8; auth[1] as usize];
                    socket.read_exact(&mut got_user).await.ok();
                    socket.read_exact(&mut auth[..1]).await.ok();
                    let mut got_pass = vec![0u8; auth[0] as usize];
                    socket.read_exact(&mut got_pass).await.ok();
                    let ok = got_user == want_user.as_bytes() && got_pass == want_pass.as_bytes();
                    let _ = socket.write_all(&[1, if ok { 0 } else { 1 }]).await;
                    if !ok {
                        return;
                    }
                }

                let mut head = [0u8; 4];
                if socket.read_exact(&mut head).await.is_err() {
                    return;
                }
                let atyp = head[3];
                let target = match atyp {
                    1 => {
                        let mut ip = [0u8; 4];
                        socket.read_exact(&mut ip).await.ok();
                        Ipv4Addr::from(ip).to_string()
                    }
                    3 => {
                        let mut len = [0u8; 1];
                        socket.read_exact(&mut len).await.ok();
                        let mut name = vec![0u8; len[0] as usize];
                        socket.read_exact(&mut name).await.ok();
                        String::from_utf8_lossy(&name).to_string()
                    }
                    4 => {
                        let mut ip = [0u8; 16];
                        socket.read_exact(&mut ip).await.ok();
                        Ipv6Addr::from(ip).to_string()
                    }
                    _ => return,
                };
                let mut port = [0u8; 2];
                socket.read_exact(&mut port).await.ok();
                let target = format!("{target}:{}", u16::from_be_bytes(port));
                obs.lines.lock().unwrap().push(target.clone());

                let Ok(mut upstream) = TcpStream::connect(&target).await else {
                    let _ = socket
                        .write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0])
                        .await;
                    return;
                };
                let _ = socket
                    .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
                    .await;
                let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
            });
        }
    });
    ProxyServer { addr, obs, handle }
}

fn settings(raw: &str) -> ProxySettings {
    ProxySettings {
        all: ProxySlot::from_raw(raw).unwrap(),
        ..ProxySettings::default()
    }
}

fn origin() -> (SocketAddr, JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let handle = tokio::spawn(async move {
        let listener = TcpListener::from_std(listener).unwrap();
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let _ = read_head(&mut socket).await;
                let body = b"origin-body";
                let _ = socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
                let _ = socket.write_all(body).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (addr, handle)
}

#[tokio::test]
async fn forward_proxy_uses_absolute_form_and_basic_auth() {
    let body = payload(1024);
    let proxy = start_forward_proxy(Some(("u", "p")), body.clone(), 200).await;
    let dest = temp_path("forward");

    let mut opts = options();
    opts.proxy = settings(&format!("http://u:p@127.0.0.1:{}", proxy.addr.port()));
    let url = "http://origin.invalid/file.bin".to_string();
    let report = download(&request(url, dest.clone(), opts), &no_dns())
        .await
        .expect("forward 代理下载应成功");

    assert!(report.via_proxy);
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    let lines = proxy.obs.lines.lock().unwrap();
    assert_eq!(
        lines.as_slice(),
        ["GET http://origin.invalid/file.bin HTTP/1.1"],
        "http 目标必须用 absolute-form 转发（HttpRequest.cc:144）"
    );
    assert_eq!(
        proxy.obs.auths.lock().unwrap().as_slice(),
        [Some(expected_basic("u", "p"))]
    );
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn forward_proxy_407_maps_to_error_33() {
    let proxy = start_forward_proxy(Some(("u", "p")), Vec::new(), 200).await;
    let mut opts = options();
    opts.proxy = settings(&format!("http://bad:creds@127.0.0.1:{}", proxy.addr.port()));
    let err = download(
        &request("http://origin.invalid/x".into(), temp_path("407"), opts),
        &no_dns(),
    )
    .await
    .expect_err("407 应报错");
    assert_eq!(err.to_aria2_code(), 33, "{err}");
}

#[tokio::test]
async fn connect_tunnel_407_maps_to_error_1() {
    // C++ `AbstractProxyResponseCommand.cc:74-76`：隧道 CONNECT 非 200 一律
    // EX_PROXY_CONNECTION_FAILED → errorCode=1，不走 33。
    let proxy = start_connect_proxy(Some(("u", "p"))).await;
    let mut opts = options();
    opts.proxy = settings(&format!("http://bad:creds@127.0.0.1:{}", proxy.addr.port()));
    let err = download(
        &request("https://127.0.0.1:9/x".into(), temp_path("tunnel-407"), opts),
        &no_dns(),
    )
    .await
    .expect_err("隧道 407 应报错");
    assert_eq!(err.to_aria2_code(), 1, "{err}");
}

#[tokio::test]
async fn proxy_method_tunnel_wraps_plain_http_in_connect() {
    let (origin, origin_handle) = origin();
    let proxy = start_connect_proxy(None).await;
    let dest = temp_path("tunnel-http");

    let mut opts = options();
    opts.proxy = settings(&format!("http://127.0.0.1:{}", proxy.addr.port()));
    opts.proxy_method_tunnel = true;
    let url = format!("http://127.0.0.1:{}/file.bin", origin.port());
    let report = download(&request(url, dest.clone(), opts), &no_dns())
        .await
        .expect("--proxy-method=tunnel 应走 CONNECT");

    assert!(report.via_proxy);
    assert_eq!(std::fs::read(&dest).unwrap(), b"origin-body");
    assert_eq!(
        proxy.obs.lines.lock().unwrap().as_slice(),
        [format!("CONNECT 127.0.0.1:{} HTTP/1.1", origin.port())]
    );
    origin_handle.abort();
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn connect_tunnel_never_offers_h2_alpn() {
    let body = payload(512);
    let tls_server = start_tls_server(&["h2", "http/1.1"], body.clone(), Duration::ZERO).await;
    let proxy = start_connect_proxy(Some(("u", "p"))).await;
    let dest = temp_path("tunnel-https");

    let mut opts = options();
    opts.check_certificate = false;
    opts.proxy = settings(&format!("http://u:p@127.0.0.1:{}", proxy.addr.port()));
    let url = format!("https://127.0.0.1:{}/file.bin", tls_server.addr.port());
    let report = download(&request(url, dest.clone(), opts), &no_dns())
        .await
        .expect("CONNECT 隧道内 TLS 下载应成功");

    assert!(report.via_proxy);
    assert!(!report.http2, "隧道内禁用 h2（Q6 I5）");
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert_eq!(
        proxy.obs.lines.lock().unwrap().as_slice(),
        [format!("CONNECT 127.0.0.1:{} HTTP/1.1", tls_server.addr.port())]
    );
    assert_eq!(
        tls_server.obs.alpns.lock().unwrap().as_slice(),
        ["http/1.1"],
        "隧道内 ALPN 只能宣告 http/1.1"
    );
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn socks5_with_auth_tunnels_to_origin() {
    let (origin, origin_handle) = origin();
    let proxy = start_socks5_proxy(Some(("u", "p"))).await;
    let dest = temp_path("socks5");

    let mut opts = options();
    opts.proxy = settings(&format!("socks5://u:p@127.0.0.1:{}", proxy.addr.port()));
    let url = format!("http://127.0.0.1:{}/file.bin", origin.port());
    let report = download(&request(url, dest.clone(), opts), &no_dns())
        .await
        .expect("SOCKS5 认证 + CONNECT 应成功");

    assert!(report.via_proxy);
    assert_eq!(std::fs::read(&dest).unwrap(), b"origin-body");
    assert_eq!(
        proxy.obs.lines.lock().unwrap().as_slice(),
        [format!("127.0.0.1:{}", origin.port())],
        "socks5:// 由本地解析域名后交 IP 给代理"
    );
    origin_handle.abort();
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn socks5_bad_password_maps_to_error_33() {
    let (origin, origin_handle) = origin();
    let proxy = start_socks5_proxy(Some(("u", "p"))).await;
    let mut opts = options();
    opts.proxy = settings(&format!("socks5://u:wrong@127.0.0.1:{}", proxy.addr.port()));
    let err = download(
        &request(
            format!("http://127.0.0.1:{}/x", origin.port()),
            temp_path("socks-bad"),
            opts,
        ),
        &no_dns(),
    )
    .await
    .expect_err("错误密码必须失败");
    assert_eq!(err.to_aria2_code(), 33, "{err}");
    origin_handle.abort();
}

#[tokio::test]
async fn no_proxy_bypasses_configured_proxy() {
    let (origin, origin_handle) = origin();
    let proxy = start_forward_proxy(None, b"proxy-body".to_vec(), 200).await;
    let dest = temp_path("no-proxy");

    let mut opts = options();
    opts.proxy = ProxySettings {
        all: ProxySlot::from_raw(&format!("http://127.0.0.1:{}", proxy.addr.port())).unwrap(),
        no_proxy: vec![engine_net::http::proxy::NoProxyRule::parse("127.0.0.1").unwrap()],
        ..ProxySettings::default()
    };
    let url = format!("http://127.0.0.1:{}/file.bin", origin.port());
    let report = download(&request(url, dest.clone(), opts), &no_dns())
        .await
        .expect("no-proxy 命中后应直连");

    assert!(!report.via_proxy);
    assert_eq!(std::fs::read(&dest).unwrap(), b"origin-body");
    assert!(proxy.obs.lines.lock().unwrap().is_empty(), "代理不应收到请求");
    origin_handle.abort();
    let _ = std::fs::remove_file(&dest);
}

// ---------------------------------------------------------------------------
// 双栈 / Happy Eyeballs（RFC 8305）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum MockDnsAnswer {
    A(Ipv4Addr),
}

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
    async fn start(answer: MockDnsAnswer, aaaa: Option<Ipv6Addr>) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 512];
            loop {
                let Ok((len, peer)) = socket.recv_from(&mut buf).await else {
                    break;
                };
                if let Some(resp) = dns_response(&buf[..len], answer, aaaa) {
                    let _ = socket.send_to(&resp, peer).await;
                }
            }
        });
        Self { addr, handle }
    }

    fn resolver(&self, ipv6: bool) -> DnsResolver {
        DnsResolver::new(&DnsConfig {
            servers: vec![self.addr],
            ipv6,
            use_hosts: false,
            timeout: Duration::from_secs(2),
            attempts: 1,
            ..DnsConfig::default()
        })
        .unwrap()
    }
}

fn dns_response(query: &[u8], answer: MockDnsAnswer, aaaa: Option<Ipv6Addr>) -> Option<Vec<u8>> {
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
    let answer_data = match qtype {
        1 => {
            let MockDnsAnswer::A(ip) = answer;
            Some((1u16, 4u16, ip.octets().to_vec()))
        }
        28 => aaaa.map(|ip| (28u16, 16u16, ip.octets().to_vec())),
        _ => None,
    };

    let mut resp = Vec::new();
    resp.extend_from_slice(&query[0..2]);
    resp.extend_from_slice(&[0x81, 0x80]);
    resp.extend_from_slice(&[0, 1]);
    resp.extend_from_slice(&{
        let count: u16 = if answer_data.is_some() { 1 } else { 0 };
        let mut bytes = [0u8; 2];
        bytes[0] = (count >> 8) as u8;
        bytes[1] = count as u8;
        bytes
    });
    resp.extend_from_slice(&[0, 0, 0, 0]);
    resp.extend_from_slice(question);
    if let Some((rtype, rdlen, data)) = answer_data {
        resp.extend_from_slice(&[0xc0, 0x0c]);
        resp.extend_from_slice(&rtype.to_be_bytes());
        resp.extend_from_slice(&[0, 1]);
        resp.extend_from_slice(&[0, 0, 0, 60]);
        resp.extend_from_slice(&rdlen.to_be_bytes());
        resp.extend_from_slice(&data);
    }
    Some(resp)
}

#[tokio::test]
async fn happy_eyeballs_falls_back_when_ipv6_is_unreachable() {
    let dns = MockDns::start(
        MockDnsAnswer::A(Ipv4Addr::LOCALHOST),
        Some(Ipv6Addr::LOCALHOST),
    )
    .await;
    let (origin, origin_handle) = origin();
    let dest = temp_path("dual");

    let mut opts = options();
    opts.connect_timeout = Duration::from_millis(900);
    let url = format!("http://dual.test:{}/file.bin", origin.port());
    let report = download(&request(url, dest.clone(), opts), &dns.resolver(true))
        .await
        .expect("IPv6 不可达时必须回退 IPv4");

    assert_eq!(report.completed_length, b"origin-body".len() as u64);
    assert_eq!(std::fs::read(&dest).unwrap(), b"origin-body");
    origin_handle.abort();
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn disable_ipv6_filters_aaaa_records() {
    let dns = MockDns::start(
        MockDnsAnswer::A(Ipv4Addr::LOCALHOST),
        Some(Ipv6Addr::LOCALHOST),
    )
    .await;
    let resolver = dns.resolver(false);
    let ips = resolver.resolve("dual.test").await.unwrap();
    assert_eq!(ips, vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    assert_eq!(
        resolver.resolve("127.0.0.1").await.unwrap(),
        vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
    );
}
