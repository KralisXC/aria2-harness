//! HTTP/2 多路复用（复刻 C++ `Http2Multiplexer`，基线 `docs/c3/Q6-h2-baseline.md`）。
//!
//! 关键不变量：
//! - I1/I3：同一 authority（+TLS 身份 + 代理 + 流上限）共用一个真实 socket；
//!   不同键各自新建，互不 retire；
//! - I4：只有 HTTPS 且 ALPN 协商出 `h2` 才建 mux；服务端只给 `http/1.1` 时把
//!   已握手的连接原样交回调用方（不浪费一次 TCP/TLS）；
//! - I5：代理 CONNECT 隧道不进入本池（调用方用 http1-only TLS 配置）；
//! - 流上限：`min(--split, --max-connection-per-server)`，用 Semaphore 强制，
//!   绝不无脑开流（Q6 §5）；
//! - 数据面：接收窗口对齐 C++（流 1 MiB / 连接 8 MiB，`Http2Multiplexer.cc:54-55`），
//!   流控由 hyper 按真实消费归还（调用方 drop body 即 RST_STREAM，等价 `retire` 语义）；
//! - EOF：`Incoming` 仅在流真正结束（END_STREAM）时返回 `None`，传输失败返回
//!   `Err`，不会被误判成 EOF（Q6 §3 最关键一条）。

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use http::{Request, Response};
use http_body_util::Empty;
use hyper::body::{Bytes, Incoming};
use hyper::client::conn::http2;
use hyper::client::conn::http2::SendRequest;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_rustls::client::TlsStream;

use crate::error::NetworkError;

/// Q6 §3：单次读 socket 的块大小（也是 nghttp2 时代的 staging 粒度）。
pub const READ_CHUNK_SIZE: usize = 16 * 1024;

/// 流级接收窗口。对齐 C++ `Http2Multiplexer.cc:54`（1 MiB/stream，带宽时延积预算）。
const INITIAL_STREAM_WINDOW: u32 = 1024 * 1024;
/// 连接级接收窗口。对齐 C++ `Http2Multiplexer.cc:55`（8 MiB，覆盖 16 流 × 1 MiB 的聚合预算）。
const INITIAL_CONNECTION_WINDOW: u32 = 8 * 1024 * 1024;

/// 连接池键。`max_streams` 进入键，避免不同 `--split/-x` 的任务共用一套配额。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MuxKey {
    pub scheme: String,
    pub authority: String,
    pub tls: String,
    pub proxy: String,
    pub max_streams: usize,
}

impl MuxKey {
    pub fn new(
        scheme: &str,
        authority: &str,
        tls_fingerprint: &str,
        proxy_id: &str,
        max_streams: usize,
    ) -> Self {
        Self {
            scheme: scheme.to_ascii_lowercase(),
            authority: authority.to_ascii_lowercase(),
            tls: tls_fingerprint.to_string(),
            proxy: proxy_id.to_string(),
            max_streams: max_streams.max(1),
        }
    }
}

/// `acquire` 的结果。
pub enum Pooled {
    /// HTTP/2 连接：后续请求以 stream 形式提交。
    H2(Arc<H2Mux>),
    /// 服务端未协商 h2：调用方用这条已握手的连接走 HTTP/1.1。
    Http1(TlsStream<TcpStream>),
}

/// 一条 HTTP/2 连接（唯一 socket 持有者）。
pub struct H2Mux {
    sender: SendRequest<Empty<Bytes>>,
    permits: Arc<Semaphore>,
    max_streams: usize,
    total_streams: Arc<AtomicU64>,
    active_streams: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
}

impl H2Mux {
    fn new(sender: SendRequest<Empty<Bytes>>, max_streams: usize, closed: Arc<AtomicBool>) -> Self {
        let max_streams = max_streams.max(1);
        Self {
            sender,
            permits: Arc::new(Semaphore::new(max_streams)),
            max_streams,
            total_streams: Arc::new(AtomicU64::new(0)),
            active_streams: Arc::new(AtomicU64::new(0)),
            closed,
        }
    }

    pub fn is_alive(&self) -> bool {
        !self.closed.load(Ordering::SeqCst) && !self.sender.is_closed()
    }

    pub fn max_streams(&self) -> usize {
        self.max_streams
    }

    pub fn total_streams(&self) -> u64 {
        self.total_streams.load(Ordering::SeqCst)
    }

    pub fn active_streams(&self) -> u64 {
        self.active_streams.load(Ordering::SeqCst)
    }

    /// 在 mux 上开一条 stream。达到上限时等待（绝不无脑开流）。
    pub async fn send(
        &self,
        request: Request<Empty<Bytes>>,
    ) -> Result<(Response<Incoming>, H2Lease), NetworkError> {
        if !self.is_alive() {
            return Err(NetworkError::Network("HTTP/2 连接已关闭".into()));
        }
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| NetworkError::Network("HTTP/2 连接已关闭".into()))?;

        self.total_streams.fetch_add(1, Ordering::SeqCst);
        self.active_streams.fetch_add(1, Ordering::SeqCst);

        // SendRequest 可克隆：克隆体共享同一连接，但不需要串行化发送。
        let mut sender = self.sender.clone();
        match sender.send_request(request).await {
            Ok(response) => Ok((
                response,
                H2Lease {
                    _permit: permit,
                    active: Arc::clone(&self.active_streams),
                },
            )),
            Err(err) => {
                self.active_streams.fetch_sub(1, Ordering::SeqCst);
                Err(net_err(err))
            }
        }
    }
}

/// stream 租约：持有期间占用一个流配额；drop 即归还（RST 语义由 body 的 Drop 触发）。
pub struct H2Lease {
    _permit: OwnedSemaphorePermit,
    active: Arc<AtomicU64>,
}

impl Drop for H2Lease {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn net_err(err: hyper::Error) -> NetworkError {
    if err.is_incomplete_message() || err.is_canceled() || err.is_body_write_aborted() {
        NetworkError::ConnectionReset
    } else {
        NetworkError::Network(format!("HTTP/2 错误：{err}"))
    }
}

/// 连接池：**每个 RequestGroup（一次任务运行）一个**，与 C++ `Http2Multiplexer`
/// 的持有者一致；键含 authority/TLS 身份/代理/流上限。
pub struct H2Pool {
    entries: Mutex<HashMap<MuxKey, Arc<H2Mux>>>,
}

impl Default for H2Pool {
    fn default() -> Self {
        Self::new()
    }
}

impl H2Pool {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn get_alive(&self, key: &MuxKey) -> Option<Arc<H2Mux>> {
        let mut map = self.entries.lock().unwrap();
        match map.get(key) {
            Some(mux) if mux.is_alive() => Some(Arc::clone(mux)),
            Some(_) => {
                map.remove(key);
                None
            }
            None => None,
        }
    }

    /// 取一个可用 mux；没有则用 `connect` 建一条 TLS 连接并尝试 h2 握手。
    pub async fn acquire<F, Fut>(&self, key: MuxKey, connect: F) -> Result<Pooled, NetworkError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<TlsStream<TcpStream>, NetworkError>>,
    {
        if let Some(mux) = self.get_alive(&key) {
            return Ok(Pooled::H2(mux));
        }

        let tls = connect().await?;
        // I4：只有 ALPN 真正协商出 h2 才建 mux。
        if tls.get_ref().1.alpn_protocol() != Some(b"h2") {
            return Ok(Pooled::Http1(tls));
        }

        let max_streams = key.max_streams;
        let mut builder = http2::Builder::new(TokioExecutor::new());
        builder
            .initial_stream_window_size(INITIAL_STREAM_WINDOW)
            .initial_connection_window_size(INITIAL_CONNECTION_WINDOW)
            .max_frame_size(READ_CHUNK_SIZE as u32)
            .max_concurrent_streams(max_streams as u32);
        let (sender, connection) = builder
            .handshake(TokioIo::new(tls))
            .await
            .map_err(net_err)?;

        let closed = Arc::new(AtomicBool::new(false));
        let closed_flag = Arc::clone(&closed);
        tokio::spawn(async move {
            // 驱动整条连接；错误/GOAWAY 之后置 closed，下一次 acquire 会重建。
            let _ = connection.await;
            closed_flag.store(true, Ordering::SeqCst);
        });

        let mux = Arc::new(H2Mux::new(sender, max_streams, closed));

        // 并发建立时的竞态：后来者复用先入池的 mux，自己的连接随发送端 drop 关闭。
        let mut map = self.entries.lock().unwrap();
        if let Some(existing) = map.get(&key) {
            if existing.is_alive() {
                return Ok(Pooled::H2(Arc::clone(existing)));
            }
        }
        map.insert(key, Arc::clone(&mux));
        Ok(Pooled::H2(mux))
    }

    /// 池快照（A/B 测试与审计用）。
    pub fn stats(&self) -> PoolStats {
        let mut map = self.entries.lock().unwrap();
        map.retain(|_, mux| mux.is_alive());
        PoolStats {
            muxes: map.len(),
            total_streams: map.values().map(|mux| mux.total_streams()).sum(),
            active_streams: map.values().map(|mux| mux.active_streams()).sum(),
        }
    }

    /// 测试钩子：清空池（不主动 GOAWAY，连接随 Arc 释放关闭）。
    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }

    /// 计算每 authority 的并发流上限：`min(split, max-connection-per-server)`。
    ///
    /// `split=None`（`--split=-1`）按 max-conn 截断；aria2 默认 `split=5`、
    /// `max-connection-per-server=1` → 默认只有 1 条并发流，与单连接行为一致。
    pub fn stream_limit(split: Option<usize>, max_connection_per_server: usize) -> usize {
        let max_conn = max_connection_per_server.clamp(1, 128);
        split.unwrap_or(max_conn).clamp(1, 128).min(max_conn)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolStats {
    /// 当前存活的 h2 连接数（= 真实 socket 数）。
    pub muxes: usize,
    /// 自建连以来提交过的 stream 总数。
    pub total_streams: u64,
    /// 当前在途 stream 数。
    pub active_streams: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_limit_matches_aria2_defaults() {
        assert_eq!(H2Pool::stream_limit(Some(5), 1), 1);
        assert_eq!(H2Pool::stream_limit(Some(5), 5), 5);
        assert_eq!(H2Pool::stream_limit(Some(2), 5), 2);
        assert_eq!(H2Pool::stream_limit(None, 5), 5);
        assert_eq!(H2Pool::stream_limit(Some(5), 200), 5);
    }

    #[test]
    fn mux_key_is_case_insensitive_for_authority() {
        let a = MuxKey::new("HTTPS", "Example.COM:443", "cc=true", "", 1);
        let b = MuxKey::new("https", "example.com:443", "cc=true", "", 1);
        assert_eq!(a, b);
    }

    #[test]
    fn empty_pool_stats_are_zero() {
        let pool = H2Pool::new();
        assert_eq!(
            pool.stats(),
            PoolStats {
                muxes: 0,
                total_streams: 0,
                active_streams: 0
            }
        );
    }
}
