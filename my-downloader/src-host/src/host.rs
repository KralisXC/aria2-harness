//! 宿主抽象层：把插件系统与具体窗口框架（tao / wry）解耦。
//!
//! 插件侧只依赖 [`HostContext`] 三项能力：向前端投递事件、探测前端是否存活、
//! 唤回主窗口。由外壳（`main.rs` 的 wry 实现）在启动时经 [`install_host`]
//! 注入进程级单例，于是 `plugin/` 下不再出现任何框架句柄（如 `tauri::AppHandle`）。

use std::sync::{Arc, OnceLock};

use serde_json::Value;

/// 宿主上下文（插件系统与 UI 之间的唯一出口）。
pub trait HostContext: Send + Sync + 'static {
    /// 向 UI 派发一条事件；`event` 为前端 `listen` 订阅的名字。
    fn emit(&self, event: &str, payload: Value);
    /// 前端窗口是否仍存活（退出期返回 false，避免向已销毁窗口投递消息）。
    fn frontend_alive(&self) -> bool;
    /// 唤回主窗口：已最小化就还原，已销毁就重建。
    ///
    /// 浏览器扩展投递任务时调用 —— 用户可能正在托盘态挂机，必须把确认弹窗
    /// 送到眼前；窗口已销毁时重建后由前端 mount 时的暂存查询补回数据。
    fn show_window(&self);
}

/// 进程级宿主槽位（`OnceLock`：只在启动时注入一次，终身不变）。
static HOST: OnceLock<Arc<dyn HostContext>> = OnceLock::new();

/// 注入宿主实现（幂等：重复注入只保留首个）。
pub fn install_host(host: Arc<dyn HostContext>) {
    let _ = HOST.set(host);
}

/// 取宿主实现；尚未注入时返回 `None`。
pub fn host() -> Option<Arc<dyn HostContext>> {
    HOST.get().cloned()
}
