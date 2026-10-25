//! 5.4.4 断点持久化：`*.aria2` 控制文件 + session 条目（本地，不访问外网）。

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engine_net::error::NetworkError;
use engine_net::http::client::{DownloadRequest, HttpOptions, download};
use engine_net::progress::control::{ControlFile, ControlFileError, ControlFileStore};
use engine_net::progress::session::{SessionEntry, SessionFile};

fn temp_path(tag: &str, suffix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间")
        .as_nanos();
    let mut path = std::env::temp_dir();
    path.push(format!(
        "engine-net-resume-{}-{tag}-{nanos}{suffix}",
        std::process::id()
    ));
    path
}

#[test]
fn control_file_store_roundtrip_and_dedup() {
    let path = temp_path("control", ".aria2");
    let mut store = ControlFileStore::new(path.clone());

    let mut control = ControlFile::new(10, 5);
    control.mark_piece_done(0);

    assert!(store.save(&control).expect("首次写入"));
    assert!(!store.save(&control).expect("内容未变应跳过"));
    assert!(store.exists());

    let loaded = store.load().expect("读取").expect("应存在");
    assert_eq!(loaded, control);
    assert_eq!(loaded.completed_bytes(), 5);

    store.remove().expect("删除");
    assert!(!store.exists());
    assert!(store.load().expect("读取缺失文件").is_none());
}

#[test]
fn control_file_rejects_piece_length_change() {
    // aria2 只在「已有进度」时才因 piece length 变化而拒绝（DefaultBtProgressInfoFile.cc:400）。
    let mut control = ControlFile::new(10, 5);
    control.mark_piece_done(0);
    let err = control
        .validate_against(10, 1024, false)
        .expect_err("piece length 变化应被拒绝");
    assert_eq!(err, ControlFileError::PieceLengthChanged);
    assert_eq!(err.to_aria2_code(), 10);
    // 无进度时允许改 piece length。
    assert!(ControlFile::new(10, 5).validate_against(10, 1024, false).is_ok());
}

#[test]
fn control_file_rejects_total_length_change() {
    let control = ControlFile::new(10, 5);
    let err = control
        .validate_against(20, 5, false)
        .expect_err("总长变化应被拒绝");
    assert_eq!(err.to_aria2_code(), 8);
}

#[test]
fn session_roundtrip_on_disk() {
    let path = temp_path("session", ".session");
    let session = SessionFile {
        entries: vec![SessionEntry {
            uris: vec!["http://127.0.0.1:9/f".into()],
            options: vec![
                ("gid".into(), "fffe0000000000aa".into()),
                ("pause".into(), "true".into()),
                ("header".into(), "Accept: */*".into()),
            ],
        }],
    };

    session.save(&path).expect("写 session");
    let loaded = SessionFile::load(&path).expect("读 session").expect("应存在");
    assert_eq!(loaded, session);
    assert_eq!(loaded.entries[0].gid(), Some("fffe0000000000aa"));
    assert!(loaded.entries[0].is_paused());

    let _ = std::fs::remove_file(&path);
}

/// 用控制文件的 completed_bytes 作为 resume_from，验证断点重新接入 HTTP Range。
#[tokio::test]
async fn resume_offset_comes_from_control_file() {
    // 本地服务器：只接受 Range: bytes=5-，返回 206 + 余下 5 字节。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定测试端口");
    let port = listener.local_addr().expect("本地地址").port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = socket.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let text = String::from_utf8_lossy(&buf).to_ascii_lowercase();
        assert!(text.contains("range: bytes=5-"), "实际请求：{text}");
        let body = b"56789";
        let head = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 5-9/10\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(body).await;
        let _ = socket.shutdown().await;
    });

    let dest = temp_path("partial", ".bin");
    std::fs::write(&dest, b"01234").expect("预置部分文件");

    let control_path = temp_path("partial", ".aria2");
    let mut control = ControlFile::new(10, 5);
    control.mark_piece_done(0);
    ControlFileStore::new(control_path.clone())
        .save(&control)
        .expect("写控制文件");

    let resume_from = ControlFileStore::new(control_path.clone())
        .load()
        .expect("读控制文件")
        .expect("控制文件存在")
        .completed_bytes();
    assert_eq!(resume_from, 5);

    let resolver = engine_net::dns::resolver::DnsResolver::new(
        &engine_net::dns::resolver::DnsConfig {
            servers: vec!["127.0.0.1:1".parse().unwrap()],
            use_hosts: false,
            timeout: Duration::from_millis(50),
            attempts: 1,
            ..Default::default()
        },
    )
    .expect("构造 resolver");

    let request = DownloadRequest {
        url: format!("http://127.0.0.1:{port}/f.bin"),
        dest: dest.clone(),
        resume_from,
        options: HttpOptions {
            connect_timeout: Duration::from_secs(2),
            read_timeout: Some(Duration::from_secs(2)),
            ..HttpOptions::default()
        },
    };

    let report = download(&request, &resolver).await.expect("断点下载");
    assert_eq!(report.total_length, Some(10));
    assert_eq!(std::fs::read(&dest).expect("读结果"), b"0123456789");
    assert_eq!(NetworkError::RangeNotSupported.to_aria2_code(), 8);

    let _ = std::fs::remove_file(&dest);
    let _ = std::fs::remove_file(&control_path);
}
