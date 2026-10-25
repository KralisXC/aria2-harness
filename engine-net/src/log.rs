//! 结构化日志：Phase 1 输出 stdout，Phase 2 接 service 的 `data/logs/engine-net.log`。
//!
//! 所有事件统一带 `[engine-net]` 前缀；默认只放本 crate 的 info 与其它 crate 的
//! warn，避免 hickory/hyper 的内部日志污染引擎输出。

use std::fmt;

use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, format};
use tracing_subscriber::registry::LookupSpan;

struct Prefixed;

impl<S, N> FormatEvent<S, N> for Prefixed
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        write!(writer, "[engine-net] ")?;
        format::Format::default()
            .with_target(false)
            .format_event(ctx, writer, event)
    }
}

/// 默认过滤器：本 crate info，其它 warn。
pub const DEFAULT_FILTER: &str = "engine_net=info,warn";

/// 初始化 stdout 日志；重复调用是安全的（已初始化则忽略）。
pub fn init() {
    init_with_filter(DEFAULT_FILTER);
}

/// 用自定义过滤器初始化（例如 `engine_net=debug,warn`）。
pub fn init_with_filter(filter: &str) {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(filter))
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let _ = tracing_subscriber::fmt()
        .event_format(Prefixed)
        .with_env_filter(filter)
        .with_writer(std::io::stdout)
        .try_init();
}
