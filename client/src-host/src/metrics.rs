//! 可观测性底座（增量 18 · P0-C′）。
//!
//! 两块内容：
//! 1. [`AppMetrics`]：全局**零锁**计数器（`AtomicU64` + `Relaxed`），埋在关键路径上，
//!    供前端 `get_metrics` 命令拉取。红线：绝不引入 `Mutex`，埋点路径零阻塞。
//! 2. [`init_tracing`]：`tracing` 基础设施初始化（终端 stdout + 按天轮转落盘）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// 应用日志目录（与插件审计日志同目录，独立于源码目录）；随便携包 HOME 走。
fn log_dir() -> std::path::PathBuf {
    crate::paths::logs_dir()
}
/// 应用日志文件名（按天轮转，产物形如 `app.log.2026-10-18`）。
const LOG_FILE: &str = "app.log";
/// 轮转日志保留天数：超出即删除（含 `app.log.*` 与 `plugin-audit.log.*`）。
const LOG_RETENTION_DAYS: u64 = 7;

/// 关键路径计数器。全部为原子量，读写均无需加锁。
#[derive(Default)]
pub struct AppMetrics {
    /// 实际下发到引擎的 RPC 调用次数（`get_tasks` 记为 3 次，而非 1 次）。
    pub rpc_calls: AtomicU64,
    /// 上述 RPC 的失败次数。
    pub rpc_errors: AtomicU64,
    /// 插件越权被拒（`audit_deny`）次数。
    pub plugin_deny: AtomicU64,
    /// 插件事件队列满导致的丢弃次数。
    pub plugin_dropped: AtomicU64,
    /// 前置拦截等待插件超时（> `INTERCEPT_TIMEOUT`）次数。
    pub intercept_timeout: AtomicU64,
}

/// 全局单例。五个字段都以 `AtomicU64::new(0)` 初始化 —— 该调用是 `const` 的，
/// 因此直接 `static` 即可，无需 `once_cell`，也无需任何锁。
pub static METRICS: AppMetrics = AppMetrics {
    rpc_calls: AtomicU64::new(0),
    rpc_errors: AtomicU64::new(0),
    plugin_deny: AtomicU64::new(0),
    plugin_dropped: AtomicU64::new(0),
    intercept_timeout: AtomicU64::new(0),
};

/// 计数器快照（前端契约：五个 `u64` 字段）。
pub fn snapshot() -> serde_json::Value {
    serde_json::json!({
        "rpc_calls": METRICS.rpc_calls.load(Ordering::Relaxed),
        "rpc_errors": METRICS.rpc_errors.load(Ordering::Relaxed),
        "plugin_deny": METRICS.plugin_deny.load(Ordering::Relaxed),
        "plugin_dropped": METRICS.plugin_dropped.load(Ordering::Relaxed),
        "intercept_timeout": METRICS.intercept_timeout.load(Ordering::Relaxed),
    })
}

/// `tracing-appender` 的非阻塞写盘守卫。
///
/// **必须常驻**：`WorkerGuard` 一旦被 drop，后台写盘线程随即结束，`app.log` 会
/// 静默停止写入（且不报任何错）。因此把它钉在 `static` 上，生命周期 = 进程生命周期。
static LOG_GUARD: OnceLock<tracing_appender::non_blocking::WorkerGuard> = OnceLock::new();

/// 初始化日志：终端（stdout） + 文件（按天轮转 `<HOME>\logs\app.log`）。
///
/// 级别由环境变量 `RUST_LOG` 控制；未设置时默认 `info`。
/// 使用 `try_init` 而非 `init`，重复初始化只会静默失败，不会 panic（测试友好）。
pub fn init_tracing() {
    let dir = log_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("[tracing] 创建日志目录失败（{}）：{e}", dir.display());
    }

    // 空间约束：按天切割的归档只保留最近 7 天，启动时先清一次（成本 = 一次目录遍历）。
    purge_old_logs(&dir);

    let appender = tracing_appender::rolling::daily(&dir, LOG_FILE);
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);
    // 保留：`OnceLock` 幂等写入 —— 重复初始化时 set 返回 Err（旧 guard 保留），属预期。
    let _ = LOG_GUARD.set(guard);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false),
        )
        .try_init();

    // 落盘基线：写一条启动事件，确保 `app.log` 非空、便于运行期验证。
    tracing::info!(dir = %dir.display(), "tracing 底座已就绪");
}

/// 清理超过保留期的轮转日志（`app.log.*` / `plugin-audit.log.*`）。
///
/// 只删除**带日期后缀的归档**，正在写入的 `app.log` / `plugin-audit.log` 绝不碰：
/// 它们由各自的 writer 持有句柄，这里删了会造成后续写入落在"已删除文件"上。
/// 任何单文件失败只记 eprintln，不阻断启动。
fn purge_old_logs(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let max_age = std::time::Duration::from_secs(LOG_RETENTION_DAYS * 86_400);

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let rotated = name.starts_with("app.log.") || name.starts_with("plugin-audit.log.");
        if !rotated {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        // 未来时间戳（时钟回拨）按"未过期"处理。
        let age = now.duration_since(modified).unwrap_or_default();
        if age <= max_age {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => eprintln!("[tracing] 已清理过期日志：{name}"),
            Err(e) => eprintln!("[tracing] 清理过期日志失败（{name}）：{e}"),
        }
    }
}
