//! 系统休眠 / 网络切换感知（Windows，增量 40）。
//!
//! 目标：笔记本合盖、系统睡眠、Wi-Fi 切换后，下载能"无感自愈"。
//!
//! 三条红线：
//! 1. 系统通知回调运行在**系统线程**上，只允许做一次非阻塞 `send()`，
//!    绝不做 IO / RPC / 拿锁 —— 否则会拖住电源状态切换；
//! 2. 所有 RPC 动作都在 tokio 消费任务里执行，每一步都有明确超时上界
//!    （休眠事件留给应用的处理窗口只有约 2 秒）；
//! 3. `aria2.unpauseAll` 会恢复**所有**暂停任务，包括用户手动暂停的。
//!    因此休眠前必须先快照 `status == "paused"` 的 gid，唤醒后重新暂停。

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::IpHelper::{
    CancelMibChangeNotify2, NotifyNetworkConnectivityHintChange,
};
use windows::Win32::Networking::WinSock::{
    NetworkConnectivityLevelHintInternetAccess, NL_NETWORK_CONNECTIVITY_HINT,
};
use windows::Win32::System::Power::{
    PowerRegisterSuspendResumeNotification, PowerUnregisterSuspendResumeNotification,
    DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND,
};

use crate::rpc_client::Client;

/// 快照手动暂停列表的上界：引擎挂死时也不能拖住休眠流程。
const SNAPSHOT_TIMEOUT: Duration = Duration::from_millis(300);
/// 休眠前 saveSession 的上界。
const SAVE_SESSION_TIMEOUT: Duration = Duration::from_millis(500);
/// 休眠前 forcePauseAll 的上界。
const PAUSE_ALL_TIMEOUT: Duration = Duration::from_millis(400);
/// 唤醒后恢复流程的上界。
const RESUME_TIMEOUT: Duration = Duration::from_millis(2000);
/// 恢复单个手动暂停任务的上界。
const REPAUSE_TIMEOUT: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, Debug)]
enum SystemEvent {
    /// 系统即将休眠（PBT_APMSUSPEND）。
    Suspend,
    /// 系统已唤醒（PBT_APMRESUMESUSPEND / PBT_APMRESUMEAUTOMATIC）。
    Resume,
    /// 网络连通性恢复。
    NetworkRestored,
}

/// 回调 → 消费任务的事件通道（无界但只在电源/网络事件时投递，量极小）。
static TX: OnceLock<UnboundedSender<SystemEvent>> = OnceLock::new();
/// `start()` 幂等保护。
static STARTED: AtomicBool = AtomicBool::new(false);
/// 当前是否处于"已休眠 / 等待恢复"状态。
static SUSPENDED: AtomicBool = AtomicBool::new(false);
/// 休眠前用户手动暂停的 gid 快照。
static PAUSED_SNAPSHOT: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// 注册句柄：(电源通知句柄, 网络通知句柄原始值)。
/// `HANDLE` 不是 `Send`，所以网络句柄以 `usize` 存储。
static HANDLES: Mutex<(Option<HPOWERNOTIFY>, Option<usize>)> = Mutex::new((None, None));

/// 启动监听：注册系统回调 + 拉起 tokio 消费任务。必须在 `bootstrap` 里调用一次。
pub fn start() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let (tx, rx) = unbounded_channel();
    if TX.set(tx).is_err() {
        return;
    }
    // Safety: 注册只发生在启动阶段；回调函数与泄漏的订阅参数在进程生命周期内一直有效。
    unsafe {
        register_callbacks();
    }
    crate::rt().spawn(consume(rx));
}

/// 停止监听（进程退出时调用；句柄注销失败只记录，不阻断退出）。
pub fn stop() {
    let (power, network) = {
        let mut guard = HANDLES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        (guard.0.take(), guard.1.take())
    };
    if let Some(handle) = power {
        // Safety: 句柄由本模块注册，且只注销一次。
        let rc = unsafe { PowerUnregisterSuspendResumeNotification(handle) };
        if rc.0 != 0 {
            tracing::warn!("[power] 注销休眠通知失败：Win32 错误码 {}", rc.0);
        }
    }
    if let Some(raw) = network {
        // Safety: 句柄来自 NotifyNetworkConnectivityHintChange，且只注销一次。
        let rc = unsafe { CancelMibChangeNotify2(HANDLE(raw as *mut c_void)) };
        if rc.0 != 0 {
            tracing::warn!("[power] 注销网络通知失败：Win32 错误码 {}", rc.0);
        }
    }
}

unsafe fn register_callbacks() {
    // 回调模式要求 hRecipient 指向 DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS，且该结构
    // 在注销前必须一直有效 —— 这里有意泄漏到进程结束。
    let params = Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
        Callback: Some(suspend_callback),
        Context: std::ptr::null_mut(),
    }));

    let mut raw_handle: *mut c_void = std::ptr::null_mut();
    // Safety: `params` 泄漏到进程结束，注册期间始终有效。
    let rc = unsafe {
        PowerRegisterSuspendResumeNotification(
            DEVICE_NOTIFY_CALLBACK,
            HANDLE(params as *mut _ as *mut c_void),
            &mut raw_handle,
        )
    };
    if rc.0 == 0 {
        let handle = HPOWERNOTIFY(raw_handle as isize);
        HANDLES.lock().unwrap_or_else(|p| p.into_inner()).0 = Some(handle);
    }
    else {
        tracing::warn!(
            "[power] 注册休眠/唤醒通知失败（Win32 错误码 {}），休眠自愈能力降级",
            rc.0
        );
    }

    let mut network_handle = HANDLE(std::ptr::null_mut());
    // Safety: 回调与句柄生命周期覆盖进程整个运行期。
    let rc = unsafe {
        NotifyNetworkConnectivityHintChange(
            Some(network_callback),
            None,
            false,
            &mut network_handle,
        )
    };
    if rc.0 == 0 {
        HANDLES.lock().unwrap_or_else(|p| p.into_inner()).1 =
            Some(network_handle.0 as usize);
    }
    else {
        tracing::warn!(
            "[power] 注册网络变化通知失败（Win32 错误码 {}），网络自愈能力降级",
            rc.0
        );
    }
}

/// 电源事件回调：只投递事件，立即返回。
unsafe extern "system" fn suspend_callback(
    _context: *const c_void,
    event_type: u32,
    _setting: *const c_void,
) -> u32 {
    let event = match event_type {
        PBT_APMSUSPEND => Some(SystemEvent::Suspend),
        PBT_APMRESUMESUSPEND | PBT_APMRESUMEAUTOMATIC => Some(SystemEvent::Resume),
        _ => None,
    };
    if let Some(event) = event {
        if let Some(tx) = TX.get() {
            let _ = tx.send(event);
        }
    }
    // ERROR_SUCCESS
    0
}

/// 网络连通性回调：只投递事件，立即返回。
unsafe extern "system" fn network_callback(
    _context: *const c_void,
    hint: NL_NETWORK_CONNECTIVITY_HINT,
) {
    if hint.ConnectivityLevel == NetworkConnectivityLevelHintInternetAccess {
        if let Some(tx) = TX.get() {
            let _ = tx.send(SystemEvent::NetworkRestored);
        }
    }
}

async fn consume(mut rx: UnboundedReceiver<SystemEvent>) {
    let mut last_network_recovery: Option<Instant> = None;
    while let Some(event) = rx.recv().await {
        match event {
            SystemEvent::Suspend => {
                SUSPENDED.store(true, Ordering::SeqCst);
                on_suspend().await;
            }
            SystemEvent::Resume => {
                SUSPENDED.store(false, Ordering::SeqCst);
                on_resume("resume").await;
            }
            SystemEvent::NetworkRestored => {
                // 正常网络切换不调用 unpauseAll：那会误恢复用户手动暂停的任务，
                // 底层连接重建交给 aria2 自身重试 + RPC 客户端指数退避。
                // 只有"休眠中网络恢复"才补一次完整自愈。
                if !SUSPENDED.swap(false, Ordering::SeqCst) {
                    emit("system://network-restored", json!({ "at": epoch_millis() }));
                    continue;
                }
                let now = Instant::now();
                if let Some(last) = last_network_recovery {
                    if now.duration_since(last) < Duration::from_secs(3) {
                        continue;
                    }
                }
                last_network_recovery = Some(now);
                on_resume("network").await;
            }
        }
    }
}

/// 休眠前：快照手动暂停项 → saveSession 落盘 → forcePauseAll 立即断连。
async fn on_suspend() {
    emit("system://suspend", json!({ "at": epoch_millis() }));
    let Some(client) = client_snapshot().await else {
        tracing::warn!("[power] 休眠：RPC 客户端未就绪，跳过落盘与断连");
        return;
    };

    // 顺序不可交换：必须在 forcePauseAll 之前记录"用户手动暂停"的 gid。
    let paused = snapshot_paused(&client).await;
    let paused_count = paused.len();
    *PAUSED_SNAPSHOT.lock().unwrap_or_else(|p| p.into_inner()) = paused;

    match tokio::time::timeout(SAVE_SESSION_TIMEOUT, client.save_session()).await {
        Ok(Ok(())) => tracing::info!("[power] 休眠前 saveSession 完成"),
        Ok(Err(err)) => tracing::warn!("[power] 休眠前 saveSession 失败：{err}"),
        Err(_) => tracing::warn!("[power] 休眠前 saveSession 超时"),
    }
    match tokio::time::timeout(PAUSE_ALL_TIMEOUT, client.force_pause_all()).await {
        Ok(Ok(())) => tracing::info!(
            "[power] 休眠前 forcePauseAll 完成（连接已断开，手动暂停项 {} 个）",
            paused_count
        ),
        Ok(Err(err)) => tracing::warn!("[power] 休眠前 forcePauseAll 失败：{err}"),
        Err(_) => tracing::warn!("[power] 休眠前 forcePauseAll 超时"),
    }
}

/// 唤醒后：unpauseAll 自愈 → 重新暂停用户手动暂停的任务。
async fn on_resume(reason: &'static str) {
    emit(
        "system://resumed",
        json!({ "reason": reason, "at": epoch_millis() }),
    );
    let Some(client) = client_snapshot().await else {
        tracing::warn!("[power] 恢复：RPC 客户端未就绪，等待自动重连");
        return;
    };
    match tokio::time::timeout(RESUME_TIMEOUT, client.unpause_all()).await {
        Ok(Ok(())) => tracing::info!("[power] 恢复：unpauseAll 完成"),
        Ok(Err(err)) => tracing::warn!("[power] 恢复：unpauseAll 失败：{err}"),
        Err(_) => tracing::warn!("[power] 恢复：unpauseAll 超时"),
    }

    let paused = std::mem::take(&mut *PAUSED_SNAPSHOT.lock().unwrap_or_else(|p| p.into_inner()));
    for gid in paused {
        match tokio::time::timeout(REPAUSE_TIMEOUT, client.force_pause(&gid)).await {
            Ok(Ok(())) => tracing::debug!("[power] 恢复：重新暂停用户手动暂停任务 {gid}"),
            Ok(Err(err)) => {
                tracing::warn!("[power] 恢复：重新暂停 {gid} 失败：{err}");
            }
            Err(_) => tracing::warn!("[power] 恢复：重新暂停 {gid} 超时"),
        }
    }
}

/// 读取 cancelled=false 的等待队列，取出 `status == "paused"` 的 gid。
async fn snapshot_paused(client: &Client) -> Vec<String> {
    let keys = Some(vec!["gid".to_string(), "status".to_string()]);
    match tokio::time::timeout(
        SNAPSHOT_TIMEOUT,
        client.tell_waiting(0, 1000, keys),
    )
    .await
    {
        Ok(Ok(items)) => items
            .into_iter()
            .filter(|item| item.get("status").and_then(Value::as_str) == Some("paused"))
            .filter_map(|item| {
                item.get("gid")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect(),
        Ok(Err(err)) => {
            tracing::warn!("[power] 快照暂停任务失败：{err}");
            Vec::new()
        }
        Err(_) => {
            tracing::warn!("[power] 快照暂停任务超时");
            Vec::new()
        }
    }
}

async fn client_snapshot() -> Option<Client> {
    crate::state().client.lock().await.clone()
}

fn emit(event: &str, payload: Value) {
    if let Some(host) = crate::host::host() {
        // 退出期窗口可能已销毁：宿主未注入或前端已退出时静默丢弃。
        if host.frontend_alive() {
            host.emit(event, payload);
        }
    }
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
