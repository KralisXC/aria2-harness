//! hickory-resolver 封装：hosts / TTL 缓存 / 双栈 `Vec<IpAddr>`。
//!
//! 对齐口径（`docs/c3/Q8-dns-baseline.md`）：
//! - nameserver 默认与产品 aria2c 启动参数一致（`--async-dns-server=223.5.5.5,119.29.29.29`）；
//! - 产品固定 `--disable-ipv6=true`，故默认只返回 IPv4；
//! - hosts 文件由 hickory 读取系统路径（含 Windows `System32\drivers\etc\hosts`）；
//! - TTL 缓存由 hickory 自带 LRU 承担，容量可配。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use hickory_resolver::config::{
    LookupIpStrategy, NameServerConfig, NameServerConfigGroup, ResolveHosts, ResolverConfig,
    ResolverOpts,
};
use hickory_resolver::name_server::TokioConnectionProvider;
use hickory_resolver::proto::xfer::Protocol;
use hickory_resolver::Resolver;

use crate::error::NetworkError;

/// 与 `aria2_service.rs` 的 `--async-dns-server` 保持一致的默认值。
pub const DEFAULT_NAMESERVERS: [SocketAddr; 2] = [
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(223, 5, 5, 5)), 53),
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(119, 29, 29, 29)), 53),
];

#[derive(Debug, Clone)]
pub struct DnsConfig {
    /// 自定义 DNS 服务器（对应 aria2 `--async-dns-server`）；空 = 用系统配置。
    pub servers: Vec<SocketAddr>,
    /// 是否返回 IPv6（产品默认关闭，对齐 `--disable-ipv6=true`）。
    pub ipv6: bool,
    /// 缓存条目上限（hickory `ResolverOpts::cache_size`）。
    pub cache_size: usize,
    /// 是否读取系统 hosts。
    pub use_hosts: bool,
    /// 单次查询超时。
    pub timeout: Duration,
    /// 单服务器尝试次数。
    pub attempts: usize,
}

impl Default for DnsConfig {
    fn default() -> Self {
        Self {
            servers: DEFAULT_NAMESERVERS.to_vec(),
            ipv6: false,
            cache_size: 256,
            use_hosts: true,
            timeout: Duration::from_secs(5),
            attempts: 2,
        }
    }
}

/// 解析器句柄，可 `Clone` 后在多任务间共享（hickory 内部已用 Arc）。
#[derive(Clone)]
pub struct DnsResolver {
    inner: Resolver<TokioConnectionProvider>,
    ipv6: bool,
}

impl DnsResolver {
    pub fn new(cfg: &DnsConfig) -> Result<Self, NetworkError> {
        let builder = if cfg.servers.is_empty() {
            // 系统配置（Windows 走 ipconfig/注册表，Unix 走 resolv.conf）。
            Resolver::builder_tokio().map_err(|err| {
                NetworkError::DnsFailure(format!("读取系统 DNS 配置失败：{err}"))
            })?
        } else {
            let mut servers = Vec::with_capacity(cfg.servers.len() * 2);
            for addr in &cfg.servers {
                // UDP 优先，TC 截断时 hickory 会退到 TCP；两条都显式注册。
                servers.push(NameServerConfig::new(*addr, Protocol::Udp));
                servers.push(NameServerConfig::new(*addr, Protocol::Tcp));
            }
            let config = ResolverConfig::from_parts(
                None,
                Vec::new(),
                NameServerConfigGroup::from(servers),
            );
            Resolver::builder_with_config(config, TokioConnectionProvider::default())
        };

        let mut builder = builder;
        let opts: &mut ResolverOpts = builder.options_mut();
        opts.cache_size = cfg.cache_size;
        opts.use_hosts_file = if cfg.use_hosts {
            ResolveHosts::Always
        } else {
            ResolveHosts::Never
        };
        opts.timeout = cfg.timeout;
        opts.attempts = cfg.attempts;
        // 先按 A/AAAA 双查再合并，由下面的排序决定尝试顺序。
        opts.ip_strategy = LookupIpStrategy::Ipv4AndIpv6;
        // 与 aria2 默认一致：不做自动搜索域追加（避免 a.test 被拼成 a.test.localdomain）。
        opts.ndots = 0;

        Ok(Self {
            inner: builder.build(),
            ipv6: cfg.ipv6,
        })
    }

    /// 把主机名解析成 IP 列表（字面 IP 直通）。
    /// 返回去重后的地址集合；连接顺序由 `dns::connect` 按 RFC 8305 决定。
    pub async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, NetworkError> {
        let name = host.trim_end_matches('.');
        if let Ok(ip) = name.parse::<IpAddr>() {
            return Ok(vec![ip]);
        }

        let lookup = self
            .inner
            .lookup_ip(name)
            .await
            .map_err(|err| NetworkError::DnsFailure(format!("{name}: {err}")))?;

        let mut ips: Vec<IpAddr> = lookup
            .iter()
            .filter(|ip| self.ipv6 || ip.is_ipv4())
            .collect();

        let mut seen = std::collections::HashSet::new();
        ips.retain(|ip| seen.insert(*ip));

        // 顺序交给 `dns::connect` 的 RFC 8305 排列（IPv6 优先、两族交替）；
        // `--disable-ipv6=true` 时这里只剩 IPv4，与 Phase 1 行为一致。
        if ips.is_empty() {
            return Err(NetworkError::DnsFailure(format!(
                "{name}: 无可用地址（ipv6={}）",
                self.ipv6
            )));
        }
        Ok(ips)
    }

    /// 便于测试断言配置：返回构造时的 IPv6 开关。
    pub fn ipv6_enabled(&self) -> bool {
        self.ipv6
    }
}

/// `host:port` 的默认端口推导（http=80 / https=443）。
pub fn default_port(scheme: &str) -> u16 {
    if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn literal_ip_passthrough_does_not_query_dns() {
        let resolver = DnsResolver::new(&DnsConfig {
            // 指向必然不可达的地址：字面 IP 不应触发查询。
            servers: vec!["127.0.0.1:1".parse().unwrap()],
            timeout: Duration::from_millis(50),
            attempts: 1,
            ..DnsConfig::default()
        })
        .expect("构造 resolver");

        let ips = resolver.resolve("10.1.2.3").await.expect("字面 IP");
        assert_eq!(ips, vec!["10.1.2.3".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn default_port_matches_scheme() {
        assert_eq!(default_port("https"), 443);
        assert_eq!(default_port("HTTP"), 80);
    }
}
