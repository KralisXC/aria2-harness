//! 可复用的 HTTP 会话（Phase 4）：一次 HTTP 交换的底层 API。
//!
//! `HttpSession` 持有一份 `HttpOptions`、两套 TLS 配置（常规 / http1-only）、
//! 一个任务级 `H2Pool` 和 DNS 解析器；调度器的所有 worker 共享同一个
//! `Arc<HttpSession>`，因此 HTTP/2 连接、代理与 Happy Eyeballs 逻辑完全复用
//! Phase 3，不重复建池。
//!
//! `get_range` 只负责「发一个 GET（可带 Range）+ 跟随重定向」，不做状态码
//! 语义判断；调用方（单连接路径或调度器）各自解释 200/206/416。

use std::path::Path;
use std::sync::Arc;

use http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, LOCATION, SET_COOKIE};
use http::{HeaderMap, Request, Response, StatusCode, Uri};
use http_body_util::Empty;
use hyper::body::{Bytes, Incoming};

use crate::dns::connect::connect;
use crate::dns::resolver::{DnsResolver, default_port};
use crate::error::NetworkError;
use crate::http::client::{build_request, host_header, parse_content_range, validate_uri};
use crate::http::h2::{H2Lease, H2Pool, MuxKey, Pooled};
use crate::http::proxy;
use crate::http::redirect::{is_redirect, resolve_location};
use crate::http::tls::{client_config, server_name};

/// 闭区间 Range（`end` 含端点）；`end=None` 表示开放式 `bytes=start-`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: Option<u64>,
}

impl ByteRange {
    pub fn new(start: u64, end: Option<u64>) -> Self {
        Self { start, end }
    }

    /// `bytes=5-9` / `bytes=5-`。
    pub fn header_value(self) -> String {
        match self.end {
            Some(end) => format!("bytes={}-{}", self.start, end),
            None => format!("bytes={}-", self.start),
        }
    }
}

/// 一次 Range 请求的结果（body 尚未消费）。
pub struct RangeResponse {
    pub final_url: String,
    pub redirects: Vec<String>,
    pub status: StatusCode,
    /// 由 `Content-Length` 推导（仅 200 有意义）。
    pub total_length: Option<u64>,
    /// `Content-Range` 解析结果 `(start, end, total)`（仅 206）。
    pub content_range: Option<(u64, u64, Option<u64>)>,
    /// 416 的 `Content-Range: bytes * /N` 总长。
    pub unsatisfied_total: Option<u64>,
    /// `Content-Disposition` 原文（beta8.3：任务启动时据此贪婪取名）。
    pub content_disposition: Option<String>,
    /// `Content-Type` 原文（beta9：用于识别「抓到的是网页而不是文件」）。
    pub content_type: Option<String>,
    pub body: Incoming,
    /// 活到 body 读完为止（drop = 归还 H2 流配额）。
    pub lease: Option<H2Lease>,
    pub via_proxy: bool,
    pub http2: bool,
}

/// 跳转链 Cookie 罐（beta10）。
///
/// 两条规则与浏览器略有不同，按 aria2 / JetBrains 鉴权链路的实际需要确定：
/// 1. **初始 Cookie 无条件携带到每一跳**（跨 host 也不丢，CDN 常要回源站的
///    鉴权 Cookie）；
/// 2. 跳转响应里的 `Set-Cookie` 只按域吸收：无 `Domain` 时要求与下一跳同 host，
///    有 `Domain` 时要求下一跳是它本身或子域 —— 绝不把 A 站新 Cookie 发给 B 站。
#[derive(Debug, Default, Clone)]
pub(crate) struct CookieJar {
    pairs: Vec<(String, String)>,
}

impl CookieJar {
    fn from_headers(headers: &[(String, String)]) -> Self {
        let mut jar = Self::default();
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("cookie") {
                jar.extend(value);
            }
        }
        jar
    }

    /// 合并 `a=1; b=2` 形式的串（同名后写覆盖）。
    fn extend(&mut self, raw: &str) {
        for part in raw.split(';') {
            let Some((name, value)) = part.trim().split_once('=') else {
                continue;
            };
            let name = name.trim();
            if !name.is_empty() {
                self.set(name, value.trim());
            }
        }
    }

    fn set(&mut self, name: &str, value: &str) {
        if let Some(slot) = self
            .pairs
            .iter_mut()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
        {
            slot.1 = value.to_string();
            return;
        }
        self.pairs.push((name.to_string(), value.to_string()));
    }

    /// 拼成单个 `Cookie` 头；空罐返回 `None`（不发空头）。
    fn header_value(&self) -> Option<String> {
        if self.pairs.is_empty() {
            return None;
        }
        Some(
            self.pairs
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    /// 吸收 `headers` 里的 `Set-Cookie`，只保留域匹配 `next_host` 的项。
    fn absorb(&mut self, headers: &HeaderMap, from_host: &str, next_host: &str) {
        for raw in headers.get_all(SET_COOKIE) {
            let Ok(raw) = raw.to_str() else {
                continue;
            };
            let Some((name, value, domain)) = parse_set_cookie(raw) else {
                continue;
            };
            if !cookie_domain_matches(domain.as_deref(), from_host, next_host) {
                continue;
            }
            self.set(&name, &value);
        }
    }
}

/// 解析 `Set-Cookie: name=value; Domain=...; Path=/`，只关心 Domain 属性。
fn parse_set_cookie(raw: &str) -> Option<(String, String, Option<String>)> {
    let mut parts = raw.split(';');
    let (name, value) = parts.next()?.trim().split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let mut domain = None;
    for attr in parts {
        let Some((key, value)) = attr.trim().split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("domain") {
            let value = value.trim().trim_start_matches('.').to_ascii_lowercase();
            if !value.is_empty() {
                domain = Some(value);
            }
        }
    }
    Some((name.to_string(), value.trim().to_string(), domain))
}

/// RFC 6265 的尾部域匹配：无 Domain = host-only（必须同 host）。
fn cookie_domain_matches(domain: Option<&str>, from_host: &str, next_host: &str) -> bool {
    let from_host = from_host.to_ascii_lowercase();
    let next_host = next_host.to_ascii_lowercase();
    match domain {
        None => from_host == next_host,
        Some(domain) => next_host == domain || next_host.ends_with(&format!(".{domain}")),
    }
}

/// 任务级 HTTP 会话。
pub struct HttpSession {
    options: crate::http::client::HttpOptions,
    resolver: DnsResolver,
    tls: Arc<rustls::ClientConfig>,
    tls_http1: Arc<rustls::ClientConfig>,
    pool: H2Pool,
}

impl HttpSession {
    pub fn new(
        options: crate::http::client::HttpOptions,
        resolver: DnsResolver,
    ) -> Result<Arc<Self>, NetworkError> {
        // 两条 TLS 配置：常规（可能宣告 h2）与隧道/HTTP1（只宣告 http/1.1）。
        let tls = client_config(&options.tls_options(false))?;
        let tls_http1 = client_config(&options.tls_options(true))?;
        Ok(Arc::new(Self {
            options,
            resolver,
            tls,
            tls_http1,
            pool: H2Pool::new(),
        }))
    }

    pub fn options(&self) -> &crate::http::client::HttpOptions {
        &self.options
    }

    /// 发一个 GET（可带 Range），自动跟随重定向。
    pub async fn get_range(
        &self,
        url: &str,
        range: Option<ByteRange>,
    ) -> Result<RangeResponse, NetworkError> {
        let mut current: Uri = url
            .parse()
            .map_err(|err| NetworkError::HttpProtocol(format!("URL 非法 `{url}`：{err}")))?;
        validate_uri(&current)?;
        let mut redirects: Vec<String> = Vec::new();
        // beta10：初始 Cookie（可能来自扩展 / 前端）在整条链上无条件携带。
        let mut jar = CookieJar::from_headers(&self.options.headers);

        loop {
            let cookie = jar.header_value();
            // `--timeout` 同时覆盖「等响应头」：服务端永久不响应时必须在有限时间内
            // 失败，否则探测/worker 会挂死（Phase 4 降级兜底）。body 的读超时由
            // 调用方按帧计时。
            let outcome = match self.options.read_timeout {
                Some(limit) => {
                    match tokio::time::timeout(limit, self.send_get(&current, range, cookie.as_deref()))
                        .await
                    {
                        Ok(result) => result?,
                        Err(_) => return Err(NetworkError::Timeout),
                    }
                }
                None => self.send_get(&current, range, cookie.as_deref()).await?,
            };
            let status = outcome.response.status();
            if is_redirect(status.as_u16()) {
                if redirects.len() >= self.options.max_redirect {
                    return Err(NetworkError::TooManyRedirects);
                }
                let location = outcome
                    .response
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| {
                        NetworkError::HttpProtocol(format!("{status} 缺少 Location 头"))
                    })?;
                let next = resolve_location(&current, location)?;
                validate_uri(&next)?;
                // beta10：只吸收 Domain 匹配下一跳的 Set-Cookie；原 Cookie 已在罐里，
                // 跨 host 也照常携带（JetBrains/CDN 鉴权链路要求）。
                jar.absorb(
                    outcome.response.headers(),
                    current.host().unwrap_or_default(),
                    next.host().unwrap_or_default(),
                );
                redirects.push(next.to_string());
                current = next;
                continue;
            }

            let total_length = outcome
                .response
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let content_range = parse_content_range_header(&outcome.response);
            let unsatisfied = parse_unsatisfied_content_range(&outcome.response);
            let content_disposition = outcome
                .response
                .headers()
                .get(CONTENT_DISPOSITION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let content_type = outcome
                .response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);

            return Ok(RangeResponse {
                final_url: current.to_string(),
                redirects,
                status,
                total_length,
                content_range,
                unsatisfied_total: unsatisfied,
                content_disposition,
                content_type,
                body: outcome.response.into_body(),
                lease: outcome.lease,
                via_proxy: outcome.via_proxy,
                http2: outcome.http2,
            });
        }
    }

    async fn send_get(
        &self,
        uri: &Uri,
        range: Option<ByteRange>,
        extra_cookie: Option<&str>,
    ) -> Result<SendOutcome, NetworkError> {
        let options = &self.options;
        let scheme = uri.scheme_str().unwrap_or("http");
        let host = uri
            .host()
            .ok_or_else(|| NetworkError::HttpProtocol("URI 缺 host".into()))?;
        let port = uri.port_u16().unwrap_or_else(|| default_port(scheme));

        // ---------------- 代理路径 ----------------
        if let Some(proxy_config) = options
            .proxy
            .for_scheme(scheme)
            .filter(|_| !options.proxy.bypass(host))
        {
            let forward = scheme == "http" && !options.proxy_method_tunnel;
            let established = proxy::establish(
                proxy_config,
                host,
                port,
                forward,
                &options.user_agent,
                &self.resolver,
                options.connect_timeout,
                &options.tls_options(true),
            )
            .await?;

            let request = build_request(
                uri,
                options,
                range,
                forward,
                false,
                established.authorization.as_deref(),
                extra_cookie,
            )?;

            let (response, ip) = if scheme == "https" {
                let connector = tokio_rustls::TlsConnector::from(Arc::clone(&self.tls_http1));
                let name = server_name(host)?;
                let stream = tokio::time::timeout(
                    options.connect_timeout,
                    connector.connect(name, established.stream),
                )
                .await
                .map_err(|_| NetworkError::Timeout)?
                .map_err(|err| NetworkError::CertError(err.to_string()))?;
                let response = send_on_http1(stream, request).await?;
                (response, established.proxy_ip)
            } else {
                let response = send_on_http1(established.stream, request).await?;
                (response, established.proxy_ip)
            };

            return Ok(SendOutcome {
                response,
                ip,
                lease: None,
                via_proxy: true,
                http2: false,
            });
        }

        // ---------------- 直连路径 ----------------
        let connected = connect(host, port, &self.resolver, options.connect_timeout).await?;
        let ip = connected.ip;

        if scheme == "https" {
            if options.enable_http2 {
                let key = MuxKey::new(
                    "https",
                    &host_header(uri),
                    &options.tls_options(false).fingerprint(),
                    "",
                    options.stream_limit(),
                );
                let tls_config = Arc::clone(&self.tls);
                let timeout = options.connect_timeout;
                let host_owned = host.to_string();
                let pooled = self
                    .pool
                    .acquire(key, || async move {
                        let connector = tokio_rustls::TlsConnector::from(tls_config);
                        let name = server_name(&host_owned)?;
                        tokio::time::timeout(timeout, connector.connect(name, connected.stream))
                            .await
                            .map_err(|_| NetworkError::Timeout)?
                            .map_err(|err| NetworkError::CertError(err.to_string()))
                    })
                    .await?;

                return match pooled {
                    Pooled::H2(mux) => {
                        let request =
                            build_request(uri, options, range, false, true, None, extra_cookie)?;
                        let (response, lease) = mux.send(request).await?;
                        Ok(SendOutcome {
                            response,
                            ip,
                            lease: Some(lease),
                            via_proxy: false,
                            http2: true,
                        })
                    }
                    Pooled::Http1(stream) => {
                        let request =
                            build_request(uri, options, range, false, false, None, extra_cookie)?;
                        let response = send_on_http1(stream, request).await?;
                        Ok(SendOutcome {
                            response,
                            ip,
                            lease: None,
                            via_proxy: false,
                            http2: false,
                        })
                    }
                };
            }

            let connector = tokio_rustls::TlsConnector::from(Arc::clone(&self.tls_http1));
            let name = server_name(host)?;
            let stream = tokio::time::timeout(
                options.connect_timeout,
                connector.connect(name, connected.stream),
            )
            .await
            .map_err(|_| NetworkError::Timeout)?
            .map_err(|err| NetworkError::CertError(err.to_string()))?;
            let request = build_request(uri, options, range, false, false, None, extra_cookie)?;
            let response = send_on_http1(stream, request).await?;
            return Ok(SendOutcome {
                response,
                ip,
                lease: None,
                via_proxy: false,
                http2: false,
            });
        }

        let request = build_request(uri, options, range, false, false, None, extra_cookie)?;
        let response = send_on_http1(connected.stream, request).await?;
        Ok(SendOutcome {
            response,
            ip,
            lease: None,
            via_proxy: false,
            http2: false,
        })
    }
}

struct SendOutcome {
    response: Response<Incoming>,
    #[allow(dead_code)]
    ip: std::net::IpAddr,
    lease: Option<H2Lease>,
    via_proxy: bool,
    http2: bool,
}

/// 完成一次 HTTP/1.1 交换；连接驱动任务常驻，直到 body 读完。
async fn send_on_http1<IO>(
    io: IO,
    request: Request<Empty<Bytes>>,
) -> Result<Response<Incoming>, NetworkError>
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(io))
        .await
        .map_err(net_err)?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    sender.send_request(request).await.map_err(net_err)
}

fn net_err(err: hyper::Error) -> NetworkError {
    if err.is_incomplete_message() || err.is_body_write_aborted() {
        NetworkError::ConnectionReset
    } else {
        NetworkError::Network(err.to_string())
    }
}

fn parse_content_range_header(response: &Response<Incoming>) -> Option<(u64, u64, Option<u64>)> {
    let value = response
        .headers()
        .get(http::header::CONTENT_RANGE)?
        .to_str()
        .ok()?;
    parse_content_range(value).ok()
}

/// `Content-Range: bytes * /N`（416 响应）。
fn parse_unsatisfied_content_range(response: &Response<Incoming>) -> Option<u64> {
    let value = response
        .headers()
        .get(http::header::CONTENT_RANGE)?
        .to_str()
        .ok()?;
    let rest = value.strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    if range.trim() != "*" {
        return None;
    }
    total.trim().parse::<u64>().ok()
}

/// 调度器探测用：`Range: bytes=0-0`。
pub fn probe_range() -> ByteRange {
    ByteRange::new(0, Some(0))
}

/// beta9：响应体是网页而不是文件（`Content-Type: text/html`）。
///
/// 防爬站点（如 PyCharm 官网）常把深层重定向落到登录/验证页，返回 200 +
/// `text/html`；若照常写盘会得到一个打不开的假文件，这里让调用方提前终止。
pub fn is_html_body(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| {
        value
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("text/html")
    })
}

/// beta9：用户 / 服务器明确要求下载网页文件时放行（`.html/.htm/.mht/.mhtml`）。
///
/// 判据只看扩展名：显式 `out` 命名的 `dest`，或 `Content-Disposition` 里的文件名。
pub fn allows_html_download(dest: &Path, disposition: Option<&str>) -> bool {
    fn has_html_ext(name: &str) -> bool {
        let name = name.trim().trim_matches(['"', '\'']);
        Path::new(name)
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                matches!(
                    ext.to_ascii_lowercase().as_str(),
                    "html" | "htm" | "mht" | "mhtml"
                )
            })
    }

    if dest
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(has_html_ext)
    {
        return true;
    }
    disposition.is_some_and(|raw| {
        // 只取 `filename=...` / `filename*=...` 段，忽略前面的 attachment/inline。
        raw.rsplit(';').next().is_some_and(|part| {
            let value = part.split_once('=').map_or(part, |(_, value)| value);
            value.rsplit('/').next().is_some_and(has_html_ext)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_content_type_matches_only_html() {
        assert!(is_html_body(Some("text/html")));
        assert!(is_html_body(Some("text/html; charset=utf-8")));
        assert!(is_html_body(Some("TEXT/HTML")));
        assert!(!is_html_body(Some("application/octet-stream")));
        assert!(!is_html_body(Some("text/plain")));
        assert!(!is_html_body(None));
    }

    #[test]
    fn explicit_html_download_is_allowed() {
        let dest = Path::new("C:/Downloads/page.html");
        assert!(allows_html_download(dest, None));
        assert!(allows_html_download(
            Path::new("C:/Downloads/Download_123"),
            Some(r#"attachment; filename="page.htm""#)
        ));
        // 普通文件名 + 网页响应 = 拦截。
        assert!(!allows_html_download(
            Path::new("C:/Downloads/Download_123"),
            None
        ));
        assert!(!allows_html_download(
            Path::new("C:/Downloads/setup.exe"),
            Some(r#"attachment; filename="setup.exe""#)
        ));
    }

    #[test]
    fn cookie_jar_carries_initial_cookie_and_absorbs_same_host_set_cookie() {
        let mut jar = CookieJar::from_headers(&[("Cookie".to_string(), "a=1; b=2".to_string())]);
        assert_eq!(jar.header_value().as_deref(), Some("a=1; b=2"));

        let mut headers = HeaderMap::new();
        headers.append(SET_COOKIE, "s=2; Path=/; HttpOnly".parse().unwrap());
        jar.absorb(&headers, "a.test", "a.test");
        assert_eq!(jar.header_value().as_deref(), Some("a=1; b=2; s=2"));

        // 同名覆盖：新的 a=9 替换旧的 a=1，不再追加第二份。
        headers.clear();
        headers.append(SET_COOKIE, "a=9".parse().unwrap());
        jar.absorb(&headers, "a.test", "a.test");
        assert_eq!(jar.header_value().as_deref(), Some("a=9; b=2; s=2"));
    }

    #[test]
    fn cookie_jar_drops_cross_host_set_cookie() {
        let mut jar = CookieJar::from_headers(&[("cookie".to_string(), "auth=1".to_string())]);
        let mut headers = HeaderMap::new();
        headers.append(SET_COOKIE, "s=2".parse().unwrap());
        // 无 Domain：只允许同 host。原 Cookie 仍无条件保留。
        jar.absorb(&headers, "a.test", "b.test");
        assert_eq!(jar.header_value().as_deref(), Some("auth=1"));
    }

    #[test]
    fn set_cookie_domain_matches_only_itself_or_subdomain() {
        let mut headers = HeaderMap::new();
        headers.append(SET_COOKIE, "s=2; Domain=.cdn.test".parse().unwrap());

        let mut jar = CookieJar::default();
        jar.absorb(&headers, "a.test", "edge.cdn.test");
        assert_eq!(jar.header_value().as_deref(), Some("s=2"));

        let mut other = CookieJar::default();
        other.absorb(&headers, "a.test", "evilcdn.test");
        assert_eq!(other.header_value(), None);
    }
}
