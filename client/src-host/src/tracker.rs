//! 增量 29（P1-G）：BT Tracker 列表的定时更新与注入。
//!
//! 目标：让 BT 任务始终用上高连通率的公有 tracker 池，避免内置列表随时间老化。
//!
//! 设计要点（对齐用户裁决）：
//! - **aria2 自拉**：复用引擎自身的 HTTP 能力下载 tracker 文件，绝不引入 `ureq`
//!   等额外依赖 —— 新增一个 `addUri` 任务，下载完成后读本地文件即可；
//! - **源**：ngosang `trackers_best.txt`（兼顾连通率与握手性能，避免并发风暴）；
//! - **注入**：以 `changeGlobalOption` 下发 `bt-tracker`，对后续 BT 任务即时生效；
//! - **节流**：`data/tracker.json` 记录 `last_update`，每日最多更新一次；
//! - **隔离**：自拉任务的 gid 记入内部集合，使其**不出现在** UI 任务列表 / 通知
//!   事件 / 历史库 —— 用户不该看到一个"下载 tracker.txt"的任务。
//!
//! 线程/锁约束：本模块**全部在 tokio 异步上下文**内运行（调度 tick 与轮询都用
//! `tokio::time::sleep`），仅在读/写状态文件与读 tracker 文本时做**瞬时**同步 IO，
//! 不持锁、不阻塞事件循环。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::rpc_client::Client;
use serde_json::{json, Map, Value};

use crate::error::AppError;

/// tracker 列表来源（ngosang best：连通率与握手性能的平衡选择）。
const TRACKER_URL: &str = "https://raw.githubusercontent.com/ngosang/trackerslist/master/trackers_best.txt";
/// aria2 自拉落盘的文件名（下载到 `data_dir()` 下）。
const TRACKER_FILE_NAME: &str = "aria2-trackers.txt";
/// 状态文件名（记录 `last_update`，用于每日节流）。
const STATE_FILE_NAME: &str = "tracker.json";
/// 更新周期：每日一次。
const UPDATE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// 自检 tick：即使引擎启动较晚，也能在 10 分钟内补做，而非苦等一整个日周期。
const CHECK_TICK: Duration = Duration::from_secs(10 * 60);
/// 自拉任务的状态轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// 自拉任务的完成等待上界（小文本文件，60s 足够）。
const POLL_TIMEOUT: Duration = Duration::from_secs(60);
/// 数据目录覆盖环境变量（红线：路径不写死，可配置）。
const DATA_DIR_ENV: &str = "ARIA2_HARNESS_DATA";

/// 数据目录：默认 `<HOME>\data`（便携包 = exe 同级，开发期 = 仓库根），
/// 可用 `ARIA2_HARNESS_DATA` 覆盖。
pub fn data_dir() -> PathBuf {
    match std::env::var(DATA_DIR_ENV) {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => crate::paths::data_dir(),
    }
}

fn state_file() -> PathBuf {
    data_dir().join(STATE_FILE_NAME)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 内部任务 gid 隔离
// ---------------------------------------------------------------------------

/// 内部任务（当前只有 tracker 自拉）的 gid 集合。
/// 进程级 `OnceLock<Mutex<..>>`：gid 在运行期产生，需要可变容器。
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

fn mark_internal(gid: &str) {
    if let Ok(mut set) = internal_gids().lock() {
        set.insert(gid.to_string());
    }
}

// ---------------------------------------------------------------------------
// 调度
// ---------------------------------------------------------------------------

/// 常驻调度任务：每次自检是否到达更新周期，到期则执行一次自拉 + 注入。
///
/// 由 [`crate::bootstrap`] 拉起；引擎未就绪时**静默跳过**（下一个 tick 再试），
/// 不把"引擎尚未连接"当成失败刷日志。
///
/// **失败只落一条 `warn!`**：Tracker 是内部增益任务，GitHub 返回 4xx/5xx、超时、
/// 列表为空都只影响"本次没换到更新的 tracker 池"，绝不向 UI 抛 `Event::Error`
/// （该 gid 已登记为内部任务，事件循环与插件总线都会把它过滤掉）。
pub async fn run_scheduler() {
    loop {
        match maybe_update().await {
            Ok(()) => {}
            Err(err) => {
                tracing::warn!("Tracker 列表更新未成功（内部任务，已静默处理，下个周期重试）：{err}")
            }
        }
        tokio::time::sleep(CHECK_TICK).await;
    }
}

/// 是否到达更新周期：状态文件缺失 / 损坏 / 距上次更新 ≥ 24h 时返回 true。
fn is_due() -> bool {
    let Ok(text) = std::fs::read_to_string(state_file()) else {
        return true;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return true;
    };
    let Some(last) = value.get("last_update").and_then(Value::as_u64) else {
        return true;
    };
    unix_now().saturating_sub(last) >= UPDATE_INTERVAL.as_secs()
}

async fn maybe_update() -> Result<(), AppError> {
    if !is_due() {
        return Ok(());
    }
    // 引擎尚未就绪：静默跳过（下一个 tick 再试），避免启动期刷屏误报。
    let client = match crate::client_or_err(crate::state()).await {
        Ok(client) => client,
        Err(_) => return Ok(()),
    };
    update_now(client).await
}

/// 执行一次完整更新：派发自拉任务 → 等待完成 → 解析 → 注入 `bt-tracker` → 落状态。
async fn update_now(client: Client) -> Result<(), AppError> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| AppError::fs(format!("创建数据目录失败（{}）", dir.display()), e))?;

    // aria2 自拉：单连接（split=1）避免小文件被多连接切开；强制覆盖同名文件。
    let mut options = Map::new();
    options.insert("dir".to_string(), json!(dir.to_string_lossy()));
    options.insert("out".to_string(), json!(TRACKER_FILE_NAME));
    options.insert("auto-file-renaming".to_string(), json!("false"));
    options.insert("allow-overwrite".to_string(), json!("true"));
    options.insert("split".to_string(), json!("1"));

    crate::metrics::METRICS
        .rpc_calls
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let gid = client
        .add_uri(vec![TRACKER_URL.to_string()], options)
        .await
        .map_err(|e| {
            crate::metrics::METRICS
                .rpc_errors
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            AppError::from(e)
        })?;
    // 立即登记为内部任务：后续通知 / 列表查询 / 落库都会跳过它。
    mark_internal(&gid);
    tracing::info!(gid = %gid, "已派发 Tracker 列表自拉任务（内部任务，不展示）");

    wait_complete(&client, &gid).await?;

    let path = dir.join(TRACKER_FILE_NAME);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| AppError::fs(format!("读取 Tracker 列表失败（{}）", path.display()), e))?;
    let list = parse_trackers(&text);
    if list.is_empty() {
        return Err(AppError::Internal("Tracker 列表为空，放弃注入".to_string()));
    }

    // 注入全局 `bt-tracker`：对所有后续 BT 任务即时生效。
    let mut options = Map::new();
    options.insert("bt-tracker".to_string(), json!(list.join(",")));
    crate::change_global_option(options).await?;

    write_state(list.len());
    tracing::info!(count = list.len(), "Tracker 列表已更新并注入（bt-tracker）");
    Ok(())
}

/// 手动触发一次更新（L3 UI 的「立即同步」）。
///
/// 与定时调度共用 [`update_now`]，因此同样具备每日节流之外的强制刷新语义：
/// 无论 `last_update` 是否到期都会真实执行一次自拉 + 注入。
pub async fn refresh() -> Result<(), AppError> {
    let client = crate::client_or_err(crate::state()).await?;
    update_now(client).await
}

/// 当前 Tracker 快照（只读）：注入中的列表 + 最近更新元数据。
///
/// 数据来源：`aria2.getGlobalOption` 的 `bt-tracker`（真实生效值）与
/// `data/tracker.json`（最近更新时间 / 来源）。任一缺失都以空值兜底，绝不报错。
pub async fn snapshot() -> Result<Value, AppError> {
    let client = crate::client_or_err(crate::state()).await?;
    crate::metrics::METRICS
        .rpc_calls
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let options = client
        .call("aria2.getGlobalOption", vec![])
        .await
        .map_err(|e| {
            crate::metrics::METRICS
                .rpc_errors
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            AppError::from(e)
        })?;

    let raw = options
        .get("bt-tracker")
        .and_then(Value::as_str)
        .unwrap_or("");
    let items: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    let state = std::fs::read_to_string(state_file())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or(Value::Null);
    let last_update = state.get("last_update").and_then(Value::as_u64);
    let source = state
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or(TRACKER_URL);

    Ok(json!({
        "items": items,
        "count": items.len(),
        "lastUpdate": last_update,
        "source": source,
        "injecting": !items.is_empty(),
    }))
}

/// 轮询自拉任务直至终态。返回 `Ok(())` 表示 `complete`。
async fn wait_complete(client: &Client, gid: &str) -> Result<(), AppError> {
    let keys = Some(vec![
        "status".to_string(),
        "errorCode".to_string(),
        "errorMessage".to_string(),
    ]);
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        let status = client
            .tell_status(gid, keys.clone())
            .await
            .map_err(AppError::from)?;
        match status.get("status").and_then(Value::as_str).unwrap_or("") {
            "complete" => return Ok(()),
            "error" | "removed" => {
                return Err(AppError::Internal(format!(
                    "Tracker 自拉任务终止：status={:?}, errorCode={:?}, errorMessage={:?}",
                    status.get("status"),
                    status.get("errorCode"),
                    status.get("errorMessage"),
                )));
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(AppError::Internal("Tracker 自拉任务超时未完成".to_string()));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// 写状态文件（`last_update` 用于节流）。失败只记警告：不影响已完成的注入。
fn write_state(count: usize) {
    let payload = json!({
        "last_update": unix_now(),
        "count": count,
        "source": TRACKER_URL,
    });
    let Ok(text) = serde_json::to_string_pretty(&payload) else {
        tracing::warn!("序列化 Tracker 状态失败");
        return;
    };
    if let Err(e) = std::fs::write(state_file(), text) {
        tracing::warn!("写入 Tracker 状态文件失败（{}）：{e}", state_file().display());
    }
}

/// 解析 tracker 文本：按行切分、去空白、跳过 `#` 注释行、按首次出现顺序去重。
pub fn parse_trackers(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let item = line.trim();
        if item.is_empty() || item.starts_with('#') {
            continue;
        }
        if seen.insert(item.to_string()) {
            out.push(item.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_uri_per_line_skipping_blanks_and_comments() {
        let text = "\n# comment\nudp://a:1/announce\n\n  http://b/announce  \n";
        assert_eq!(
            parse_trackers(text),
            vec!["udp://a:1/announce", "http://b/announce"]
        );
    }

    #[test]
    fn deduplicates_preserving_first_occurrence() {
        let text = "udp://x:1\nhttp://y\nudp://x:1\n";
        assert_eq!(parse_trackers(text), vec!["udp://x:1", "http://y"]);
    }

    #[test]
    fn empty_text_yields_empty_list() {
        assert!(parse_trackers("\n\n  \n").is_empty());
    }

    #[test]
    fn internal_gid_registry_is_isolation_for_ui() {
        mark_internal("gid-tracker-test");
        assert!(is_internal("gid-tracker-test"));
        assert!(!is_internal("gid-user-task"));
    }
}
