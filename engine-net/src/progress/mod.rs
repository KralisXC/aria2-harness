//! HTTP 断点进度：legacy `*.aria2` + 实验特性 `<file>.sqlite`（R5 修订后双格式兼容），
//! 以及 `aria2.session` 条目。

pub mod control;
pub mod migrate;
pub mod session;
pub mod sqlite_control;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use control::{ControlFile, ControlFileError, ControlFileStore};
use sqlite_control::SqliteControlStore;

/// 控制文件实际形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressFormat {
    /// `<file>.aria2`（R5 原口径）。
    Legacy,
    /// `<file>.sqlite`（产品 `--sqlite-progress=true` 默认口径）。
    Sqlite,
}

impl ProgressFormat {
    pub fn suffix(self) -> &'static str {
        match self {
            ProgressFormat::Legacy => sqlite_control::LEGACY_SUFFIX,
            ProgressFormat::Sqlite => sqlite_control::SQLITE_SUFFIX,
        }
    }
}

/// 双格式控制文件存储。
///
/// 选择规则（R5 修订后）：
/// 1. 目标已有 `<file>.sqlite` → 继续用 SQLite；
/// 2. 否则已有 `<file>.aria2` → 继续用 legacy；
/// 3. 都没有 → 按 `--sqlite-progress`（产品默认 true）。
///
/// 「写入沿用载入格式」保证 engine-net 与 aria2c 可以互相接管同一任务的进度，
/// 不会因为切换引擎而凭空多出一份控制文件。
#[derive(Debug)]
pub struct ProgressStore {
    format: ProgressFormat,
    legacy: ControlFileStore,
    sqlite: SqliteControlStore,
}

impl ProgressStore {
    pub fn open(dest: &Path, sqlite_preferred: bool) -> Self {
        let sqlite_path = sqlite_control::sqlite_path(dest);
        let legacy_path = sqlite_control::legacy_path(dest);
        let format = if sqlite_path.is_file() {
            ProgressFormat::Sqlite
        } else if legacy_path.is_file() {
            ProgressFormat::Legacy
        } else if sqlite_preferred {
            ProgressFormat::Sqlite
        } else {
            ProgressFormat::Legacy
        };
        Self {
            format,
            legacy: ControlFileStore::new(legacy_path),
            sqlite: SqliteControlStore::new(sqlite_path),
        }
    }

    pub fn format(&self) -> ProgressFormat {
        self.format
    }

    /// 当前写入格式对应的路径。
    pub fn path(&self) -> &Path {
        match self.format {
            ProgressFormat::Legacy => self.legacy.path(),
            ProgressFormat::Sqlite => self.sqlite.path(),
        }
    }

    /// 两种格式都尝试读取（以选中的格式为准；选中格式无记录时再看另一份）。
    pub fn load(&self) -> Result<Option<ControlFile>, ControlFileError> {
        let primary = match self.format {
            ProgressFormat::Legacy => self.legacy.load()?,
            ProgressFormat::Sqlite => self.sqlite.load()?,
        };
        if primary.is_some() {
            return Ok(primary);
        }
        match self.format {
            ProgressFormat::Legacy => self.sqlite.load(),
            ProgressFormat::Sqlite => self.legacy.load(),
        }
    }

    pub fn save(&mut self, control: &ControlFile) -> Result<bool, ControlFileError> {
        match self.format {
            ProgressFormat::Legacy => self.legacy.save(control),
            ProgressFormat::Sqlite => self.sqlite.save(control),
        }
    }

    /// 删除两份控制文件（含 SQLite 的 `-wal` / `-shm`）。
    pub fn remove(&self) -> std::io::Result<()> {
        self.legacy.remove()?;
        self.sqlite.remove()
    }
}

/// 兼容旧调用点：legacy 路径（`<file>.aria2`）。
pub fn control_path(dest: &Path) -> PathBuf {
    sqlite_control::legacy_path(dest)
}

/// 异步落盘：把 SQLite 写操作移到 `spawn_blocking`，绝不阻塞 tokio 事件循环。
///
/// Phase 4 调度器每完成一个 piece 调一次；`Arc<Mutex<..>>` 保证写入串行，
/// 避免旧快照覆盖新快照。
pub async fn save_blocking(
    store: Arc<Mutex<ProgressStore>>,
    control: ControlFile,
) -> Result<bool, ControlFileError> {
    tokio::task::spawn_blocking(move || {
        let mut guard = store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.save(&control)
    })
    .await
    .map_err(|err| ControlFileError::Io(format!("进度落盘任务失败：{err}")))?
}

/// 同步落盘（暂停/终止等必须「返回即已写」的路径）。
///
/// 多线程 runtime 上使用 `block_in_place`，让出当前 worker 给其它任务；
/// current_thread runtime（单测）直接执行，避免 `block_in_place` 约束。
pub fn save_now(store: &Mutex<ProgressStore>, control: &ControlFile) -> Result<bool, ControlFileError> {
    let write = || {
        let mut guard = store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.save(control)
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(write)
        }
        _ => write(),
    }
}
