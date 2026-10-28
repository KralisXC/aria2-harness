//! `ctx.httpGet` 的宿主实现（beta10 · 模块 A2）。
//!
//! 运行位置：**插件专属线程内同步阻塞执行**（不再经 tokio 桥），因此：
//! - 绝不占用 tokio 工作线程；
//! - 调用期间由宿主侧超时兜底（QuickJS deadline 会被临时挂起，见 `quickjs.rs`）。
//!
//! 硬约束（全部不可由插件放宽）：
//! - 仅 `http` / `https`；
//! - 连接/读超时 ≤ 15s，重定向 ≤ 5 跳；
//! - 响应体 ≤ min(manifest.maxHttpBytes, 8MB)；
//! - SSRF：拒绝回环 / 私网 / 链路本地 / 未指定地址（DNS 解析后逐个校验）。

use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::time::Duration;

use serde_json::{json, Map, Value};

/// 允许的最大响应体（宿主硬上限，manifest 只能声明得更小）。
pub const HARD_MAX_HTTP_BYTES: usize = 8 * 1024 * 1024;
/// 单次请求的总时限（连接 + 读体）。
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// 最多跟随的重定向次数。
const MAX_REDIRECTS: u32 = 5;

/// 执行一次 `ctx.httpGet`，返回 JSON 字符串（JS 侧直接 `JSON.parse`）。
pub fn http_get(payload: &str, max_bytes: usize) -> String {
    let opts: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    let url = opts
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if url.is_empty() {
        return err("URL 不能为空");
    }

    let host = match check_ssrf(&url) {
        Ok(host) => host,
        Err(msg) => return err(&msg),
    };

    let limit = max_bytes.clamp(1, HARD_MAX_HTTP_BYTES);
    let timeout_ms = opts
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .unwrap_or(10_000)
        .clamp(100, HTTP_TIMEOUT.as_millis() as u64);
    let response_type = opts
        .get("responseType")
        .and_then(Value::as_str)
        .unwrap_or("text");

    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_millis(timeout_ms.min(5_000))))
        .timeout_global(Some(Duration::from_millis(timeout_ms)))
        .max_redirects(MAX_REDIRECTS)
        .user_agent(concat!("Aria2-Harness/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();

    let mut request = agent.get(&url);
    if let Some(headers) = opts.get("headers").and_then(Value::as_object) {
        for (key, value) in headers {
            if let Some(text) = value.as_str() {
                request = request.header(key.as_str(), text);
            }
        }
    }

    let started = std::time::Instant::now();
    let mut response = match request.call() {
        Ok(response) => response,
        Err(failure) => {
            // ureq 的 Err 可能携带 HTTP 状态（4xx/5xx 也会走 Err 分支）。
            return err(&format!("HTTP 请求失败（{host}）：{failure}"));
        }
    };

    let status = response.status().as_u16();
    let mut header_map = Map::new();
    for (name, value) in response.headers().iter() {
        if let Ok(text) = value.to_str() {
            header_map.insert(name.as_str().to_ascii_lowercase(), json!(text));
        }
    }

    // 先按上限 +1 读取，用来判定「是否超限」，避免把超大响应完整读进内存。
    let mut bytes = Vec::new();
    let read = response
        .body_mut()
        .as_reader()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes);
    if let Err(e) = read {
        return err(&format!("读取响应失败：{e}"));
    }
    if bytes.len() > limit {
        return err(&format!("响应体超过上限（{limit} 字节）"));
    }

    let body = if response_type.eq_ignore_ascii_case("base64") {
        base64_encode(&bytes)
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };

    tracing::debug!(
        "插件 httpGet 完成：host={host} status={status} bytes={} 耗时={}ms",
        bytes.len(),
        started.elapsed().as_millis()
    );

    json!({
        "ok": true,
        "status": status,
        "headers": Value::Object(header_map),
        "body": body,
        "bytes": bytes.len(),
    })
    .to_string()
}

fn err(msg: &str) -> String {
    json!({ "ok": false, "error": msg }).to_string()
}

/// 校验 URL 协议并返回 host（小写、已去掉端口 / userinfo）。
pub fn check_ssrf(url: &str) -> Result<String, String> {
    let lower = url.to_ascii_lowercase();
    let (scheme, rest) = if let Some(rest) = lower.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = lower.strip_prefix("http://") {
        ("http", rest)
    } else {
        return Err("仅支持 http / https 协议".to_string());
    };

    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{scheme} URL 缺少主机名"))?;
    // 去掉 userinfo（`user:pass@host`）：只保留最后一个 `@` 之后的部分。
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = split_host(authority);
    if host.is_empty() {
        return Err("URL 缺少主机名".to_string());
    }

    // 端口：http 默认 80，https 默认 443；显式端口按原样使用。
    let port = authority
        .rsplit_once(':')
        .map(|(_, p)| p)
        .filter(|p| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty())
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(if scheme == "https" { 443 } else { 80 });

    let addrs = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("域名解析失败（{host}）：{e}"))?;
    let mut any = false;
    for addr in addrs {
        any = true;
        if is_forbidden_ip(&addr.ip()) {
            return Err(format!("拒绝访问内网 / 回环地址：{host}"));
        }
    }
    if !any {
        return Err(format!("域名无可用地址：{host}"));
    }
    Ok(host)
}

/// 从 authority 中取 host（兼容 IPv6 `[::1]:8080` 与 `host:port`）。
fn split_host(authority: &str) -> String {
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or_default().to_string();
    }
    authority.split(':').next().unwrap_or_default().to_string()
}

/// 是否禁止访问的地址（回环 / 私网 / 链路本地 / 未指定 / 组播 / 广播）。
pub fn is_forbidden_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_v4(v4),
        IpAddr::V6(v6) => is_forbidden_v6(v6),
    }
}

fn is_forbidden_v4(ip: &Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        // 100.64.0.0/10（CGNAT）：运营商内网，同样不允许。
        || (ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]))
}

fn is_forbidden_v6(ip: &Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // 唯一本地地址 fc00::/7 与链路本地 fe80::/10。
        || (ip.segments()[0] & 0xfe00) == 0xfc00
        || (ip.segments()[0] & 0xffc0) == 0xfe80
        // IPv4 映射地址：按其内嵌 v4 判定。
        || ip
            .to_ipv4_mapped()
            .is_some_and(|v4| is_forbidden_v4(&v4))
}

/// 最小 base64 编码（避免为一次编码引入额外依赖）。
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 0x3f] as char);
        out.push(TABLE[(n >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::{base64_encode, check_ssrf, is_forbidden_ip};

    #[test]
    fn rejects_non_http_schemes() {
        assert!(check_ssrf("file:///C:/Windows/win.ini").is_err());
        assert!(check_ssrf("ftp://example.com/a").is_err());
        assert!(check_ssrf("javascript:alert(1)").is_err());
    }

    #[test]
    fn rejects_private_and_loopback_literals() {
        assert!(check_ssrf("http://127.0.0.1:8080/a").is_err());
        assert!(check_ssrf("http://10.0.0.5/a").is_err());
        assert!(check_ssrf("http://169.254.1.1/a").is_err());
        assert!(check_ssrf("http://[::1]:8080/a").is_err());
        assert!(check_ssrf("http://192.168.1.1/a").is_err());
    }

    #[test]
    fn forbidden_ip_matrix() {
        for ip in ["127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.0.1", "0.0.0.0"] {
            assert!(
                is_forbidden_ip(&IpAddr::V4(ip.parse::<Ipv4Addr>().unwrap())),
                "{ip} 应被拒绝"
            );
        }
        for ip in ["::1", "fe80::1", "fc00::1", "::ffff:127.0.0.1"] {
            assert!(
                is_forbidden_ip(&IpAddr::V6(ip.parse::<Ipv6Addr>().unwrap())),
                "{ip} 应被拒绝"
            );
        }
        assert!(!is_forbidden_ip(&IpAddr::V4(
            Ipv4Addr::new(1, 1, 1, 1)
        )));
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
