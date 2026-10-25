//! 可复用的 HTTP 会话（Phase 4）：一次 HTTP 交换的底层 API。
//!
//! `HttpSession` 持有一份 `HttpOptions`、两套 TLS 配置（常规 / http1-only）、
//! 一个任务级 `H2Pool` 和 DNS 解析器；调度器的所有 worker 共享同一个
//! `Arc<HttpSession>`，因此 HTTP/2 连接、代理与 Happy Eyeballs 逻辑完全复用
//! Phase 3，不重复建池。
//!
//! `get_range` 只负责「发一个 GET（可带 Range）+ 跟随重定向」，不做状态码
//! 语义判断；调用方（单连接路径或调度器）各自解释 200/206/416。

use std::sync::Arc;

use http::header::{CONTENT_LENGTH, LOCATION};
use http::{Request, Response, StatusCode, Uri};
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
    pub body: Incoming,
    /// 活到 body 读完为止（drop = 归还 H2 流配额）。
    pub lease: Option<H2Lease>,
    pub via_proxy: bool,
    pub http2: bool,
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

        loop {
            // `--timeout` 同时覆盖「等响应头」：服务端永久不响应时必须在有限时间内
            // 失败，否则探测/worker 会挂死（Phase 4 降级兜底）。body 的读超时由
            // 调用方按帧计时。
            let outcome = match self.options.read_timeout {
                Some(limit) => match tokio::time::timeout(limit, self.send_get(&current, range)).await {
                    Ok(result) => result?,
                    Err(_) => return Err(NetworkError::Timeout),
                },
                None => self.send_get(&current, range).await?,
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

            return Ok(RangeResponse {
                final_url: current.to_string(),
                redirects,
                status,
                total_length,
                content_range,
                unsatisfied_total: unsatisfied,
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
                        let request = build_request(uri, options, range, false, true, None)?;
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
                        let request = build_request(uri, options, range, false, false, None)?;
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
            let request = build_request(uri, options, range, false, false, None)?;
            let response = send_on_http1(stream, request).await?;
            return Ok(SendOutcome {
                response,
                ip,
                lease: None,
                via_proxy: false,
                http2: false,
            });
        }

        let request = build_request(uri, options, range, false, false, None)?;
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
