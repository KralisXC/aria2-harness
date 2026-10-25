//! 单写者分片调度器（Phase 4）。
//!
//! 所有 piece 状态只由本协程修改：worker 通过事件通道上报，调度器用
//! `(worker, token)` 校验事件的时效性，迟到的 `Progress`/`PieceDone` 一律丢弃。
//!
//! 抢占采用两阶段：`Downloading → Reclaiming`（已发 `Stop`）→ 收到 `Stopped`
//! 才回到 `Missing` 并重新派发。绝不同时让两个连接写同一个 piece。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::download::worker::{self, PieceError, WorkerCmd, WorkerConfig, WorkerEvent};
use crate::download::{RunProgress, SlicingOptions};
use crate::error::NetworkError;
use crate::http::session::{probe_range, HttpSession};
use crate::progress::control::{num_pieces, ControlFile};
use crate::progress::ProgressStore;

/// 事件通道容量：足够吸收多 worker 的 64KiB 进度突发。
const EVENT_CHANNEL: usize = 1024;
/// 单 worker 命令通道（派发 1 条 + 抢占 Stop 1 条）。
const CMD_CHANNEL: usize = 4;
/// C++ baseline：任务速度高水位低于 100KiB/s 时不启用抢占。
const PREEMPT_SPEED_FLOOR: u64 = 100 * 1024;
/// 限速适配阈值达到该秒数时本任务禁用抢占（`SegmentMan.cc` 语义）。
const PREEMPT_LIMIT_DISABLE: Duration = Duration::from_secs(120);
/// 僵死兜底下限。
const STALL_FLOOR: Duration = Duration::from_secs(120);
/// 上报给前端的分片数上限：超出按组平均降采样，避免 WS 帧与前端 DOM 过重。
const MAX_REPORTED_PIECES: usize = 256;

pub struct SchedulerInput {
    pub session: Arc<HttpSession>,
    pub url: String,
    pub dest: PathBuf,
    pub total: u64,
    pub piece_length: u32,
    pub planned: usize,
    pub read_timeout: Option<Duration>,
    pub max_tries: usize,
    pub retry_wait: Duration,
    pub slicing: SlicingOptions,
    pub control: Arc<StdMutex<ControlFile>>,
    pub store: Arc<StdMutex<ProgressStore>>,
    /// 已经因 416/总长变化重启过一次。
    pub restarted: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SchedulerOutcome {
    Completed { total: u64 },
    /// 416/总长变化且尚无已完成字节：调用方按新总长重规划一次。
    Restart { total: u64 },
    /// 中途失去 Range 支持且尚无已完成字节：调用方走单连接。
    FallbackSingle,
}

enum PieceState {
    Missing,
    Assigned {
        owner: usize,
        token: u64,
    },
    Downloading {
        owner: usize,
        token: u64,
        written: u64,
        last_progress: Instant,
    },
    /// 已发 `Stop`，等 worker 确认后才回到 Missing。
    Reclaiming {
        owner: usize,
        token: u64,
        written: u64,
    },
    Done,
}

struct WorkerSlot {
    cmds: mpsc::Sender<WorkerCmd>,
    join: Option<JoinHandle<()>>,
    piece: Option<u64>,
}

pub async fn run<F>(
    input: SchedulerInput,
    on_progress: &mut F,
) -> Result<SchedulerOutcome, NetworkError>
where
    F: FnMut(RunProgress),
{
    // 1) 预分配：磁盘满立即返回 34，避免 worker 各自撞盘。
    {
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&input.dest)
            .await
            .map_err(NetworkError::from)?;
        file.set_len(input.total).await.map_err(NetworkError::from)?;
    }

    // 2) 位图 → 内存状态（控制文件是跨引擎迁移的真源，允许空洞）。
    let piece_count = num_pieces(input.total, input.piece_length);
    let mut pieces: Vec<PieceState> = Vec::with_capacity(piece_count as usize);
    let mut completed_done = 0u64;
    {
        let control = input
            .control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for index in 0..piece_count {
            if control.is_piece_done(index) {
                pieces.push(PieceState::Done);
                completed_done += span_len(input.total, input.piece_length, index);
            } else {
                pieces.push(PieceState::Missing);
            }
        }
    }
    if pieces.iter().all(|piece| matches!(piece, PieceState::Done)) {
        on_progress(RunProgress {
            completed: completed_done,
            total: input.total,
            pieces: Some(vec![100; pieces.len().max(1)]),
            active_workers: Some(0),
        });
        return Ok(SchedulerOutcome::Completed { total: input.total });
    }

    // 3) worker 与通道
    let worker_count = input.planned.max(1).min(piece_count.max(1) as usize);
    let (events_tx, mut events_rx) = mpsc::channel::<WorkerEvent>(EVENT_CHANNEL);
    let mut workers: Vec<WorkerSlot> = Vec::with_capacity(worker_count);
    for index in 0..worker_count {
        let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>(CMD_CHANNEL);
        let cfg = WorkerConfig {
            index,
            session: Arc::clone(&input.session),
            url: input.url.clone(),
            dest: input.dest.clone(),
            read_timeout: input.read_timeout,
            max_tries: input.max_tries.max(1),
            retry_wait: input.retry_wait,
        };
        let sender = events_tx.clone();
        let join = tokio::spawn(worker::run(cfg, cmd_rx, sender));
        workers.push(WorkerSlot {
            cmds: cmd_tx,
            join: Some(join),
            piece: None,
        });
    }
    drop(events_tx);

    // 4) 顺序落盘任务：避免并发保存让旧快照覆盖新快照。
    let (save_tx, mut save_rx) = mpsc::unbounded_channel::<ControlFile>();
    let store = Arc::clone(&input.store);
    let saver = tokio::spawn(async move {
        while let Some(control) = save_rx.recv().await {
            if let Err(err) = crate::progress::save_blocking(Arc::clone(&store), control).await {
                tracing::warn!(%err, "进度落盘失败");
            }
        }
    });

    // 5) 抢占参数
    let mut preemption_enabled =
        input.slicing.enable_dynamic_slicing && input.slicing.max_preempts > 0;
    let mut stall = input.slicing.stall;
    if let Some(limit) = Some(input.slicing.effective_limit()).filter(|value| *value > 0) {
        let adaptive = 2u64
            .saturating_mul(input.piece_length as u64)
            .saturating_mul(worker_count as u64)
            / limit;
        let threshold = stall.max(Duration::from_secs(adaptive));
        if threshold >= PREEMPT_LIMIT_DISABLE {
            preemption_enabled = false;
        } else {
            stall = threshold;
        }
    }
    let stall_deadline = stall.max(STALL_FLOOR);

    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut token_seq = 0u64;
    let mut next_missing = 0usize;
    let mut inflight_written = 0u64;
    let mut last_tick_sampled = completed_done;
    let mut speed_high_water = 0u64;
    let mut last_activity = Instant::now();
    let mut preempts: HashMap<u64, u8> = HashMap::new();
    let mut preempted_at: HashMap<u64, Instant> = HashMap::new();
    let mut replanned = false;
    let mut idle_stall_ticks = 0u8;
    let mut outcome: Option<SchedulerOutcome> = None;
    let mut error: Option<NetworkError> = None;

    dispatch(
        &mut pieces,
        &mut workers,
        &input,
        &mut token_seq,
        &mut next_missing,
    );

    while outcome.is_none() && error.is_none() {
        tokio::select! {
            maybe_event = events_rx.recv() => {
                let Some(event) = maybe_event else {
                    error = Some(NetworkError::Network("所有分片 worker 已退出".into()));
                    break;
                };
                handle_event(
                    event,
                    &input,
                    &mut pieces,
                    &mut workers,
                    &mut inflight_written,
                    &mut completed_done,
                    &mut last_activity,
                    &mut replanned,
                    &mut next_missing,
                    &save_tx,
                    on_progress,
                    &mut outcome,
                    &mut error,
                )
                .await;
            }
            _ = ticker.tick() => {
                let now = Instant::now();
                // 任务速度含在途字节：大分片慢速下载也能形成速度采样。
                let sampled = completed_done + inflight_written;
                let delta = sampled.saturating_sub(last_tick_sampled);
                last_tick_sampled = sampled;
                speed_high_water = speed_high_water.max(delta);

                if preemption_enabled
                    && speed_high_water >= PREEMPT_SPEED_FLOOR
                    && workers.iter().any(|slot| slot.piece.is_none())
                    && !pieces.iter().any(|piece| matches!(piece, PieceState::Missing))
                {
                    if let Some(victim) = pick_victim(
                        &pieces,
                        now,
                        stall,
                        input.slicing.victim_cooldown,
                        input.slicing.max_preempts,
                        &preempts,
                        &preempted_at,
                        input.piece_length,
                    ) {
                        let (owner, token, written) = match &pieces[victim as usize] {
                            PieceState::Downloading { owner, token, written, .. } => {
                                (*owner, *token, *written)
                            }
                            _ => unreachable!("victim 必须是 Downloading"),
                        };
                        if workers[owner].cmds.try_send(WorkerCmd::Stop).is_ok() {
                            pieces[victim as usize] = PieceState::Reclaiming {
                                owner,
                                token,
                                written,
                            };
                            let counter = preempts.entry(victim).or_insert(0);
                            *counter = counter.saturating_add(1);
                            preempted_at.insert(victim, now);
                            next_missing = next_missing.min(victim as usize);
                            tracing::info!(
                                piece = victim,
                                worker = owner,
                                written,
                                "动态切割：抢占僵死分片"
                            );
                        }
                    }
                }

                // 死锁兜底：有缺失片、有空闲 worker，且没有待确认的抢占。
                let idle = workers.iter().filter(|slot| slot.piece.is_none()).count();
                let missing = pieces
                    .iter()
                    .filter(|piece| matches!(piece, PieceState::Missing))
                    .count();
                let reclaiming = pieces
                    .iter()
                    .any(|piece| matches!(piece, PieceState::Reclaiming { .. }));
                if missing > 0 && idle > 0 && !reclaiming {
                    idle_stall_ticks = idle_stall_ticks.saturating_add(1);
                    if idle_stall_ticks >= 2 {
                        error = Some(NetworkError::Network("调度器无法派发分片".into()));
                        break;
                    }
                } else {
                    idle_stall_ticks = 0;
                }

                // 僵死兜底：长时间没有任何字节进展。
                if now.duration_since(last_activity) > stall_deadline {
                    error = Some(NetworkError::Timeout);
                    break;
                }

                // 每秒一次的完整分片快照：事件驱动的上报不带 pieces（控制成本），
                // 只有这里与收尾会覆盖上层的 pieceProgress。
                on_progress(RunProgress {
                    completed: completed_done + inflight_written,
                    total: input.total,
                    pieces: Some(piece_percentages(&pieces, input.total, input.piece_length)),
                    active_workers: Some(active_worker_count(&workers)),
                });
            }
        }

        if outcome.is_some() || error.is_some() {
            break;
        }

        dispatch(
            &mut pieces,
            &mut workers,
            &input,
            &mut token_seq,
            &mut next_missing,
        );

        if pieces.iter().all(|piece| matches!(piece, PieceState::Done)) {
            outcome = Some(SchedulerOutcome::Completed {
                total: input.total,
            });
        }
    }

    // 6) 收尾：停 worker → 落盘队列排空 → 最终同步落盘（暂停/失败都不丢位图）。
    for slot in &mut workers {
        let _ = slot.cmds.try_send(WorkerCmd::Shutdown);
        if let Some(join) = slot.join.take() {
            join.abort();
        }
    }
    drop(save_tx);
    let _ = saver.await;

    let snapshot = input
        .control
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Err(err) = crate::progress::save_now(&input.store, &snapshot) {
        tracing::warn!(%err, "最终进度落盘失败");
    }

    match (outcome, error) {
        (Some(outcome), _) => {
            on_progress(RunProgress {
                completed: snapshot.completed_bytes(),
                total: input.total,
                pieces: Some(piece_percentages(&pieces, input.total, input.piece_length)),
                active_workers: Some(0),
            });
            Ok(outcome)
        }
        (None, Some(error)) => Err(error),
        (None, None) => Err(NetworkError::Network("调度器提前结束".into())),
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_event<F>(
    event: WorkerEvent,
    input: &SchedulerInput,
    pieces: &mut [PieceState],
    workers: &mut [WorkerSlot],
    inflight_written: &mut u64,
    completed_done: &mut u64,
    last_activity: &mut Instant,
    replanned: &mut bool,
    next_missing: &mut usize,
    save_tx: &mpsc::UnboundedSender<ControlFile>,
    on_progress: &mut F,
    outcome: &mut Option<SchedulerOutcome>,
    error: &mut Option<NetworkError>,
) where
    F: FnMut(RunProgress),
{
    match event {
        WorkerEvent::Started {
            worker,
            token,
            piece,
        } => {
            if let PieceState::Assigned {
                owner,
                token: want,
            } = &pieces[piece as usize]
            {
                if *owner == worker && *want == token {
                    pieces[piece as usize] = PieceState::Downloading {
                        owner: worker,
                        token,
                        written: 0,
                        last_progress: Instant::now(),
                    };
                }
            }
        }
        WorkerEvent::Progress {
            worker,
            token,
            piece,
            written,
        } => {
            let now = Instant::now();
            if let PieceState::Downloading {
                owner,
                token: want,
                written: current,
                last_progress,
            } = &mut pieces[piece as usize]
            {
                if *owner == worker && *want == token {
                    if written > *current {
                        *inflight_written += written - *current;
                        *current = written;
                    }
                    *last_progress = now;
                    *last_activity = now;
                    on_progress(RunProgress {
                        completed: *completed_done + *inflight_written,
                        total: input.total,
                        pieces: None,
                        active_workers: Some(active_worker_count(workers)),
                    });
                }
            }
        }
        WorkerEvent::PieceDone {
            worker,
            token,
            piece,
        } => {
            let index = piece as usize;
            let matched = match &pieces[index] {
                PieceState::Downloading {
                    owner,
                    token: want,
                    written,
                    ..
                }
                | PieceState::Reclaiming {
                    owner,
                    token: want,
                    written,
                } if *owner == worker && *want == token => Some(*written),
                _ => None,
            };
            let Some(written) = matched else { return };
            *inflight_written = inflight_written.saturating_sub(written);
            pieces[index] = PieceState::Done;
            *completed_done += span_len(input.total, input.piece_length, piece);
            workers[worker].piece = None;
            *last_activity = Instant::now();

            {
                let mut control = input
                    .control
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                control.mark_piece_done(piece);
                let _ = save_tx.send(control.clone());
            }
            on_progress(RunProgress {
                completed: *completed_done + *inflight_written,
                total: input.total,
                pieces: None,
                active_workers: Some(active_worker_count(workers)),
            });
        }
        WorkerEvent::Stopped {
            worker,
            token,
            piece,
        } => {
            let index = piece as usize;
            let matched = match &pieces[index] {
                PieceState::Reclaiming {
                    owner,
                    token: want,
                    written,
                }
                | PieceState::Downloading {
                    owner,
                    token: want,
                    written,
                    ..
                } if *owner == worker && *want == token => Some(*written),
                _ => None,
            };
            let Some(written) = matched else { return };
            *inflight_written = inflight_written.saturating_sub(written);
            pieces[index] = PieceState::Missing;
            workers[worker].piece = None;
            *next_missing = (*next_missing).min(index);
        }
        WorkerEvent::Failed {
            worker,
            token,
            piece,
            error: piece_error,
        } => {
            let index = piece as usize;
            let matched = match &pieces[index] {
                PieceState::Downloading {
                    owner,
                    token: want,
                    written,
                    ..
                }
                | PieceState::Reclaiming {
                    owner,
                    token: want,
                    written,
                } if *owner == worker && *want == token => Some(*written),
                _ => None,
            };
            let Some(written) = matched else { return };
            *inflight_written = inflight_written.saturating_sub(written);
            workers[worker].piece = None;

            enum FailureKind {
                NoRange,
                Replan(Option<u64>),
                Fatal(NetworkError),
            }
            let kind = match piece_error {
                PieceError::NoRange | PieceError::ContentRangeMismatch => FailureKind::NoRange,
                PieceError::Unsatisfiable { total } => FailureKind::Replan(total),
                PieceError::TotalChanged { total } => FailureKind::Replan(Some(total)),
                PieceError::Fatal(err) => FailureKind::Fatal(err),
            };
            match kind {
                FailureKind::NoRange => {
                    if *completed_done > 0 {
                        *error = Some(NetworkError::RangeNotSupported);
                    } else {
                        *outcome = Some(SchedulerOutcome::FallbackSingle);
                    }
                }
                FailureKind::Replan(reported) => {
                    if input.restarted || *replanned || *completed_done > 0 {
                        *error = Some(NetworkError::RangeNotSupported);
                        return;
                    }
                    *replanned = true;
                    // 只重探一次：拿到新的真总长才允许重规划。
                    match input.session.get_range(&input.url, Some(probe_range())).await {
                        Ok(probe) => {
                            let new_total = match probe.status.as_u16() {
                                206 => probe.content_range.and_then(|(_, _, total)| total),
                                416 => probe.unsatisfied_total,
                                _ => None,
                            };
                            drop(probe);
                            match new_total.or(reported) {
                                Some(total) if total != input.total => {
                                    *outcome = Some(SchedulerOutcome::Restart { total });
                                }
                                _ => *error = Some(NetworkError::RangeNotSupported),
                            }
                        }
                        Err(err) => *error = Some(err),
                    }
                }
                FailureKind::Fatal(err) => {
                    // worker 内部已按 max-tries 重试；到这里即放弃该 URI。
                    *error = Some(err);
                }
            }
        }
    }
}

fn dispatch(
    pieces: &mut [PieceState],
    workers: &mut [WorkerSlot],
    input: &SchedulerInput,
    token_seq: &mut u64,
    next_missing: &mut usize,
) {
    for worker_index in 0..workers.len() {
        if workers[worker_index].piece.is_some() {
            continue;
        }
        let mut index = *next_missing;
        while index < pieces.len() && !matches!(pieces[index], PieceState::Missing) {
            index += 1;
        }
        if index >= pieces.len() {
            *next_missing = index;
            return;
        }
        *next_missing = index;

        *token_seq += 1;
        let token = *token_seq;
        let (start, end) = span(input.total, input.piece_length, index as u64);
        let command = WorkerCmd::Download {
            piece: index as u64,
            token,
            start,
            end,
            total: input.total,
        };
        // 每次派发只放一个 piece：抢占后整片重下，不会有交叉写。
        if workers[worker_index].cmds.try_send(command).is_ok() {
            pieces[index] = PieceState::Assigned {
                owner: worker_index,
                token,
            };
            workers[worker_index].piece = Some(index as u64);
        }
    }
}

/// 选中最长僵死的 `Downloading` piece；并列取较大 index（`SegmentMan.cc`）。
#[allow(clippy::too_many_arguments)]
fn pick_victim(
    pieces: &[PieceState],
    now: Instant,
    stall: Duration,
    cooldown: Duration,
    max_preempts: u8,
    preempts: &HashMap<u64, u8>,
    preempted_at: &HashMap<u64, Instant>,
    piece_length: u32,
) -> Option<u64> {
    let mut best: Option<(u64, Duration)> = None;
    for (index, piece) in pieces.iter().enumerate() {
        let PieceState::Downloading {
            written,
            last_progress,
            ..
        } = piece
        else {
            continue;
        };
        // 已过半的分片不再抢占：丢弃半片等于白扔流量。
        if written.saturating_mul(2) >= piece_length as u64 {
            continue;
        }
        let idle = now.saturating_duration_since(*last_progress);
        if idle < stall {
            continue;
        }
        if preempts.get(&(index as u64)).copied().unwrap_or(0) >= max_preempts {
            continue;
        }
        if let Some(at) = preempted_at.get(&(index as u64)) {
            if now.saturating_duration_since(*at) < cooldown {
                continue;
            }
        }
        let better = match best {
            None => true,
            Some((best_index, best_idle)) => {
                idle > best_idle || (idle == best_idle && (index as u64) > best_index)
            }
        };
        if better {
            best = Some((index as u64, idle));
        }
    }
    best.map(|(index, _)| index)
}

/// 单片完成百分比：Done=100；下载中按已写字节折算；其余 0。
fn piece_percent(state: &PieceState, total: u64, piece_length: u32, index: usize) -> u8 {
    let span = span_len(total, piece_length, index as u64).max(1);
    match state {
        PieceState::Done => 100,
        PieceState::Downloading { written, .. } | PieceState::Reclaiming { written, .. } => {
            ((written.saturating_mul(100)) / span).min(100) as u8
        }
        _ => 0,
    }
}

/// 全量分片百分比；片数超过上限时按组取平均（前端只需相对分布）。
fn piece_percentages(pieces: &[PieceState], total: u64, piece_length: u32) -> Vec<u8> {
    if pieces.is_empty() {
        return Vec::new();
    }
    let percent: Vec<u8> = pieces
        .iter()
        .enumerate()
        .map(|(index, piece)| piece_percent(piece, total, piece_length, index))
        .collect();
    if percent.len() <= MAX_REPORTED_PIECES {
        return percent;
    }
    let group = percent.len().div_ceil(MAX_REPORTED_PIECES);
    percent
        .chunks(group)
        .map(|chunk| {
            let sum: u32 = chunk.iter().map(|value| *value as u32).sum();
            (sum / chunk.len() as u32) as u8
        })
        .collect()
}

/// 当前真正持有分片的 worker 数（Assigned 也算，因为它已占用连接配额）。
fn active_worker_count(workers: &[WorkerSlot]) -> u32 {
    workers.iter().filter(|slot| slot.piece.is_some()).count() as u32
}

fn span_len(total: u64, piece_length: u32, index: u64) -> u64 {
    let start = index.saturating_mul(piece_length as u64);
    if start >= total {
        return 0;
    }
    (total - start).min(piece_length as u64)
}

fn span(total: u64, piece_length: u32, index: u64) -> (u64, u64) {
    let start = index.saturating_mul(piece_length as u64);
    let len = span_len(total, piece_length, index);
    (start, start + len.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piece_percentages_mix_done_partial_missing() {
        let pieces = vec![
            PieceState::Done,
            PieceState::Downloading {
                owner: 0,
                token: 1,
                written: 512,
                last_progress: Instant::now(),
            },
            PieceState::Missing,
        ];
        assert_eq!(
            piece_percentages(&pieces, 3 * 1024, 1024),
            vec![100, 50, 0]
        );
    }

    #[test]
    fn piece_percentages_downsample_large_files() {
        // 1024 片 → 每组 4 片，输出 256 项且平均值正确。
        let pieces: Vec<PieceState> = (0..1024).map(|_| PieceState::Done).collect();
        let out = piece_percentages(&pieces, 1024 * 1024, 1024);
        assert_eq!(out.len(), MAX_REPORTED_PIECES);
        assert!(out.iter().all(|value| *value == 100));
    }
}
