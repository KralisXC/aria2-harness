//! 阶段三：插件系统（Harness）宿主侧骨架（M1）。
//!
//! 模块划分：
//! - `manifest`   manifest.json 解析 / 校验 / SHA-256 完整性
//! - `hostapi`    能力令牌 + 宿主桥（ctx 能力的 Rust 侧实现）
//! - `quickjs`    QuickJsRuntime（QuickJS 逻辑沙箱，专属线程内持有）
//! - `runtime`    PluginRuntime 抽象层 + 后端分流（增量 25）
//! - `wasm_runtime` Wasmtime 强隔离后端（增量 25 · `wasm-runtime` feature 门控）
//! - `dispatcher` 引擎事件 -> Hook 的有界队列分发
//! - `interceptor` add_download 前置拦截会话（跨线程 URL 改写汇合点）
//! - `registry`   插件授权与启用状态持久化（plugins/registry.json）
//! - `audit`      事件丢弃审计日志（异步写盘，绝不阻塞事件循环）
//! - `manager`    PluginManager：扫描目录 / 加载 / 生命周期
//!
//! 红线对齐：QuickJS 的 `Runtime` / `Context` 均为 `!Send`，因此每个插件独占一条
//! 专属线程，运行时绝不跨线程；宿主 -> 插件只传 POD JSON，绝不传引擎句柄。

pub mod audit;
pub mod dispatcher;
pub mod hostapi;
pub mod interceptor;
pub mod manager;
pub mod manifest;
pub mod quickjs;
pub mod registry;
/// 运行时抽象层（增量 25）：`PluginRuntime` trait + 后端分流。
pub mod runtime;
/// Wasmtime 强隔离后端（增量 25 · P2-D）。由 `wasm-runtime` feature 门控：
/// 未开启时不参与编译，manifest 声明 `"engine": "wasm"` 会得到明确的拒绝原因。
#[cfg(feature = "wasm-runtime")]
pub mod wasm_runtime;

pub use manager::PluginManager;

/// 插件根目录名（扫描时挂在 `paths::plugins_dir()` 下；开发期 = 仓库根下的 `plugins`）。
pub fn plugin_root() -> std::path::PathBuf {
    crate::paths::plugins_dir()
}

/// 每个插件专属线程的有界队列容量（满则丢弃事件，绝不阻塞宿主事件循环）。
pub const QUEUE_CAP: usize = 256;
