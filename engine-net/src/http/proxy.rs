//! 代理：HTTP 代理（forward / CONNECT 隧道）与 SOCKS5（本地/远端 DNS）。
//!
//! 对齐口径：
//! - HTTP 目标的 HTTP 代理走 **absolute-form 转发**，HTTPS 目标走 **CONNECT 隧道**
//!   （`HttpRequest.cc:144-166`、`createProxyRequest()`）；
//! - `Proxy-Authorization: Basic <base64(user:password)>`（`HttpRequest.cc:357-365`）；
//! - `--no-proxy` 支持主机名/域/单 IP/CIDR，命中后直连；
//! - CONNECT 隧道场景由调用方使用只宣告 `http/1.1` 的 TLS 配置（Q6 I5）。
//!
//! SOCKS5 为本项目增强（上游 aria2 1.37 无此协议）：`socks5://` 本地解析、
//! `socks5h://` 交给代理解析。

use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use base64::Engine;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::dns::resolver::DnsResolver;
use crate::error::NetworkError;
use crate::http::tls::{TlsOptions, client_config, server_name};

/// 代理连接上承载的字节流（可能是裸 TCP，也可能是到代理的 TLS）。
pub trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}
pub type BoxStream = Box<dyn AsyncStream>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyKind {
    /// `http://`（明文 HTTP 代理）。
    Http,
    /// `https://`（以 TLS 连接代理本身）。
    Https,
    /// `socks5://`（本地解析域名）。
    Socks5,
    /// `socks5h://`（代理解析域名）。
    Socks5h,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyAuth {
    pub user: String,
    pub pass: String,
}

impl ProxyAuth {
    pub fn header(&self) -> String {
        let raw = format!("{}:{}", self.user, self.pass);
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    pub auth: Option<ProxyAuth>,
}

impl ProxyConfig {
    /// 解析 aria2 的 `--all-proxy=[http://][USER:PASSWORD@]HOST[:PORT]`；
    /// 空串表示“显式禁用”（返回 `Ok(None)`，由调用方保留覆盖语义）。
    pub fn parse(raw: &str) -> Result<Option<Self>, NetworkError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(None);
        }
        let (kind, rest) = if let Some(rest) = raw.strip_prefix("http://") {
            (ProxyKind::Http, rest)
        } else if let Some(rest) = raw.strip_prefix("https://") {
            (ProxyKind::Https, rest)
        } else if let Some(rest) = raw.strip_prefix("socks5h://") {
            (ProxyKind::Socks5h, rest)
        } else if let Some(rest) = raw.strip_prefix("socks5://") {
            (ProxyKind::Socks5, rest)
        } else {
            (ProxyKind::Http, raw)
        };
        let rest = rest.trim_end_matches('/');

        // 用户信息从最后一个 '@' 前切分（密码里允许出现 '@' 之前的字符）。
        let (auth, authority) = match rest.rsplit_once('@') {
            Some((userinfo, authority)) => {
                let (user, pass) = match userinfo.split_once(':') {
                    Some((user, pass)) => (user.to_string(), pass.to_string()),
                    None => (userinfo.to_string(), String::new()),
                };
                (Some(ProxyAuth { user, pass }), authority)
            }
            None => (None, rest),
        };

        let (host, port) = split_host_port(authority, default_proxy_port(kind))?;
        Ok(Some(Self {
            kind,
            host,
            port,
            auth,
        }))
    }

    /// 覆盖内嵌认证（`--all-proxy-user/passwd` 等显式选项优先）。
    pub fn with_credentials(mut self, user: Option<String>, pass: Option<String>) -> Self {
        if user.is_some() || pass.is_some() {
            let mut auth = self.auth.take().unwrap_or(ProxyAuth {
                user: String::new(),
                pass: String::new(),
            });
            if let Some(user) = user {
                auth.user = user;
            }
            if let Some(pass) = pass {
                auth.pass = pass;
            }
            self.auth = Some(auth);
        }
        self
    }

    /// 池键/审计用：`kind://host:port`（不含口令）。
    pub fn id(&self) -> String {
        format!(
            "{}://{}:{}",
            match self.kind {
                ProxyKind::Http => "http",
                ProxyKind::Https => "https",
                ProxyKind::Socks5 => "socks5",
                ProxyKind::Socks5h => "socks5h",
            },
            self.host,
            self.port
        )
    }
}

fn default_proxy_port(kind: ProxyKind) -> u16 {
    match kind {
        ProxyKind::Http => 80,
        ProxyKind::Https => 443,
        ProxyKind::Socks5 | ProxyKind::Socks5h => 1080,
    }
}

fn split_host_port(authority: &str, default_port: u16) -> Result<(String, u16), NetworkError> {
    // IPv6 字面量：[addr]:port
    if let Some(stripped) = authority.strip_prefix('[') {
        let (host, rest) = stripped
            .split_once(']')
            .ok_or_else(|| NetworkError::ProxyError(format!("代理地址非法 `{authority}`")))?;
        let port = match rest.strip_prefix(':') {
            Some(port) if !port.is_empty() => port
                .parse()
                .map_err(|_| NetworkError::ProxyError(format!("代理端口非法 `{port}`")))?,
            _ => default_port,
        };
        return Ok((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !port.is_empty() => Ok((
            host.to_string(),
            port.parse()
                .map_err(|_| NetworkError::ProxyError(format!("代理端口非法 `{port}`")))?,
        )),
        _ if !authority.is_empty() => Ok((authority.to_string(), default_port)),
        _ => Err(NetworkError::ProxyError("代理地址为空".into())),
    }
}

/// 代理槽位。空串等价“未设置”（C++ `makeProxyUri` 解析失败即回退 all-proxy，
/// 见 `AbstractCommand.cc:660-690`）。
#[derive(Debug, Clone, Default)]
pub enum ProxySlot {
    #[default]
    Unset,
    Set(ProxyConfig),
}

impl ProxySlot {
    pub fn from_raw(raw: &str) -> Result<Self, NetworkError> {
        match ProxyConfig::parse(raw)? {
            Some(config) => Ok(ProxySlot::Set(config)),
            None => Ok(ProxySlot::Unset),
        }
    }

    fn resolved<'a>(&'a self, fallback: &'a ProxySlot) -> Option<&'a ProxyConfig> {
        match self {
            ProxySlot::Set(config) => Some(config),
            ProxySlot::Unset => match fallback {
                ProxySlot::Set(config) => Some(config),
                ProxySlot::Unset => None,
            },
        }
    }
}

/// `--no-proxy` 单条规则。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoProxyRule {
    /// 主机名/域名/单个 IP。以 `.` 开头时按后缀匹配子域（`util.cc:2411`）。
    Host(String),
    Cidr { base: IpAddr, prefix: u8 },
}

impl NoProxyRule {
    pub fn parse(raw: &str) -> Option<Self> {
        let text = raw.trim();
        if text.is_empty() {
            return None;
        }
        if let Some((base, prefix)) = text.split_once('/') {
            let base: IpAddr = base.trim().parse().ok()?;
            let prefix: u8 = prefix.trim().parse().ok()?;
            let max = if base.is_ipv4() { 32 } else { 128 };
            if prefix > max {
                return None;
            }
            return Some(Self::Cidr { base, prefix });
        }
        Some(Self::Host(text.to_ascii_lowercase()))
    }

    /// 与 `util::noProxyDomainMatch` 同口径：`.` 前缀后缀匹配（IP 字面量除外），
    /// 其余一律精确相等。
    pub fn matches(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        match self {
            NoProxyRule::Host(rule) => {
                let numeric = host.parse::<IpAddr>().is_ok();
                if rule.starts_with('.') && !numeric {
                    host.ends_with(rule.as_str())
                } else {
                    host == *rule
                }
            }
            NoProxyRule::Cidr { base, prefix } => host
                .parse::<IpAddr>()
                .map(|ip| cidr_contains(*base, *prefix, ip))
                .unwrap_or(false),
        }
    }
}

fn cidr_contains(base: IpAddr, prefix: u8, ip: IpAddr) -> bool {
    match (base, ip) {
        (IpAddr::V4(base), IpAddr::V4(ip)) if prefix <= 32 => {
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            (u32::from(base) & mask) == (u32::from(ip) & mask)
        }
        (IpAddr::V6(base), IpAddr::V6(ip)) if prefix <= 128 => {
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - prefix)
            };
            (u128::from(base) & mask) == (u128::from(ip) & mask)
        }
        _ => false,
    }
}

/// 任务级代理设置（`--all-proxy` / `--http-proxy` / `--https-proxy` / `--no-proxy`）。
#[derive(Debug, Clone, Default)]
pub struct ProxySettings {
    pub all: ProxySlot,
    pub http: ProxySlot,
    pub https: ProxySlot,
    pub no_proxy: Vec<NoProxyRule>,
}

impl ProxySettings {
    pub fn for_scheme(&self, scheme: &str) -> Option<&ProxyConfig> {
        if scheme.eq_ignore_ascii_case("https") {
            self.https.resolved(&self.all)
        } else {
            self.http.resolved(&self.all)
        }
    }

    pub fn bypass(&self, host: &str) -> bool {
        self.no_proxy.iter().any(|rule| rule.matches(host))
    }

    pub fn is_empty(&self) -> bool {
        self.for_scheme("http").is_none() && self.for_scheme("https").is_none()
    }
}

/// 代理建连结果。
pub struct EstablishedProxy {
    pub stream: BoxStream,
    /// true = HTTP absolute-form 转发（不隧道）；false = 已建立隧道。
    pub forward: bool,
    /// forward 模式需要调用方在请求头里补 `Proxy-Authorization`。
    pub authorization: Option<String>,
    /// 代理本身的 IP（审计/测试；目标真实 IP 在隧道里不可见）。
    pub proxy_ip: IpAddr,
}

impl std::fmt::Debug for EstablishedProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EstablishedProxy")
            .field("forward", &self.forward)
            .field("authorization", &self.authorization.is_some())
            .finish()
    }
}

/// 通过代理连到 `target_host:target_port`。
///
/// `forward` 仅对 HTTP 系代理的 http:// 目标有效（aria2 的 absolute-form 语义）。
pub async fn establish(
    proxy: &ProxyConfig,
    target_host: &str,
    target_port: u16,
    forward: bool,
    user_agent: &str,
    resolver: &DnsResolver,
    timeout: Duration,
    proxy_tls: &TlsOptions,
) -> Result<EstablishedProxy, NetworkError> {
    // beta9：代理自身的 TCP 建连失败统一归为 `ProxyConnectionFailed` ——
    // 代理软件被关闭时这里通常拿到 ConnectionRefused / Timeout，若不包裹就会
    // 冒泡成普通网络错误，任务直接判死；包裹后 registry 可降级为「暂停」。
    let connected = crate::dns::connect::connect(
        &proxy.host,
        proxy.port,
        resolver,
        timeout,
    )
    .await
    .map_err(|err| {
        NetworkError::ProxyConnectionFailed(format!(
            "无法连接代理 {}:{}（{err}）",
            proxy.host, proxy.port
        ))
    })?;
    let proxy_ip = connected.ip;

    let mut stream: BoxStream = match proxy.kind {
        ProxyKind::Https => {
            let config = client_config(proxy_tls)?;
            let connector = TlsConnector::from(config);
            let name = server_name(&proxy.host)?;
            let tls = tokio::time::timeout(timeout, connector.connect(name, connected.stream))
                .await
                .map_err(|_| NetworkError::Timeout)?
                .map_err(|err| NetworkError::ProxyError(format!("连接 HTTPS 代理失败：{err}")))?;
            Box::new(tls)
        }
        _ => Box::new(connected.stream),
    };

    match proxy.kind {
        ProxyKind::Http | ProxyKind::Https => {
            if forward {
                return Ok(EstablishedProxy {
                    stream,
                    forward: true,
                    authorization: proxy.auth.as_ref().map(ProxyAuth::header),
                    proxy_ip,
                });
            }
            let authorization = connect_tunnel(
                &mut stream,
                target_host,
                target_port,
                proxy.auth.as_ref(),
                user_agent,
                timeout,
            )
            .await?;
            Ok(EstablishedProxy {
                stream,
                forward: false,
                authorization,
                proxy_ip,
            })
        }
        ProxyKind::Socks5 | ProxyKind::Socks5h => {
            socks5_tunnel(
                &mut stream,
                proxy,
                target_host,
                target_port,
                resolver,
                timeout,
            )
            .await?;
            Ok(EstablishedProxy {
                stream,
                forward: false,
                authorization: None,
                proxy_ip,
            })
        }
    }
}

async fn connect_tunnel(
    stream: &mut BoxStream,
    target_host: &str,
    target_port: u16,
    auth: Option<&ProxyAuth>,
    user_agent: &str,
    timeout: Duration,
) -> Result<Option<String>, NetworkError> {
    let authority = format!("{target_host}:{target_port}");
    let mut request = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: {user_agent}\r\n"
    );
    let authorization = auth.map(ProxyAuth::header);
    if let Some(value) = &authorization {
        request.push_str(&format!("Proxy-Authorization: {value}\r\n"));
    }
    request.push_str("\r\n");

    tokio::time::timeout(timeout, stream.write_all(request.as_bytes()))
        .await
        .map_err(|_| NetworkError::Timeout)?
        .map_err(NetworkError::from)?;

    let (head, leftover) = tokio::time::timeout(timeout, read_http_head(stream))
        .await
        .map_err(|_| NetworkError::Timeout)??;
    let status = parse_status_code(&head)?;
    // C++ `AbstractProxyResponseCommand.cc:74-76`：CONNECT 非 200（含 407）一律
    // 抛 `EX_PROXY_CONNECTION_FAILED`，最终 errorCode=1，没有单独的认证码分支。
    if status != 200 {
        return Err(NetworkError::ProxyConnectionFailed(format!(
            "CONNECT {authority} 被代理拒绝：HTTP {status}"
        )));
    }
    if !leftover.is_empty() {
        // CONNECT 成功后代理不会先发数据；保守起见把多读的字节塞回流。
        // 这里通过 Box 重新包一层前缀流，避免丢字节。
        let inner = std::mem::replace(stream, Box::new(tokio::io::empty()));
        *stream = Box::new(PrefixedStream {
            prefix: leftover,
            inner,
        });
    }
    Ok(authorization)
}

async fn socks5_tunnel(
    stream: &mut BoxStream,
    proxy: &ProxyConfig,
    target_host: &str,
    target_port: u16,
    resolver: &DnsResolver,
    timeout: Duration,
) -> Result<(), NetworkError> {
    let remote_dns = proxy.kind == ProxyKind::Socks5h;

    // 1) 方法协商
    let greeting: &[u8] = if proxy.auth.is_some() {
        &[0x05, 0x02, 0x00, 0x02]
    } else {
        &[0x05, 0x01, 0x00]
    };
    let method = io_step(timeout, async {
        stream.write_all(greeting).await?;
        let mut reply = [0u8; 2];
        stream.read_exact(&mut reply).await?;
        if reply[0] != 0x05 {
            return Err(NetworkError::ProxyError(format!(
                "SOCKS5 版本非法：{}",
                reply[0]
            )));
        }
        Ok(reply[1])
    })
    .await?;

    match method {
        0x00 => {}
        0x02 => {
            let auth = proxy.auth.as_ref().ok_or(NetworkError::ProxyAuthRequired)?;
            let user = auth.user.as_bytes();
            let pass = auth.pass.as_bytes();
            if user.len() > 255 || pass.len() > 255 {
                return Err(NetworkError::ProxyError(
                    "SOCKS5 用户名/密码超过 255 字节".into(),
                ));
            }
            io_step(timeout, async {
                let mut request = vec![0x01, user.len() as u8];
                request.extend_from_slice(user);
                request.push(pass.len() as u8);
                request.extend_from_slice(pass);
                stream.write_all(&request).await?;
                let mut reply = [0u8; 2];
                stream.read_exact(&mut reply).await?;
                if reply[1] != 0x00 {
                    return Err(NetworkError::ProxyAuthRequired);
                }
                Ok(())
            })
            .await?;
        }
        0xFF => return Err(NetworkError::ProxyAuthRequired),
        method => {
            return Err(NetworkError::ProxyError(format!(
                "SOCKS5 代理选择了不支持的方法：0x{method:02x}"
            )));
        }
    }

    // 2) CONNECT 请求
    let address = if remote_dns {
        encode_socks_addr(target_host)?
    } else {
        let ips = resolver.resolve(target_host).await?;
        let ip = ips
            .first()
            .copied()
            .ok_or_else(|| NetworkError::DnsFailure(format!("{target_host}: 无可用地址")))?;
        encode_socks_ip(ip)
    };
    io_step(timeout, async {
        let mut request = vec![0x05, 0x01, 0x00];
        request.extend_from_slice(&address);
        request.extend_from_slice(&target_port.to_be_bytes());
        stream.write_all(&request).await?;

        let mut reply = [0u8; 4];
        stream.read_exact(&mut reply).await?;
        if reply[0] != 0x05 {
            return Err(NetworkError::ProxyError(format!(
                "SOCKS5 应答版本非法：{}",
                reply[0]
            )));
        }
        match reply[1] {
            0x00 => {}
            0x02 => return Err(NetworkError::ProxyAuthRequired),
            0x05 => return Err(NetworkError::ProxyError("SOCKS5 连接被拒绝".into())),
            code => {
                return Err(NetworkError::ProxyError(format!(
                    "SOCKS5 CONNECT 失败：rep=0x{code:02x}"
                )));
            }
        }
        // 读掉 BND.ADDR / BND.PORT
        match reply[3] {
            0x01 => {
                let mut rest = [0u8; 4 + 2];
                stream.read_exact(&mut rest).await?;
            }
            0x04 => {
                let mut rest = [0u8; 16 + 2];
                stream.read_exact(&mut rest).await?;
            }
            0x03 => {
                let mut len = [0u8; 1];
                stream.read_exact(&mut len).await?;
                let mut rest = vec![0u8; len[0] as usize + 2];
                stream.read_exact(&mut rest).await?;
            }
            atyp => {
                return Err(NetworkError::ProxyError(format!(
                    "SOCKS5 应答地址类型非法：{atyp}"
                )));
            }
        }
        Ok(())
    })
    .await
}

/// 方法协商后的认证子协商已在 `socks5_tunnel` 内联完成。
fn encode_socks_addr(host: &str) -> Result<Vec<u8>, NetworkError> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(encode_socks_ip(ip));
    }
    let bytes = host.as_bytes();
    if bytes.is_empty() || bytes.len() > 255 {
        return Err(NetworkError::ProxyError(format!(
            "SOCKS5 域名长度非法：{} 字节",
            bytes.len()
        )));
    }
    let mut out = Vec::with_capacity(bytes.len() + 2);
    out.push(0x03);
    out.push(bytes.len() as u8);
    out.extend_from_slice(bytes);
    Ok(out)
}

fn encode_socks_ip(ip: IpAddr) -> Vec<u8> {
    match ip {
        IpAddr::V4(v4) => {
            let mut out = vec![0x01];
            out.extend_from_slice(&v4.octets());
            out
        }
        IpAddr::V6(v6) => {
            let mut out = vec![0x04];
            out.extend_from_slice(&v6.octets());
            out
        }
    }
}

/// 统一加超时的 IO 步骤（闭包内做完整握手，超时覆盖所有读写）。
async fn io_step<T, Fut>(timeout: Duration, future: Fut) -> Result<T, NetworkError>
where
    Fut: std::future::Future<Output = Result<T, NetworkError>>,
{
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| NetworkError::Timeout)?
}

/// 读到 `\r\n\r\n` 为止，返回（头部字节, 多读的剩余字节）。
async fn read_http_head(
    stream: &mut BoxStream,
) -> Result<(Vec<u8>, Vec<u8>), NetworkError> {
    const MAX_HEAD: usize = 64 * 1024;
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.map_err(NetworkError::from)?;
        if read == 0 {
            return Err(NetworkError::ProxyError(
                "代理在响应头结束前关闭连接".into(),
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(pos) = find_head_end(&buffer) {
            let leftover = buffer[pos..].to_vec();
            buffer.truncate(pos);
            return Ok((buffer, leftover));
        }
        if buffer.len() > MAX_HEAD {
            return Err(NetworkError::ProxyError("代理响应头超过 64 KiB".into()));
        }
    }
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n").map(|pos| pos + 4)
}

fn parse_status_code(head: &[u8]) -> Result<u16, NetworkError> {
    let text = String::from_utf8_lossy(head);
    let line = text.lines().next().unwrap_or_default();
    let mut parts = line.split_whitespace();
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/") {
        return Err(NetworkError::ProxyError(format!(
            "代理响应状态行非法：`{line}`"
        )));
    }
    parts
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| NetworkError::ProxyError(format!("代理响应状态码非法：`{line}`")))
}

/// 把握手时多读的字节放回流的头部。
struct PrefixedStream {
    prefix: Vec<u8>,
    inner: BoxStream,
}

impl AsyncRead for PrefixedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if !self.prefix.is_empty() {
            let take = self.prefix.len().min(buf.remaining());
            let bytes: Vec<u8> = self.prefix.drain(..take).collect();
            buf.put_slice(&bytes);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for PrefixedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// 便捷：把 `TcpStream` 装箱（测试与调用方复用）。
pub fn boxed(stream: TcpStream) -> BoxStream {
    Box::new(stream)
}

/// 给调用方用的目标地址格式（forward 模式的绝对 URI 由 client 构造）。
pub fn authority(host: &str, port: u16) -> String {
    format!("{host}:{port}")
}

/// 让 `SocketAddr` 之类的外部类型也参与 `ProxySettings::bypass` 的便捷入口。
pub fn bypass_addr(settings: &ProxySettings, addr: &SocketAddr) -> bool {
    settings.bypass(&addr.ip().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_http_and_socks_proxy_urls() {
        let proxy = ProxyConfig::parse("http://127.0.0.1:8080")
            .unwrap()
            .expect("应解析出代理");
        assert_eq!(proxy.kind, ProxyKind::Http);
        assert_eq!(proxy.host, "127.0.0.1");
        assert_eq!(proxy.port, 8080);
        assert!(proxy.auth.is_none());

        let proxy = ProxyConfig::parse("socks5h://user:pass@proxy.test")
            .unwrap()
            .expect("应解析出代理");
        assert_eq!(proxy.kind, ProxyKind::Socks5h);
        assert_eq!(proxy.port, 1080);
        assert_eq!(
            proxy.auth,
            Some(ProxyAuth {
                user: "user".into(),
                pass: "pass".into()
            })
        );

        let proxy = ProxyConfig::parse("proxy.test:3128")
            .unwrap()
            .expect("应解析出代理");
        assert_eq!(proxy.kind, ProxyKind::Http);
        assert_eq!(proxy.port, 3128);

        assert!(ProxyConfig::parse("").unwrap().is_none());
        assert!(ProxyConfig::parse("http://[::1]:8080").unwrap().is_some());
    }

    #[test]
    fn explicit_credentials_override_url_auth() {
        let proxy = ProxyConfig::parse("http://a:b@127.0.0.1:8080")
            .unwrap()
            .unwrap()
            .with_credentials(Some("u".into()), Some("p".into()));
        assert_eq!(proxy.auth.unwrap().user, "u");
    }

    #[test]
    fn slots_implement_specific_over_all_semantics() {
        let all = ProxySlot::from_raw("http://all.test:1").unwrap();
        let http = ProxySlot::from_raw("http://http.test:2").unwrap();
        let https_empty = ProxySlot::from_raw("").unwrap();
        let settings = ProxySettings {
            all: all.clone(),
            http,
            https: https_empty,
            no_proxy: vec![],
        };
        assert_eq!(
            settings.for_scheme("http").map(ProxyConfig::id),
            Some("http://http.test:2".to_string())
        );
        // 空串 = 未设置 → 回退 all-proxy（与 C++ makeProxyUri 一致）。
        assert_eq!(
            settings.for_scheme("https").map(ProxyConfig::id),
            Some("http://all.test:1".to_string())
        );

        let settings = ProxySettings {
            all,
            http: ProxySlot::Unset,
            https: ProxySlot::Unset,
            no_proxy: vec![],
        };
        assert_eq!(
            settings.for_scheme("https").map(ProxyConfig::id),
            Some("http://all.test:1".to_string())
        );
    }

    #[test]
    fn no_proxy_matches_domains_and_cidr() {
        let rules: Vec<NoProxyRule> = "localhost, .corp.test, 10.0.0.0/8, 192.168.1.5"
            .split(',')
            .filter_map(NoProxyRule::parse)
            .collect();
        let settings = ProxySettings {
            no_proxy: rules,
            ..ProxySettings::default()
        };
        assert!(settings.bypass("localhost"));
        assert!(settings.bypass("a.corp.test"));
        // `.corp.test` 只匹配子域，不匹配 apex（`util.h:866`）。
        assert!(!settings.bypass("corp.test"));
        assert!(settings.bypass("10.1.2.3"));
        assert!(settings.bypass("192.168.1.5"));
        assert!(!settings.bypass("192.168.1.6"));
        assert!(!settings.bypass("example.com"));
    }

    #[test]
    fn auth_header_is_basic_base64() {
        let auth = ProxyAuth {
            user: "u".into(),
            pass: "p".into(),
        };
        assert_eq!(auth.header(), "Basic dTpw");
    }

    #[test]
    fn status_line_parsing() {
        let head = b"HTTP/1.1 200 Connection established\r\n\r\n";
        assert_eq!(parse_status_code(head).unwrap(), 200);
        assert!(parse_status_code(b"garbage\r\n\r\n").is_err());
    }
}
