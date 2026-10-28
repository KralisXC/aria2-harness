//! `ctx.addTasksAndWait` 的宿主实现（beta10 · 模块 A3）。
//!
//! 运行位置：tokio 侧（`hostapi::serve`），插件线程通过 [`HostBridge`] 同步等待。
//! 因此本模块**绝不能**在 IPC 分发器的串行路径里运行 —— `spawn_worker` 已改为
//! 每次调用 `tokio::spawn` + 信号量，长等待不会卡住其他插件调用。
//!
//! 关键约束：
//! - 单次最多 `min(manifest.maxTasksPerCall, 500)` 条；
//! - 所有 gid 在派发后**立即**登记为内部任务（UI / 历史 / 插件事件全部隐身）；
//! - 批量按 `concurrency` 分批提交并等待终态，总预算由 `timeoutMs` 兜底；
//! - **停滞熔断**（beta12.1）：120 秒内 `completedLength` 没有任何增长即判定卡死，
//!   该分片立即 `forceRemove` 并返回 error，避免插件线程无限等待；
//! - 超时 / 取消时 `forceRemove` 未完成任务，清理内部任务记录（文件保留）；
//! - 运行中的 gid 按 `plugin_id` 登记，供 UI 删除任务时反向 `forceRemove`
//!   （见 [`take_active_gids`]）。

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::internal_gids;
use crate::rpc_client::Client;

/// 宿主硬上限：单次批量任务条数。
pub const HARD_MAX_TASKS: usize = 500;
/// 轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(400);
/// 单批之间的最大并发提交数（防止一次把 500 个任务全塞进引擎队列）。
const MAX_CONCURRENCY: usize = 16;
/// 停滞熔断阈值：这么久没有任何字节增长即视为卡死。
const STALL_TIMEOUT: Duration = Duration::from_secs(120);

/// 一次批量任务中的单条规格。
#[derive(Debug, Clone)]
pub struct TaskSpec {
    pub url: String,
    pub out: String,
    pub dir: String,
    pub headers: Vec<(String, String)>,
    pub referer: String,
}

/// 一次批量调用的聚合规格。
#[derive(Debug, Clone)]
pub struct BatchSpec {
    pub tasks: Vec<TaskSpec>,
    pub timeout: Duration,
    pub concurrency: usize,
}

/// 解析并校验 JS 侧传入的 payload。
pub fn parse_payload(payload: &str, max_tasks: usize) -> Result<BatchSpec, String> {
    let value: Value = serde_json::from_str(payload)
        .map_err(|e| format!("addTasksAndWait 参数不是合法 JSON：{e}"))?;

    let list = value
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or_else(|| "addTasksAndWait 需要 tasks 数组".to_string())?;
    let limit = max_tasks.clamp(1, HARD_MAX_TASKS);
    if list.is_empty() {
        return Err("tasks 不能为空".to_string());
    }
    if list.len() > limit {
        return Err(format!(
            "单次最多 {limit} 条任务（本次 {} 条）",
            list.len()
        ));
    }

    let mut tasks = Vec::with_capacity(list.len());
    for (index, item) in list.iter().enumerate() {
        let url = item
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if url.is_empty() {
            return Err(format!("tasks[{index}].url 不能为空"));
        }
        let out = item
            .get("out")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if out.is_empty() {
            return Err(format!("tasks[{index}].out 不能为空"));
        }
        if out.contains('/') || out.contains('\\') || out.contains("..") {
            return Err(format!("tasks[{index}].out 不能包含目录分隔符或 .."));
        }
        let dir = item
            .get("dir")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let referer = item
            .get("referer")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let headers = item
            .get("headers")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .filter_map(|(k, v)| v.as_str().map(|text| (k.clone(), text.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        tasks.push(TaskSpec {
            url,
            out,
            dir,
            headers,
            referer,
        });
    }

    let timeout_ms = value
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .unwrap_or(600_000)
        .clamp(30_000, 1_800_000);
    let concurrency = value
        .get("concurrency")
        .and_then(Value::as_u64)
        .unwrap_or(8)
        .clamp(1, MAX_CONCURRENCY as u64) as usize;

    Ok(BatchSpec {
        tasks,
        timeout: Duration::from_millis(timeout_ms),
        concurrency,
    })
}

/// 执行一次批量任务并等待终态；返回 JSON 字符串。
pub async fn add_and_wait(
    client: &Client,
    plugin_id: &str,
    payload: &str,
    max_tasks: usize,
) -> String {
    let spec = match parse_payload(payload, max_tasks) {
        Ok(spec) => spec,
        Err(msg) => return err(&msg),
    };

    // 任务级选项：显式 out / dir / header / referer；其余保持引擎默认。
    let mut entries: Vec<Entry> = Vec::with_capacity(spec.tasks.len());
    for task in &spec.tasks {
        let mut options = Map::new();
        options.insert("out".to_string(), json!(task.out));
        // 分片文件必须可覆盖：同一虚拟任务重跑时不能因为"文件已存在"失败。
        options.insert("allow-overwrite".to_string(), json!("true"));
        options.insert("auto-file-renaming".to_string(), json!("false"));
        // 分片通常很小，多连接只会放大握手成本。
        options.insert("split".to_string(), json!("1"));
        options.insert("max-connection-per-server".to_string(), json!("1"));
        if !task.dir.trim().is_empty() {
            let dir = match super::hostapi::guard_path(&task.dir) {
                Ok(path) => path,
                Err(msg) => return err(&msg),
            };
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return err(&format!("创建分片目录失败：{e}"));
            }
            options.insert("dir".to_string(), json!(dir.to_string_lossy()));
        } else {
            // beta12.3：插件未指定 dir 时**强制**落到全局设置里的下载目录，
            // 绝不交给引擎默认值（否则会下到 C:\Users\<用户>\Downloads，
            // 与用户在设置里配置的目录不一致）。
            let dir = crate::configured_download_dir();
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return err(&format!("创建分片目录失败：{e}"));
            }
            options.insert("dir".to_string(), json!(dir.to_string_lossy()));
        }
        if !task.headers.is_empty() {
            let headers: Vec<String> = task
                .headers
                .iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect();
            options.insert("header".to_string(), json!(headers));
        }
        if !task.referer.is_empty() {
            options.insert("referer".to_string(), json!(task.referer));
        }
        entries.push(Entry {
            url: task.url.clone(),
            options,
            gid: None,
            status: "pending".to_string(),
            error_code: String::new(),
            error_message: String::new(),
            path: String::new(),
            completed_length: 0,
            last_progress_at: Instant::now(),
        });
    }

    let deadline = Instant::now() + spec.timeout;
    let batch_size = spec.concurrency.max(1);
    let mut cancelled = false;

    for start in (0..entries.len()).step_by(batch_size) {
        let end = (start + batch_size).min(entries.len());
        if is_cancelled(plugin_id) {
            cancelled = true;
            break;
        }
        // 提交本批
        for entry in entries.iter_mut().take(end).skip(start) {
            match client
                .add_uri(vec![entry.url.clone()], entry.options.clone())
                .await
            {
                Ok(gid) => {
                    internal_gids::mark_internal(&gid);
                    register_active_gid(plugin_id, &gid);
                    entry.gid = Some(gid);
                    entry.status = "active".to_string();
                }
                Err(e) => {
                    entry.status = "error".to_string();
                    entry.error_message = format!("派发失败：{e}");
                }
            }
        }
        // 等待本批终态
        wait_batch(client, &mut entries[start..end], deadline, plugin_id, &mut cancelled).await;
        if cancelled {
            break;
        }
    }

    // 超时 / 取消：强制移除未完成任务，避免内部任务长期占用队列。
    let mut aborted = Vec::new();
    for entry in &mut entries {
        if !is_terminal(&entry.status) {
            if let Some(gid) = entry.gid.clone() {
                let _ = client.force_remove(&gid).await;
                aborted.push(gid);
            }
            entry.status = if cancelled { "cancelled" } else { "timeout" }.to_string();
            if entry.error_message.is_empty() {
                entry.error_message = if cancelled {
                    "插件任务已取消".to_string()
                } else {
                    "等待超时".to_string()
                };
            }
        }
    }

    // 清理内部任务记录（文件保留）：完成 / 失败 / 超时都做一次。
    for gid in entries.iter().filter_map(|e| e.gid.as_deref()) {
        let _ = client.remove_download_result(gid).await;
        internal_gids::unmark_internal(gid);
        unregister_active_gid(plugin_id, gid);
    }

    let tasks: Vec<Value> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            json!({
                "index": index,
                "gid": entry.gid,
                "status": entry.status,
                "errorCode": entry.error_code,
                "errorMessage": entry.error_message,
                "path": entry.path,
            })
        })
        .collect();

    let ok = tasks.iter().all(|task| {
        task.get("status").and_then(Value::as_str) == Some("complete")
    });
    json!({ "ok": ok, "tasks": tasks, "aborted": aborted.len() }).to_string()
}

struct Entry {
    url: String,
    options: Map<String, Value>,
    gid: Option<String>,
    status: String,
    error_code: String,
    error_message: String,
    path: String,
    /// 上次观察到的 `completedLength`（停滞熔断只认字节增长）。
    completed_length: u64,
    /// 最后一次观察到字节增长的时刻。
    last_progress_at: Instant,
}

fn is_terminal(status: &str) -> bool {
    matches!(status, "complete" | "error" | "removed")
}

/// 轮询一批任务的终态；返回时本批要么全部终态，要么总超时 / 取消。
async fn wait_batch(
    client: &Client,
    entries: &mut [Entry],
    deadline: Instant,
    plugin_id: &str,
    cancelled: &mut bool,
) {
    // `gid` 必须显式索取：aria2（含内嵌 native 引擎）在指定 keys 时会**严格过滤**，
    // 未列出的字段一律不返回。漏掉 gid 会让 snapshot 的 gid→任务 映射整体失效，
    // 所有任务都会被误判成「已从引擎移除」（beta10 E2E 实测）。
    let keys: Option<Vec<String>> = Some(vec![
        "gid".to_string(),
        "status".to_string(),
        "errorCode".to_string(),
        "errorMessage".to_string(),
        "dir".to_string(),
        "files".to_string(),
        // 停滞熔断依赖字节增长：必须显式索取，否则引擎会过滤掉该字段。
        "completedLength".to_string(),
    ]);

    loop {
        if is_cancelled(plugin_id) {
            *cancelled = true;
            return;
        }
        if entries.iter().all(|entry| is_terminal(&entry.status)) {
            return;
        }

        let alive: HashMap<String, Value> = match snapshot(client, keys.clone()).await {
            Ok(map) => map,
            Err(e) => {
                // 引擎断线：立即收敛，避免在已失效的连接上无限轮询。
                for entry in entries.iter_mut().filter(|e| !is_terminal(&e.status)) {
                    entry.status = "error".to_string();
                    entry.error_message = format!("引擎不可用：{e}");
                }
                return;
            }
        };

        for entry in entries.iter_mut() {
            if is_terminal(&entry.status) {
                continue;
            }
            let Some(gid) = entry.gid.as_deref() else {
                continue;
            };
            let Some(value) = alive.get(gid) else {
                // 不在 active / waiting / stopped 列表：视为已移除。
                entry.status = "removed".to_string();
                entry.error_message = "任务已从引擎移除".to_string();
                continue;
            };
            let status = value
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !is_terminal(&status) {
                entry.status = status;
                // beta12.1 停滞熔断：120 秒没有字节增长即强制移除该分片。
                let completed = value
                    .get("completedLength")
                    .and_then(|v| {
                        v.as_str()
                            .and_then(|s| s.parse::<u64>().ok())
                            .or_else(|| v.as_u64())
                    })
                    .unwrap_or(0);
                if completed > entry.completed_length {
                    entry.completed_length = completed;
                    entry.last_progress_at = Instant::now();
                } else if entry.last_progress_at.elapsed() >= STALL_TIMEOUT {
                    entry.status = "error".to_string();
                    entry.error_message = format!(
                        "下载停滞超过 {} 秒（无进度），已终止该分片任务",
                        STALL_TIMEOUT.as_secs()
                    );
                    if let Some(gid) = entry.gid.clone() {
                        let _ = client.force_remove(&gid).await;
                    }
                }
                continue;
            }
            entry.status = status.clone();
            entry.error_code = value
                .get("errorCode")
                .and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_u64().map(|n| n.to_string())))
                .unwrap_or_default();
            entry.error_message = value
                .get("errorMessage")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if status == "complete" {
                entry.path = value
                    .get("files")
                    .and_then(Value::as_array)
                    .and_then(|files| files.first())
                    .and_then(|file| file.get("path"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
            }
        }

        if Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// 一次性拉取 active + waiting + stopped，构建 gid → 任务快照。
async fn snapshot(
    client: &Client,
    keys: Option<Vec<String>>,
) -> Result<HashMap<String, Value>, String> {
    let mut map = HashMap::new();
    let active = client
        .tell_active(keys.clone())
        .await
        .map_err(|e| e.to_string())?;
    for task in active {
        if let Some(gid) = task.get("gid").and_then(Value::as_str) {
            map.insert(gid.to_string(), Value::Object(task));
        }
    }
    let waiting = client
        .tell_waiting(0, 1000, keys.clone())
        .await
        .map_err(|e| e.to_string())?;
    for task in waiting {
        if let Some(gid) = task.get("gid").and_then(Value::as_str) {
            map.insert(gid.to_string(), Value::Object(task));
        }
    }
    let stopped = client
        .tell_stopped(0, 1000, keys)
        .await
        .map_err(|e| e.to_string())?;
    for task in stopped {
        if let Some(gid) = task.get("gid").and_then(Value::as_str) {
            map.insert(gid.to_string(), Value::Object(task));
        }
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// 运行中 gid 登记（UI 删除插件任务时反向 forceRemove）
// ---------------------------------------------------------------------------

static ACTIVE_GIDS: OnceLock<StdMutex<HashMap<String, HashSet<String>>>> = OnceLock::new();

fn active_gids() -> &'static StdMutex<HashMap<String, HashSet<String>>> {
    ACTIVE_GIDS.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn register_active_gid(plugin_id: &str, gid: &str) {
    if let Ok(mut map) = active_gids().lock() {
        map.entry(plugin_id.to_string())
            .or_default()
            .insert(gid.to_string());
    }
}

fn unregister_active_gid(plugin_id: &str, gid: &str) {
    if let Ok(mut map) = active_gids().lock() {
        let empty = match map.get_mut(plugin_id) {
            Some(set) => {
                set.remove(gid);
                set.is_empty()
            }
            None => false,
        };
        if empty {
            map.remove(plugin_id);
        }
    }
}

/// 取走某插件当前仍在运行的全部分片 gid（同时清空登记）。
///
/// 供 `cancel_plugin_task` 在杀掉 FFmpeg 后向引擎发 `forceRemove`，
/// 彻底掐断后台偷跑的流量。
pub fn take_active_gids(plugin_id: &str) -> Vec<String> {
    active_gids()
        .lock()
        .ok()
        .and_then(|mut map| map.remove(plugin_id))
        .map(|set| set.into_iter().collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 取消令牌（插件禁用 / 卸载 / 热重载时置位）
// ---------------------------------------------------------------------------

static CANCEL_FLAGS: OnceLock<StdMutex<HashSet<String>>> = OnceLock::new();

fn cancel_flags() -> &'static StdMutex<HashSet<String>> {
    CANCEL_FLAGS.get_or_init(|| StdMutex::new(HashSet::new()))
}

/// 请求取消某插件正在进行的批量任务（下一次轮询即生效）。
pub fn cancel(plugin_id: &str) {
    if let Ok(mut set) = cancel_flags().lock() {
        set.insert(plugin_id.to_string());
    }
}

/// 清除取消标记（插件重新启用时调用）。
pub fn clear_cancel(plugin_id: &str) {
    if let Ok(mut set) = cancel_flags().lock() {
        set.remove(plugin_id);
    }
}

fn is_cancelled(plugin_id: &str) -> bool {
    cancel_flags()
        .lock()
        .map(|set| set.contains(plugin_id))
        .unwrap_or(false)
}

fn err(msg: &str) -> String {
    json!({ "ok": false, "error": msg }).to_string()
}

#[cfg(test)]
mod tests {
    use super::{parse_payload, HARD_MAX_TASKS};

    #[test]
    fn parses_minimal_batch() {
        let payload = r#"{
            "tasks": [
                {"url":"https://e.test/a.ts","out":"a.ts","dir":"hls/1","referer":"https://e.test/"},
                {"url":"https://e.test/b.ts","out":"b.ts","dir":"hls/1","headers":{"Cookie":"k=v"}}
            ],
            "timeoutMs": 60000,
            "concurrency": 4
        }"#;
        let spec = parse_payload(payload, 100).unwrap();
        assert_eq!(spec.tasks.len(), 2);
        assert_eq!(spec.concurrency, 4);
        assert_eq!(spec.timeout.as_millis(), 60_000);
        assert_eq!(spec.tasks[1].headers[0].0, "Cookie");
    }

    #[test]
    fn rejects_empty_and_oversized_batches() {
        assert!(parse_payload(r#"{"tasks":[]}"#, 10).is_err());
        let many: Vec<String> = (0..11)
            .map(|i| format!(r#"{{"url":"https://e.test/{i}.ts","out":"{i}.ts"}}"#))
            .collect();
        let payload = format!(r#"{{"tasks":[{}]}}"#, many.join(","));
        assert!(parse_payload(&payload, 10).is_err(), "超过上限必须拒绝");
        assert!(parse_payload(&payload, HARD_MAX_TASKS).is_ok());
    }

    #[test]
    fn rejects_out_with_directory_separator() {
        let payload = r#"{"tasks":[{"url":"https://e.test/a.ts","out":"../a.ts"}]}"#;
        assert!(parse_payload(payload, 10).is_err());
    }

    #[test]
    fn clamps_timeout_and_concurrency() {
        let payload = r#"{"tasks":[{"url":"https://e.test/a.ts","out":"a.ts"}],"timeoutMs":1,"concurrency":999}"#;
        let spec = parse_payload(payload, 10).unwrap();
        assert_eq!(spec.timeout.as_millis(), 30_000);
        assert_eq!(spec.concurrency, 16);
    }
}
