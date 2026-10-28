//! 5.4.3 最小 HTTP 直链：Range GET / 重定向 / 落盘（全部本地，不访问外网）。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engine_net::dns::resolver::{DnsConfig, DnsResolver};
use engine_net::http::client::{DownloadRequest, HttpOptions, download};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Default)]
struct Req {
    path: String,
    headers: HashMap<String, String>,
}

struct TestServer {
    addr: SocketAddr,
    handle: JoinHandle<()>,
}

impl TestServer {
    async fn start<F>(handler: F) -> Self
    where
        F: Fn(&Req) -> Vec<u8> + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定测试端口");
        let addr = listener.local_addr().expect("本地地址");
        let handler = Arc::new(handler);

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let req = parse_request(&buf);
                    let resp = handler(&req);
                    let _ = socket.write_all(&resp).await;
                    let _ = socket.shutdown().await;
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

fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\n");
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn temp_path(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间")
        .as_nanos();
    let mut path = std::env::temp_dir();
    path.push(format!(
        "engine-net-{}-{tag}-{nanos}.bin",
        std::process::id()
    ));
    path
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

fn options() -> HttpOptions {
    HttpOptions {
        connect_timeout: Duration::from_secs(2),
        read_timeout: Some(Duration::from_secs(2)),
        ..HttpOptions::default()
    }
}

#[tokio::test]
async fn downloads_full_body_without_range() {
    let server = TestServer::start(|req| {
        assert_eq!(req.path, "/file.bin");
        assert!(req.headers.get("range").is_none());
        response("200 OK", &[("Content-Type", "application/octet-stream")], b"hello world")
    })
    .await;

    let dest = temp_path("full");
    let req = DownloadRequest {
        url: format!("http://127.0.0.1:{}/file.bin", server.addr.port()),
        dest: dest.clone(),
        resume_from: 0,
        options: options(),
    };

    let report = download(&req, &resolver()).await.expect("下载应成功");
    assert_eq!(report.total_length, Some(11));
    assert_eq!(report.written, 11);
    assert_eq!(report.completed_length, 11);
    assert!(!report.range_used);
    assert_eq!(std::fs::read(&dest).unwrap(), b"hello world");
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn resumes_with_range_and_206() {
    let seen = Arc::new(Mutex::new(None::<Req>));
    let seen_handler = Arc::clone(&seen);
    let server = TestServer::start(move |req| {
        *seen_handler.lock().unwrap() = Some(req.clone());
        response(
            "206 Partial Content",
            &[("Content-Range", "bytes 5-9/10")],
            b"56789",
        )
    })
    .await;

    let dest = temp_path("resume");
    std::fs::write(&dest, b"01234").unwrap();

    let req = DownloadRequest {
        url: format!("http://127.0.0.1:{}/file.bin", server.addr.port()),
        dest: dest.clone(),
        resume_from: 5,
        options: options(),
    };

    let report = download(&req, &resolver()).await.expect("续传应成功");
    assert_eq!(report.range_used, true);
    assert_eq!(report.total_length, Some(10));
    assert_eq!(report.completed_length, 10);

    let captured = seen.lock().unwrap().clone().expect("服务端应收到请求");
    assert_eq!(
        captured.headers.get("range").map(String::as_str),
        Some("bytes=5-")
    );

    assert_eq!(std::fs::read(&dest).unwrap(), b"0123456789");
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn follows_302_redirect() {
    let server = TestServer::start(|req| {
        if req.path == "/start" {
            response("302 Found", &[("Location", "/final")], b"")
        } else {
            response("200 OK", &[], b"redirected")
        }
    })
    .await;

    let dest = temp_path("redirect");
    let req = DownloadRequest {
        url: format!("http://127.0.0.1:{}/start", server.addr.port()),
        dest: dest.clone(),
        resume_from: 0,
        options: options(),
    };

    let report = download(&req, &resolver()).await.expect("跟随重定向");
    assert_eq!(report.redirects.len(), 1);
    assert!(report.final_url.ends_with("/final"));
    assert_eq!(std::fs::read(&dest).unwrap(), b"redirected");
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn server_ignoring_range_maps_to_cannot_resume() {
    let server = TestServer::start(|_| response("200 OK", &[], b"0123456789")).await;

    let dest = temp_path("ignore-range");
    std::fs::write(&dest, b"01234").unwrap();

    let req = DownloadRequest {
        url: format!("http://127.0.0.1:{}/file.bin", server.addr.port()),
        dest: dest.clone(),
        resume_from: 5,
        options: options(),
    };

    let err = download(&req, &resolver()).await.expect_err("应报 Range 不支持");
    assert_eq!(err.to_aria2_code(), 8);
    // 校验失败发生在打开文件之前，原文件不得被截断。
    assert_eq!(std::fs::read(&dest).unwrap(), b"01234");
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn not_found_maps_to_resource_not_found() {
    let server = TestServer::start(|_| response("404 Not Found", &[], b"")).await;

    let dest = temp_path("404");
    let req = DownloadRequest {
        url: format!("http://127.0.0.1:{}/missing", server.addr.port()),
        dest: dest.clone(),
        resume_from: 0,
        options: options(),
    };

    let err = download(&req, &resolver()).await.expect_err("404 应报错");
    assert_eq!(err.to_aria2_code(), 3);
    let _ = std::fs::remove_file(&dest);
}

// ---------------------------------------------------------------------------
// beta10 模块 D：跳转链 Cookie 透传
// ---------------------------------------------------------------------------

#[tokio::test]
async fn redirect_chain_carries_cookie_and_absorbs_set_cookie() {
    let seen = Arc::new(Mutex::new(Vec::<Req>::new()));
    let seen_handler = Arc::clone(&seen);
    let server = TestServer::start(move |req| {
        seen_handler.lock().unwrap().push(req.clone());
        match req.path.as_str() {
            "/start" => response(
                "302 Found",
                &[("Location", "/mid"), ("Set-Cookie", "s=2; Path=/; HttpOnly")],
                b"",
            ),
            "/mid" => response("302 Found", &[("Location", "/final")], b""),
            _ => response("200 OK", &[], b"ok"),
        }
    })
    .await;

    let mut opts = options();
    opts.headers = vec![("Cookie".to_string(), "a=1".to_string())];

    let dest = temp_path("cookie-chain");
    let req = DownloadRequest {
        url: format!("http://127.0.0.1:{}/start", server.addr.port()),
        dest: dest.clone(),
        resume_from: 0,
        options: opts,
    };

    let report = download(&req, &resolver()).await.expect("跟随重定向");
    assert_eq!(report.redirects.len(), 2);
    assert!(report.final_url.ends_with("/final"));

    let requests = seen.lock().unwrap().clone();
    let cookie_of = |path: &str| {
        requests
            .iter()
            .find(|seen| seen.path == path)
            .and_then(|seen| seen.headers.get("cookie").cloned())
            .unwrap_or_default()
    };
    // 每一跳都携带初始 Cookie。
    assert_eq!(cookie_of("/start"), "a=1");
    // 第一跳的 Set-Cookie 同 host，进入下一跳。
    assert!(cookie_of("/mid").contains("a=1"));
    assert!(cookie_of("/mid").contains("s=2"));
    assert!(cookie_of("/final").contains("s=2"));

    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn twenty_redirects_succeed() {
    // /r/0 → ... → /r/20：跟随 20 次 302 后拿到 200。
    let server = TestServer::start(|req| {
        let n: u32 = req.path.trim_start_matches("/r/").parse().unwrap_or(0);
        if n < 20 {
            response(
                "302 Found",
                &[("Location", &format!("/r/{}", n + 1))],
                b"",
            )
        } else {
            response("200 OK", &[], b"done")
        }
    })
    .await;

    let dest = temp_path("redirect-20");
    let req = DownloadRequest {
        url: format!("http://127.0.0.1:{}/r/0", server.addr.port()),
        dest: dest.clone(),
        resume_from: 0,
        options: options(),
    };

    let report = download(&req, &resolver()).await.expect("20 跳应成功");
    assert_eq!(report.redirects.len(), 20);
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn the_twenty_first_redirect_is_rejected() {
    let server = TestServer::start(|_| response("302 Found", &[("Location", "/loop")], b"")).await;

    let dest = temp_path("redirect-loop");
    let req = DownloadRequest {
        url: format!("http://127.0.0.1:{}/loop", server.addr.port()),
        dest: dest.clone(),
        resume_from: 0,
        options: options(),
    };

    let err = download(&req, &resolver())
        .await
        .expect_err("第 21 跳应报重定向过多");
    assert!(matches!(
        err,
        engine_net::error::NetworkError::TooManyRedirects
    ));
    let _ = std::fs::remove_file(&dest);
}

// ---------------------------------------------------------------------------
// beta10 模块 E：HTML 误下载兜底
// ---------------------------------------------------------------------------

#[tokio::test]
async fn html_login_page_after_redirect_fails_before_writing() {
    let server = TestServer::start(|req| {
        if req.path == "/start" {
            response("302 Found", &[("Location", "/login")], b"")
        } else {
            response(
                "200 OK",
                &[("Content-Type", "text/html; charset=utf-8")],
                b"<html>login expired</html>",
            )
        }
    })
    .await;

    let dest = temp_path("html-login");
    let req = DownloadRequest {
        url: format!("http://127.0.0.1:{}/start", server.addr.port()),
        dest: dest.clone(),
        resume_from: 0,
        options: options(),
    };

    let err = download(&req, &resolver()).await.expect_err("登录页应被拦截");
    assert_eq!(err.to_aria2_code(), 22);
    assert!(!dest.exists(), "判定必须在打开目标文件之前");
}

#[tokio::test]
async fn html_with_attachment_disposition_is_allowed() {
    let server = TestServer::start(|_| {
        response(
            "200 OK",
            &[
                ("Content-Type", "text/html"),
                ("Content-Disposition", "attachment; filename=\"page.html\""),
            ],
            b"<html>saved</html>",
        )
    })
    .await;

    let dest = temp_path("html-attachment");
    let req = DownloadRequest {
        url: format!("http://127.0.0.1:{}/page", server.addr.port()),
        dest: dest.clone(),
        resume_from: 0,
        options: options(),
    };

    let report = download(&req, &resolver()).await.expect("显式附件应放行");
    assert_eq!(report.written, b"<html>saved</html>".len() as u64);
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn html_error_page_keeps_rate_limit_semantics() {
    let server = TestServer::start(|_| {
        response(
            "403 Forbidden",
            &[("Content-Type", "text/html")],
            b"<html>forbidden</html>",
        )
    })
    .await;

    let dest = temp_path("html-403");
    let req = DownloadRequest {
        url: format!("http://127.0.0.1:{}/blocked", server.addr.port()),
        dest: dest.clone(),
        resume_from: 0,
        options: options(),
    };

    let err = download(&req, &resolver()).await.expect_err("403 应走限流");
    // 403 的 HTML 错误页不能被当成「误下载网页」：交给限流降级处理。
    assert!(err.is_rate_limited());
    assert!(!dest.exists());
}
