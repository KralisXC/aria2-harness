//! hyper HTTP 客户端：Range GET + 落盘 + 重定向 + 断点（Phase 1），
//! Phase 3 增加代理与 HTTP/2 多路复用，Phase 4 抽出 [`HttpSession`] 供调度器复用。
//!
//! 连接策略：
//! - 明文 http：HTTP/1.1（与 aria2 一致，不做 h2c）；
//! - https + `--enable-http2=true`：TLS 握手宣告 `h2, http/1.1`，协商到 h2 就进
//!   `h2::H2Pool`（同 authority 复用一条 socket、流数受 `--split`/`-x` 约束）；
//!   服务端只给 http/1.1 时复用同一条已握手连接；
//! - 代理：http 目标走 absolute-form 转发，https 目标走 CONNECT 隧道；
//!   隧道内 ALPN 只宣告 http/1.1（Q6 I5）；
//! - SNI 用 URL 域名，TCP 连解析出的 IP（Q9）。
//!
//! Phase 4 起，本模块的 `download_with_progress` 只是单连接兼容路径；
//! 多段并发由 `download::scheduler` 基于 [`crate::http::session::HttpSession`] 实现。

use std::path::PathBuf;
use std::time::Duration;

use http::header::{ACCEPT, COOKIE, HOST, RANGE, REFERER, USER_AGENT};
use http::{HeaderName, HeaderValue, Request, StatusCode, Uri};
use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use tokio::fs::File;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::dns::resolver::{DnsResolver, default_port};
use crate::error::NetworkError;
use crate::http::proxy::ProxySettings;
use crate::http::session::{
    ByteRange, HttpSession, allows_html_download, is_html_body,
};
use crate::http::tls::TlsOptions;

/// 单次下载的可选项（对齐 aria2 同名参数）。
#[derive(Debug, Clone)]
pub struct HttpOptions {
    pub user_agent: String,
    pub referer: Option<String>,
    /// `header=Name: value` 形式的附加请求头。
    pub headers: Vec<(String, String)>,
    /// aria2 `--check-certificate`。
    pub check_certificate: bool,
    /// aria2 `--enable-http2`（默认 true）。
    pub enable_http2: bool,
    /// aria2 `--certificate`：PEM 客户端证书。
    pub client_certificate: Option<PathBuf>,
    /// aria2 `--private-key`：PEM 客户端私钥。
    pub private_key: Option<PathBuf>,
    /// aria2 `--all-proxy` / `--http-proxy` / `--https-proxy` / `--no-proxy`。
    pub proxy: ProxySettings,
    /// aria2 `--proxy-method=tunnel`：http 目标也走 CONNECT（`AbstractCommand.cc:901`）。
    pub proxy_method_tunnel: bool,
    /// aria2 `--split`；`None` 表示 -1（无限），由 `-x` 截断。
    pub split: Option<usize>,
    /// aria2 `-x/--max-connection-per-server`（1–128，默认 1）。
    pub max_connection_per_server: usize,
    /// aria2 `--max-redirect`。
    pub max_redirect: usize,
    pub connect_timeout: Duration,
    /// 单帧读取超时（None = 不限）。
    pub read_timeout: Option<Duration>,
    /// aria2 `--max-tries`：单 URI 的最大尝试次数（含首次）。
    pub max_tries: usize,
    /// aria2 `--retry-wait`：重试前的等待。
    pub retry_wait: Duration,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            user_agent: "aria2/1.37.0".to_string(),
            referer: None,
            headers: Vec::new(),
            check_certificate: true,
            enable_http2: true,
            client_certificate: None,
            private_key: None,
            proxy: ProxySettings::default(),
            proxy_method_tunnel: false,
            split: Some(5),
            max_connection_per_server: 1,
            max_redirect: 20,
            connect_timeout: Duration::from_secs(30),
            read_timeout: None,
            max_tries: 5,
            retry_wait: Duration::ZERO,
        }
    }
}

impl HttpOptions {
    /// `http1_only=true` 用于代理隧道：ALPN 不宣告 h2（Q6 I5）。
    pub fn tls_options(&self, http1_only: bool) -> TlsOptions {
        let options = TlsOptions {
            check_certificate: self.check_certificate,
            enable_http2: self.enable_http2 && !http1_only,
            client_certificate: self.client_certificate.clone(),
            private_key: self.private_key.clone(),
        };
        options
    }

    /// 每 authority 的并发流上限：min(split, max-connection-per-server)。
    pub fn stream_limit(&self) -> usize {
        crate::http::h2::H2Pool::stream_limit(self.split, self.max_connection_per_server)
    }
}

#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub url: String,
    pub dest: PathBuf,
    /// 已完成的字节数；0 表示全新下载。
    pub resume_from: u64,
    pub options: HttpOptions,
}

#[derive(Debug, Clone)]
pub struct DownloadReport {
    pub final_url: String,
    pub redirects: Vec<String>,
    /// 由 Content-Length / Content-Range 推导的总长。
    pub total_length: Option<u64>,
    /// 本次写入字节数。
    pub written: u64,
    /// resume_from + written。
    pub completed_length: u64,
    pub range_used: bool,
    /// 本次请求是否走了代理（审计/测试用）。
    pub via_proxy: bool,
    /// 本次响应是否来自 HTTP/2 stream。
    pub http2: bool,
}

fn net_err(err: hyper::Error) -> NetworkError {
    if err.is_incomplete_message() || err.is_body_write_aborted() {
        NetworkError::ConnectionReset
    } else {
        NetworkError::Network(err.to_string())
    }
}

/// 传输进度回调（`written` 是本次传输累计字节，`total_length` 来自响应头）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressUpdate {
    pub written: u64,
    pub total_length: Option<u64>,
    /// 每片完成百分比（0..=100）；`None` = 本次不上报，沿用上层已有快照。
    pub pieces: Option<Vec<u8>>,
    /// 当前活跃 worker 数；`None` = 未知。
    pub active_workers: Option<u32>,
}

/// 下载单个 URL 到 `dest`，自动跟随重定向。
pub async fn download(
    req: &DownloadRequest,
    resolver: &DnsResolver,
) -> Result<DownloadReport, NetworkError> {
    download_with_progress(req, resolver, |_| {}).await
}

/// 带进度回调的单连接下载（Phase 1–3 兼容路径）。
pub async fn download_with_progress<F>(
    req: &DownloadRequest,
    resolver: &DnsResolver,
    mut on_progress: F,
) -> Result<DownloadReport, NetworkError>
where
    F: FnMut(ProgressUpdate),
{
    let session = HttpSession::new(req.options.clone(), resolver.clone())?;
    let range = (req.resume_from > 0).then(|| ByteRange::new(req.resume_from, None));
    let response = session.get_range(&req.url, range).await?;

    // beta9：防爬站点会把深层重定向落到登录/验证页（200 + text/html）。
    // 在写盘前终止，避免留下一个打不开的假文件。
    // 只判断成功响应：403/429 的错误页同样是 text/html，必须留给限流降级处理。
    if matches!(response.status.as_u16(), 200 | 206)
        && is_html_body(response.content_type.as_deref())
        && !allows_html_download(&req.dest, response.content_disposition.as_deref())
    {
        return Err(NetworkError::HtmlInsteadOfFile);
    }

    let (offset, total_length, range_used) =
        validate_response(response.status, response.total_length, response.content_range, req.resume_from)?;

    // 先抛一次零进度，让上层尽早拿到总长（tellStatus 的 totalLength）。
    on_progress(ProgressUpdate {
        written: 0,
        total_length,
        pieces: None,
        active_workers: Some(1),
    });

    let mut file = open_dest(&req.dest, offset).await?;
    // 租约必须活到 body 读完：drop body = RST_STREAM，drop 租约 = 归还流配额。
    let lease = response.lease;
    let written = copy_body(
        response.body,
        &mut file,
        req.options.read_timeout,
        total_length,
        &mut on_progress,
    )
    .await?;
    drop(lease);
    file.flush().await.map_err(NetworkError::from)?;

    Ok(DownloadReport {
        final_url: response.final_url,
        redirects: response.redirects,
        total_length,
        written,
        completed_length: offset + written,
        range_used,
        via_proxy: response.via_proxy,
        http2: response.http2,
    })
}

pub(crate) fn validate_uri(uri: &Uri) -> Result<(), NetworkError> {
    match uri.scheme_str() {
        Some("http") | Some("https") => {}
        Some(other) => {
            return Err(NetworkError::HttpProtocol(format!(
                "只支持 http/https，收到 `{other}`"
            )));
        }
        None => return Err(NetworkError::HttpProtocol("URI 缺 scheme".into())),
    }
    if uri.host().is_none() {
        return Err(NetworkError::HttpProtocol("URI 缺 host".into()));
    }
    Ok(())
}

/// 校验状态码 / Content-Range，返回 (落盘起点, 总长, 是否用了 Range)。
pub(crate) fn validate_response(
    status: StatusCode,
    content_length: Option<u64>,
    content_range: Option<(u64, u64, Option<u64>)>,
    resume_from: u64,
) -> Result<(u64, Option<u64>, bool), NetworkError> {
    match status.as_u16() {
        200 => {
            if resume_from > 0 {
                // 服务器忽略 Range：aria2 归为 CANNOT_RESUME（错误码 8）。
                return Err(NetworkError::RangeNotSupported);
            }
            Ok((0, content_length, false))
        }
        206 => {
            let (start, _end, total) = content_range
                .ok_or_else(|| NetworkError::HttpProtocol("206 缺少 Content-Range".into()))?;
            if start != resume_from {
                return Err(NetworkError::RangeNotSupported);
            }
            Ok((start, total, true))
        }
        other => Err(NetworkError::HttpStatus(other)),
    }
}

/// `bytes 5-9/10` 或 `bytes 5-9/*`。
pub(crate) fn parse_content_range(value: &str) -> Result<(u64, u64, Option<u64>), NetworkError> {
    let bad = || NetworkError::HttpProtocol(format!("Content-Range 非法：`{value}`"));
    let rest = value.strip_prefix("bytes ").ok_or_else(bad)?;
    let (range, total) = rest.split_once('/').ok_or_else(bad)?;
    let (start, end) = range.split_once('-').ok_or_else(bad)?;
    let start = start.trim().parse::<u64>().map_err(|_| bad())?;
    let end = end.trim().parse::<u64>().map_err(|_| bad())?;
    if end < start {
        return Err(bad());
    }
    let total = match total.trim() {
        "*" => None,
        text => Some(text.parse::<u64>().map_err(|_| bad())?),
    };
    Ok((start, end, total))
}

async fn open_dest(dest: &std::path::Path, offset: u64) -> Result<File, NetworkError> {
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(NetworkError::from)?;
        }
    }
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dest)
        .await
        .map_err(NetworkError::from)?;
    if offset > 0 {
        let mut file = file;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(NetworkError::from)?;
        return Ok(file);
    }
    Ok(file)
}

/// 读 body 落盘。EOF 只由 `Incoming` 在流真正结束时给出；
/// 传输中断是 `Err`，不会被误当成 EOF（Q6 §3）。
pub(crate) async fn copy_body<F>(
    body: hyper::body::Incoming,
    file: &mut File,
    read_timeout: Option<Duration>,
    total_length: Option<u64>,
    on_progress: &mut F,
) -> Result<u64, NetworkError>
where
    F: FnMut(ProgressUpdate),
{
    let mut body = body;
    let mut written = 0u64;
    loop {
        let next = match read_timeout {
            Some(limit) => tokio::time::timeout(limit, body.frame())
                .await
                .map_err(|_| NetworkError::Timeout)?,
            None => body.frame().await,
        };
        let Some(frame) = next else { break };
        let frame = frame.map_err(net_err)?;
        if let Ok(data) = frame.into_data() {
            file.write_all(&data).await.map_err(NetworkError::from)?;
            written += data.len() as u64;
            on_progress(ProgressUpdate {
                written,
                total_length,
                pieces: None,
                active_workers: Some(1),
            });
        }
    }
    Ok(written)
}

pub(crate) fn host_header(uri: &Uri) -> String {
    let host = uri.host().unwrap_or_default();
    let scheme = uri.scheme_str().unwrap_or("http");
    match uri.port_u16() {
        Some(port) if port != default_port(scheme) => format!("{host}:{port}"),
        _ => host.to_string(),
    }
}

fn path_and_query(uri: &Uri) -> String {
    uri.path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string())
}

/// 组装请求。
///
/// - `absolute=true`：HTTP 代理 forward 模式，request-target 用完整 URI；
/// - `h2=true`：HTTP/2 用完整 URI 让 hyper 生成 `:scheme/:authority`，不发 Host 头；
/// - `extra_cookie`：跳转链 Cookie 罐的完整 `Cookie` 值（已含 `options.headers`
///   里的初始 Cookie 与本链吸收的 `Set-Cookie`）。给了它就跳过 `options.headers`
///   里的 `cookie`，避免同名头重复下发。
pub(crate) fn build_request(
    uri: &Uri,
    options: &HttpOptions,
    range: Option<ByteRange>,
    absolute: bool,
    h2: bool,
    proxy_authorization: Option<&str>,
    extra_cookie: Option<&str>,
) -> Result<Request<Empty<Bytes>>, NetworkError> {
    let target = if absolute || h2 {
        uri.to_string()
    } else {
        path_and_query(uri)
    };
    let mut builder = Request::builder()
        .method("GET")
        .uri(target)
        .header(USER_AGENT, &options.user_agent)
        .header(ACCEPT, "*/*");
    if !h2 {
        builder = builder.header(HOST, host_header(uri));
    }
    if let Some(range) = range {
        builder = builder.header(RANGE, range.header_value());
    }
    if let Some(referer) = &options.referer {
        builder = builder.header(REFERER, referer);
    }
    if let Some(value) = proxy_authorization {
        builder = builder.header(
            HeaderName::from_static("proxy-authorization"),
            HeaderValue::from_str(value)
                .map_err(|err| NetworkError::HttpProtocol(format!("代理认证头非法：{err}")))?,
        );
    }
    if let Some(cookie) = extra_cookie {
        let value = HeaderValue::from_str(cookie)
            .map_err(|err| NetworkError::HttpProtocol(format!("Cookie 头非法：{err}")))?;
        builder = builder.header(COOKIE, value);
    }
    for (name, value) in &options.headers {
        if extra_cookie.is_some() && name.eq_ignore_ascii_case("cookie") {
            // 已由 extra_cookie 统一合并（含跳转链吸收的 Set-Cookie）。
            continue;
        }
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|err| NetworkError::HttpProtocol(format!("非法请求头名 `{name}`：{err}")))?;
        let value = HeaderValue::from_str(value)
            .map_err(|err| NetworkError::HttpProtocol(format!("非法请求头值：{err}")))?;
        builder = builder.header(name, value);
    }
    builder
        .body(Empty::<Bytes>::new())
        .map_err(|err| NetworkError::HttpProtocol(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_range() {
        assert_eq!(
            parse_content_range("bytes 5-9/10").unwrap(),
            (5, 9, Some(10))
        );
        assert_eq!(parse_content_range("bytes 0-0/*").unwrap(), (0, 0, None));
        assert!(parse_content_range("bytes x-9/10").is_err());
    }

    #[test]
    fn host_header_omits_default_port() {
        let uri: Uri = "http://a.test/file".parse().unwrap();
        assert_eq!(host_header(&uri), "a.test");
        let uri: Uri = "http://a.test:8080/file".parse().unwrap();
        assert_eq!(host_header(&uri), "a.test:8080");
        let uri: Uri = "https://a.test:443/file".parse().unwrap();
        assert_eq!(host_header(&uri), "a.test");
    }

    #[test]
    fn stream_limit_uses_split_and_max_connection() {
        let mut options = HttpOptions::default();
        assert_eq!(options.stream_limit(), 1);

        options.split = Some(8);
        options.max_connection_per_server = 8;
        assert_eq!(options.stream_limit(), 8);

        options.split = Some(3);
        assert_eq!(options.stream_limit(), 3);
    }

    #[test]
    fn h2_only_request_target_is_absolute() {
        let uri: Uri = "https://example.com/a?b=1".parse().unwrap();
        let options = HttpOptions::default();
        let request = build_request(&uri, &options, None, false, true, None, None).unwrap();
        assert_eq!(request.uri().to_string(), "https://example.com/a?b=1");
        assert!(request.headers().get(HOST).is_none());

        let request = build_request(&uri, &options, None, false, false, None, None).unwrap();
        assert_eq!(request.uri().to_string(), "/a?b=1");
        assert_eq!(request.headers().get(HOST).unwrap(), "example.com");
    }

    #[test]
    fn range_header_is_closed_or_open_ended() {
        let uri: Uri = "http://a.test/f".parse().unwrap();
        let options = HttpOptions::default();
        let request = build_request(
            &uri,
            &options,
            Some(ByteRange::new(5, Some(9))),
            false,
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(request.headers().get(RANGE).unwrap(), "bytes=5-9");

        let request = build_request(
            &uri,
            &options,
            Some(ByteRange::new(5, None)),
            false,
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(request.headers().get(RANGE).unwrap(), "bytes=5-");
    }

    #[test]
    fn extra_cookie_replaces_header_cookie_without_duplicating() {
        let uri: Uri = "http://a.test/f".parse().unwrap();
        let mut options = HttpOptions::default();
        options.headers = vec![("Cookie".to_string(), "a=1".to_string())];

        // 给了 extra_cookie：只下发合并后的罐值，不再重复 options 里的 Cookie。
        let request = build_request(&uri, &options, None, false, false, None, Some("a=1; s=2"))
            .unwrap();
        let cookies: Vec<_> = request.headers().get_all(COOKIE).iter().collect();
        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0], "a=1; s=2");

        // 没给：沿用 options.headers 里的原始 Cookie。
        let request = build_request(&uri, &options, None, false, false, None, None).unwrap();
        assert_eq!(request.headers().get(COOKIE).unwrap(), "a=1");
    }

    #[test]
    fn tunnel_tls_never_offers_h2() {
        let options = HttpOptions::default();
        assert!(!options.tls_options(true).enable_http2);
        assert!(options.tls_options(false).enable_http2);
    }
}
