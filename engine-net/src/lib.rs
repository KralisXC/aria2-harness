//! engine-net —— C-3 原生 HTTP(S) 引擎（Phase 0 空壳）。
//!
//! 模块树按 weilai.md §5.3 建立（Phase 1 初始化，各文件在 5.4.2–5.4.7 逐步填充）。
//! 范围见 `docs/c3/Q2-scope.md`、形态见 `docs/c3/Q4-binary.md`。

pub mod backpressure;
pub mod dns;
pub mod download;
pub mod error;
pub mod http;
pub mod log;
pub mod progress;
pub mod rpc;
pub mod task;

/// 引擎标识：service 单入口路由时用于回填 `_engine` 扩展字段。
pub const ENGINE_NAME: &str = "native";

/// engine-net 侧 GID 命名空间前缀（红线 R1：`fffe` + 12 位十六进制）。
pub const GID_PREFIX: u32 = 0xfffe;

/// 本 crate 版本，供 `getVersion` 的 `_engine` 扩展字段使用。
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
