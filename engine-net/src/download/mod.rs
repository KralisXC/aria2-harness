//! Phase 4：多段 Range 并发调度（动态切割）。
//!
//! 入口 [`run`] 负责：
//! 1. 用 `Range: bytes=0-0` 探测（复用 [`HttpSession`]，与调度器共享连接池）；
//! 2. 校验/初始化 `ControlFile` 位图（允许空洞，跨引擎迁移的载体）；
//! 3. 满足条件时交给 [`scheduler`] 多 worker 调度，否则回退单连接路径；
//! 4. 处理 416/总长变化导致的**一次**重规划，保证不会形成重试风暴。

pub mod scheduler;
pub mod worker;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::dns::resolver::DnsResolver;
use crate::error::NetworkError;
use crate::http::client::HttpOptions;
use crate::http::session::{probe_range, HttpSession};
use crate::progress::control::ControlFile;
use crate::progress::ProgressStore;

/// aria2 `OptionHandlerFactory.cc`: `--min-split-size` 默认 20MiB。
pub const DEFAULT_MIN_SPLIT_SIZE: u64 = 20 * 1024 * 1024;

/// 动态切割参数（任务级，由 RPC 层解析）。
#[derive(Debug, Clone)]
pub struct SlicingOptions {
    pub min_split_size: u64,
    pub enable_dynamic_slicing: bool,
    pub stall: Duration,
    pub max_preempts: u8,
    pub victim_cooldown: Duration,
    /// 0 表示不限速。
    pub max_overall_download_limit: u64,
    pub max_download_limit: u64,
}

impl Default for SlicingOptions {
    fn default() -> Self {
        Self {
            min_split_size: DEFAULT_MIN_SPLIT_SIZE,
            enable_dynamic_slicing: true,
            stall: Duration::from_secs(12),
            max_preempts: 2,
            victim_cooldown: Duration::from_secs(5),
            max_overall_download_limit: 0,
            max_download_limit: 0,
        }
    }
}

impl SlicingOptions {
    /// 有效限速（B/s）：两个限速取更严格者，0 表示不限。
    pub fn effective_limit(&self) -> u64 {
        match (self.max_overall_download_limit, self.max_download_limit) {
            (0, 0) => 0,
            (a, 0) => a,
            (0, b) => b,
            (a, b) => a.min(b),
        }
    }
}

/// worker 数：`min(stream_limit, total/min_split_size, 剩余片数)`。
/// `total < 2*min_split_size` 时强制单连接。
pub fn planned_workers(
    total: u64,
    piece_length: u32,
    stream_limit: usize,
    min_split_size: u64,
    remaining_pieces: u64,
) -> usize {
    if total == 0 || piece_length == 0 || min_split_size == 0 {
        return 1;
    }
    if total < min_split_size.saturating_mul(2) {
        return 1;
    }
    let by_size = (total / min_split_size) as usize;
    let by_pieces = remaining_pieces.max(1) as usize;
    stream_limit.max(1).min(by_size.max(1)).min(by_pieces).max(1)
}

/// 控制文件的共享槽位：registry 用它在暂停时读取精确位图，
/// `download::run` 在探测拿到总长后写入/替换具体对象。
pub type SharedControl = Arc<Mutex<Option<Arc<Mutex<ControlFile>>>>>;

pub struct RunInput {
    pub url: String,
    pub dest: PathBuf,
    pub http: HttpOptions,
    pub piece_length: u32,
    pub slicing: SlicingOptions,
    pub control: SharedControl,
    pub store: Arc<Mutex<ProgressStore>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Completed { total: u64 },
    /// 不支持 Range、长度过短或中途失去 Range：调用方走单连接路径。
    SingleConnection,
}

/// 多段调度进度：除字节数外，携带分片百分比与活跃连接数。
///
/// `pieces = None` 表示本次不覆盖上层已存的分片快照（事件驱动上报时为控成本
/// 只更新字节数与 worker 数，真正的分片快照由 1s ticker 提供）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunProgress {
    pub completed: u64,
    pub total: u64,
    pub pieces: Option<Vec<u8>>,
    pub active_workers: Option<u32>,
}

/// 探测并调度单个 URI。
pub async fn run<F>(
    input: RunInput,
    resolver: &DnsResolver,
    mut on_progress: F,
) -> Result<RunOutcome, NetworkError>
where
    F: FnMut(RunProgress),
{
    let session = HttpSession::new(input.http.clone(), resolver.clone())?;
    // 前端下发的并发参数是否真的穿透到这里，只看这一行即可。
    tracing::info!(
        "[scheduler] options: split={:?}, max-connection-per-server={}, stream_limit={}, min_split_size={}, piece_length={}",
        input.http.split,
        input.http.max_connection_per_server,
        input.http.stream_limit(),
        input.slicing.min_split_size,
        input.piece_length
    );
    let existing = existing_control(&input.control);
    let existing_completed = existing
        .as_ref()
        .map(ControlFile::completed_bytes)
        .unwrap_or(0);

    // 已有完成位图时**不做探测**（对齐 aria2）：控制文件里的总长就是真相，
    // 直接发续传 Range。若服务器忽略 Range，单连接/调度路径按 §5 返回 8。
    // 这样既避免了对「bytes=0-0 返回 200」的服务器误判为不支持 Range，
    // 也不会额外消耗一条连接。
    let resumable = existing
        .as_ref()
        .filter(|control| existing_completed > 0 && control.total_length > 0)
        .map(|control| control.total_length);

    let (total, final_url) = match resumable {
        Some(total) if total == 0 => return Ok(RunOutcome::Completed { total: 0 }),
        Some(total) => (total, input.url.clone()),
        None => {
            let probe = session.get_range(&input.url, Some(probe_range())).await?;
            let status = probe.status.as_u16();
            let final_url = probe.final_url.clone();
            let total = match status {
                206 => match probe.content_range.and_then(|(_, _, total)| total) {
                    Some(0) => return Ok(RunOutcome::Completed { total: 0 }),
                    Some(total) => total,
                    None => {
                        drop(probe);
                        return Err(NetworkError::RangeNotSupported);
                    }
                },
                200 => {
                    drop(probe);
                    tracing::info!(
                        "[scheduler] planned workers: 1 (reason=server-no-range, url={})",
                        input.url
                    );
                    return Ok(RunOutcome::SingleConnection);
                }
                416 => match probe.unsatisfied_total {
                    Some(0) => return Ok(RunOutcome::Completed { total: 0 }),
                    Some(total) => total,
                    None => {
                        drop(probe);
                        return Err(NetworkError::RangeNotSupported);
                    }
                },
                other => {
                    drop(probe);
                    return Err(NetworkError::HttpStatus(other));
                }
            };
            drop(probe);
            (total, final_url)
        }
    };

    let mut control = match install_control(&input, total) {
        Ok(control) => control,
        Err(err) => return Err(err),
    };

    // 位图已经全部完成：直接返回（跨引擎接管的完成态）。
    if control.lock().unwrap().is_complete() {
        return Ok(RunOutcome::Completed { total });
    }

    // 短文件即使支持 Range 也走单连接（对齐 aria2 的最小切割阈值）。
    let stream_limit = input.http.stream_limit();
    let mut total = total;
    let mut restarted = false;

    loop {
        let (remaining, _done) = {
            let guard = control.lock().unwrap();
            let done = (0..guard.num_pieces())
                .filter(|idx| guard.is_piece_done(*idx))
                .count() as u64;
            (guard.num_pieces() - done, done)
        };
        // aria2 语义：总长不足 2*min-split-size 不切割，直接走单连接。
        if total < input.slicing.min_split_size.saturating_mul(2) {
            tracing::info!("[scheduler] file too small, fallback");
            tracing::info!(
                "[scheduler] planned workers: 1 (reason=below-min-split, total={total}, min_split_size={})",
                input.slicing.min_split_size
            );
            return Ok(RunOutcome::SingleConnection);
        }
        let planned = planned_workers(
            total,
            input.piece_length,
            stream_limit,
            input.slicing.min_split_size,
            remaining,
        );
        tracing::info!(
            "[scheduler] planned workers: {planned} (total={total}, piece_length={}, stream_limit={stream_limit}, min_split_size={}, remaining_pieces={remaining})",
            input.piece_length,
            input.slicing.min_split_size
        );

        let scheduler_input = scheduler::SchedulerInput {
            session: Arc::clone(&session),
            url: final_url.clone(),
            dest: input.dest.clone(),
            total,
            piece_length: input.piece_length,
            planned,
            read_timeout: input.http.read_timeout,
            max_tries: input.http.max_tries,
            retry_wait: input.http.retry_wait,
            slicing: input.slicing.clone(),
            control: Arc::clone(&control),
            store: Arc::clone(&input.store),
            restarted,
        };

        match scheduler::run(scheduler_input, &mut on_progress).await? {
            scheduler::SchedulerOutcome::Completed { total: finished } => {
                return Ok(RunOutcome::Completed { total: finished })
            }
            scheduler::SchedulerOutcome::FallbackSingle => {
                return Ok(RunOutcome::SingleConnection)
            }
            scheduler::SchedulerOutcome::Restart { total: new_total } => {
                // 调度器保证只在「无已完成字节」时提出重规划，且最多一次。
                if restarted {
                    return Err(NetworkError::RangeNotSupported);
                }
                restarted = true;
                total = new_total;
                control = replace_control(&input, new_total)?;
            }
        }
    }
}

fn existing_control(control: &SharedControl) -> Option<ControlFile> {
    let guard = control.lock().unwrap_or_else(|p| p.into_inner());
    guard.as_ref().map(|control| {
        control
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    })
}

fn existing_completed_bytes(control: &SharedControl) -> u64 {
    existing_control(control)
        .map(|control| control.completed_bytes())
        .unwrap_or(0)
}

/// 校验并安装控制文件；不一致且已有进度 → CANNOT_RESUME(8)。
fn install_control(input: &RunInput, total: u64) -> Result<Arc<Mutex<ControlFile>>, NetworkError> {
    let mut slot = input.control.lock().unwrap_or_else(|p| p.into_inner());
    let existing = slot
        .as_ref()
        .map(|control| control.lock().unwrap_or_else(|p| p.into_inner()).clone());
    let control = match existing {
        Some(existing)
            if existing.total_length == total && existing.piece_length == input.piece_length =>
        {
            existing
        }
        Some(existing) if existing.completed_bytes() > 0 => {
            return Err(NetworkError::RangeNotSupported);
        }
        _ => ControlFile::new(total, input.piece_length),
    };
    let shared = Arc::new(Mutex::new(control));
    *slot = Some(Arc::clone(&shared));
    Ok(shared)
}

/// 416/总长变化后的重规划：只允许在「无已完成字节」时调用。
fn replace_control(input: &RunInput, total: u64) -> Result<Arc<Mutex<ControlFile>>, NetworkError> {
    if existing_completed_bytes(&input.control) > 0 {
        return Err(NetworkError::RangeNotSupported);
    }
    let mut slot = input.control.lock().unwrap_or_else(|p| p.into_inner());
    let shared = Arc::new(Mutex::new(ControlFile::new(total, input.piece_length)));
    *slot = Some(Arc::clone(&shared));
    Ok(shared)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planned_workers_follow_split_and_size() {
        // 64MiB / 1MiB 片，min-split 20MiB，split=4 → min(4, 3, 64) = 3。
        assert_eq!(planned_workers(64 << 20, 1 << 20, 4, 20 << 20, 64), 3);
        // 短文件强制单连接。
        assert_eq!(planned_workers(16 << 20, 1 << 20, 4, 20 << 20, 16), 1);
        // split=1 恒为 1。
        assert_eq!(planned_workers(128 << 20, 1 << 20, 1, 4 << 20, 128), 1);
        // 剩余片数不足时按片数截断。
        assert_eq!(planned_workers(128 << 20, 1 << 20, 8, 4 << 20, 2), 2);
    }

    #[test]
    fn effective_limit_prefers_stricter_cap() {
        let mut options = SlicingOptions::default();
        assert_eq!(options.effective_limit(), 0);
        options.max_overall_download_limit = 1024;
        assert_eq!(options.effective_limit(), 1024);
        options.max_download_limit = 512;
        assert_eq!(options.effective_limit(), 512);
    }
}
