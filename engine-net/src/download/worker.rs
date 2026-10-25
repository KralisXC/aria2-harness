//! 单个分片 worker：一次 `Download` 命令只请求一个 piece（Range 精确闭区间），
//! 每个尝试打开独立文件句柄按绝对偏移写入，token 用于丢弃抢占后的迟到事件。
//!
//! 停止语义：收到 `Stop` 立即 drop body（H2 为 RST_STREAM），回 `Stopped`；
//! 已写字节留在文件里，但调度器会把该 piece 重新标记为 Missing 并整片重下，
//! 因此不会出现「两个连接写同一未完成 piece」。
//!
//! `Started` 在发起请求**之前**上报：调度器的僵死时钟从派发即开始计时，
//! 服务端迟迟不发响应头也能被抢占（对齐 `SegmentMan` 的速度采样）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::fs::OpenOptions;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;

use http_body_util::BodyExt;

use crate::error::NetworkError;
use crate::http::session::{ByteRange, HttpSession};

/// 进度上报批次（与 Phase 3 READ_CHUNK_SIZE 一致）。
const PROGRESS_BATCH: u64 = 64 * 1024;

#[derive(Debug)]
pub enum WorkerCmd {
    Download {
        piece: u64,
        token: u64,
        start: u64,
        end: u64,
        total: u64,
    },
    Stop,
    Shutdown,
}

#[derive(Debug)]
pub enum PieceError {
    /// 服务端对带 Range 的请求返回 200（不再支持 Range）。
    NoRange,
    /// 416 Requested Range Not Satisfiable。
    Unsatisfiable { total: Option<u64> },
    /// 206 的 Content-Range 与请求区间不符。
    ContentRangeMismatch,
    /// 206 的总长与调度器计划不一致（服务器文件变了）。
    TotalChanged { total: u64 },
    /// 其它错误（网络/状态码/证书/代理）。
    Fatal(NetworkError),
}

#[derive(Debug)]
pub enum WorkerEvent {
    Started {
        worker: usize,
        token: u64,
        piece: u64,
    },
    Progress {
        worker: usize,
        token: u64,
        piece: u64,
        /// 当前 piece 的累计写入（跨重试单调递增）。
        written: u64,
    },
    PieceDone {
        worker: usize,
        token: u64,
        piece: u64,
    },
    Stopped {
        worker: usize,
        token: u64,
        piece: u64,
    },
    Failed {
        worker: usize,
        token: u64,
        piece: u64,
        error: PieceError,
    },
}

pub struct WorkerConfig {
    pub index: usize,
    pub session: Arc<HttpSession>,
    pub url: String,
    pub dest: PathBuf,
    pub read_timeout: Option<Duration>,
    pub max_tries: usize,
    pub retry_wait: Duration,
}

enum DownloadOutcome {
    Done,
    Stopped,
    Failed(PieceError),
}

pub async fn run(
    cfg: WorkerConfig,
    mut cmds: mpsc::Receiver<WorkerCmd>,
    events: mpsc::Sender<WorkerEvent>,
) {
    while let Some(cmd) = cmds.recv().await {
        match cmd {
            WorkerCmd::Download {
                piece,
                token,
                start,
                end,
                total,
            } => {
                // 进入 Downloading 的信号先发，抢占的僵死时钟从此刻起算。
                if events
                    .send(WorkerEvent::Started {
                        worker: cfg.index,
                        token,
                        piece,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
                let outcome = download_piece(&cfg, piece, token, start, end, total, &mut cmds, &events).await;
                let event = match outcome {
                    DownloadOutcome::Done => WorkerEvent::PieceDone {
                        worker: cfg.index,
                        token,
                        piece,
                    },
                    DownloadOutcome::Stopped => WorkerEvent::Stopped {
                        worker: cfg.index,
                        token,
                        piece,
                    },
                    DownloadOutcome::Failed(error) => WorkerEvent::Failed {
                        worker: cfg.index,
                        token,
                        piece,
                        error,
                    },
                };
                if events.send(event).await.is_err() {
                    return;
                }
            }
            // 空闲状态收到 Stop 无需确认（调度器只在 Working 时发 Stop）。
            WorkerCmd::Stop => {}
            WorkerCmd::Shutdown => return,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn download_piece(
    cfg: &WorkerConfig,
    piece: u64,
    token: u64,
    start: u64,
    end: u64,
    total: u64,
    cmds: &mut mpsc::Receiver<WorkerCmd>,
    events: &mpsc::Sender<WorkerEvent>,
) -> DownloadOutcome {
    let mut attempt = 0usize;
    // 跨重试单调的已写字节：调度器按此更新进度与「过半不抢占」护栏。
    let mut written_base = 0u64;
    loop {
        match attempt_piece(
            cfg,
            piece,
            token,
            start,
            end,
            total,
            written_base,
            cmds,
            events,
        )
        .await
        {
            Ok(outcome) => return outcome,
            Err((error, written)) => {
                written_base += written;
                let retryable = matches!(&error, PieceError::Fatal(err) if err.is_retryable());
                if !retryable || attempt + 1 >= cfg.max_tries {
                    return DownloadOutcome::Failed(error);
                }
                attempt += 1;
                if !cfg.retry_wait.is_zero() {
                    tokio::select! {
                        _ = tokio::time::sleep(cfg.retry_wait) => {}
                        cmd = cmds.recv() => return stopped_or_shutdown(cmd),
                    }
                }
            }
        }
    }
}

/// `Ok(outcome)` 表示已完成/已停止；`Err((error, written))` 表示本次尝试失败，
/// `written` 是本次尝试已落盘字节（用于跨重试累计进度）。
#[allow(clippy::too_many_arguments)]
async fn attempt_piece(
    cfg: &WorkerConfig,
    piece: u64,
    token: u64,
    start: u64,
    end: u64,
    total: u64,
    written_base: u64,
    cmds: &mut mpsc::Receiver<WorkerCmd>,
    events: &mpsc::Sender<WorkerEvent>,
) -> Result<DownloadOutcome, (PieceError, u64)> {
    let response = tokio::select! {
        cmd = cmds.recv() => return Ok(stopped_or_shutdown(cmd)),
        res = cfg.session.get_range(&cfg.url, Some(ByteRange::new(start, Some(end)))) => {
            match res {
                Ok(response) => response,
                Err(err) => return Err((PieceError::Fatal(err), 0)),
            }
        }
    };

    let status = response.status.as_u16();
    if status == 200 {
        return Ok(DownloadOutcome::Failed(PieceError::NoRange));
    }
    if status == 416 {
        return Ok(DownloadOutcome::Failed(PieceError::Unsatisfiable {
            total: response.unsatisfied_total,
        }));
    }
    if status != 206 {
        return Ok(DownloadOutcome::Failed(PieceError::Fatal(
            NetworkError::HttpStatus(status),
        )));
    }

    let (got_start, got_end, got_total) = match response.content_range {
        Some(value) => value,
        None => return Ok(DownloadOutcome::Failed(PieceError::ContentRangeMismatch)),
    };
    if got_start != start || got_end != end {
        return Ok(DownloadOutcome::Failed(PieceError::ContentRangeMismatch));
    }
    if let Some(got) = got_total {
        if got != total {
            return Ok(DownloadOutcome::Failed(PieceError::TotalChanged {
                total: got,
            }));
        }
    }

    // 独立文件句柄按绝对偏移写；Windows 默认共享模式允许同进程多句柄。
    let mut file = OpenOptions::new()
        .write(true)
        .open(&cfg.dest)
        .await
        .map_err(|err| (PieceError::Fatal(NetworkError::from(err)), 0u64))?;
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(|err| (PieceError::Fatal(NetworkError::from(err)), 0u64))?;

    let mut body = response.body;
    // 租约必须活到 body 读完；提前 drop = RST_STREAM + 归还 H2 流配额。
    let lease = response.lease;

    let mut written = 0u64;
    let mut last_report = 0u64;
    loop {
        let next = match cfg.read_timeout {
            Some(limit) => tokio::select! {
                cmd = cmds.recv() => return Ok(stopped_or_shutdown(cmd)),
                res = tokio::time::timeout(limit, body.frame()) => {
                    match res {
                        Err(_) => return Err((PieceError::Fatal(NetworkError::Timeout), written)),
                        Ok(frame) => frame,
                    }
                }
            },
            None => tokio::select! {
                cmd = cmds.recv() => return Ok(stopped_or_shutdown(cmd)),
                frame = body.frame() => frame,
            },
        };

        let Some(frame) = next else { break };
        let frame = match frame {
            Ok(frame) => frame,
            Err(err) => {
                let error = if err.is_incomplete_message() || err.is_body_write_aborted() {
                    NetworkError::ConnectionReset
                } else {
                    NetworkError::Network(err.to_string())
                };
                return Err((PieceError::Fatal(error), written));
            }
        };
        if let Ok(data) = frame.into_data() {
            if let Err(err) = file.write_all(&data).await {
                let error = NetworkError::from(err);
                // 磁盘满/打开失败等非重试错误也要让调度器立即拿到。
                if error.is_retryable() {
                    return Err((PieceError::Fatal(error), written));
                }
                return Ok(DownloadOutcome::Failed(PieceError::Fatal(error)));
            }
            written += data.len() as u64;
            if written - last_report >= PROGRESS_BATCH {
                last_report = written;
                if events
                    .send(WorkerEvent::Progress {
                        worker: cfg.index,
                        token,
                        piece,
                        written: written_base + written,
                    })
                    .await
                    .is_err()
                {
                    return Ok(DownloadOutcome::Stopped);
                }
            }
        }
    }

    let expected = end - start + 1;
    if written != expected {
        return Err((PieceError::Fatal(NetworkError::ConnectionReset), written));
    }

    if let Err(err) = file.flush().await {
        return Ok(DownloadOutcome::Failed(PieceError::Fatal(NetworkError::from(err))));
    }
    drop(lease);
    Ok(DownloadOutcome::Done)
}

fn stopped_or_shutdown(cmd: Option<WorkerCmd>) -> DownloadOutcome {
    match cmd {
        // 工作期间不会收到新派发；把它当作停止是安全的（调度器会整片重派）。
        Some(WorkerCmd::Stop) | Some(WorkerCmd::Shutdown) | Some(WorkerCmd::Download { .. }) | None => {
            DownloadOutcome::Stopped
        }
    }
}
