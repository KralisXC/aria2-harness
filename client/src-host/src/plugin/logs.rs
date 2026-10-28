//! 插件日志环形缓冲（beta11 · B3）。
//!
//! 插件 `ctx.log` / `console.log` 的输出除了写宿主控制台，还需要实时进入前端的
//! 「日志控制台」。本模块提供进程内环形缓冲：
//! - 每插件最多 500 条，最老淘汰；
//! - 最多保留 64 个插件的日志，超限淘汰**最久未写入**的插件；
//! - `level` 归一为 `debug` / `info` / `warn` / `error`（未知值一律降级 `info`）。
//!
//! 只做内存缓冲，不落盘：日志是调试视图，重启即清空。

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// 单插件保留的日志条数上限。
const MAX_ENTRIES: usize = 500;
/// 同时保留日志的插件数上限。
const MAX_PLUGINS: usize = 64;

/// 进入前端日志控制台的一条记录。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginLogEntry {
    /// 毫秒时间戳（Unix epoch）。
    pub ts: u64,
    /// 归一化后的级别：`debug` / `info` / `warn` / `error`。
    pub level: String,
    pub msg: String,
}

static LOGS: OnceLock<Mutex<HashMap<String, VecDeque<PluginLogEntry>>>> = OnceLock::new();

fn logs() -> &'static Mutex<HashMap<String, VecDeque<PluginLogEntry>>> {
    LOGS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 级别归一：未知值一律视为 `info`（前端配色只认这四档）。
fn normalize_level(level: &str) -> &'static str {
    match level.trim().to_ascii_lowercase().as_str() {
        "debug" => "debug",
        "warn" | "warning" => "warn",
        "error" => "error",
        _ => "info",
    }
}

/// 追加一条日志，返回归一化后的条目（调用方据此推 `plugin://log`）。
pub fn push(plugin_id: &str, level: &str, msg: &str) -> PluginLogEntry {
    let entry = PluginLogEntry {
        ts: now_ms(),
        level: normalize_level(level).to_string(),
        msg: msg.to_string(),
    };

    let mut guard = logs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // 插件数超限：淘汰最后一条日志时间戳最小的那个（最久未写入）。
    if !guard.contains_key(plugin_id) && guard.len() >= MAX_PLUGINS {
        let oldest = guard
            .iter()
            .min_by_key(|(_, queue)| queue.back().map(|entry| entry.ts).unwrap_or(0))
            .map(|(id, _)| id.clone());
        if let Some(oldest) = oldest {
            guard.remove(&oldest);
        }
    }

    let queue = guard.entry(plugin_id.to_string()).or_default();
    if queue.len() >= MAX_ENTRIES {
        queue.pop_front();
    }
    queue.push_back(entry.clone());
    entry
}

/// 取最近 `limit` 条（按时间正序；`limit == 0` 表示全部）。
pub fn snapshot(plugin_id: &str, limit: usize) -> Vec<PluginLogEntry> {
    let guard = logs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(queue) = guard.get(plugin_id) else {
        return Vec::new();
    };
    let start = if limit == 0 || limit >= queue.len() {
        0
    } else {
        queue.len() - limit
    };
    queue.iter().skip(start).cloned().collect()
}

/// 清空单个插件的日志（前端「清空日志」按钮）。
pub fn clear(plugin_id: &str) {
    let mut guard = logs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(queue) = guard.get_mut(plugin_id) {
        queue.clear();
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_unknown_level_and_caps_entries() {
        let id = "test.logs.cap";
        clear(id);
        let entry = push(id, "VERBOSE", "hello");
        assert_eq!(entry.level, "info", "未知级别必须归一为 info");
        push(id, "warning", "w");
        let recent = snapshot(id, 0);
        assert_eq!(recent.last().map(|e| e.level.as_str()), Some("warn"));

        for index in 0..(MAX_ENTRIES + 10) {
            push(id, "debug", &format!("line {index}"));
        }
        let all = snapshot(id, 500);
        assert_eq!(all.len(), MAX_ENTRIES, "每插件最多保留 500 条");
        let newest = format!("line {}", MAX_ENTRIES + 9);
        assert_eq!(
            all.last().map(|e| e.msg.as_str()),
            Some(newest.as_str()),
            "环形缓冲必须淘汰最老记录"
        );
        assert_eq!(snapshot(id, 3).len(), 3, "limit 只返回最近 N 条");
        clear(id);
        assert!(snapshot(id, 0).is_empty());
    }
}
