//! 内部任务 gid 注册表（从 `tracker` 模块抽取，供插件分片任务复用）。
//!
//! 「内部任务」= 宿主/插件为自己派发的下载，不代表用户意图，必须对
//! UI 任务列表、历史库与插件事件总线全部隐身：
//! - `lib.rs::run_ws_loop` 里的通知过滤；
//! - `lib.rs::is_internal_task`（列表/历史落库）；
//! - `plugin::dispatcher::run`（插件原始事件总线）。
//!
//! 进程级 `OnceLock<Mutex<..>>`：gid 在运行期产生，需要可变容器。

use std::collections::HashSet;
use std::sync::{Mutex as StdMutex, OnceLock};

static INTERNAL_GIDS: OnceLock<StdMutex<HashSet<String>>> = OnceLock::new();

fn internal_gids() -> &'static StdMutex<HashSet<String>> {
    INTERNAL_GIDS.get_or_init(|| StdMutex::new(HashSet::new()))
}

/// 该 gid 是否为内部任务（UI / 历史库 / 通知都据此过滤）。
pub fn is_internal(gid: &str) -> bool {
    internal_gids()
        .lock()
        .map(|set| set.contains(gid))
        .unwrap_or(false)
}

/// 登记一个内部任务 gid（派发后**立即**调用，早于任何状态事件）。
pub(crate) fn mark_internal(gid: &str) {
    if let Ok(mut set) = internal_gids().lock() {
        set.insert(gid.to_string());
    }
}

/// 注销内部任务 gid（任务记录清理后调用，避免集合无限增长）。
pub(crate) fn unmark_internal(gid: &str) {
    if let Ok(mut set) = internal_gids().lock() {
        set.remove(gid);
    }
}

#[cfg(test)]
mod tests {
    use super::{is_internal, mark_internal, unmark_internal};

    #[test]
    fn mark_and_unmark_isolate_ui() {
        mark_internal("gid-internal-shared");
        assert!(is_internal("gid-internal-shared"));
        assert!(!is_internal("gid-user-task"));
        unmark_internal("gid-internal-shared");
        assert!(!is_internal("gid-internal-shared"));
    }
}
