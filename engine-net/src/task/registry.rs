//! GID → Task 映射（红线 R1：`fffe` + 12 位十六进制）。
//!
//! Phase 1 用内存 registry；落库到 `aria2-harness.db` 的 engine 列留 Phase 2。
//!
//! 状态机：`waiting → active → complete / error`，可经 `pause → paused → unpause`
//! 往返；`removed` 为终态。每次启动/暂停/恢复/删除都推进 generation（见
//! `task::cancel`），迟到的进度回调按 generation 丢弃。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::backpressure::{Backpressure, DEFAULT_MAX_CONNECTIONS};
use crate::dns::resolver::DnsResolver;
use crate::download::{self, SharedControl, SlicingOptions};
use crate::error::NetworkError;
use crate::http::client::{DownloadRequest, HttpOptions, ProgressUpdate, download_with_progress};
use crate::progress::ProgressStore;
use crate::progress::control::{ControlFile, DEFAULT_PIECE_LENGTH};
use crate::task::cancel::{bump, is_stale};
use crate::task::event::{TaskEvent, TaskEventKind};
use crate::GID_PREFIX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// aria2 `waiting`
    Pending,
    /// aria2 `active`
    Active,
    /// aria2 `paused`
    Paused,
    /// aria2 `complete`
    Complete,
    /// aria2 `error`
    Error,
    /// aria2 `removed`
    Removed,
}

impl TaskStatus {
    /// RPC `tellStatus.status` 的字符串值。
    pub fn as_aria2(self) -> &'static str {
        match self {
            TaskStatus::Pending => "waiting",
            TaskStatus::Active => "active",
            TaskStatus::Paused => "paused",
            TaskStatus::Complete => "complete",
            TaskStatus::Error => "error",
            TaskStatus::Removed => "removed",
        }
    }

    fn terminal(self) -> bool {
        matches!(
            self,
            TaskStatus::Complete | TaskStatus::Error | TaskStatus::Removed
        )
    }
}

/// 建任务所需的全部信息（RPC 层把 options 解析成这个结构）。
#[derive(Debug, Clone)]
pub struct TaskSpec {
    pub uris: Vec<String>,
    /// 落盘绝对/相对路径（`dir` + `out` 已在 RPC 层解析）。
    pub dest: PathBuf,
    pub http: HttpOptions,
    pub piece_length: u32,
    /// Phase 4：动态切割参数。
    pub slicing: SlicingOptions,
    /// 全新任务使用哪种控制文件（对齐 `--sqlite-progress`，产品默认 true）。
    /// 已有控制文件的任务按实际存在的格式写入，不受此值影响。
    pub sqlite_progress: bool,
}

impl TaskSpec {
    pub fn new(uris: Vec<String>, dest: PathBuf) -> Self {
        Self {
            uris,
            dest,
            http: HttpOptions::default(),
            piece_length: DEFAULT_PIECE_LENGTH,
            slicing: SlicingOptions::default(),
            sqlite_progress: true,
        }
    }
}

/// `tellStatus` 所需的任务快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub gid: String,
    pub status: TaskStatus,
    pub total_length: u64,
    pub completed_length: u64,
    pub download_speed: u64,
    pub dest: PathBuf,
    pub uris: Vec<String>,
    pub error_code: Option<i32>,
    pub error_message: Option<String>,
    pub piece_length: u32,
    /// 每片完成百分比（0..=100）；空数组表示无切片信息（单连接或未开始）。
    pub piece_progress: Vec<u8>,
    /// 当前活跃 worker（并发连接）数。
    pub active_workers: u32,
    /// 预计剩余秒数（0 = 无法估算或已完成）。
    pub eta: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TaskError {
    #[error("GID `{0}` 不存在")]
    GidNotFound(String),
    #[error("任务状态不允许该操作：{0}")]
    InvalidState(String),
    #[error("addUri 至少需要一个 URI")]
    NoUri,
}

struct TaskState {
    status: TaskStatus,
    total_length: u64,
    completed_length: u64,
    error_code: Option<i32>,
    error_message: Option<String>,
    started_at: Option<Instant>,
    bytes_at_start: u64,
    piece_progress: Vec<u8>,
    active_workers: u32,
}

impl TaskState {
    fn new() -> Self {
        Self {
            status: TaskStatus::Pending,
            total_length: 0,
            completed_length: 0,
            error_code: None,
            error_message: None,
            started_at: None,
            bytes_at_start: 0,
            piece_progress: Vec::new(),
            active_workers: 0,
        }
    }
}

struct TaskHandle {
    gid: String,
    spec: TaskSpec,
    state: Mutex<TaskState>,
    generation: AtomicU64,
    join: Mutex<Option<JoinHandle<()>>>,
    events: broadcast::Sender<TaskEvent>,
    /// Phase 4：调度器维护的实时位图；暂停时读取它做「精确落盘」。
    shared_control: SharedControl,
    /// `onDownloadProgress` 的节流时间戳（1s 一拍，与前端轮询同频）。
    last_progress: Mutex<Option<Instant>>,
}

/// `onDownloadProgress` 的最小间隔：避免 WebSocket 被高频进度帧淹没。
const PROGRESS_EVENT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

impl TaskHandle {
    /// 向 service 侧广播任务事件（无订阅者时静默丢弃）。
    fn emit(&self, kind: TaskEventKind) {
        let _ = self
            .events
            .send(TaskEvent::new(self.gid.clone(), kind));
    }

    /// `onDownloadProgress`：携带完整快照，序列化时数值字段全部转字符串。
    fn emit_progress(&self, snapshot: TaskSnapshot) {
        let _ = self
            .events
            .send(TaskEvent::with_progress(self.gid.clone(), snapshot));
    }
    fn snapshot(&self) -> TaskSnapshot {
        let state = self.state.lock().unwrap();
        let speed = match (state.status, state.started_at) {
            (TaskStatus::Active, Some(started)) => {
                let elapsed = started.elapsed().as_secs_f64();
                if elapsed > 0.0 {
                    ((state.completed_length.saturating_sub(state.bytes_at_start)) as f64 / elapsed)
                        as u64
                } else {
                    0
                }
            }
            _ => 0,
        };
        // ETA 在后端统一计算，前端只做展示：剩余字节 / 当前速度，封顶 999999 秒。
        let remaining = state.total_length.saturating_sub(state.completed_length);
        let eta = if speed > 0 && remaining > 0 {
            (remaining / speed).min(999_999)
        } else {
            0
        };
        TaskSnapshot {
            gid: self.gid.clone(),
            status: state.status,
            total_length: state.total_length,
            completed_length: state.completed_length,
            download_speed: speed,
            dest: self.spec.dest.clone(),
            uris: self.spec.uris.clone(),
            error_code: state.error_code,
            error_message: state.error_message.clone(),
            piece_length: self.spec.piece_length,
            piece_progress: state.piece_progress.clone(),
            active_workers: state.active_workers,
            eta,
        }
    }

    fn mark_active(&self, generation: u64) {
        if is_stale(generation, self.generation.load(Ordering::SeqCst)) {
            return;
        }
        let started = {
            let mut state = self.state.lock().unwrap();
            if state.status == TaskStatus::Removed || state.status.terminal() {
                return;
            }
            let started = state.status != TaskStatus::Active;
            state.status = TaskStatus::Active;
            state.error_code = None;
            state.error_message = None;
            state.started_at = Some(Instant::now());
            state.bytes_at_start = state.completed_length;
            started
        };
        if started {
            // 新一轮下载：下一条进度立即推送，不必等满一个节流周期。
            *self
                .last_progress
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            self.emit(TaskEventKind::Start);
        }
    }

    fn set_status(&self, generation: u64, status: TaskStatus) -> bool {
        if is_stale(generation, self.generation.load(Ordering::SeqCst)) {
            return false;
        }
        let (changed, previous) = {
            let mut state = self.state.lock().unwrap();
            if is_stale(generation, self.generation.load(Ordering::SeqCst)) {
                return false;
            }
            if state.status == TaskStatus::Removed {
                return false;
            }
            let previous = state.status;
            state.status = status;
            state.started_at = None;
            // 离开 Active（暂停/移除/终态）后不再有活跃连接。
            state.active_workers = 0;
            (previous != status, previous)
        };
        if changed {
            match status {
                TaskStatus::Paused => self.emit(TaskEventKind::Pause),
                // aria2 只在非终态任务被移除时发 onDownloadStop；
                // 已完成/已失败任务的 removeDownloadResult 不产生通知。
                TaskStatus::Removed if !previous.terminal() => self.emit(TaskEventKind::Stop),
                _ => {}
            }
        }
        true
    }

    fn fail(&self, generation: u64, code: i32, message: String) {
        if is_stale(generation, self.generation.load(Ordering::SeqCst)) {
            return;
        }
        {
            let mut state = self.state.lock().unwrap();
            if state.status == TaskStatus::Removed {
                return;
            }
            state.status = TaskStatus::Error;
            state.error_code = Some(code);
            state.error_message = Some(message.clone());
            state.started_at = None;
            state.active_workers = 0;
        }
        tracing::warn!(gid = %self.gid, code, %message, "任务失败");
        self.emit(TaskEventKind::Error);
    }

    fn complete(&self, generation: u64, total: u64, completed: u64) {
        if is_stale(generation, self.generation.load(Ordering::SeqCst)) {
            return;
        }
        {
            let mut state = self.state.lock().unwrap();
            if state.status == TaskStatus::Removed {
                return;
            }
            state.status = TaskStatus::Complete;
            state.total_length = total;
            state.completed_length = completed;
            state.error_code = None;
            state.error_message = None;
            state.started_at = None;
            state.active_workers = 0;
            if state.total_length > 0 {
                if state.piece_progress.is_empty() {
                    state.piece_progress = vec![100];
                } else {
                    state.piece_progress.iter_mut().for_each(|value| *value = 100);
                }
            }
        }
        self.emit(TaskEventKind::Complete);
    }

    fn update_progress(&self, generation: u64, update: ProgressUpdate, base: u64) {
        if is_stale(generation, self.generation.load(Ordering::SeqCst)) {
            return;
        }
        {
            let mut state = self.state.lock().unwrap();
            if state.status == TaskStatus::Removed || state.status.terminal() {
                return;
            }
            if let Some(total) = update.total_length {
                state.total_length = total;
            }
            state.completed_length = state.completed_length.max(base + update.written);
            if let Some(pieces) = update.pieces {
                state.piece_progress = pieces;
            } else if state.piece_progress.is_empty() && state.total_length > 0 {
                // 单连接 / 无切片数据：退化为单片百分比，前端据此仍能画一段进度。
                let percent = (state
                    .completed_length
                    .saturating_mul(100)
                    / state.total_length)
                    .min(100) as u8;
                state.piece_progress = vec![percent];
            }
            if let Some(workers) = update.active_workers {
                state.active_workers = workers;
            }
        }

        // 进度事件按 1s 节流（与前端轮询同频）：锁外判断，避免把快照锁嵌套进节流锁。
        let due = {
            let mut slot = self
                .last_progress
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match *slot {
                Some(at) if at.elapsed() < PROGRESS_EVENT_INTERVAL => false,
                _ => {
                    *slot = Some(Instant::now());
                    true
                }
            }
        };
        if due {
            self.emit_progress(self.snapshot());
        }
    }

    fn current_progress(&self) -> (u64, u64) {
        let state = self.state.lock().unwrap();
        (state.total_length, state.completed_length)
    }

    /// 暂停/取消前把当前进度写成控制文件（总长未知时不写）。
    fn persist_control(&self) {
        // 优先用调度器的实时位图（含空洞、精确到 piece）。
        let snapshot = self
            .shared_control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .map(|control| {
                control
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
            });
        let control = match snapshot {
            Some(control) => control,
            None => {
                let (total, completed) = self.current_progress();
                if total == 0 {
                    return;
                }
                let mut fresh = ControlFile::new(total, self.spec.piece_length);
                fresh.mark_completed_bytes(completed);
                fresh
            }
        };
        if control.total_length == 0 {
            return;
        }
        let store = Mutex::new(ProgressStore::open(&self.spec.dest, self.spec.sqlite_progress));
        if let Err(err) = crate::progress::save_now(&store, &control) {
            tracing::warn!(gid = %self.gid, %err, "暂停时进度落盘失败");
        }
    }

    fn abort(&self) {
        if let Some(join) = self.join.lock().unwrap().take() {
            join.abort();
        }
    }

    /// 中止协程，并在协程**真正被 drop**（文件/SQLite 句柄关闭）之后清理控制文件。
    ///
    /// Windows 上若在协程仍持有句柄时删除 `<file>.sqlite` / 目标文件，会得到
    /// `os error 32（另一个程序正在使用此文件）`。`JoinHandle::abort()` 只是请求取消，
    /// 必须 `await` 它才能确保 future 已释放。
    fn abort_and_release(&self, cleanup_control: bool) {
        let join = self.join.lock().unwrap().take();
        let dest = self.spec.dest.clone();
        let sqlite = self.spec.sqlite_progress;
        if let Some(join) = join {
            join.abort();
            if cleanup_control {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move {
                        let _ = join.await;
                        let _ = ProgressStore::open(&dest, sqlite).remove();
                    });
                    return;
                }
            }
        }
        if cleanup_control {
            let _ = ProgressStore::open(&dest, sqlite).remove();
        }
    }
}

/// 任务注册表。`DnsResolver` 可 Clone，registry 自身 `Send + Sync`。
pub struct Registry {
    resolver: DnsResolver,
    tasks: Mutex<HashMap<String, Arc<TaskHandle>>>,
    next_id: AtomicU64,
    gate: Arc<Backpressure>,
    events: broadcast::Sender<TaskEvent>,
}

impl Registry {
    pub fn new(resolver: DnsResolver) -> Self {
        Self::with_limit(resolver, DEFAULT_MAX_CONNECTIONS)
    }

    pub fn with_limit(resolver: DnsResolver, max_connections: usize) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            resolver,
            tasks: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            gate: Backpressure::new(max_connections),
            events,
        }
    }

    /// 订阅任务事件（service 在 6800 上转发为 `aria2.onDownload*` 通知）。
    pub fn subscribe_events(&self) -> broadcast::Receiver<TaskEvent> {
        self.events.subscribe()
    }

    /// 注册并立即调度下载；返回 16 位 GID（`fffe` + 12 位十六进制）。
    pub fn add_uri(&self, spec: TaskSpec) -> Result<String, TaskError> {
        if spec.uris.is_empty() {
            return Err(TaskError::NoUri);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let gid = format!("{GID_PREFIX:04x}{id:012x}");
        let handle = Arc::new(TaskHandle {
            gid: gid.clone(),
            spec,
            state: Mutex::new(TaskState::new()),
            generation: AtomicU64::new(0),
            join: Mutex::new(None),
            events: self.events.clone(),
            shared_control: Arc::new(Mutex::new(None)),
            last_progress: Mutex::new(None),
        });
        self.tasks
            .lock()
            .unwrap()
            .insert(gid.clone(), Arc::clone(&handle));
        tracing::info!(gid = %gid, uris = ?handle.spec.uris, "任务已创建");

        let generation = handle.generation.load(Ordering::SeqCst);
        self.spawn(handle, generation);
        Ok(gid)
    }

    fn spawn(&self, handle: Arc<TaskHandle>, generation: u64) {
        let resolver = self.resolver.clone();
        let gate = Arc::clone(&self.gate);
        let task = Arc::clone(&handle);
        let join = tokio::spawn(async move {
            run_task(task, resolver, gate, generation).await;
        });
        *handle.join.lock().unwrap() = Some(join);
    }

    pub fn snapshot(&self, gid: &str) -> Option<TaskSnapshot> {
        self.get(gid).ok().map(|handle| handle.snapshot())
    }

    pub fn list(&self) -> Vec<TaskSnapshot> {
        self.tasks
            .lock()
            .unwrap()
            .values()
            .map(|handle| handle.snapshot())
            .collect()
    }

    /// 当前任务总数（`getSessionInfo` 等只读用途）。
    pub fn count(&self) -> usize {
        self.tasks.lock().unwrap().len()
    }

    pub fn pause(&self, gid: &str) -> Result<(), TaskError> {
        let handle = self.get(gid)?;
        {
            let state = handle.state.lock().unwrap();
            if !matches!(state.status, TaskStatus::Pending | TaskStatus::Active) {
                return Err(TaskError::InvalidState(format!(
                    "状态 `{}` 不能暂停",
                    state.status.as_aria2()
                )));
            }
        }
        // 先推进 generation（迟到回调立即失效），再落控制文件、最后 abort。
        let generation = bump(handle.generation.load(Ordering::SeqCst));
        handle.generation.store(generation, Ordering::SeqCst);
        handle.persist_control();
        handle.abort();
        handle.set_status(generation, TaskStatus::Paused);
        Ok(())
    }

    pub fn unpause(&self, gid: &str) -> Result<(), TaskError> {
        let handle = self.get(gid)?;
        {
            let state = handle.state.lock().unwrap();
            if state.status != TaskStatus::Paused {
                return Err(TaskError::InvalidState(format!(
                    "状态 `{}` 不能恢复",
                    state.status.as_aria2()
                )));
            }
        }
        let generation = bump(handle.generation.load(Ordering::SeqCst));
        handle.generation.store(generation, Ordering::SeqCst);
        handle.set_status(generation, TaskStatus::Pending);
        self.spawn(handle, generation);
        Ok(())
    }

    /// 删除任务：推进 generation、终止协程、清理控制文件。
    pub fn remove(&self, gid: &str) -> Result<(), TaskError> {
        let handle = self.get(gid)?;
        let generation = bump(handle.generation.load(Ordering::SeqCst));
        handle.generation.store(generation, Ordering::SeqCst);
        handle.set_status(generation, TaskStatus::Removed);
        // 句柄释放后再删控制文件（见 `abort_and_release` 的 Windows 说明）。
        handle.abort_and_release(true);
        Ok(())
    }

    /// `aria2.removeDownloadResult`：删除**单条**已停止任务的结果记录。
    ///
    /// 与 [`Registry::remove`] 的区别：`remove` 对齐 aria2 语义，只把任务置为 `removed`，
    /// 结果仍留在 `tellStopped` 里等 `removeDownloadResult` 清理；这里才是真正把记录
    /// 从任务表摘掉，使 `tellActive/tellWaiting/tellStopped/tellStatus` 都不再找得到它。
    ///
    /// 对仍在运行的任务按 aria2 语义报错（要求先 remove / forceRemove），避免把
    /// 活动任务的句柄直接摘掉造成协程泄漏。
    pub fn remove_result(&self, gid: &str) -> Result<(), TaskError> {
        let handle = self.get(gid)?;
        if matches!(
            handle.snapshot().status,
            TaskStatus::Pending | TaskStatus::Active | TaskStatus::Paused
        ) {
            return Err(TaskError::InvalidState(format!(
                "状态 `{}` 不能移除结果记录",
                handle.snapshot().status.as_aria2()
            )));
        }
        let generation = bump(handle.generation.load(Ordering::SeqCst));
        handle.generation.store(generation, Ordering::SeqCst);
        handle.set_status(generation, TaskStatus::Removed);
        // 句柄释放后再删控制文件（见 `abort_and_release` 的 Windows 说明）。
        handle.abort_and_release(true);
        self.tasks.lock().unwrap().remove(gid);
        Ok(())
    }

    /// `aria2.purgeDownloadResult`：清掉所有已停止（complete/error/removed）的记录。
    ///
    /// 返回被清理的条数；控制文件同样在协程释放后删除。
    pub fn purge_results(&self) -> usize {
        let handles: Vec<Arc<TaskHandle>> = self
            .tasks
            .lock()
            .unwrap()
            .values()
            .filter(|handle| handle.snapshot().status.terminal())
            .cloned()
            .collect();
        for handle in &handles {
            handle.abort_and_release(true);
            self.tasks.lock().unwrap().remove(&handle.gid);
        }
        handles.len()
    }

    /// `aria2.pauseAll` / `aria2.forcePauseAll`：暂停所有 active / waiting 任务。
    pub fn pause_all(&self) -> usize {
        let gids: Vec<String> = self
            .list()
            .into_iter()
            .filter(|snapshot| matches!(snapshot.status, TaskStatus::Pending | TaskStatus::Active))
            .map(|snapshot| snapshot.gid)
            .collect();
        gids.iter().filter(|gid| self.pause(gid).is_ok()).count()
    }

    /// `aria2.unpauseAll`：恢复所有 paused 任务。
    pub fn unpause_all(&self) -> usize {
        let gids: Vec<String> = self
            .list()
            .into_iter()
            .filter(|snapshot| snapshot.status == TaskStatus::Paused)
            .map(|snapshot| snapshot.gid)
            .collect();
        gids.iter()
            .filter(|gid| self.unpause(gid).is_ok())
            .count()
    }

    fn get(&self, gid: &str) -> Result<Arc<TaskHandle>, TaskError> {
        self.tasks
            .lock()
            .unwrap()
            .get(gid)
            .cloned()
            .ok_or_else(|| TaskError::GidNotFound(gid.to_string()))
    }
}

/// aria2 的控制文件路径规则：目标文件 + `.aria2`。
pub fn control_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_os_string();
    name.push(".aria2");
    PathBuf::from(name)
}

async fn run_task(
    task: Arc<TaskHandle>,
    resolver: DnsResolver,
    gate: Arc<Backpressure>,
    generation: u64,
) {
    // 等待下载槽位（背压）；等待期间 tellStatus 显示 waiting。
    let _permit = gate.acquire().await;
    if is_stale(generation, task.generation.load(Ordering::SeqCst)) {
        return;
    }
    task.mark_active(generation);
    tracing::info!(gid = %task.gid, url = %task.spec.uris[0], "开始下载");

    let store = Arc::new(Mutex::new(ProgressStore::open(
        &task.spec.dest,
        task.spec.sqlite_progress,
    )));
    let mut tracker = match store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .load()
    {
        Ok(existing) => existing,
        // 对齐 C++：`SqliteProgressInfoFile::load()` / `DefaultBtProgressInfoFile::load()`
        // 解析失败时调用 `resetPieceStorageState()` 后**继续下载**，不终止任务。
        // 这里等价处理：记 Warning、从零开始；损坏的控制文件会在下次保存时被覆盖，
        // 任务完成/删除时由 `ProgressStore::remove()` 一并清理。
        Err(err) => {
            tracing::warn!(
                gid = %task.gid,
                dest = %task.spec.dest.display(),
                error = %err,
                "控制文件解析失败，按 aria2 语义静默复位进度并从零继续下载"
            );
            None
        }
    };

    if let Some(control) = tracker.as_ref() {
        // aria2 只在「已有进度或存在 in-flight」时对 piece length 变化报错 10。
        if control.piece_length != task.spec.piece_length
            && (control.completed_bytes() > 0 || !control.in_flight.is_empty())
        {
            task.fail(generation, 10, "检测到 piece length 变化".to_string());
            return;
        }
    }

    // 调度器与暂停落盘共用这份实时位图。
    {
        let mut slot = task
            .shared_control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = tracker
            .clone()
            .map(|control| Arc::new(Mutex::new(control)));
    }

    let mut completed_base = tracker
        .as_ref()
        .map(|control| control.completed_bytes())
        .unwrap_or(0);
    let mut total_seen: Option<u64> = None;
    let mut last_error: Option<NetworkError> = None;

    for url in &task.spec.uris {
        let mut attempt = 0usize;
        // false = 先尝试 Phase 4 多段调度；true = 该 URI 只能走单连接。
        let mut single_only = false;
        loop {
            if is_stale(generation, task.generation.load(Ordering::SeqCst)) {
                return;
            }

            // ---------- Phase 4：多段 Range 动态切割 ----------
            if !single_only {
                let task_ref = Arc::clone(&task);
                let input = download::RunInput {
                    url: url.clone(),
                    dest: task.spec.dest.clone(),
                    http: task.spec.http.clone(),
                    piece_length: task.spec.piece_length,
                    slicing: task.spec.slicing.clone(),
                    control: Arc::clone(&task.shared_control),
                    store: Arc::clone(&store),
                };
                let result = download::run(input, &resolver, |progress| {
                    if is_stale(generation, task_ref.generation.load(Ordering::SeqCst)) {
                        return;
                    }
                    task_ref.update_progress(
                        generation,
                        ProgressUpdate {
                            written: progress.completed,
                            total_length: Some(progress.total),
                            pieces: progress.pieces,
                            active_workers: progress.active_workers,
                        },
                        0,
                    );
                })
                .await;

                if is_stale(generation, task.generation.load(Ordering::SeqCst)) {
                    return;
                }

                match result {
                    Ok(download::RunOutcome::Completed { total }) => {
                        if let Err(err) = store
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove()
                        {
                            tracing::warn!(gid = %task.gid, %err, "清理控制文件失败");
                        }
                        tracing::info!(gid = %task.gid, total, "下载完成（多段调度）");
                        task.complete(generation, total, total);
                        return;
                    }
                    Ok(download::RunOutcome::SingleConnection) => {
                        tracker = snapshot_control(&task);
                        completed_base = tracker
                            .as_ref()
                            .map(|control| control.contiguous_prefix())
                            .unwrap_or(0);
                        // 单连接路径按连续字节推进，暂停落盘走 current_progress。
                        *task
                            .shared_control
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
                        single_only = true;
                        continue;
                    }
                    Err(err) => {
                        if err == NetworkError::Cancelled {
                            return;
                        }
                        // 调度器已按 piece 落盘；刷新本地视图后再决定重试。
                        tracker = snapshot_control(&task);
                        completed_base = tracker
                            .as_ref()
                            .map(|control| control.completed_bytes())
                            .unwrap_or(0);
                        if !err.is_retryable() || attempt + 1 >= task.spec.http.max_tries {
                            tracing::warn!(
                                gid = %task.gid,
                                url = %url,
                                %err,
                                "放弃当前 URI（多段调度）"
                            );
                            last_error = Some(err);
                            break;
                        }
                        attempt += 1;
                        if !task.spec.http.retry_wait.is_zero() {
                            tokio::time::sleep(task.spec.http.retry_wait).await;
                        }
                        continue;
                    }
                }
            }

            let request = DownloadRequest {
                url: url.clone(),
                dest: task.spec.dest.clone(),
                resume_from: completed_base,
                options: task.spec.http.clone(),
            };
            let task_ref = Arc::clone(&task);
            let mut total_mismatch = false;

            let result = download_with_progress(&request, &resolver, |update| {
                if is_stale(generation, task_ref.generation.load(Ordering::SeqCst)) {
                    return;
                }
                if let Some(total) = update.total_length {
                    total_seen = Some(total);
                }
                if let (Some(total), Some(control)) = (update.total_length, tracker.as_ref()) {
                    if control.total_length != total && completed_base > 0 {
                        total_mismatch = true;
                    }
                }

                let completed = completed_base + update.written;
                // `update` 会被移交给 update_progress，先取走落盘需要的总长。
                let total_for_control = update.total_length;
                task_ref.update_progress(generation, update, completed_base);

                if let Some(total) = total_for_control {
                    let control = tracker.get_or_insert_with(|| {
                        let mut fresh = ControlFile::new(total, task_ref.spec.piece_length);
                        fresh.mark_completed_bytes(completed_base);
                        fresh
                    });
                    if control.total_length == total && control.mark_completed_bytes(completed) {
                        // 每完成一个整片落一次盘（SHA-1 去重由 store 负责）。
                        if let Err(err) = store
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .save(control)
                        {
                            tracing::warn!(gid = %task_ref.gid, %err, "进度落盘失败");
                        }
                    }
                }
            })
            .await;

            if is_stale(generation, task.generation.load(Ordering::SeqCst)) {
                return;
            }

            match result {
                Ok(report) => {
                    if total_mismatch {
                        task.fail(generation, 8, "控制文件总长与实际响应不符".to_string());
                        return;
                    }
                    let total = report
                        .total_length
                        .or(total_seen)
                        .unwrap_or(report.completed_length);
                    if let Some(control) = tracker.as_mut() {
                        if control.total_length == total {
                            control.mark_completed_bytes(report.completed_length);
                        }
                    }
                    if let Some(control) = tracker.as_ref() {
                        if let Err(err) = store
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .save(control)
                        {
                            tracing::warn!(gid = %task.gid, %err, "进度落盘失败");
                        }
                    }
                    // aria2 完成后删除控制文件。
                    if let Err(err) = store
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove()
                    {
                        tracing::warn!(gid = %task.gid, %err, "清理控制文件失败");
                    }
                    tracing::info!(gid = %task.gid, total, "下载完成");
                    task.complete(generation, total, report.completed_length);
                    return;
                }
                Err(err) => {
                    if err == NetworkError::Cancelled {
                        return;
                    }
                    if let Some(control) = tracker.as_ref() {
                        if let Err(err) = store
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .save(control)
                        {
                            tracing::warn!(gid = %task.gid, %err, "进度落盘失败");
                        }
                    }
                    completed_base = task.current_progress().1;

                    if total_mismatch {
                        task.fail(generation, 8, "控制文件总长与实际响应不符".to_string());
                        return;
                    }

                    // 重试语义对齐 aria2：max-tries 含首次；retry-wait 默认 0。
                    let can_retry =
                        err.is_retryable() && attempt + 1 < task.spec.http.max_tries;
                    if !can_retry {
                        tracing::warn!(gid = %task.gid, url = %url, %err, "放弃当前 URI");
                        last_error = Some(err);
                        break;
                    }
                    attempt += 1;
                    tracing::warn!(
                        gid = %task.gid,
                        url = %url,
                        attempt,
                        max_tries = task.spec.http.max_tries,
                        %err,
                        "重试"
                    );
                    if !task.spec.http.retry_wait.is_zero() {
                        tokio::time::sleep(task.spec.http.retry_wait).await;
                    }
                }
            }
        }
    }

    let err = last_error.unwrap_or_else(|| NetworkError::Network("没有可用 URI".to_string()));
    task.fail(generation, err.to_aria2_code(), err.to_string());
}

fn snapshot_control(task: &TaskHandle) -> Option<ControlFile> {
    task.shared_control
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .map(|control| {
            control
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gid_prefix_is_fffe_and_sixteen_hex() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let resolver = DnsResolver::new(&Default::default()).expect("resolver");
            let registry = Registry::new(resolver);
            // 不真正跑下载：用非法 URI 的任务，随后立刻删除。
            let spec = TaskSpec::new(vec!["http://127.0.0.1:1/x".into()], PathBuf::from("x.bin"));
            let gid = registry.add_uri(spec).expect("add");
            assert_eq!(gid.len(), 16);
            assert!(gid.starts_with("fffe"));
            assert!(gid[4..].chars().all(|c| c.is_ascii_hexdigit()));
            registry.remove(&gid).expect("remove");
        });
    }

    #[test]
    fn remove_keeps_removed_status_for_tellstatus() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let resolver = DnsResolver::new(&Default::default()).expect("resolver");
            let registry = Registry::new(resolver);
            let spec = TaskSpec::new(vec!["http://127.0.0.1:1/x".into()], PathBuf::from("x.bin"));
            let gid = registry.add_uri(spec).expect("add");
            registry.remove(&gid).expect("remove");
            let snapshot = registry.snapshot(&gid).expect("snapshot");
            assert_eq!(snapshot.status, TaskStatus::Removed);
        });
    }
}
