//! 全局错误模型（P2-A）：统一 `Result<T, AppError>`，序列化为
//! `{ "code": "XXX", "message": "YYY" }`，供前端做精细化交互（重试 / 引导 / 分类提示）。
//!
//! **红线：不用 `#[derive(Serialize)]`**。derive 走 externally-tagged 语义，会输出
//! `{"Aria2Rpc":"..."}` 这种"用变体名当 key"的形式，前端拿不到稳定的 `code` /
//! `message` 两字段契约。故手写 `Serialize`，只吐这两个字段。

use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;

/// 统一错误类型。`Debug + Display` 由 `thiserror` 派生，`Serialize` 手写。
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// aria2 WebSocket JSON-RPC 调用失败。
    ///
    /// 可直接 `#[from]`：自研 [`crate::rpc_client::RpcError`] 已实现
    /// `std::error::Error`，且字段全为 `String` / `i64` / `Duration`，
    /// 天然 `Send + Sync`。
    #[error("aria2 RPC 调用失败：{0}")]
    Aria2Rpc(#[from] crate::rpc_client::RpcError),

    /// 引擎尚未连接（RPC 未就绪 / 核心服务未拉起）。
    ///
    /// 文案由调用方给全句并原样透传 —— 既有"请启动 Service"的操作指引就在这句话里，
    /// 加前缀反而会把它埋掉。
    #[error("{0}")]
    Aria2Disconnected(String),

    /// 插件加载失败（manifest 非法 / SHA-256 完整性不过 / 运行时初始化失败）。
    #[error("插件加载失败：{0}")]
    PluginLoad(String),

    /// 插件 hook 执行失败（QuickJS 异常 / Wasm trap / 燃料耗尽）。
    ///
    /// 刻意**不用** `#[from]`：`rquickjs::Error` 含 `UserData(..)` 等内部类型，
    /// Send/Sync 未逐字段核实；而 `wasmtime::Error` 实为 `anyhow::Error`，根本不实现
    /// `std::error::Error`，结构上无法 `#[from]`。统一取字符串，两个后端一致。
    #[error("插件执行失败：{0}")]
    PluginRuntime(String),

    /// 路径越权被拒（`guard_path` 的安全判定），与普通 IO 故障分流，
    /// 便于前端区分"你选错目录了"和"磁盘坏了"。
    #[error("文件访问被拒绝：{0}")]
    FsDenied(String),

    /// 文件系统 IO 失败。带上下文（是哪个路径、在做哪一步）比裸 `#[from]` 有用得多，
    /// 故用两字段结构变体 + [`AppError::fs`] 构造器。
    #[error("文件操作失败：{context}")]
    Fs {
        context: String,
        #[source]
        source: std::io::Error,
    },

    /// 前置拦截（`onBeforeAddDownload`）等待超时 —— **非致命**，宿主已原样放行，
    /// 只作提示，前端可据此给黄标而非红字。
    #[error("插件前置拦截超时（{0} 秒），已原样放行")]
    InterceptTimeout(u64),

    /// 网络层失败：WS 断链、命名管道连接不上。
    #[error("网络连接失败：{0}")]
    Network(String),

    /// 核心服务（`aria2-service`）不可用：连不上 / 握手失败 / 未提权。
    #[error("核心服务不可用：{0}")]
    Service(String),

    /// 调用方传参非法（缺字段 / 类型不符 / 未知命令）。
    #[error("请求参数非法：{0}")]
    InvalidArgs(String),

    /// 安装包含 `exec:local` 的插件前，必须由用户显式信任（rc1 · 模块一）。
    ///
    /// 这是**可恢复**的安全闸门：前端据此弹红色确认框，用户同意后带
    /// `allowExecLocal=true` 重试同一安装包；拒绝则安装中止（staging 已清理）。
    #[error("此插件包含外部可执行文件，是否信任并安装？{0}")]
    ConsentRequired(String),

    /// 序列化 / 反序列化失败。
    #[error("数据序列化失败：{0}")]
    Serde(#[from] serde_json::Error),

    /// 兜底：未归类错误，也承接迁移期尚未改造的裸 `String`。
    #[error("{0}")]
    Internal(String),

    /// SQLite 持久化失败（打开 / 迁移 / 读写）。带上下文（哪张表、哪个 gid）比裸
    /// `#[from]` 有用得多，故与 [`AppError::Fs`] 同款两字段结构。
    ///
    /// 增量 28（P2-B）：`rusqlite::Error` 已实现 `std::error::Error + Send + Sync`
    /// （探针断言），但**刻意不用 `#[from]`** —— 数据库错误几乎总要带"是谁在做哪一步"
    /// 的上下文，否则日志里只剩一句 `database is locked` 无从定位。
    ///
    /// `source` 为 `Option`：schema 版本高于程序（拒绝降级）这类错误没有底层
    /// `rusqlite::Error` 可挂，不该伪造一个。
    #[error("数据库操作失败：{context}")]
    Db {
        context: String,
        #[source]
        source: Option<rusqlite::Error>,
    },
}

impl AppError {
    /// 稳定的机器可读码（前端 `switch (err.code)` 用）。
    ///
    /// 与 `Display` 文案**解耦**：文案可以随时改措辞，`code` 是给程序看的契约。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Aria2Rpc(_) => "ARIA2_RPC",
            Self::Aria2Disconnected(_) => "ARIA2_DISCONNECTED",
            Self::PluginLoad(_) => "PLUGIN_LOAD",
            Self::PluginRuntime(_) => "PLUGIN_RUNTIME",
            Self::FsDenied(_) => "FS_DENIED",
            Self::Fs { .. } => "FS",
            Self::InterceptTimeout(_) => "INTERCEPT_TIMEOUT",
            Self::Network(_) => "NETWORK",
            Self::Service(_) => "SERVICE_UNAVAILABLE",
            Self::InvalidArgs(_) => "INVALID_ARGS",
            Self::ConsentRequired(_) => "PLUGIN_CONSENT_REQUIRED",
            Self::Serde(_) => "SERDE",
            Self::Internal(_) => "INTERNAL",
            Self::Db { .. } => "DB",
        }
    }

    /// `Fs` 构造器：IO 错误几乎总要带路径上下文，故这个变体不设 `#[from]`。
    pub fn fs(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Fs {
            context: context.into(),
            source,
        }
    }

    /// `Db` 构造器（增量 28）：同 [`AppError::fs`]，强制调用方给上下文。
    pub fn db(context: impl Into<String>, source: rusqlite::Error) -> Self {
        Self::Db {
            context: context.into(),
            source: Some(source),
        }
    }
}

impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // 只吐两个字段。任何内部细节（io::ErrorKind、aria2 错误码）都不外泄给前端。
        let mut st = serializer.serialize_struct("AppError", 2)?;
        st.serialize_field("code", self.code())?;
        st.serialize_field("message", &self.to_string())?;
        st.end()
    }
}

/// 迁移桥：把既有 `Result<T, String>` 一路 `?` 上来时自动兜成 [`AppError::Internal`]。
///
/// 这让 59 处改造可以**分文件推进**而不必一次性全改，也让受红线保护的
/// `guard_path` / `interceptor` 保持原签名、仅在调用点转换。
impl From<String> for AppError {
    fn from(message: String) -> Self {
        Self::Internal(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 序列化契约锁定：必须是 `{code, message}` 两字段，而不是 externally-tagged 的
    /// `{"Variant": ".."}`。前端 `ipc.ts` 的 `IpcError` 构造直接依赖这个形状。
    #[test]
    fn serializes_as_code_message_pair() {
        let err = AppError::PluginLoad("manifest.json 缺失".to_string());
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v, json!({ "code": "PLUGIN_LOAD", "message": "插件加载失败：manifest.json 缺失" }));
        assert!(v.get("PluginLoad").is_none(), "不得退化成 externally-tagged");
    }

    /// `#[from]` 通路：`serde_json::Error` 转进来必须是 `Serde` 变体而非兜底。
    #[test]
    fn serde_error_uses_from_conversion() {
        let io = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let err: AppError = io.into();
        assert_eq!(err.code(), "SERDE");
    }

    /// 迁移桥：裸 `String` 走 `?` 时兜成 `Internal`，且**不加**任何前缀
    /// （保证未迁移路径的既有文案一字不改）。
    #[test]
    fn string_bridge_preserves_message_verbatim() {
        let err: AppError = "原有的可读错误".to_string().into();
        assert_eq!(err.code(), "INTERNAL");
        assert_eq!(err.to_string(), "原有的可读错误");
    }

    /// `Fs` 变体保留 io source（`# [source]` 生效），供日志抓底层原因。
    #[test]
    fn fs_variant_keeps_source() {
        use std::error::Error;
        let err = AppError::fs(
            "创建下载目录失败（C:\\x）",
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        );
        assert_eq!(err.code(), "FS");
        assert!(err.to_string().contains("C:\\x"));
        assert!(err.source().is_some());
    }

    /// `AppError` 必须能跨线程（`dispatcher` 的 `sync_channel` 回执、
    /// tokio 任务里跨 `.await` 传播）。
    #[test]
    fn app_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AppError>();
    }
}
