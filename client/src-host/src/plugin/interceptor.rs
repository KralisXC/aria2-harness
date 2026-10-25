//! 前置拦截会话（微内核：宿主不含站点解析逻辑，URL 改写权下放给插件）。
//!
//! 跨线程汇合模型：
//! - 宿主（tokio 侧，`add_download`）`begin()` 建立会话并等待 `oneshot`；
//! - 每个订阅插件在专属线程内同步执行 hook；如需改写则调 `ctx.resolveUrl()` → `resolve()`；
//! - 插件线程在 hook 返回后调 `complete()` 回报"已完成"，全部完成即唤醒宿主；
//! - 超时或无人订阅则原样放行（`finish()` 取不到结果）。
//!
//! 仲裁（增量 19 · P0-E）：`dispatch` 只是把 job 投进各插件**各自的线程队列**，
//! 入队顺序 ≠ 各线程的真实执行顺序，故"先到先得"存在竞态。改为**确定性优先级仲裁**：
//! 会话记录全部待回报拦截器的 `plugin_id -> priority`；某插件调用 `resolve` 时，只要仍有
//! **更高优先级**（数值更小）的拦截器尚未回报完成，就拒绝该次改写 —— 从而保证最高优先级
//! 的改写者必胜，且全程不阻塞任何线程（纯内存判断）。
//!
//! 本模块是**纯状态机**：只做内存状态迁移并返回 [`ResolveOutcome`]，不做审计落盘
//! （审计由唯一边界收敛点 `hostapi::respond` 依结果触发），因此可零 I/O 单测。
//!
//! `session` 由 `std::sync::Mutex` 保护（跨同步/异步边界共享，无 await 持锁）；
//! `lock` 为异步串行锁，保证同一时刻只有一次拦截会话，避免并发 add 互相覆盖。

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use tokio::sync::{oneshot, Mutex as AsyncMutex};

/// 一次 `resolveUrl` 的仲裁结果（供边界层决定审计与响应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// 接受并锁定（当前无更高优先级拦截器在处理，亦无人抢先改写）。
    Accepted,
    /// 已被其它插件改写，本次改写被忽略。
    Superseded,
    /// 仍有更高优先级（数值更小）的拦截器尚未回报完成，本次改写被拒绝。
    LowerPriority,
    /// 会话已结束（超时收尾或全部完成），本次结果迟到被丢弃。
    Late,
}

/// 一次拦截会话的聚合状态。
struct Active {
    /// 尚未回报完成的拦截器：`plugin_id -> priority`（数值越小越优先）。
    pending: HashMap<String, i64>,
    /// 胜出插件覆盖后的 URL。
    result: Option<String>,
    /// 当前锁定该会话的插件 ID（胜者）。
    resolved_by: Option<String>,
    /// 全部完成时唤醒宿主。
    tx: Option<oneshot::Sender<()>>,
}

#[derive(Default)]
pub struct Interceptor {
    /// 会话级串行锁（异步）。
    lock: AsyncMutex<()>,
    /// 当前会话（同一时刻至多一个）。
    session: StdMutex<Option<Active>>,
}

impl Interceptor {
    /// 取得串行锁；持锁期间完成"建会话 → 广播 → 等待 → 取结果"全流程。
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.lock.lock().await
    }

    /// 开启会话。入参为本次订阅拦截 hook 的 `(plugin_id, priority)` 列表（调用方已按
    /// priority 升序排列）；空列表表示无人订阅，返回 `None`（宿主零等待直接放行）。
    pub fn begin(&self, interceptors: Vec<(String, i64)>) -> Option<oneshot::Receiver<()>> {
        if interceptors.is_empty() {
            return None;
        }
        let (tx, rx) = oneshot::channel();
        *self.session_guard() = Some(Active {
            pending: interceptors.into_iter().collect(),
            result: None,
            resolved_by: None,
            tx: Some(tx),
        });
        Some(rx)
    }

    /// 插件尝试覆盖 URL，返回仲裁结果。**不做任何 I/O**（纯状态机，可零 I/O 单测）。
    pub fn resolve(&self, plugin_id: &str, url: &str) -> ResolveOutcome {
        let mut guard = self.session_guard();
        let Some(active) = guard.as_mut() else {
            // 会话已收尾（`finish()` 已取走）→ 迟到。
            return ResolveOutcome::Late;
        };
        // `tx` 已消费 = 全部插件已回报完成、宿主即将 `finish()`，此刻写入注定被丢弃 → 迟到。
        if active.tx.is_none() {
            return ResolveOutcome::Late;
        }
        // 已被他人锁定 → 抢占失败（同一插件可覆盖自己上一次的改写）。
        if let Some(winner) = active.resolved_by.as_deref() {
            if winner != plugin_id {
                return ResolveOutcome::Superseded;
            }
        }
        // 确定性优先级仲裁：仍有更高优先级（数值更小）的拦截器未回报完成 → 拒绝。
        if let Some(&caller) = active.pending.get(plugin_id) {
            if active.pending.values().any(|&p| p < caller) {
                return ResolveOutcome::LowerPriority;
            }
        }
        active.result = Some(url.to_string());
        active.resolved_by = Some(plugin_id.to_string());
        ResolveOutcome::Accepted
    }

    /// 单个插件回报"onBeforeAddDownload 已执行完毕"，从待回报集合中移除自己。
    pub fn complete(&self, plugin_id: &str) {
        let mut guard = self.session_guard();
        let Some(active) = guard.as_mut() else { return };
        active.pending.remove(plugin_id);
        if active.pending.is_empty() {
            if let Some(tx) = active.tx.take() {
                // 唤醒宿主：若宿主已因 3s 超时收尾（receiver 已 drop），send 会失败 ——
                // 属预期竞态，记 debug 即可（超时放行路径本就无副作用）。
                if tx.send(()).is_err() {
                    tracing::debug!("拦截会话唤醒失败：宿主已超时收尾并 drop receiver");
                }
            }
        }
    }

    /// 会话收尾：取出胜出 URL 并清空会话（清空后迟到的 resolve/complete 均无副作用）。
    pub fn finish(&self) -> Option<String> {
        self.session_guard().take().and_then(|active| active.result)
    }

    /// 取会话锁（对 poisoned 情况做容错，避免宿主 panic）。
    fn session_guard(&self) -> std::sync::MutexGuard<'_, Option<Active>> {
        self.session.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subs(items: &[(&str, i64)]) -> Vec<(String, i64)> {
        items.iter().map(|(id, p)| (id.to_string(), *p)).collect()
    }

    #[test]
    fn empty_interceptors_yield_no_session() {
        let it = Interceptor::default();
        assert!(it.begin(vec![]).is_none());
    }

    #[test]
    fn highest_priority_locks_first() {
        let it = Interceptor::default();
        let _rx = it.begin(subs(&[("a", 10), ("b", 20)]));
        // A 先改写 → 锁定会话
        assert_eq!(it.resolve("a", "url-a"), ResolveOutcome::Accepted);
        // B 改写 → 已被 A 抢先，拒绝
        assert_eq!(it.resolve("b", "url-b"), ResolveOutcome::Superseded);
        assert_eq!(it.finish().as_deref(), Some("url-a"));
    }

    #[test]
    fn lower_rejected_while_higher_still_pending() {
        let it = Interceptor::default();
        let _rx = it.begin(subs(&[("a", 10), ("b", 20)]));
        // A（更高优先级）仍在处理 → 哪怕 B 跑得更快，其改写也被拒
        assert_eq!(it.resolve("b", "url-b"), ResolveOutcome::LowerPriority);
        // A 随后改写 → 成功
        assert_eq!(it.resolve("a", "url-a"), ResolveOutcome::Accepted);
        assert_eq!(it.finish().as_deref(), Some("url-a"));
    }

    #[test]
    fn lower_succeeds_after_higher_completes_without_resolving() {
        let it = Interceptor::default();
        let _rx = it.begin(subs(&[("a", 10), ("b", 20)]));
        assert_eq!(it.resolve("b", "url-b"), ResolveOutcome::LowerPriority);
        // A 完成但**未改写** → 不再阻塞 B
        it.complete("a");
        assert_eq!(it.resolve("b", "url-b"), ResolveOutcome::Accepted);
        assert_eq!(it.finish().as_deref(), Some("url-b"));
    }

    #[test]
    fn same_plugin_may_overwrite_its_own_result() {
        let it = Interceptor::default();
        let _rx = it.begin(subs(&[("a", 10)]));
        assert_eq!(it.resolve("a", "v1"), ResolveOutcome::Accepted);
        assert_eq!(it.resolve("a", "v2"), ResolveOutcome::Accepted);
        assert_eq!(it.finish().as_deref(), Some("v2"));
    }

    #[test]
    fn resolve_after_finish_is_late() {
        let it = Interceptor::default();
        let _rx = it.begin(subs(&[("a", 10)]));
        it.finish();
        assert_eq!(it.resolve("a", "url-a"), ResolveOutcome::Late);
    }

    #[test]
    fn resolve_after_all_completed_is_late() {
        let it = Interceptor::default();
        let _rx = it.begin(subs(&[("a", 10)]));
        // 唯一拦截器完成（未改写）→ `tx` 已消费、会话尚未被 finish 取走
        it.complete("a");
        assert_eq!(it.resolve("a", "url-a"), ResolveOutcome::Late);
    }

    #[test]
    fn complete_converges_only_when_all_report() {
        let it = Interceptor::default();
        let mut rx = it.begin(subs(&[("a", 10), ("b", 20)])).unwrap();
        it.complete("b");
        assert!(rx.try_recv().is_err(), "尚有 A 未完成，不应唤醒宿主");
        it.complete("a");
        assert!(rx.try_recv().is_ok(), "全部完成后应唤醒宿主");
    }
}
