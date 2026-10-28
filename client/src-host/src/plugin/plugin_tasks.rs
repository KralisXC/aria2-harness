//! 虚拟任务注册表（beta10 · 模块 B）。
//!
//! 插件接管的下载（如 HLS）不会出现在 aria2 的任务列表 / 历史库中；宿主在此维护
//! 一份进程内视图，由插件通过 `ctx.emit("plugin-task", {...})` 推进状态，再经
//! `plugin://task` 事件推给前端。
//!
//! 任务 id 使用 `plugin:<pluginId>:<序号>` 前缀，与 aria2 GID 命名空间隔离。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

/// 终态：不再接受进度推进（取消标记除外，保持幂等）。
const PHASE_COMPLETE: &str = "complete";
const PHASE_ERROR: &str = "error";
const PHASE_CANCELLED: &str = "cancelled";
/// beta11 · FF-7：默认进度展示模式（确定进度条）。
const DEFAULT_MODE: &str = "progress";

static REGISTRY: OnceLock<Mutex<HashMap<String, PluginTask>>> = OnceLock::new();

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginTask {
    pub id: String,
    pub plugin_id: String,
    pub title: String,
    pub url: String,
    /// `detecting` / `downloading` / `merging` / `complete` / `error` / `cancelled`
    pub phase: String,
    /// 进度展示模式：`progress`（默认）或 `indeterminate`（直播录制等无确定进度的任务）。
    pub mode: String,
    /// 0–100。
    pub progress: f64,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
    /// beta12.3：插件上报的分片临时目录（删除任务时级联清理）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

fn registry() -> &'static Mutex<HashMap<String, PluginTask>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 创建一条虚拟任务（claim 成功后由宿主调用）。
pub fn create(plugin_id: &str, title: &str, url: &str) -> PluginTask {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let now = now_ms();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let task = PluginTask {
        id: format!("plugin:{plugin_id}:{now}-{seq}"),
        plugin_id: plugin_id.to_string(),
        title: if title.trim().is_empty() {
            derive_title(url)
        } else {
            title.trim().to_string()
        },
        url: url.to_string(),
        phase: "detecting".to_string(),
        mode: DEFAULT_MODE.to_string(),
        progress: 0.0,
        detail: String::new(),
        output_path: None,
        temp_dir: None,
        error: None,
        created_at: now,
        updated_at: now,
    };
    registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(task.id.clone(), task.clone());
    task
}

/// 前端 / 宿主使用的任务快照（按更新时间倒序）。
pub fn list() -> Vec<PluginTask> {
    let mut out: Vec<PluginTask> = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .values()
        .cloned()
        .collect();
    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    out
}

/// 处理插件上报的状态事件，返回更新后的任务（未知 id 或非本插件任务返回 None）。
pub fn apply_event(plugin_id: &str, data: &Value) -> Option<PluginTask> {
    let id = data.get("id").and_then(Value::as_str)?.trim();
    if id.is_empty() {
        return None;
    }
    let mut guard = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let task = guard.get_mut(id)?;
    if task.plugin_id != plugin_id {
        tracing::warn!(
            "插件 {} 试图更新不属于自己的虚拟任务 {}",
            plugin_id,
            id
        );
        return None;
    }

    if let Some(title) = data.get("title").and_then(Value::as_str) {
        if !title.trim().is_empty() {
            task.title = title.trim().to_string();
        }
    }
    if let Some(phase) = data.get("phase").and_then(Value::as_str) {
        let phase = phase.trim();
        if !phase.is_empty() {
            // 已到终态的任务不再被后续事件改写（取消除外，保持幂等）。
            if !is_terminal(&task.phase) || phase == PHASE_CANCELLED {
                task.phase = phase.to_string();
            }
        }
    }
    if let Some(progress) = data.get("progress").and_then(Value::as_f64) {
        task.progress = progress.clamp(0.0, 100.0);
    }
    // beta11 · FF-7：`mode` 可选（progress | indeterminate），未知值忽略保持原值。
    if let Some(mode) = data.get("mode").and_then(Value::as_str) {
        let mode = mode.trim();
        if mode == DEFAULT_MODE || mode == "indeterminate" {
            task.mode = mode.to_string();
        }
    }
    if let Some(detail) = data.get("detail").and_then(Value::as_str) {
        task.detail = detail.to_string();
    }
    if let Some(path) = data.get("outputPath").and_then(Value::as_str) {
        if !path.trim().is_empty() {
            task.output_path = Some(path.trim().to_string());
        }
    }
    // beta12.3：临时目录（camelCase `tempDir`），供删除任务时级联清理。
    if let Some(dir) = data.get("tempDir").and_then(Value::as_str) {
        if !dir.trim().is_empty() {
            task.temp_dir = Some(dir.trim().to_string());
        }
    }
    if let Some(error) = data.get("error").and_then(Value::as_str) {
        if !error.trim().is_empty() {
            task.error = Some(error.trim().to_string());
        }
    }
    if task.phase == PHASE_COMPLETE {
        task.progress = 100.0;
    }
    task.updated_at = now_ms();
    Some(task.clone())
}

/// 标记取消（UI 取消 / 插件禁用）。返回更新后的任务。
pub fn cancel(id: &str) -> Option<PluginTask> {
    let mut guard = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let task = guard.get_mut(id)?;
    if !is_terminal(&task.phase) {
        task.phase = PHASE_CANCELLED.to_string();
        task.detail = "已取消".to_string();
        task.updated_at = now_ms();
    }
    Some(task.clone())
}

/// 插件被禁用 / 关闭时，把其未完成任务统一标记为取消。
pub fn cancel_all_for_plugin(plugin_id: &str) {
    let mut guard = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = now_ms();
    for task in guard.values_mut() {
        if task.plugin_id == plugin_id && !is_terminal(&task.phase) {
            task.phase = PHASE_CANCELLED.to_string();
            task.detail = "插件已禁用或关闭".to_string();
            task.updated_at = now;
        }
    }
}

/// 从前端列表中移除一条已结束的虚拟任务（宿主侧注册表同步删除）。
///
/// 运行中的任务必须先 [`cancel`]：这里只做删除，不负责中断插件长任务。
pub fn remove(id: &str) -> Option<PluginTask> {
    registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(id)
}

/// 任务所属插件 id（任务不存在返回 None）。
pub fn plugin_of(id: &str) -> Option<String> {
    registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(id)
        .map(|task| task.plugin_id.clone())
}

/// 单任务快照。
pub fn get(id: &str) -> Option<PluginTask> {
    registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(id)
        .cloned()
}

fn is_terminal(phase: &str) -> bool {
    matches!(phase, PHASE_COMPLETE | PHASE_ERROR | PHASE_CANCELLED)
}

fn derive_title(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let last = without_query
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    if last.trim().is_empty() {
        "视频任务".to_string()
    } else {
        last.to_string()
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
    use serde_json::json;

    #[test]
    fn create_and_apply_event_updates_progress() {
        let task = create("test.route.a", "", "https://cdn.test/path/demo.m3u8?token=1");
        assert_eq!(task.title, "demo.m3u8");
        assert_eq!(task.phase, "detecting");

        let updated = apply_event(
            "test.route.a",
            &json!({
                "id": task.id,
                "phase": "downloading",
                "progress": 42.5,
                "detail": "12/30 分片",
                "outputPath": "E:/downloads/demo.mp4",
                "tempDir": "E:/downloads/.harness-plugin/demo"
            }),
        )
        .expect("应更新");
        assert_eq!(updated.phase, "downloading");
        assert_eq!(updated.progress, 42.5);
        assert_eq!(updated.detail, "12/30 分片");
        assert_eq!(updated.output_path.as_deref(), Some("E:/downloads/demo.mp4"));
        assert_eq!(
            updated.temp_dir.as_deref(),
            Some("E:/downloads/.harness-plugin/demo"),
            "beta12.3：临时目录必须能从 tempDir 事件读到"
        );
    }

    /// beta11 · FF-7：`mode` 默认 progress，插件可声明 indeterminate（直播录制）。
    #[test]
    fn indeterminate_mode_is_propagated() {
        let task = create("test.route.mode", "直播", "https://cdn.test/live.m3u8");
        assert_eq!(task.mode, "progress", "默认必须是 progress");
        let updated = apply_event(
            "test.route.mode",
            &json!({ "id": task.id, "mode": "indeterminate", "detail": "已录制 00:15:32" }),
        )
        .unwrap();
        assert_eq!(updated.mode, "indeterminate");
        // 未知 mode 不覆盖当前值（避免前端渲染出第三种不可识别的模式）
        let again = apply_event("test.route.mode", &json!({ "id": task.id, "mode": "bogus" })).unwrap();
        assert_eq!(again.mode, "indeterminate");
    }

    #[test]
    fn other_plugin_cannot_update_task() {
        let task = create("test.route.owner", "T", "https://cdn.test/a.m3u8");
        assert!(apply_event(
            "test.route.intruder",
            &json!({ "id": task.id, "phase": "complete" })
        )
        .is_none());
    }

    #[test]
    fn terminal_phase_is_not_overwritten() {
        let task = create("test.route.terminal", "T", "https://cdn.test/a.m3u8");
        apply_event("test.route.terminal", &json!({ "id": task.id, "phase": "error" }));
        let updated = apply_event(
            "test.route.terminal",
            &json!({ "id": task.id, "phase": "downloading", "progress": 10 }),
        )
        .unwrap();
        assert_eq!(updated.phase, "error");
    }

    #[test]
    fn cancel_marks_task_and_plugin_lookup_works() {
        let task = create("test.route.cancel", "T", "https://cdn.test/a.m3u8");
        assert_eq!(plugin_of(&task.id).as_deref(), Some("test.route.cancel"));
        let cancelled = cancel(&task.id).unwrap();
        assert_eq!(cancelled.phase, "cancelled");
    }

    #[test]
    fn remove_drops_task_from_registry() {
        let task = create("test.route.remove", "T", "https://cdn.test/a.m3u8");
        assert!(remove(&task.id).is_some());
        assert!(get(&task.id).is_none(), "删除后不应再能查到");
        assert!(remove(&task.id).is_none(), "重复删除返回 None");
    }
}
