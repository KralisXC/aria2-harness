//! host:port → 已建连 TcpStream + 实际 IP。
//!
//! SNI 与 IP 解耦：TCP 连 IP（本模块），TLS 的 `ServerName` 用域名（`http::tls`）。
//!
//! Phase 3：按 RFC 8305（Happy Eyeballs）并发建连。要点：
//! - 地址族交错排列（IPv6 优先，然后 v4/v6 交替），对齐 Q8 的“恢复 IPv6 时按 RFC 8305”；
//! - 首个尝试立即发起，后续尝试每隔 250 ms 启动一个（RFC 8305 §5 推荐值）；
//! - 任一成功即返回并取消其余尝试；全部失败返回最后一个错误；
//! - `--disable-ipv6=true` 时解析集合本身只有 IPv4，行为与 Phase 1 顺序尝试一致。

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::task::JoinSet;

use crate::dns::resolver::DnsResolver;
use crate::error::NetworkError;

/// RFC 8305 §5 推荐的重试间隔。
pub const CONNECTION_ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// 建连结果：调用方需要真实 IP 写审计日志 / 连接统计。
#[derive(Debug)]
pub struct Connected {
    pub stream: TcpStream,
    pub ip: IpAddr,
}

/// 按 RFC 8305 排列地址：首地址族优先（这里固定 IPv6 优先），随后两族交替。
pub fn rfc8305_order(ips: Vec<IpAddr>) -> Vec<IpAddr> {
    let mut v6 = Vec::new();
    let mut v4 = Vec::new();
    for ip in ips {
        if ip.is_ipv6() {
            v6.push(ip);
        } else {
            v4.push(ip);
        }
    }
    if v4.is_empty() {
        return v6;
    }
    if v6.is_empty() {
        return v4;
    }
    let mut ordered = Vec::with_capacity(v6.len() + v4.len());
    let mut v6_iter = v6.into_iter();
    let mut v4_iter = v4.into_iter();
    loop {
        let mut pushed = false;
        if let Some(ip) = v6_iter.next() {
            ordered.push(ip);
            pushed = true;
        }
        if let Some(ip) = v4_iter.next() {
            ordered.push(ip);
            pushed = true;
        }
        if !pushed {
            break;
        }
    }
    ordered
}

/// 解析域名后按 Happy Eyeballs 建连。
pub async fn connect(
    host: &str,
    port: u16,
    resolver: &DnsResolver,
    timeout: Duration,
) -> Result<Connected, NetworkError> {
    let ips = resolver.resolve(host).await?;
    connect_ips(ips, port, timeout).await
}

/// 直接对已解析的 IP 集合建连（测试与代理本地解析共用）。
pub async fn connect_ips(
    ips: Vec<IpAddr>,
    port: u16,
    timeout: Duration,
) -> Result<Connected, NetworkError> {
    if ips.is_empty() {
        return Err(NetworkError::DnsFailure("无可用地址".into()));
    }
    if ips.len() == 1 {
        return attempt_one(ips[0], port, timeout).await;
    }

    let mut pending = rfc8305_order(ips).into_iter();
    let mut attempts = JoinSet::new();
    let mut last_error: Option<NetworkError> = None;
    let mut exhausted = false;
    let mut next_start = tokio::time::Instant::now();

    loop {
        if attempts.is_empty() && exhausted {
            break;
        }

        tokio::select! {
            _ = tokio::time::sleep_until(next_start), if !exhausted => {
                match pending.next() {
                    Some(ip) => {
                        attempts.spawn(async move { attempt_one(ip, port, timeout).await });
                        next_start = tokio::time::Instant::now() + CONNECTION_ATTEMPT_DELAY;
                    }
                    None => exhausted = true,
                }
            }
            joined = attempts.join_next(), if !attempts.is_empty() => {
                match joined {
                    Some(Ok(Ok(connected))) => {
                        // 获胜者已拿到 socket；其余尝试立即取消（RFC 8305 §5）。
                        attempts.abort_all();
                        return Ok(connected);
                    }
                    Some(Ok(Err(err))) => last_error = Some(err),
                    Some(Err(join_err)) => {
                        last_error = Some(NetworkError::Network(format!(
                            "建连任务异常结束：{join_err}"
                        )));
                    }
                    None => {}
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| NetworkError::Network("所有地址均建连失败".into())))
}

async fn attempt_one(ip: IpAddr, port: u16, timeout: Duration) -> Result<Connected, NetworkError> {
    let addr = SocketAddr::new(ip, port);
    match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => {
            // 下载链路都是大块传输，关 Nagle。
            let _ = stream.set_nodelay(true);
            Ok(Connected { stream, ip })
        }
        Ok(Err(err)) => Err(NetworkError::from(err)),
        Err(_) => Err(NetworkError::Timeout),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dns::resolver::{DnsConfig, DnsResolver};

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("合法 IP")
    }

    #[test]
    fn rfc8305_interleaves_v6_and_v4() {
        let ordered = rfc8305_order(vec![
            ip("1.1.1.1"),
            ip("2.2.2.2"),
            ip("2001:db8::1"),
            ip("3.3.3.3"),
        ]);
        assert_eq!(
            ordered,
            vec![
                ip("2001:db8::1"),
                ip("1.1.1.1"),
                ip("2.2.2.2"),
                ip("3.3.3.3")
            ]
        );
    }

    #[test]
    fn rfc8305_keeps_single_family_order() {
        let v4 = vec![ip("1.1.1.1"), ip("2.2.2.2")];
        assert_eq!(rfc8305_order(v4.clone()), v4);
        let v6 = vec![ip("2001:db8::2"), ip("2001:db8::1")];
        assert_eq!(rfc8305_order(v6.clone()), v6);
    }

    #[tokio::test]
    async fn connects_to_literal_ip_without_dns_query() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定本地监听");
        let port = listener.local_addr().expect("本地地址").port();

        let resolver = DnsResolver::new(&DnsConfig {
            servers: vec!["127.0.0.1:1".parse().unwrap()],
            use_hosts: false,
            timeout: Duration::from_millis(50),
            attempts: 1,
            ..DnsConfig::default()
        })
        .expect("构造 resolver");

        let connected = connect("127.0.0.1", port, &resolver, Duration::from_secs(1))
            .await
            .expect("建连应成功");
        assert_eq!(connected.ip, IpAddr::from([127, 0, 0, 1]));
    }

    /// 一个不可达的 IPv6 地址 + 一个可用的 IPv4：250 ms 后并发启动第二个，整体应在秒级返回。
    #[tokio::test]
    async fn happy_eyeballs_falls_back_to_second_family() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定本地监听");
        let port = listener.local_addr().expect("本地地址").port();

        let started = std::time::Instant::now();
        let connected = connect_ips(
            vec![
                ip("2001:db8::1"), // TEST-NET，必不可达
                ip("127.0.0.1"),
            ],
            port,
            Duration::from_secs(2),
        )
        .await
        .expect("应回退到 IPv4");
        assert_eq!(connected.ip, ip("127.0.0.1"));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "Happy Eyeballs 不应等首个地址超时：{:?}",
            started.elapsed()
        );
    }
}
