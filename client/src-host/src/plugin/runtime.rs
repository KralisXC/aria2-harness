//! 插件运行时抽象（增量 25 · P2-D）。
//!
//! ## 懒加载红线（beta11 · A3）
//!
//! `load_source` 只接收入口产物文本/字节，**运行时不得预读**插件目录下的
//! `bin/`、`models/`。这类大文件只允许在 `ctx.exec` 或插件显式文件读取时按路径加载。
//!
//! 双子后端共用的最小接口：
//! - `QuickJsRuntime`（逻辑沙箱，默认后端）
//! - `WasmRuntime`（Wasmtime 强隔离后端，由 `wasm-runtime` feature 门控）
//!
//! ## 为什么方法名不叫 `load` / `call_hook` / `call_shutdown`
//!
//! 这三个名字**已经是 `QuickJsRuntime` 的固有方法**。若 trait 直接沿用它们，
//! `impl PluginRuntime for QuickJsRuntime` 就会与固有方法同形，可读性极差、也容易
//! 在调用点产生歧义。因此这里刻意取 `load_source` / `invoke` / `shutdown`：
//! → `impl` 块可以写在本文件里，**`quickjs.rs` 一个字符都不用动**（增量 25 红线 1）。
//!
//! ## 为什么统一到 `&mut self`
//!
//! `QuickJsRuntime` 内部靠 `rquickjs::Context::with(&self)` 的 interior mutability，
//! 全部方法都是 `&self`；而 Wasmtime 必须 `&mut Store` 才能执行。trait 统一到
//! `&mut self` 对 QuickJS 无害（转发即可），对 Wasm 必需。

use std::path::Path;

use serde_json::Value;

use crate::error::AppError;

use super::hostapi::PluginHostContext;
use super::manifest::{self, Manifest};

/// 插件运行时后端接口。
///
/// 契约：`invoke` 返回 `Err` 即视为**一次 hook 失败**，由 `manager::plugin_thread`
/// 做「连续失败计数 → 熔断」判定。双后端共用同一套熔断语义，故 Wasm 侧也应把
/// 插件内部异常编码成 `Err(String)`（而不是 panic）。
pub trait PluginRuntime {
    /// 加载入口产物：JS 传 UTF-8 文本，Wasm 传二进制（或 WAT 文本）。
    fn load_source(&mut self, source: &[u8]) -> Result<(), AppError>;

    /// 同步执行一次 hook；载荷为 POD JSON。
    fn invoke(&mut self, hook: &str, payload: &Value) -> Result<(), AppError>;

    /// 以指定预算执行 hook，并返回其 JSON 返回值（beta10 任务路由 / 长任务使用）。
    ///
    /// 默认实现丢弃返回值；QuickJS 覆盖为带预算的 `call_hook_with_budget`。
    fn invoke_with_budget(
        &mut self,
        hook: &str,
        payload: &Value,
        _budget_ms: u64,
    ) -> Result<Option<Value>, AppError> {
        self.invoke(hook, payload)?;
        Ok(None)
    }

    /// 退出前的尽力而为清理（绝不因异常打断退出流程）。
    fn shutdown(&mut self);
}

/// QuickJS 的适配实现 —— **不触碰 `quickjs.rs`**。
impl PluginRuntime for super::quickjs::QuickJsRuntime {
    fn load_source(&mut self, source: &[u8]) -> Result<(), AppError> {
        let text = std::str::from_utf8(source).map_err(|e| {
            AppError::PluginLoad(format!("插件入口不是合法 UTF-8，无法作为 JS 脚本加载：{e}"))
        })?;
        self.load(text).map_err(AppError::from)
    }

    fn invoke(&mut self, hook: &str, payload: &Value) -> Result<(), AppError> {
        self.call_hook(hook, payload).map_err(AppError::from)
    }

    fn invoke_with_budget(
        &mut self,
        hook: &str,
        payload: &Value,
        budget_ms: u64,
    ) -> Result<Option<Value>, AppError> {
        self.call_hook_with_budget(hook, payload, budget_ms)
            .map_err(AppError::from)
    }

    fn shutdown(&mut self) {
        self.call_shutdown();
    }
}

/// 按 `manifest.engine` 构造运行时（**唯一的后端分流点**）。
///
/// 分流放在这里而不是 `plugin_thread` 里，是为了让 `manager.rs` 的横切逻辑
/// （热重载 / 熔断 / 拦截回执）保持**单一**，不因新增后端而分叉。
pub fn build(
    manifest: &Manifest,
    host: PluginHostContext,
) -> Result<Box<dyn PluginRuntime>, AppError> {
    match manifest.engine.as_str() {
        manifest::ENGINE_JS => {
            let runtime =
                super::quickjs::QuickJsRuntime::new(manifest, host).map_err(AppError::from)?;
            Ok(Box::new(runtime))
        }
        manifest::ENGINE_WASM => build_wasm(manifest, host),
        other => Err(AppError::PluginLoad(format!(
            "不支持的 engine：`{other}`（仅支持 `{}` / `{}`）",
            manifest::ENGINE_JS,
            manifest::ENGINE_WASM
        ))),
    }
}

/// 带 `wasm-runtime` feature 时，走真正的 Wasmtime 后端。
#[cfg(feature = "wasm-runtime")]
fn build_wasm(
    manifest: &Manifest,
    host: PluginHostContext,
) -> Result<Box<dyn PluginRuntime>, AppError> {
    let runtime = super::wasm_runtime::WasmRuntime::new(manifest, host).map_err(AppError::from)?;
    Ok(Box::new(runtime))
}

/// 未带 `wasm-runtime` feature 时，给出**明确**的拒绝原因（而非静默跳过或 panic）。
#[cfg(not(feature = "wasm-runtime"))]
fn build_wasm(
    _manifest: &Manifest,
    _host: PluginHostContext,
) -> Result<Box<dyn PluginRuntime>, AppError> {
    Err(AppError::PluginLoad(
        "未启用 wasm-runtime feature，无法加载 Wasm 插件；\
         请以 `cargo build --features wasm-runtime` 构建后再试"
            .to_string(),
    ))
}

/// 按后端读取插件入口产物。
///
/// **关键**：`.wasm` 是二进制，绝不能走 `read_to_string` —— 非 UTF-8 字节会让它
/// 直接报错，且错误信息会伪装成「读取入口文件失败」，极易误导排查。
pub fn read_entry(engine: &str, path: &Path) -> Result<Vec<u8>, AppError> {
    let err =
        |e: std::io::Error| AppError::fs(format!("读取入口文件失败（{}）", path.display()), e);
    if engine == manifest::ENGINE_WASM {
        std::fs::read(path).map_err(err)
    } else {
        std::fs::read_to_string(path)
            .map_err(err)
            .map(String::into_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_entry_reads_binary_for_wasm_and_text_for_js() {
        let dir = tempfile::tempdir().unwrap();
        let js = dir.path().join("main.js");
        let wasm = dir.path().join("plugin.wasm");

        std::fs::write(&js, "harness.hooks.onAppStart = function(){};").unwrap();
        // 故意写入非 UTF-8 字节：这正是 read_to_string 会失败的场景
        std::fs::write(&wasm, [0x00u8, 0x61, 0x73, 0x6d, 0xff, 0xfe]).unwrap();

        let js_bytes = read_entry(manifest::ENGINE_JS, &js).unwrap();
        assert_eq!(js_bytes, b"harness.hooks.onAppStart = function(){};");

        let wasm_bytes = read_entry(manifest::ENGINE_WASM, &wasm).unwrap();
        assert_eq!(wasm_bytes, vec![0x00u8, 0x61, 0x73, 0x6d, 0xff, 0xfe]);

        // 交叉验证：二进制产物走 JS 分支必须失败（防止以后误改回 read_to_string）
        assert!(
            read_entry(manifest::ENGINE_JS, &wasm).is_err(),
            "二进制产物走文本读取必须报错"
        );
    }
}
