//! 背压：Semaphore（在途连接上限）+ 有界 channel（事件洪峰削峰）。
//!
//! Phase 1 的落盘路径天然背压：`http::client::copy_body` 串行 `write_all` 后再
//! 读下一帧，不会把响应体堆进内存。这里只保留连接级 Semaphore（默认 5，
//! 对齐 aria2 `--max-concurrent-downloads`）；4MB 有界事件 channel 留到
//! Phase 3 多分段并发时再启用，避免现在造无用基础设施。

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// aria2 `--max-concurrent-downloads` 默认值。
pub const DEFAULT_MAX_CONNECTIONS: usize = 5;

/// 事件通道预算（Phase 3 启用）。
pub const DEFAULT_BUFFER_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
pub struct Backpressure {
    permits: Arc<Semaphore>,
}

impl Backpressure {
    pub fn new(max_connections: usize) -> Arc<Self> {
        Arc::new(Self {
            permits: Arc::new(Semaphore::new(max_connections.max(1))),
        })
    }

    /// 取得一个下载槽位；持有返回值期间占用。
    pub async fn acquire(&self) -> OwnedSemaphorePermit {
        Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .expect("Semaphore 不会被关闭")
    }

    pub fn available(&self) -> usize {
        self.permits.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn limits_concurrent_slots() {
        let gate = Backpressure::new(2);
        let first = gate.acquire().await;
        let second = gate.acquire().await;
        assert_eq!(gate.available(), 0);

        let blocked = tokio::time::timeout(Duration::from_millis(50), gate.acquire()).await;
        assert!(blocked.is_err(), "超出并发上限时必须等待");

        drop(first);
        assert_eq!(gate.available(), 1);
        let third = tokio::time::timeout(Duration::from_millis(50), gate.acquire())
            .await
            .expect("释放后应立即获得槽位");
        drop(second);
        drop(third);
    }
}
