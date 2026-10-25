//! Phase 5 长稳与资源回收：512 MiB × 3 轮、128 并发。
//!
//! 纪律：
//! - 只用 debug 构建，禁止 `--release` 大文件；
//! - 源文件与目的文件都放 `target/phase5-tmp`，每轮结束删目的文件，最后整目录清理；
//! - 服务端按 Range 从磁盘流式读取，不把 512 MiB 放进进程内存（避免污染 RSS 采样）。
//!
//! 运行：`cargo test --features native-http --test phase5_soak -- --ignored --nocapture`

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const MIB: u64 = 1024 * 1024;
const TOTAL: u64 = 512 * MIB;
const ROUNDS: usize = 3;

// ---------------------------------------------------------------------------
// 源文件 / 摘要
// ---------------------------------------------------------------------------

fn work_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/phase5-tmp")
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        eprintln!("[soak] 已清理 {}", self.0.display());
    }
}

/// 按 1 MiB 确定性块写入 512 MiB 源文件，同时流式计算 SHA-256。
fn write_source(path: &Path) -> String {
    use std::io::Write;
    let mut file = std::fs::File::create(path).expect("创建源文件");
    let mut block = vec![0u8; MIB as usize];
    for (index, byte) in block.iter_mut().enumerate() {
        *byte = (index * 37 % 251) as u8;
    }
    let mut digest = Sha256::new();
    for _ in 0..TOTAL / MIB {
        file.write_all(&block).expect("写源文件");
        digest.update(&block);
    }
    file.flush().expect("刷新源文件");
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn file_sha256(path: &Path) -> String {
    use std::io::Read;
    let mut file = std::fs::File::open(path).expect("打开目的文件");
    let mut buffer = vec![0u8; MIB as usize];
    let mut digest = Sha256::new();
    loop {
        let read = file.read(&mut buffer).expect("读目的文件");
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// ---------------------------------------------------------------------------
// 磁盘 Range 服务端（流式读取，不驻留内存）
// ---------------------------------------------------------------------------

struct FileServer {
    addr: std::net::SocketAddr,
    /// 历史并发峰值（`take_max` 读它并归零）。
    peak: Arc<AtomicUsize>,
    handle: JoinHandle<()>,
}

impl Drop for FileServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl FileServer {
    async fn start(source: PathBuf) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let active_task = Arc::clone(&active);
        let peak_task = Arc::clone(&peak);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let source = source.clone();
                let active = Arc::clone(&active_task);
                let peak = Arc::clone(&peak_task);
                tokio::spawn(async move {
                    serve(socket, &source, active, peak).await;
                });
            }
        });
        Self {
            addr,
            peak,
            handle,
        }
    }

    fn url(&self, name: &str) -> String {
        format!("http://{}/{name}", self.addr)
    }

    fn take_max(&self) -> usize {
        self.peak.swap(0, Ordering::SeqCst)
    }
}

async fn read_head(socket: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return buf,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            return buf;
        }
        if buf.len() > 64 * 1024 {
            return buf;
        }
    }
}

fn parse_range(head: &[u8]) -> Option<(u64, Option<u64>)> {
    let text = String::from_utf8_lossy(head);
    for line in text.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("range") {
            continue;
        }
        let value = value.trim().strip_prefix("bytes=")?;
        let (start, end) = value.split_once('-')?;
        let start: u64 = start.trim().parse().ok()?;
        let end: Option<u64> = end.trim().parse().ok();
        return Some((start, end));
    }
    None
}

async fn serve(
    mut socket: TcpStream,
    source: &Path,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
) {
    let head = read_head(&mut socket).await;
    if head.is_empty() {
        return;
    }
    let Some((start, end)) = parse_range(&head) else {
        let _ = socket
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        return;
    };
    // 探测（bytes=0-0）不计并发：与 Phase 4 的夹具口径一致。
    let counted = (start, end) != (0, Some(0));
    if counted {
        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(now, Ordering::SeqCst);
    }

    let total = TOTAL;
    if start >= total {
        let _ = socket
            .write_all(
                format!(
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await;
        if counted {
            active.fetch_sub(1, Ordering::SeqCst);
        }
        return;
    }
    let end = end.unwrap_or(total - 1).min(total - 1);
    let len = end - start + 1;

    let mut file = match tokio::fs::File::open(source).await {
        Ok(file) => file,
        Err(_) => {
            let _ = socket
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
                .await;
            if counted {
                active.fetch_sub(1, Ordering::SeqCst);
            }
            return;
        }
    };
    if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        if counted {
            active.fetch_sub(1, Ordering::SeqCst);
        }
        return;
    }
    let mut body = vec![0u8; 64 * 1024];
    let head = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Length: {len}\r\nContent-Range: bytes {start}-{end}/{total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
    );
    if socket.write_all(head.as_bytes()).await.is_ok() {
        let mut remaining = len;
        while remaining > 0 {
            let want = remaining.min(body.len() as u64) as usize;
            if file.read_exact(&mut body[..want]).await.is_err() {
                break;
            }
            if socket.write_all(&body[..want]).await.is_err() {
                break;
            }
            remaining -= want as u64;
            // 每 64 KiB 停 8 ms：让 128 个 worker 的请求在服务端真正重叠，
            // 否则回环 + 页缓存会让单轮在几百毫秒内结束，并发峰值失去意义。
            tokio::time::sleep(Duration::from_millis(8)).await;
        }
    }
    let _ = socket.shutdown().await;
    if counted {
        active.fetch_sub(1, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// 当前进程采样（RSS / 句柄）
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod process_metrics {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessHandleCount, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    #[derive(Debug, Clone, Copy, Default)]
    pub struct Sample {
        pub rss: u64,
        pub handles: u32,
    }

    pub fn sample(pid: u32) -> Option<Sample> {
        unsafe {
            let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return None;
            }
            let mut counters = PROCESS_MEMORY_COUNTERS::default();
            counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            let mut handles = 0u32;
            let mem_ok = GetProcessMemoryInfo(handle, &mut counters, counters.cb);
            let handle_ok = GetProcessHandleCount(handle, &mut handles);
            CloseHandle(handle);
            if mem_ok == 0 || handle_ok == 0 {
                return None;
            }
            Some(Sample {
                rss: counters.WorkingSetSize as u64,
                handles,
            })
        }
    }
}

#[cfg(not(windows))]
mod process_metrics {
    #[derive(Debug, Clone, Copy, Default)]
    pub struct Sample {
        pub rss: u64,
        pub handles: u32,
    }

    pub fn sample(_pid: u32) -> Option<Sample> {
        None
    }
}

/// 100 ms 采样当前进程峰值。
struct Sampler {
    stop: Arc<std::sync::atomic::AtomicBool>,
    peak: Arc<std::sync::Mutex<process_metrics::Sample>>,
    join: std::thread::JoinHandle<()>,
}

impl Sampler {
    fn start(pid: u32) -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let peak = Arc::new(std::sync::Mutex::new(process_metrics::Sample::default()));
        let stop_flag = Arc::clone(&stop);
        let peak_slot = Arc::clone(&peak);
        let join = std::thread::spawn(move || {
            while !stop_flag.load(Ordering::SeqCst) {
                if let Some(sample) = process_metrics::sample(pid) {
                    let mut slot = peak_slot.lock().unwrap();
                    slot.rss = slot.rss.max(sample.rss);
                    slot.handles = slot.handles.max(sample.handles);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Self { stop, peak, join }
    }

    fn finish(self) -> process_metrics::Sample {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.join.join();
        *self.peak.lock().unwrap()
    }
}

// ---------------------------------------------------------------------------
// 长稳用例
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "512MiB×3 长稳，验收时用 --ignored 单独执行"]
async fn soak_512m_three_rounds_no_leak() {
    let dir = work_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("创建长稳目录");
    let _cleanup = Cleanup(dir.clone());

    let source = dir.join("source-512m.bin");
    eprintln!("[soak] 生成 512 MiB 源文件：{}", source.display());
    let expected = write_source(&source);
    eprintln!("[soak] 源 SHA-256 = {expected}");

    let server = FileServer::start(source.clone()).await;
    let engine = common::engine();
    let pid = std::process::id();
    let idle = process_metrics::sample(pid).expect("采样当前进程");

    let mut posts = Vec::new();
    let mut peaks = Vec::new();
    let mut concurrencies = Vec::new();
    let mut durations = Vec::new();

    for round in 1..=ROUNDS {
        let out = format!("dest-round{round}.bin");
        let dest = dir.join(&out);
        let sampler = Sampler::start(pid);
        let started = Instant::now();
        let gid = common::add_uri(
            &engine,
            &server.url("source-512m.bin"),
            &dir,
            &out,
            json!({
                "split": "128",
                "max-connection-per-server": "128",
                "piece-length": "1M",
                "min-split-size": "4M",
                "timeout": "30",
            }),
        );
        let status = common::wait_terminal(&engine, &gid, Duration::from_secs(600)).await;
        let elapsed = started.elapsed();
        let peak = sampler.finish();
        assert_eq!(
            status["status"], "complete",
            "第 {round} 轮未完成：{status}"
        );
        assert_eq!(
            status["errorCode"], "0",
            "第 {round} 轮出现错误：{status}"
        );
        // 128 并发下绝不能出现句柄耗尽类错误（Windows 上 Rust 报 EMFILE 的典型文案）。
        let message = status["errorMessage"].as_str().unwrap_or_default().to_ascii_lowercase();
        assert!(
            !message.contains("too many open files") && !message.contains("打开的文件过多"),
            "第 {round} 轮出现句柄耗尽：{message}"
        );
        assert_eq!(
            file_sha256(&dest),
            expected,
            "第 {round} 轮 SHA 不一致"
        );

        let max_active = server.take_max();
        // 客户端 worker 上限 128；服务端计数还包含「客户端已取消、服务端尚未收尾」
        // 的少量重叠连接，故上界留 32 的余量。
        assert!(
            max_active <= 160,
            "第 {round} 轮并发异常（{max_active} > 160，超出 128 worker + 收尾余量）"
        );
        assert!(
            max_active >= 96,
            "第 {round} 轮未形成接近 128 的并发（峰值 {max_active}）"
        );

        durations.push(elapsed);
        concurrencies.push(max_active);
        peaks.push(peak);
        std::fs::remove_file(&dest).expect("删除目的文件");

        // 静置 30s：`tokio::fs` 走阻塞线程池，空闲线程默认 10s 才回收；
        // 采样点必须晚于该窗口，否则测到的是线程池余温而不是泄漏。
        tokio::time::sleep(Duration::from_secs(30)).await;
        posts.push(process_metrics::sample(pid).expect("采样当前进程"));

        eprintln!(
            "[soak] round={round} 并发峰值={max_active} 耗时={elapsed:?} 峰值RSS={:.1}MiB 峰值句柄={} 静置后句柄={}",
            peak.rss as f64 / MIB as f64,
            peak.handles,
            posts[posts.len() - 1].handles
        );
    }

    let post2 = posts[ROUNDS - 2];
    let post3 = posts[ROUNDS - 1];
    // 稳态口径：第 2 轮之后才进入热态（第 1 轮含运行时/分配器一次性初始化）。
    // 泄漏会在「后一轮 vs 前一轮」上体现，而不是首轮差异。
    let handle_growth = post3.handles as i64 - post2.handles as i64;
    let rss_growth = post3.rss as i64 - post2.rss as i64;
    let peak_rss_growth = peaks[ROUNDS - 1].rss as i64 - peaks[ROUNDS - 2].rss as i64;
    let peak_handles = peaks.iter().map(|sample| sample.handles).max().unwrap();
    let peak_handle_spread =
        peak_handles as i64 - peaks.iter().map(|sample| sample.handles).min().unwrap() as i64;

    eprintln!(
        "[soak] idle RSS={:.1}MiB handles={} | post2 RSS={:.1}MiB handles={} | post3 RSS={:.1}MiB handles={}",
        idle.rss as f64 / MIB as f64,
        idle.handles,
        post2.rss as f64 / MIB as f64,
        post2.handles,
        post3.rss as f64 / MIB as f64,
        post3.handles
    );
    eprintln!(
        "[soak] 增长：post2→post3 RSS={:.1}MiB handles={} | peak RSS={:.1}MiB peak handles={}（轮间波动 {}）| 并发={:?} 用时={:?}",
        rss_growth as f64 / MIB as f64,
        handle_growth,
        peak_rss_growth as f64 / MIB as f64,
        peak_handles,
        peak_handle_spread,
        concurrencies,
        durations
    );

    assert!(
        handle_growth <= 32,
        "稳态后句柄仍线性增长（{handle_growth} > 32），疑似泄漏"
    );
    assert!(
        peak_handle_spread <= 48,
        "各轮句柄峰值持续抬升（波动 {peak_handle_spread} > 48），疑似泄漏"
    );
    assert!(
        peak_handles as i64 - idle.handles as i64 <= 1024,
        "128 并发下句柄峰值过高（{} - {} > 1024，约 {:?} 个/连接）",
        peak_handles,
        idle.handles,
        (peak_handles as i64 - idle.handles as i64) as f64 / 128.0
    );
    assert!(
        rss_growth <= 32 * MIB as i64,
        "稳态后 RSS 仍线性增长（{:.1} MiB > 32 MiB），疑似泄漏",
        rss_growth as f64 / MIB as f64
    );
    assert!(
        peak_rss_growth <= 64 * MIB as i64,
        "峰值 RSS 随轮次增长（{:.1} MiB > 64 MiB）",
        peak_rss_growth as f64 / MIB as f64
    );
}
