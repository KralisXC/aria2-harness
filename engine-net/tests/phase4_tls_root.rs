//! Phase 4 信任根验证（`#[ignore]`：会临时改动当前用户的 Root 证书存储）。
//!
//! 目的：证明 `check-certificate=true` 时走的是**操作系统证书存储**
//! （`rustls-platform-verifier`），而不是只认 webpki 内置根。企业内网 CA 正是
//! 通过这种方式被信任的。
//!
//! 流程：rcgen 生成测试 CA + 由它签发的服务端证书 → `certutil -addstore -user Root`
//! 导入 CurrentUser Root → 握手成功并完成下载 → `certutil -delstore` 清理。
//! 非 Windows 或 certutil 缺失时打印原因并跳过。
//!
//! 与 spec §7 的差异：服务端证书同时带 `sni.test` 与 IP SAN `127.0.0.1`，
//! 这样不需要改 hosts 文件即可用回环地址验证（已记入 Phase4-acceptance.md）。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engine_net::dns::resolver::{DnsConfig, DnsResolver};
use engine_net::http::client::{DownloadRequest, HttpOptions, download};
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const CA_NAME: &str = "Engine Net Test Root";

fn temp_path(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "engine-net-p4-tls-{}-{tag}-{nanos}",
        std::process::id()
    ))
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

fn digest(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 生成 CA 与服务器证书；返回 CA DER 与服务端 CertifiedKey。
fn issue_certificate() -> (CertificateDer<'static>, Arc<CertifiedKey>) {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, CA_NAME);
    let ca: CertifiedIssuer<KeyPair> = CertifiedIssuer::self_signed(ca_params, ca_key).unwrap();

    let server_key = KeyPair::generate().unwrap();
    let server_params =
        CertificateParams::new(vec!["127.0.0.1".to_string(), "sni.test".to_string()]).unwrap();
    let server_cert = server_params.signed_by(&server_key, &ca).unwrap();

    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der()));
    let signing = rustls::crypto::ring::sign::any_supported_type(&key).unwrap();
    (
        ca.der().clone(),
        Arc::new(CertifiedKey::new(vec![server_cert.der().clone()], signing)),
    )
}

async fn start_tls_server(certified: Arc<CertifiedKey>, body: Vec<u8>) -> (SocketAddr, JoinHandle<()>) {
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(AlwaysResolve(certified)));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let body = Arc::new(body);

    let handle = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            let body = Arc::clone(&body);
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(socket).await else {
                    return;
                };
                let service = service_fn(move |_| {
                    let body = Arc::clone(&body);
                    async move {
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
    (addr, handle)
}

#[derive(Debug)]
struct AlwaysResolve(Arc<CertifiedKey>);

impl rustls::server::ResolvesServerCert for AlwaysResolve {
    fn resolve(&self, _hello: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(&self.0))
    }
}

fn certutil(args: &[&str]) -> std::process::Output {
    std::process::Command::new("certutil")
        .args(args)
        .output()
        .expect("执行 certutil")
}

#[tokio::test]
#[ignore = "会临时修改 CurrentUser Root 证书存储，需手动运行"]
async fn platform_verifier_trusts_user_root_store() {
    if !cfg!(windows) {
        eprintln!("非 Windows：跳过平台信任根测试");
        return;
    }
    if std::process::Command::new("certutil")
        .arg("-?")
        .output()
        .map(|output| !output.status.success())
        .unwrap_or(true)
    {
        eprintln!("certutil 不可用：跳过平台信任根测试");
        return;
    }

    let (ca_der, certified) = issue_certificate();
    let ca_file = temp_path("root.cer");
    std::fs::write(&ca_file, ca_der.as_ref()).expect("写 CA 文件");

    let added = certutil(&["-addstore", "-user", "Root", ca_file.to_str().unwrap()]);
    assert!(
        added.status.success(),
        "导入 CurrentUser Root 失败：{}",
        String::from_utf8_lossy(&added.stderr)
    );

    let (addr, server) = start_tls_server(Arc::clone(&certified), vec![7u8; 4 * 1024 * 1024]).await;
    let dest = temp_path("body.bin");
    let request = DownloadRequest {
        url: format!("https://127.0.0.1:{}/body.bin", addr.port()),
        dest: dest.clone(),
        resume_from: 0,
        options: HttpOptions {
            check_certificate: true,
            enable_http2: false,
            connect_timeout: Duration::from_secs(5),
            read_timeout: Some(Duration::from_secs(10)),
            max_tries: 1,
            ..HttpOptions::default()
        },
    };
    let result = download(&request, &no_dns()).await;

    // 先清理证书存储，再断言结果（失败时也不留垃圾）。
    let removed = certutil(&["-delstore", "-user", "Root", CA_NAME]);
    let _ = std::fs::remove_file(&ca_file);
    server.abort();

    assert!(
        removed.status.success(),
        "清理 CurrentUser Root 失败：{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let report = result.expect("平台信任根下握手必须成功");
    assert_eq!(report.completed_length, 4 * 1024 * 1024);
    assert_eq!(digest(&std::fs::read(&dest).unwrap()), digest(&vec![7u8; 4 * 1024 * 1024]));
    let _ = std::fs::remove_file(&dest);
}
