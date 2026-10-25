//! NetworkError → aria2 errorCode 映射（红线 R4）。
//!
//! 目标：`degradation.rs` 不改一行即可复用。错误码取自
//! `aria2-1.37.0/src/error_code.h`（本仓已含 PROJECT 自加的 34/35/36），
//! 5.4.6 再补齐剩余分支。

use std::fmt;

/// 引擎内部网络错误。禁止用裸 `String` 跨层传递（手册 §5.4.6）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkError {
    /// 名称解析失败（含畸形应答、无记录）。
    DnsFailure(String),
    /// 建连/读超时。
    Timeout,
    /// 对端重置连接（RST）。
    ConnectionReset,
    /// 证书校验失败（含 SNI 非法）。
    CertError(String),
    /// HTTP 协议层错误（状态码/头非法、重定向缺 Location）。
    HttpProtocol(String),
    /// 服务器返回的失败状态码（映射见 `HttpSkipResponseCommand.cc:207-246`）。
    HttpStatus(u16),
    /// 服务器忽略 Range（`HttpResponse::validateResponse` 归到 CANNOT_RESUME）。
    RangeNotSupported,
    /// 重定向超过 `--max-redirect`。
    TooManyRedirects,
    /// 代理要求认证 / 认证失败（HTTP 407 或 SOCKS5 认证拒绝）。
    ProxyAuthRequired,
    /// CONNECT 隧道被代理拒绝（非 200）。对齐 C++ `AbstractProxyResponseCommand`
    /// 的 `EX_PROXY_CONNECTION_FAILED`：它没有专属错误码，最终落在 UNKNOWN_ERROR(1)。
    ProxyConnectionFailed(String),
    /// 代理握手失败（SOCKS5 应答错误、代理不可达/应答非法）。
    ProxyError(String),
    /// 磁盘空间不足（Phase 4：预分配或多 worker 写入触发）。
    DiskFull,
    /// 其它网络问题。
    Network(String),
    /// 任务被取消（generation 失效）。
    Cancelled,
}

impl NetworkError {
    /// 是否值得重试（对齐 aria2）：超时、连接重置、其它网络错误、504。
    ///
    /// 502/503 只有 `--retry-wait>0` 才重试（默认 0），故这里不重试；
    /// 4xx/证书/重定向/协议错误一律不重试。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            NetworkError::Timeout
                | NetworkError::ConnectionReset
                | NetworkError::Network(_)
                | NetworkError::HttpStatus(504)
        )
    }

    /// 映射到 aria2 的 `errorCode`（RPC `tellStatus.errorCode` 原样回填）。
    pub fn to_aria2_code(&self) -> i32 {
        match self {
            // UNKNOWN_ERROR：aria2 未给证书错误单独编号。
            NetworkError::CertError(_) => 1,
            NetworkError::Timeout => 2,
            NetworkError::Network(_) => 6,
            // CANNOT_RESUME：aria2 HttpResponse.cc:86
            NetworkError::RangeNotSupported => 8,
            // NAME_RESOLVE_ERROR
            NetworkError::DnsFailure(_) => 19,
            // HTTP_PROTOCOL_ERROR
            NetworkError::HttpProtocol(_) => 22,
            // RESOURCE_NOT_FOUND(3) / HTTP_AUTH_REQUIRED(33) / HTTP_SERVICE_UNAVAILABLE(29)
            NetworkError::HttpStatus(code) => match code {
                403 | 404 => 3,
                401 | 407 => 33,
                502 | 503 | 504 => 29,
                _ => 22,
            },
            // HTTP_TOO_MANY_REDIRECTS
            NetworkError::TooManyRedirects => 23,
            // HTTP_AUTH_REQUIRED（源站 401 与转发代理 407 同为 33，对齐
            // `HttpSkipResponseCommand.cc:215-221`）
            NetworkError::ProxyAuthRequired => 33,
            // UNKNOWN_ERROR：C++ CONNECT 隧道非 200 → EX_PROXY_CONNECTION_FAILED，
            // 无专属码（`AbstractProxyResponseCommand.cc:74-76`）。
            NetworkError::ProxyConnectionFailed(_) => 1,
            // NETWORK_PROBLEM（SOCKS5 应答非法/代理不可达）
            NetworkError::ProxyError(_) => 6,
            // DISK_FULL（项目扩展码 34，见 error_code.h）
            NetworkError::DiskFull => 34,
            // REMOVED
            NetworkError::Cancelled => 31,
            // CONNECTION_RESET（PROJECT 内部码，35）
            NetworkError::ConnectionReset => 35,
        }
    }
}

impl fmt::Display for NetworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetworkError::DnsFailure(msg) => write!(f, "名称解析失败：{msg}"),
            NetworkError::Timeout => write!(f, "连接或读取超时"),
            NetworkError::ConnectionReset => write!(f, "连接被对端重置"),
            NetworkError::CertError(msg) => write!(f, "证书校验失败：{msg}"),
            NetworkError::HttpProtocol(msg) => write!(f, "HTTP 协议错误：{msg}"),
            NetworkError::HttpStatus(code) => write!(f, "HTTP 状态码 {code}"),
            NetworkError::RangeNotSupported => write!(f, "服务器不支持 Range"),
            NetworkError::TooManyRedirects => write!(f, "重定向次数超过上限"),
            NetworkError::ProxyAuthRequired => write!(f, "代理要求认证"),
            NetworkError::ProxyConnectionFailed(msg) => write!(f, "代理连接失败：{msg}"),
            NetworkError::ProxyError(msg) => write!(f, "代理错误：{msg}"),
            NetworkError::DiskFull => write!(f, "磁盘空间不足"),
            NetworkError::Network(msg) => write!(f, "网络错误：{msg}"),
            NetworkError::Cancelled => write!(f, "任务已取消"),
        }
    }
}

impl std::error::Error for NetworkError {}

impl From<std::io::Error> for NetworkError {
    fn from(err: std::io::Error) -> Self {
        // Windows ERROR_DISK_FULL=112 / POSIX ENOSPC=28（Phase 4 预分配与多 worker 写入）。
        if is_disk_full(&err) {
            return NetworkError::DiskFull;
        }
        match err.kind() {
            std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe => NetworkError::ConnectionReset,
            std::io::ErrorKind::TimedOut => NetworkError::Timeout,
            _ => NetworkError::Network(err.to_string()),
        }
    }
}

fn is_disk_full(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(112) | Some(28))
}

#[cfg(test)]
mod tests {
    use super::NetworkError;

    #[test]
    fn maps_to_aria2_codes() {
        assert_eq!(NetworkError::DnsFailure("x".into()).to_aria2_code(), 19);
        assert_eq!(NetworkError::Timeout.to_aria2_code(), 2);
        assert_eq!(NetworkError::RangeNotSupported.to_aria2_code(), 8);
        assert_eq!(NetworkError::HttpProtocol("x".into()).to_aria2_code(), 22);
        assert_eq!(NetworkError::TooManyRedirects.to_aria2_code(), 23);
        assert_eq!(NetworkError::ProxyAuthRequired.to_aria2_code(), 33);
        assert_eq!(
            NetworkError::ProxyConnectionFailed("x".into()).to_aria2_code(),
            1
        );
        assert_eq!(NetworkError::ProxyError("x".into()).to_aria2_code(), 6);
        assert_eq!(NetworkError::DiskFull.to_aria2_code(), 34);
        assert_eq!(NetworkError::Cancelled.to_aria2_code(), 31);
        assert_eq!(NetworkError::ConnectionReset.to_aria2_code(), 35);
    }
}
