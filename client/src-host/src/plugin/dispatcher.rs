//! HookDispatcher：把引擎事件（自研 RPC 客户端的 broadcast）分发到各插件的有界队列。
//!
//! 红线对齐：宿主侧只做 `try_send`，**永不阻塞**；队列满则丢弃并计数。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex as StdMutex};

use crate::rpc_client::Notification;
use serde_json::{json, Value};

use crate::error::AppError;

/// 前置拦截 hook：在 URL 提交给引擎**之前**广播，允许插件改写 URL。
///
/// 由 `add_download` 主动调用 `dispatch` 触发（非引擎事件驱动），
/// 因此不参与 `hook_for_notification` 的映射。
pub const HOOK_BEFORE_ADD: &str = "onBeforeAddDownload";

/// 投递到插件专属线程的任务。
pub enum PluginJob {
    /// 加载入口产物并校验 hook 注册情况，结果经 `reply` 回传。
    ///
    /// 载荷为**字节**（增量 25）：JS 是 UTF-8 文本，Wasm 是二进制 —— 用 `String`
    /// 会让 `.wasm` 在读取阶段就因非 UTF-8 而失败。
    Load {
        source: Vec<u8>,
        reply: SyncSender<Result<(), AppError>>,
    },
    /// 一次 hook 调用（载荷为 POD JSON）。
    Hook { hook: String, payload: Value },
    /// 长任务 hook（`onTaskRun`）：使用 manifest 的 `taskTimeoutMs` 预算。
    TaskHook {
        hook: String,
        payload: Value,
        budget_ms: u64,
    },
    /// 需要同步回执的 hook（`onTaskDetected`）：插件返回的 `{claim:true/false}` 经 `reply` 回传。
    Detect {
        hook: String,
        payload: Value,
        budget_ms: u64,
        reply: SyncSender<Result<Option<Value>, String>>,
    },
    /// 优雅退出（触发 onShutdown 后线程结束）。
    Shutdown,
}

/// HookDispatcher 的订阅者快照（按 priority 升序派发）。
#[derive(Clone)]
pub struct Subscriber {
    pub plugin_id: String,
    pub priority: i64,
    /// manifest 声明的 hook 集合，用于快速过滤。
    pub hooks: Arc<HashSet<String>>,
    /// 运行时启停开关；关闭时派发阶段直接跳过该插件。
    pub enabled: Arc<AtomicBool>,
    /// 懒激活状态（beta12 · 模块 E）：声明 `activationEvents` 的插件在全息探针
    /// 命中域名前保持休眠，不接收任何 hook。
    pub activated: Arc<AtomicBool>,
    pub tx: SyncSender<PluginJob>,
}

/// 把引擎通知映射为 (hook 名, gid)。
pub fn hook_for_notification(msg: &Notification) -> Option<(&'static str, String)> {
    match msg {
        Notification::Aria2 { gid, event } => {
            let hook = match event {
                crate::rpc_client::Event::Start => "onDownloadStart",
                crate::rpc_client::Event::Pause => "onDownloadPause",
                crate::rpc_client::Event::Stop => "onDownloadStop",
                crate::rpc_client::Event::Complete => "onDownloadComplete",
                crate::rpc_client::Event::Error => "onDownloadError",
                crate::rpc_client::Event::BtComplete => "onBtComplete",
            };
            Some((hook, gid.clone()))
        }
        _ => None,
    }
}

/// 常驻任务：消费第二个 broadcast Receiver，把事件派发给插件。
///
/// `subs` 为**共享订阅者单元**（非启动快照）：每轮循环读取最新列表，
/// 因此热重载（增量 21）替换 / 新增的插件实例**即时可见**。
pub async fn run(
    mut rx: tokio::sync::broadcast::Receiver<Notification>,
    subs: Arc<StdMutex<Vec<Subscriber>>>,
) {
    loop {
        match rx.recv().await {
            Ok(msg) => {
                if let Some((hook, gid)) = hook_for_notification(&msg) {
                    // 增量 29（P1-G）：内部任务（tracker 自拉）对插件同样隐身。
                    // 这条总线是**原始广播**，不经过 lib.rs 通知循环里的过滤 —— 若不在此
                    // 拦截，tracker 拉取失败（如 GitHub 502 / 超时）会以 `onDownloadError`
                    // 派发给所有订阅插件，看起来像用户的下载任务失败了。
                    if crate::tracker::is_internal(&gid) {
                        continue;
                    }
                    // 仅在非阻塞 `try_send` 期间短持锁；`try_send` 不阻塞，
                    // 故不会把事件循环卡住（重载侧同样只会短暂等待该锁）。
                    let guard = subs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    dispatch(&guard, hook, json!({ "hook": hook, "gid": gid }));
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                eprintln!("[plugin] 事件总线落后 {n} 条，已跳过");
                // 审计留痕（非阻塞）。
                super::audit::log_lagged(n);
                // 关键：**继续循环，绝不 break** —— 落后只是"跳过若干条旧消息"，
                // 通道本身仍然可用；若在此 break，插件将永久收不到任何后续事件。
            }
        }
    }
}

/// 对一批订阅者按 hook 过滤后做非阻塞投递。
pub fn dispatch(subs: &[Subscriber], hook: &str, payload: Value) {
    for sub in subs {
        // 运行时开关：被禁用的插件不派发（挂起语义，线程仍存活但不再收到事件）。
        if !sub.enabled.load(Ordering::Relaxed) {
            continue;
        }
        // beta12 · 模块 E：休眠插件不接收任何 hook，直到全息探针命中其域名。
        if !sub.activated.load(Ordering::Relaxed) {
            continue;
        }
        if !sub.hooks.contains(hook) {
            continue;
        }
        match sub.tx.try_send(PluginJob::Hook {
            hook: hook.to_string(),
            payload: payload.clone(),
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                // 非阻塞审计：写盘动作交给独立线程，事件循环绝不因日志卡顿。
                eprintln!("[plugin:{}] 队列已满，丢弃事件 {hook}", sub.plugin_id);
                super::audit::log_dropped_event(&sub.plugin_id, hook);
                crate::metrics::METRICS
                    .plugin_dropped
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}
