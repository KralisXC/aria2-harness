//! 任务历史 SQLite 持久化（增量 28 / P2-B）。
//!
//! 设计要点：
//! - **单连接 + 阻塞池内互斥**：SQL 一律在 [`tokio::task::spawn_blocking`] 内执行，
//!   事件循环**永不**碰 SQL。写频率极低（仅任务建立与终态），单连接足够；
//!   将来要换连接池时 `Db` 就是唯一接缝，调用点零改动。
//! - **版本化迁移**：`PRAGMA user_version` 记录 schema 版本，`MIGRATIONS` 按版本号
//!   逐级推进；**拒绝降级**（库版本高于程序 → 报错而不是静默跑旧逻辑）。
//! - **幂等写入**：`gid` 是主键，`ON CONFLICT DO UPDATE` 覆盖终态字段，
//!   `created_at` 保持首次值 —— 同一任务被事件与对账重复写多次结果一致。
//!
//! 本模块**不持有任何全局槽位**（`OnceLock` 在 `lib.rs`），只做纯存储。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::error::AppError;

/// 默认落库路径：`<HOME>\data\aria2-harness.db`（便携包 = exe 同级）。
/// 覆盖数据库路径的环境变量（沿用 `ARIA2_SERVICE_PIPE` 的 env 覆盖惯例）。
const DB_PATH_ENV: &str = "ARIA2_HARNESS_DB";
/// 忙等上界：WAL 下写锁冲突极少，超时即报错，绝不无限挂起。
const BUSY_TIMEOUT: Duration = Duration::from_millis(3000);
/// 分页查询的条数上界（防前端传入 `limit: 1e9` 把内存打爆）。
const MAX_LIMIT: i64 = 500;
/// 默认每页条数。
const DEFAULT_LIMIT: i64 = 50;

/// 当前程序支持的 schema 版本。**每次加迁移必须同步 +1。**
const SCHEMA_VERSION: i32 = 2;

/// 「已结束」的状态集合。历史查询与 `clear_history` 共用，避免两处口径漂移。
const FINISHED_STATUSES: &str = "'complete','error','removed'";

/// v1 初始表结构。
///
/// 注意：`CREATE TABLE IF NOT EXISTS` 只用于 v1（库要么是空的、要么是 v1）。
/// **后续版本的迁移脚本里禁止再用 `IF NOT EXISTS`** —— SQLite 的
/// `ALTER TABLE ADD COLUMN` 不支持该子句，且版本号本身已保证"只会执行一次"。
const MIGRATION_V1: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    gid              TEXT PRIMARY KEY,               -- aria2 GID（天然幂等键）
    url              TEXT NOT NULL DEFAULT '',
    dir              TEXT NOT NULL DEFAULT '',
    status           TEXT NOT NULL,                  -- waiting/active/paused/complete/error/removed
    completed_length INTEGER NOT NULL DEFAULT 0,     -- 字节
    total_length     INTEGER NOT NULL DEFAULT 0,
    download_speed   INTEGER NOT NULL DEFAULT 0,
    error_code       INTEGER,                        -- aria2 errorCode（仅 error 时非空）
    error_message    TEXT,
    created_at       INTEGER NOT NULL,               -- unix epoch 秒
    updated_at       INTEGER NOT NULL,
    finished_at      INTEGER                         -- 终态时刻；未结束为 NULL
);
CREATE INDEX IF NOT EXISTS idx_tasks_created_at ON tasks(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_tasks_status     ON tasks(status);

-- P1-C `ctx.settings` 的预留落点（本轮只建表，不接线）
CREATE TABLE IF NOT EXISTS plugin_settings (
    plugin_id  TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,                        -- JSON 文本，值为任意类型
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (plugin_id, key)
);
"#;

/// v2：C-3 Phase 2 —— 记录任务由哪个引擎执行（`aria2` | `native`）。
///
/// **只追加不停机迁移**：老库全部视为 aria2（`--engine=aria2` 回滚档就是 v0.9.4 行为），
/// 这样历史页在升级后不会出现空引擎。
const MIGRATION_V2: &str = r#"
ALTER TABLE tasks ADD COLUMN engine TEXT NOT NULL DEFAULT 'aria2';
"#;

/// 迁移脚本表：`(目标版本, SQL)`，按版本号升序。
///
/// **红线：已发布的迁移条目永不修改，只能追加新条目。**
/// 否则老库的 `user_version` 会与新脚本内容错位 —— 轻则漏列，重则静默走错分支。
const MIGRATIONS: &[(i32, &str)] = &[(1, MIGRATION_V1), (2, MIGRATION_V2)];

/// 生效的数据库路径：环境变量优先（去空白后非空才采纳），否则用默认值。
pub fn db_path() -> String {
    std::env::var(DB_PATH_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| crate::paths::db_path().to_string_lossy().into_owned())
}

/// 一条任务记录（既是 DB 行，也是回给前端的 JSON —— `camelCase` 与既有前端契约一致）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub gid: String,
    pub url: String,
    pub dir: String,
    pub status: String,
    /// 执行该任务的引擎（`aria2` | `native`），对应 RPC 扩展字段 `_engine`。
    pub engine: String,
    pub completed_length: i64,
    pub total_length: i64,
    pub download_speed: i64,
    pub error_code: Option<i64>,
    pub error_message: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
}

/// 历史分页结果。带 `total` 前端才能渲染分页器与"共 N 条"。
#[derive(Debug, Clone, Serialize)]
pub struct TaskPage {
    pub items: Vec<TaskRecord>,
    pub total: i64,
}

impl TaskRecord {
    /// 由 aria2 `tellStatus` / `tellStopped` 的字段白名单结果构造。
    ///
    /// `fallback_url` / `fallback_dir` 仅 `add_download` 路径传入（本地已知），
    /// 用于 aria2 尚未回填 `files[].uris[]` 时兜底，保证至少有一条可用记录。
    /// `gid` 缺失时返回 `None`（调用方跳过该条，不做无主写入）。
    pub fn from_status_map(
        map: &Map<String, Value>,
        fallback_url: Option<&str>,
        fallback_dir: Option<&str>,
    ) -> Option<Self> {
        let gid = map.get("gid").and_then(|v| v.as_str())?.to_string();
        let status = map
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let now = chrono::Utc::now().timestamp();
        // 只有终态才写 finished_at；未结束的任务保持 NULL。
        let finished_at = is_finished(&status).then_some(now);

        let url = map
            .get("files")
            .and_then(|v| v.as_array())
            .and_then(|files| files.first())
            .and_then(|f| f.get("uris"))
            .and_then(|v| v.as_array())
            .and_then(|uris| uris.first())
            .and_then(|u| u.get("uri"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| fallback_url.map(str::to_string))
            .unwrap_or_default();

        let dir = map
            .get("dir")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| fallback_dir.map(str::to_string))
            .unwrap_or_default();

        Some(Self {
            gid: gid.clone(),
            url,
            dir,
            status,
            // Router 装饰的 `_engine` 是唯一真源；缺失时按 GID 前缀回落
            // （native 的 GID 固定 `fffe` + 12 hex，见 engine-net `task::gid`）。
            engine: as_str(map, "_engine")
                .map(str::to_string)
                .unwrap_or_else(|| engine_from_gid(&gid)),
            completed_length: num(map, "completedLength"),
            total_length: num(map, "totalLength"),
            download_speed: num(map, "downloadSpeed"),
            error_code: as_str(map, "errorCode").and_then(|s| s.parse::<i64>().ok()),
            error_message: as_str(map, "errorMessage")
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            created_at: now,
            updated_at: now,
            finished_at,
        })
    }

    /// 最小写入（`tellStatus` 失败或超时时的降级）：除 gid / status 外无更多字段。
    ///
    /// - **add 路径**：`status = "waiting"`（此时只拿到 `addUri` 的 gid，尚不知引擎把它
    ///   排成等待还是活跃），url/dir 取**本地已知值**。下一次写入会纠正 status。
    /// - **事件路径**：`status = "complete" | "error"`（通知自带终态），url/dir 留空 ——
    ///   SQL 侧对空串做了保护（见 `upsert_sync` 的 `CASE`），不会冲掉 add 时已写入的值。
    ///
    /// `finished_at` 由 `status` 推导：终态才写，非终态保持 `NULL`。
    pub fn minimal(gid: &str, status: &str, url: &str, dir: &str) -> Self {
        let now = chrono::Utc::now().timestamp();
        Self {
            gid: gid.to_string(),
            url: url.to_string(),
            dir: dir.to_string(),
            status: status.to_string(),
            engine: engine_from_gid(gid),
            completed_length: 0,
            total_length: 0,
            download_speed: 0,
            error_code: None,
            error_message: None,
            created_at: now,
            updated_at: now,
            finished_at: is_finished(status).then_some(now),
        }
    }
}

/// 任务状态是否属于「已结束」（与 SQL 里的 [`FINISHED_STATUSES`] 保持同一口径）。
pub fn is_finished(status: &str) -> bool {
    matches!(status, "complete" | "error" | "removed")
}

/// 引擎回落判定：native 的 GID 固定 `fffe` + 12 hex（engine-net `task::gid`），
/// 其余一律视为 aria2。**仅当拿不到 `_engine` 时使用**（如事件通知只带 GID）。
pub fn engine_from_gid(gid: &str) -> String {
    if gid.len() == 16 && gid.starts_with("fffe") {
        "native".to_string()
    } else {
        "aria2".to_string()
    }
}

/// SQLite 连接（单连接 + 阻塞池内互斥）。
///
/// 为什么是 `Arc<Mutex<_>>` 而不是裸 `Mutex<_>`：`spawn_blocking` 的闭包必须 `'static`，
/// 而 `&self.conn` 的借用生命周期只到方法体结束，**无法**移进阻塞池。克隆出的 `Arc`
/// 是自有 `'static` 句柄，锁在阻塞池内经 `blocking_lock()` 获取并持有 ——
/// 事件循环侧只做一次 `Arc::clone` + `spawn_blocking`，零阻塞。
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    /// 打开（必要时创建）数据库：建目录 → 开库 → WAL → 迁移。
    ///
    /// **同步实现**：仅在 `bootstrap` 里调用一次（毫秒级），不属于事件循环路径。
    pub fn open(path: &str) -> Result<Self, AppError> {
        if let Some(dir) = Path::new(path).parent() {
            std::fs::create_dir_all(dir).map_err(|e| {
                AppError::fs(format!("创建数据库目录失败（{}）", dir.display()), e)
            })?;
        }

        let mut conn = Connection::open(path)
            .map_err(|e| AppError::db(format!("打开数据库失败（{path}）"), e))?;
        conn.busy_timeout(BUSY_TIMEOUT)
            .map_err(|e| AppError::db("设置 busy_timeout 失败", e))?;
        // WAL：读写不互斥（读不阻塞写），且崩溃后可恢复 —— 也是不做"长事务"的前提。
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| AppError::db("启用 WAL 失败", e))?;
        // NORMAL：WAL 下的推荐档位（掉电最多丢最后一个事务，换来数量级的写性能）。
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| AppError::db("设置 synchronous 失败", e))?;

        migrate(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// 幂等写入一条任务（唯一写路径）。
    pub async fn upsert_task(&self, rec: TaskRecord) -> Result<(), AppError> {
        let conn = Arc::clone(&self.conn);
        blocking(move || {
            // 锁只在阻塞池内获取并持有 —— 事件循环侧永不等锁。
            let guard = conn.blocking_lock();
            upsert_sync(&guard, &rec)
        })
        .await
    }

    /// 批量幂等写入（单事务）。用于连接后的历史对账，避免 N 次独立事务放大写放大。
    pub async fn upsert_many(&self, recs: Vec<TaskRecord>) -> Result<usize, AppError> {
        if recs.is_empty() {
            return Ok(0);
        }
        let conn = Arc::clone(&self.conn);
        blocking(move || {
            let guard = conn.blocking_lock();
            upsert_many_sync(&guard, &recs)
        })
        .await
    }

    /// 分页查询历史（只返回已结束任务）。
    pub async fn history(&self, limit: i64, offset: i64) -> Result<TaskPage, AppError> {
        // 边界校验放在系统边界（值来自前端）：夹紧到安全区间，绝不把非法值透传进 SQL。
        let limit = if limit <= 0 {
            DEFAULT_LIMIT
        } else {
            limit.min(MAX_LIMIT)
        };
        let offset = offset.max(0);

        let conn = Arc::clone(&self.conn);
        blocking(move || {
            let guard = conn.blocking_lock();
            history_sync(&guard, limit, offset)
        })
        .await
    }

    /// 清空历史：**只删已结束**任务，绝不动 active/waiting/paused。
    /// 返回删除条数。
    pub async fn clear_history(&self) -> Result<usize, AppError> {
        let conn = Arc::clone(&self.conn);
        blocking(move || {
            let guard = conn.blocking_lock();
            clear_sync(&guard)
        })
        .await
    }
}

/// 唯一阻塞出口：把同步 SQL 送上阻塞池。
///
/// `Connection: Send`（探针断言）→ `Arc<Mutex<Connection>>` 可移进阻塞池。锁**只在阻塞池内**
/// 持有，且闭包内不做二次加锁，无死锁面；事件循环侧只做一次 `Arc::clone`，零阻塞。
async fn blocking<T, F>(f: F) -> Result<T, AppError>
where
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AppError::Internal(format!("数据库任务异常终止：{e}")))?
}

// ---------------------------------------------------------------------------
// 同步语句层（只允许在 `blocking()` 内被调用）
// ---------------------------------------------------------------------------

/// 版本化迁移：`user_version` 是唯一真源。
fn migrate(conn: &mut Connection) -> Result<(), AppError> {
    let current: i32 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|e| AppError::db("读取 schema 版本失败", e))?;

    // 拒绝降级：库比程序新说明数据可能含本程序不认识的列，静默按旧逻辑读写会丢数据。
    if current > SCHEMA_VERSION {
        return Err(AppError::Db {
            context: format!(
                "数据库 schema 版本（v{current}）高于本程序支持的 v{SCHEMA_VERSION}，\
                 请升级程序；为避免丢数据，已拒绝继续"
            ),
            source: None,
        });
    }
    if current == SCHEMA_VERSION {
        return Ok(());
    }

    // 全部待执行迁移放**同一个事务**：要么整体生效，要么整体回滚 —— 绝不会出现
    // "表建了一半、版本号却已推进"的半迁移状态。
    let tx = conn
        .transaction()
        .map_err(|e| AppError::db("开启迁移事务失败", e))?;
    for (version, sql) in MIGRATIONS.iter().filter(|(v, _)| *v > current) {
        tx.execute_batch(sql)
            .map_err(|e| AppError::db(format!("执行迁移 v{version} 失败"), e))?;
    }
    // `user_version` 的写入是事务性的：回滚时版本号一并回退。
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(|e| AppError::db("写入 schema 版本失败", e))?;
    tx.commit()
        .map_err(|e| AppError::db("提交迁移事务失败", e))?;

    tracing::info!(from = current, to = SCHEMA_VERSION, "数据库 schema 已迁移");
    Ok(())
}

/// 幂等写入：同 gid 重复写入以最后一次为准，`created_at` 保持首次值。
fn upsert_sync(conn: &Connection, rec: &TaskRecord) -> Result<(), AppError> {
    conn.execute(
        "INSERT INTO tasks (gid, url, dir, status, engine, completed_length, total_length,
                            download_speed, error_code, error_message,
                            created_at, updated_at, finished_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
         ON CONFLICT(gid) DO UPDATE SET
             -- 空串保护：事件路径的降级记录拿不到 url/dir（留空），此处不得冲掉
             -- add 时已写入的本地已知值。非空则覆盖，空则保留原值。
             url              = CASE WHEN excluded.url <> '' THEN excluded.url ELSE tasks.url END,
             dir              = CASE WHEN excluded.dir <> '' THEN excluded.dir ELSE tasks.dir END,
             status           = excluded.status,
             engine           = excluded.engine,
             completed_length = excluded.completed_length,
             total_length     = excluded.total_length,
             download_speed   = excluded.download_speed,
             error_code       = excluded.error_code,
             error_message    = excluded.error_message,
             updated_at       = excluded.updated_at,
             finished_at      = COALESCE(excluded.finished_at, tasks.finished_at)",
        params![
            rec.gid,
            rec.url,
            rec.dir,
            rec.status,
            rec.engine,
            rec.completed_length,
            rec.total_length,
            rec.download_speed,
            rec.error_code,
            rec.error_message,
            rec.created_at,
            rec.updated_at,
            rec.finished_at,
        ],
    )
    .map(|_| ())
    .map_err(|e| AppError::db(format!("写入任务失败（gid={}）", rec.gid), e))
}

/// 批量写入（单事务）。
fn upsert_many_sync(conn: &Connection, recs: &[TaskRecord]) -> Result<usize, AppError> {
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(|e| AppError::db("开启批量写入事务失败", e))?;
    let mut written = 0usize;
    for rec in recs {
        match upsert_sync(conn, rec) {
            Ok(()) => written += 1,
            Err(e) => {
                // 单条失败即整体回滚：宁可这批都不写，也不留"一半对账过"的糊涂账。
                if let Err(rb) = conn.execute_batch("ROLLBACK") {
                    // 回滚本身失败也不能吞：连接释放时 SQLite 会自动回滚，但必须留痕。
                    tracing::error!("批量写入回滚失败（连接释放时 SQLite 将自动回滚）：{rb}");
                }
                return Err(e);
            }
        }
    }
    conn.execute_batch("COMMIT")
        .map_err(|e| AppError::db("提交批量写入事务失败", e))?;
    Ok(written)
}

/// 分页查询已结束任务 + 总数。
fn history_sync(conn: &Connection, limit: i64, offset: i64) -> Result<TaskPage, AppError> {
    let total: i64 = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM tasks WHERE status IN ({FINISHED_STATUSES})"),
            [],
            |row| row.get(0),
        )
        .map_err(|e| AppError::db("统计历史条数失败", e))?;

    // `prepare_cached`：语句复用（配合 `cache` 特性），避免每次查询重新编译 SQL。
    let mut stmt = conn
        .prepare_cached(&format!(
            "SELECT gid, url, dir, status, completed_length, total_length, download_speed,
                    error_code, error_message, created_at, updated_at, finished_at, engine
             FROM tasks WHERE status IN ({FINISHED_STATUSES})
             ORDER BY created_at DESC, gid DESC
             LIMIT ?1 OFFSET ?2"
        ))
        .map_err(|e| AppError::db("准备历史查询语句失败", e))?;

    let rows = stmt
        .query_map(params![limit, offset], |row| {
            Ok(TaskRecord {
                gid: row.get(0)?,
                url: row.get(1)?,
                dir: row.get(2)?,
                status: row.get(3)?,
                completed_length: row.get(4)?,
                total_length: row.get(5)?,
                download_speed: row.get(6)?,
                error_code: row.get(7)?,
                error_message: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
                finished_at: row.get(11)?,
                engine: row.get(12)?,
            })
        })
        .map_err(|e| AppError::db("执行历史查询失败", e))?;

    let items = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| AppError::db("读取历史行失败", e))?;

    Ok(TaskPage { items, total })
}

/// 清空历史（仅已结束）。
fn clear_sync(conn: &Connection) -> Result<usize, AppError> {
    conn.execute(
        &format!("DELETE FROM tasks WHERE status IN ({FINISHED_STATUSES})"),
        [],
    )
    .map_err(|e| AppError::db("清空历史失败", e))
}

// ---------------------------------------------------------------------------
// 字段解析辅助（aria2 数字字段既有字符串形态也有数字形态，两种都要吃）
// ---------------------------------------------------------------------------

fn as_str<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    map.get(key).and_then(|v| v.as_str())
}

/// 取整数字段：优先数字形态，其次字符串形态，缺失即 0。
fn num(map: &Map<String, Value>, key: &str) -> i64 {
    match map.get(key) {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 每条测试独立的临时库：绝不污染真实数据目录。
    fn temp_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let path = dir.path().join("test.db");
        let db = Db::open(&path.to_string_lossy()).expect("打开临时库失败");
        (dir, db)
    }

    /// 测试用最小运行时：验证 `spawn_blocking` 通路真的能跑（而不是只测同步函数）。
    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("创建测试运行时失败")
    }

    fn rec(gid: &str, status: &str) -> TaskRecord {
        TaskRecord {
            gid: gid.to_string(),
            url: format!("http://example.com/{gid}"),
            dir: r"C:\Users\Public\Downloads".to_string(),
            status: status.to_string(),
            engine: "aria2".to_string(),
            completed_length: 10,
            total_length: 100,
            download_speed: 5,
            error_code: None,
            error_message: None,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            finished_at: is_finished(status).then_some(1_700_000_100),
        }
    }

    /// 幂等：同一 gid 写两次只留一行，且 `created_at` 不被覆盖、终态被覆盖。
    #[test]
    fn upsert_is_idempotent_and_preserves_created_at() {
        let (_dir, db) = temp_db();
        rt().block_on(async {
            db.upsert_task(rec("g1", "active")).await.unwrap();
            let mut later = rec("g1", "error");
            later.created_at = 1_800_000_000; // 第二次的 created_at 必须被忽略
            later.error_code = Some(1);
            later.error_message = Some("boom".to_string());
            db.upsert_task(later).await.unwrap();

            let page = db.history(50, 0).await.unwrap();
            assert_eq!(page.total, 1);
            assert_eq!(page.items.len(), 1);
            assert_eq!(page.items[0].status, "error");
            assert_eq!(page.items[0].created_at, 1_700_000_000);
            assert_eq!(page.items[0].error_code, Some(1));
            assert_eq!(page.items[0].error_message.as_deref(), Some("boom"));
        });
    }

    /// 空串保护：事件路径的降级记录（url/dir 留空）不得冲掉 add 时写入的本地值，
    /// 且终态仍必须更新（status / finished_at 要覆盖）。
    #[test]
    fn degraded_write_keeps_existing_url_and_dir() {
        let (_dir, db) = temp_db();
        rt().block_on(async {
            // add 路径：本地已知 url/dir + waiting
            db.upsert_task(TaskRecord::minimal(
                "g1",
                "waiting",
                "http://example.com/x.bin",
                r"C:\Users\Public\Downloads",
            ))
            .await
            .unwrap();

            // 事件路径：终态但拿不到 url/dir（空串）
            db.upsert_task(TaskRecord::minimal("g1", "complete", "", ""))
                .await
                .unwrap();

            let page = db.history(50, 0).await.unwrap();
            assert_eq!(page.total, 1);
            let row = &page.items[0];
            assert_eq!(row.status, "complete", "终态必须覆盖 waiting");
            assert_eq!(row.url, "http://example.com/x.bin", "空 url 不得冲掉已有值");
            assert_eq!(row.dir, r"C:\Users\Public\Downloads", "空 dir 不得冲掉已有值");
            assert!(row.finished_at.is_some(), "complete 属终态，必须写 finished_at");
        });
    }

    /// 历史查询只看已结束任务；active/waiting/paused 绝不进历史。
    #[test]
    fn history_only_returns_finished_tasks() {
        let (_dir, db) = temp_db();
        rt().block_on(async {
            for (gid, status) in [
                ("c", "complete"),
                ("e", "error"),
                ("r", "removed"),
                ("a", "active"),
                ("w", "waiting"),
                ("p", "paused"),
            ] {
                db.upsert_task(rec(gid, status)).await.unwrap();
            }
            let page = db.history(50, 0).await.unwrap();
            assert_eq!(page.total, 3);
            let mut got: Vec<&str> = page.items.iter().map(|r| r.status.as_str()).collect();
            got.sort_unstable();
            assert_eq!(got, vec!["complete", "error", "removed"]);
        });
    }

    /// 分页：`limit` / `offset` 生效；非法值被夹紧而不是透传进 SQL。
    #[test]
    fn history_paginates_and_clamps_limit() {
        let (_dir, db) = temp_db();
        rt().block_on(async {
            for i in 0..5 {
                let mut r = rec(&format!("g{i}"), "complete");
                r.created_at = 1_700_000_000 + i; // 递增 ⇒ 排序可预测
                db.upsert_task(r).await.unwrap();
            }
            let page = db.history(2, 0).await.unwrap();
            assert_eq!(page.total, 5);
            assert_eq!(page.items.len(), 2);
            assert_eq!(page.items[0].gid, "g4"); // created_at DESC

            let page = db.history(2, 2).await.unwrap();
            assert_eq!(page.items.len(), 2);
            assert_eq!(page.items[0].gid, "g2");

            let page = db.history(10_000, 0).await.unwrap();
            assert_eq!(page.items.len(), 5, "超上限应被夹紧而非报错");

            let page = db.history(-1, -5).await.unwrap();
            assert_eq!(page.items.len(), 5, "非法 limit/offset 应回退默认值");
        });
    }

    /// `clear_history` **只清已结束**：active/waiting/paused 的行必须原样保留。
    #[test]
    fn clear_history_keeps_in_flight_tasks() {
        let (_dir, db) = temp_db();
        rt().block_on(async {
            for (gid, status) in [
                ("c", "complete"),
                ("e", "error"),
                ("a", "active"),
                ("w", "waiting"),
                ("p", "paused"),
            ] {
                db.upsert_task(rec(gid, status)).await.unwrap();
            }
            assert_eq!(db.clear_history().await.unwrap(), 2);

            // 历史已空（total 计的是已结束），但在途任务仍在表里。
            let page = db.history(50, 0).await.unwrap();
            assert_eq!(page.total, 0);
            assert_eq!(db.clear_history().await.unwrap(), 0, "再次清空应为 0（幂等）");
        });
    }

    /// 迁移：首次建库写入版本号；重开不重复迁移且**数据保留**。
    #[test]
    fn migration_sets_version_and_preserves_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db").to_string_lossy().to_string();

        {
            let db = Db::open(&path).unwrap();
            rt().block_on(db.upsert_task(rec("g1", "complete"))).unwrap();
        }
        // 重开：版本已是 SCHEMA_VERSION ⇒ 跳过迁移，且数据还在。
        let db = Db::open(&path).unwrap();
        let page = rt().block_on(db.history(50, 0)).unwrap();
        assert_eq!(page.total, 1);
        drop(db);

        // 直接用一条独立连接读版本号（避免在同步测试里争 `Db` 的异步互斥）。
        let conn = Connection::open(&path).unwrap();
        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION, "重开不得重复推进版本号");
    }

    /// 拒绝降级：库版本高于程序支持版本时必须报错，绝不静默按旧逻辑读写。
    #[test]
    fn refuses_to_open_future_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db").to_string_lossy().to_string();
        drop(Db::open(&path).unwrap());

        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        let err = match Db::open(&path) {
            Ok(_) => panic!("库 schema 版本高于程序时不得成功打开"),
            Err(e) => e,
        };
        assert_eq!(err.code(), "DB");
        assert!(err.to_string().contains("高于本程序支持"));
    }

    /// 字段解析：aria2 的数字字段是字符串形态；`errorMessage` 空串要归一成 `None`。
    #[test]
    fn parses_aria2_status_map() {
        let map = json!({
            "gid": "abc",
            "status": "error",
            "completedLength": "2048",
            "totalLength": "4096",
            "downloadSpeed": "0",
            "errorCode": "1",
            "errorMessage": "",
            "dir": r"C:\Users\Public\Downloads",
            "files": [{ "uris": [{ "uri": "http://a/b.bin" }] }]
        });
        let map = map.as_object().unwrap().clone();
        let r = TaskRecord::from_status_map(&map, None, None).unwrap();
        assert_eq!(r.url, "http://a/b.bin");
        assert_eq!(r.completed_length, 2048);
        assert_eq!(r.error_code, Some(1));
        assert_eq!(r.error_message, None, "空 errorMessage 必须归一为 None");
        assert!(r.finished_at.is_some(), "error 属终态，必须写 finished_at");
    }

    /// url 兜底：aria2 尚未回填 `files[].uris[]` 时用 `add_download` 的本地 url。
    #[test]
    fn status_map_falls_back_to_local_url() {
        let map = json!({ "gid": "abc", "status": "waiting", "completedLength": "0" });
        let map = map.as_object().unwrap().clone();
        let r = TaskRecord::from_status_map(&map, Some("http://local/x"), Some(r"C:\dl")).unwrap();
        assert_eq!(r.url, "http://local/x");
        assert_eq!(r.dir, r"C:\dl");
        assert!(r.finished_at.is_none(), "waiting 非终态，不得写 finished_at");
    }

    /// C-3 Phase 2：`_engine` 是引擎归属唯一真源，并能原样落库 / 读回。
    #[test]
    fn engine_round_trips_through_db() {
        let (_dir, db) = temp_db();
        let map = json!({
            "gid": "fffe000000000001",
            "status": "complete",
            "_engine": "native",
        });
        let map = map.as_object().unwrap().clone();
        let rec = TaskRecord::from_status_map(&map, None, None).unwrap();
        assert_eq!(rec.engine, "native");

        rt().block_on(async {
            db.upsert_task(rec).await.unwrap();
            let page = db.history(50, 0).await.unwrap();
            assert_eq!(page.items[0].engine, "native");
        });
    }

    /// 拿不到 `_engine` 时按 GID 前缀回落：只有 `fffe` + 12 hex 才是 native。
    #[test]
    fn engine_falls_back_to_gid_prefix() {
        // native GID 固定 16 位：`fffe` + 12 位十六进制（engine-net `registry.rs`）。
        assert_eq!(engine_from_gid("fffe000000000001"), "native");
        assert_eq!(engine_from_gid("fefe000000000001"), "aria2");
        // 长度不符（如截断的短串）不得误判。
        assert_eq!(engine_from_gid("fffe00000001"), "aria2");
        assert_eq!(
            TaskRecord::minimal("fffe000000000002", "active", "", "").engine,
            "native"
        );
    }

    /// v1 老库升级到 v2：已有行必须全部补上 `engine='aria2'`，数据不丢。
    #[test]
    fn v1_database_upgrades_to_v2() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db").to_string_lossy().to_string();

        // 手工构造一个 v1 库（只跑 v1 脚本 + 写版本号），模拟升级前的老用户。
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATION_V1).unwrap();
            conn.execute(
                "INSERT INTO tasks (gid, url, dir, status, created_at, updated_at)
                 VALUES ('g1', 'http://a/b.bin', 'C:\\dl', 'complete', 1, 2)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }

        let db = Db::open(&path).unwrap();
        let page = rt().block_on(db.history(50, 0)).unwrap();
        assert_eq!(page.total, 1, "升级不得丢历史数据");
        assert_eq!(page.items[0].url, "http://a/b.bin");
        assert_eq!(page.items[0].engine, "aria2", "老行默认由 aria2 执行");

        drop(db);
        let conn = Connection::open(&path).unwrap();
        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }
}
