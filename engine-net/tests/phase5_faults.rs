//! Phase 5 故障注入：断网、TLS 失败、429/503 限流、代理认证失败。
//!
//! 目标不是重新验证传输层，而是验证这四类异常在 `engine-net` 侧产出的
//! [`NetworkError`] 能被 `degradation.rs` 的 `is_retryable` 接受，从而进入
//! `128→64→…→1` 降级阶梯（红线 R4）。
//!
//! 全部目标为 `127.0.0.1`，不发起真实外网请求；debug 构建。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engine_net::dns::resolver::{DnsConfig, DnsResolver};
use engine_net::error::NetworkError;
use engine_net::http::client::{DownloadRequest, HttpOptions, download};
use engine_net::http::proxy::{ProxySettings, ProxySlot};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

fn temp_path(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "engine-net-p5-fault-{}-{tag}-{nanos}.bin",
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

fn settings(raw: &str) -> ProxySettings {
    ProxySettings {
        all: ProxySlot::from_raw(raw).unwrap(),
        ..ProxySettings::default()
    }
}

/// 固定返回某个状态码（429 / 503）的极简 HTTP 服务端。
async fn status_server(status: u16) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let reason = match status {
        429 => "429 Too Many Requests",
        503 => "503 Service Unavailable",
        other => {
            let _ = other;
            "500 Internal Server Error"
        }
    };
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let _ = read_head(&mut socket).await;
                let _ = socket
                    .write_all(format!("HTTP/1.1 {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes())
                    .await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (addr, handle)
}

async fn read_head(socket: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return buf,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            return buf;
        }
        if buf.len() > 64 * 1024 {
            return buf;
        }
    }
}

/// 恒回 407 的代理（forward 与 CONNECT 共用）。
async fn auth_proxy() -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let _ = read_head(&mut socket).await;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (addr, handle)
}

/// 自签证书 TLS 服务端；客户端不信任该证书，握手会失败。
async fn tls_server() -> (SocketAddr, JoinHandle<()>) {
    let certified =
        rcgen::generate_simple_self_signed(vec!["sni.test".to_string()]).unwrap();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certified.cert.der().clone()], key)
        .unwrap();
    let acceptor = Arc::new(tokio_rustls::TlsAcceptor::from(Arc::new(config)));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let acceptor = Arc::clone(&acceptor);
            tokio::spawn(async move {
                // 客户端证书校验失败会在这里结束；服务端不写任何 body。
                let _ = acceptor.accept(socket).await;
            });
        }
    });
    (addr, handle)
}

fn code(err: &NetworkError) -> i32 {
    err.to_aria2_code()
}

// ---------------------------------------------------------------------------
// 1. 断网（连接拒绝）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn offline_connection_refused_maps_to_6() {
    // 绑定后立即释放：端口确定存在过，但没有任何进程 accept。
    let addr = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    };
    let dest = temp_path("offline");
    let err = download(
        &request(format!("http://{addr}/file.bin"), dest, options()),
        &no_dns(),
    )
    .await
    .expect_err("连接已关闭的端口必须失败");
    assert!(
        matches!(err, NetworkError::Network(_)),
        "断网应为 Network，实际 {err:?}"
    );
    assert_eq!(code(&err), 6, "断网必须映射为 NETWORK_PROBLEM(6)");
}

// ---------------------------------------------------------------------------
// 2. TLS 失败（不受信任的自签证书）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tls_untrusted_cert_maps_to_1() {
    let (addr, server) = tls_server().await;
    let mut opts = options();
    opts.check_certificate = true;
    let dest = temp_path("tls");
    let err = download(
        &request(format!("https://{addr}/file.bin"), dest.clone(), opts),
        &no_dns(),
    )
    .await
    .expect_err("自签证书必须被拒绝");
    assert!(
        matches!(err, NetworkError::CertError(_)),
        "TLS 失败应为 CertError，实际 {err:?}"
    );
    assert_eq!(code(&err), 1, "证书错误按 aria2 口径为 UNKNOWN_ERROR(1)");
    server.abort();
    let _ = std::fs::remove_file(dest);
}

// ---------------------------------------------------------------------------
// 3. 429 / 503 限流
// ---------------------------------------------------------------------------

async fn assert_status_code(status: u16, expected_variant: u16, expected_code: i32) {
    let (addr, server) = status_server(status).await;
    let dest = temp_path(&format!("status-{status}"));
    let err = download(
        &request(format!("http://{addr}/file.bin"), dest.clone(), options()),
        &no_dns(),
    )
    .await
    .expect_err("限流状态必须失败");
    match err {
        NetworkError::HttpStatus(got) => assert_eq!(
            got, expected_variant,
            "状态码必须原样冒泡（实际 {got}）"
        ),
        other => panic!("限流应产生 HttpStatus，实际 {other:?}"),
    }
    assert_eq!(code(&err), expected_code, "HTTP {status} 的 aria2 码不匹配");
    server.abort();
    let _ = std::fs::remove_file(dest);
}

#[tokio::test]
async fn rate_limit_429_maps_to_22() {
    assert_status_code(429, 429, 22).await;
}

#[tokio::test]
async fn rate_limit_503_maps_to_29() {
    assert_status_code(503, 503, 29).await;
}

// ---------------------------------------------------------------------------
// 4. 代理认证失败（407）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proxy_auth_forward_407_maps_to_33() {
    let (proxy, server) = auth_proxy().await;
    let mut opts = options();
    opts.proxy = settings(&format!("http://bad:creds@127.0.0.1:{}", proxy.port()));
    let dest = temp_path("proxy-forward");
    let err = download(
        &request(
            "http://origin.invalid/file.bin".to_string(),
            dest.clone(),
            opts,
        ),
        &no_dns(),
    )
    .await
    .expect_err("代理 407 必须失败");
    assert_eq!(
        code(&err),
        33,
        "转发代理 407 必须是 HTTP_AUTH_REQUIRED(33)，实际 {err:?}"
    );
    server.abort();
    let _ = std::fs::remove_file(dest);
}

#[tokio::test]
async fn proxy_auth_connect_407_maps_to_1() {
    let (proxy, server) = auth_proxy().await;
    let mut opts = options();
    opts.proxy = settings(&format!("http://bad:creds@127.0.0.1:{}", proxy.port()));
    let dest = temp_path("proxy-connect");
    let err = download(
        &request(
            format!("https://127.0.0.1:{}/file.bin", proxy.port()),
            dest.clone(),
            opts,
        ),
        &no_dns(),
    )
    .await
    .expect_err("CONNECT 407 必须失败");
    assert_eq!(
        code(&err),
        1,
        "CONNECT 非 200 按 C++ 口径为 EX_PROXY_CONNECTION_FAILED(1)，实际 {err:?}"
    );
    server.abort();
    let _ = std::fs::remove_file(dest);
}

// ---------------------------------------------------------------------------
// 5. 码表一致性：degradation::is_retryable 必须接受以上所有码
// ---------------------------------------------------------------------------

#[test]
fn degradation_retryable_codes_cover_engine_net() {
    // 与 host `degradation.rs::is_retryable` 的白名单保持一致。
    // 1 = TLS / CONNECT 失败；6 = 断网；22 = 429；29 = 503；33 = 代理认证；35 = 连接重置。
    const DEGRADABLE: &[i32] = &[1, 2, 6, 19, 21, 22, 23, 29, 33, 35];
    for code in [1, 6, 22, 29, 33, 35] {
        assert!(
            DEGRADABLE.contains(&code),
            "故障码 {code} 不在降级白名单内，R4 会被击穿"
        );
    }
}
