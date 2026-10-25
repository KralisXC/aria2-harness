//! DNS 垂直切片：本地 DNS 服务器 + 假域名（不访问真实网络）。
//!
//! 对应 weilai.md §5.4.2 的验证项；真实域名（raw.gitcode.com）验证需用户另行授权，
//! 见 docs/c3/Q1-root-cause.md §8（用户明确「不要对外发起真实网络请求」）。

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use engine_net::dns::resolver::{DnsConfig, DnsResolver};

/// 极简 DNS 响应器：按查询名后缀决定返回「正常 / ANCOUNT 撒谎 / 压缩指针自环」。
struct FakeDns {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
}

impl FakeDns {
    fn start() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("绑定本地 DNS 端口");
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("设置读超时");
        let addr = socket.local_addr().expect("本地地址");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();

        thread::spawn(move || {
            let mut buf = [0u8; 512];
            while !stop_thread.load(Ordering::Relaxed) {
                let (len, peer) = match socket.recv_from(&mut buf) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let query = &buf[..len];
                let Some(question) = parse_question(query) else {
                    continue;
                };
                let name = String::from_utf8_lossy(question).to_ascii_lowercase();
                let payload = if name.contains("lie") {
                    response(query, question, Mode::AncountLie)
                } else if name.contains("loop") {
                    response(query, question, Mode::PointerLoop)
                } else {
                    response(query, question, Mode::Good)
                };
                let _ = socket.send_to(&payload, peer);
            }
        });

        Self { addr, stop }
    }
}

impl Drop for FakeDns {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

enum Mode {
    Good,
    AncountLie,
    PointerLoop,
}

fn parse_question(packet: &[u8]) -> Option<&[u8]> {
    if packet.len() < 12 {
        return None;
    }
    let mut i = 12usize;
    loop {
        let len = *packet.get(i)? as usize;
        if len == 0 {
            i += 1;
            break;
        }
        i += 1 + len;
    }
    let end = i + 4; // qtype + qclass
    packet.get(12..end)
}

fn response(query: &[u8], question: &[u8], mode: Mode) -> Vec<u8> {
    let answer_name_offset = 12 + question.len();
    let mut out = Vec::with_capacity(64);

    out.extend_from_slice(&query[0..2]); // ID
    out.extend_from_slice(&[0x81, 0x80]); // QR=1 RD=1 RA=1 rcode=0
    out.extend_from_slice(&[0x00, 0x01]); // QDCOUNT

    let ancount: u16 = match mode {
        Mode::Good | Mode::PointerLoop => 1,
        Mode::AncountLie => 3,
    };
    out.extend_from_slice(&ancount.to_be_bytes());
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // NSCOUNT + ARCOUNT

    out.extend_from_slice(question);

    // 答案 NAME
    match mode {
        Mode::PointerLoop => {
            let ptr = 0xC000u16 | (answer_name_offset as u16);
            out.extend_from_slice(&ptr.to_be_bytes());
        }
        _ => out.extend_from_slice(&[0xC0, 0x0C]),
    }
    out.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // TYPE=A CLASS=IN
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0x3C]); // TTL=60
    out.extend_from_slice(&[0x00, 0x04]); // RDLENGTH
    out.extend_from_slice(&[127, 0, 0, 1]);
    out
}

fn resolver_for(dns: &FakeDns) -> DnsResolver {
    DnsResolver::new(&DnsConfig {
        servers: vec![dns.addr],
        use_hosts: false,
        timeout: Duration::from_millis(300),
        attempts: 1,
        ..DnsConfig::default()
    })
    .expect("构造 resolver")
}

#[tokio::test]
async fn resolves_from_local_dns_server() {
    let dns = FakeDns::start();
    let resolver = resolver_for(&dns);

    let ips = tokio::time::timeout(Duration::from_secs(5), resolver.resolve("good.test"))
        .await
        .expect("查询不应挂起")
        .expect("应当解析成功");

    assert_eq!(ips, vec![IpAddr::from([127, 0, 0, 1])]);
}

#[tokio::test]
async fn ancount_lie_returns_error_without_panic() {
    let dns = FakeDns::start();
    let resolver = resolver_for(&dns);

    let result = tokio::time::timeout(Duration::from_secs(5), resolver.resolve("lie.test"))
        .await
        .expect("解析器不得因畸形应答挂死");
    assert!(result.is_err());
}

#[tokio::test]
async fn pointer_loop_returns_error_without_panic() {
    let dns = FakeDns::start();
    let resolver = resolver_for(&dns);

    let result = tokio::time::timeout(Duration::from_secs(5), resolver.resolve("loop.test"))
        .await
        .expect("解析器不得因压缩指针自环挂死");
    assert!(result.is_err());
}
