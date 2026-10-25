//! Phase 4：多段 Range 并发调度（动态切割）测试。
//!
//! 全部本地回环 + debug 构建；单文件 16/64/128MiB，禁止 `--release` 大文件。

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use serde_json::json;

const MIB: usize = 1024 * 1024;

/// 64MiB / split=2 / -x=2：并发 ≤2、区间无重叠、逐字节覆盖、SHA 相等。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn split_two_concurrency_and_sha() {
    let data = make_data(64 * MIB);
    let server = Server::start(Arc::clone(&data), |req, _, data| {
        Plan::range(data, req.range).with_slow(64 * 1024, Duration::from_millis(2))
    })
    .await;
    let engine = engine();
    let dir = temp_dir("split2");

    let gid = add_uri(
        &engine,
        &server.url("file.bin"),
        &dir,
        "file.bin",
        json!({"split": "2", "max-connection-per-server": "2", "piece-length": "1M", "timeout": "10"}),
    );
    let status = wait_terminal(&engine, &gid, Duration::from_secs(120)).await;

    assert_eq!(status["status"], "complete", "{status}");
    assert_eq!(status["totalLength"], (64 * MIB).to_string());
    assert_eq!(
        file_sha256(&dir.join("file.bin")),
        sha256(&data),
        "文件内容必须与服务端一致"
    );
    assert!(
        server.metrics.max_active() <= 2,
        "并发连接超过 split/-x 上限：{}",
        server.metrics.max_active()
    );
    assert!(server.metrics.max_active() >= 2, "split=2 应真正并发");
    let pieces = server.metrics.pieces();
    assert_no_overlap(&pieces);
    assert_exact_cover(&pieces, data.len() as u64);
    assert!(
        !dir.join("file.bin.sqlite").exists(),
        "完成后必须删除控制文件"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// 128MiB / split=4 / -x=4：并发 ≤4、每字节恰好收到一次、SHA 相等。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn split_four_covers_every_byte_once() {
    let data = make_data(128 * MIB);
    let server = Server::start(Arc::clone(&data), |req, _, data| {
        Plan::range(data, req.range).with_slow(128 * 1024, Duration::from_millis(1))
    })
    .await;
    let engine = engine();
    let dir = temp_dir("split4");

    let gid = add_uri(
        &engine,
        &server.url("file.bin"),
        &dir,
        "file.bin",
        json!({
            "split": "4",
            "max-connection-per-server": "4",
            "piece-length": "1M",
            "timeout": "10",
        }),
    );
    let status = wait_terminal(&engine, &gid, Duration::from_secs(180)).await;

    assert_eq!(status["status"], "complete", "{status}");
    assert_eq!(
        file_sha256(&dir.join("file.bin")),
        sha256(&data),
        "文件内容必须与服务端一致"
    );
    assert!(
        server.metrics.max_active() <= 4,
        "并发连接超过 -x 上限：{}",
        server.metrics.max_active()
    );
    let pieces = server.metrics.pieces();
    assert_no_overlap(&pieces);
    assert_exact_cover(&pieces, data.len() as u64);
    // 「恰好一次」：区间去重前后数量一致。
    let mut unique = pieces.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), pieces.len(), "存在重复请求的区间");

    std::fs::remove_dir_all(&dir).ok();
}

/// split=1 时即使 -x=8 也只能有一条连接（取 min 语义）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn split_one_forces_serial() {
    let data = make_data(16 * MIB);
    let server = Server::start(Arc::clone(&data), |req, _, data| {
        Plan::range(data, req.range).with_slow(64 * 1024, Duration::from_millis(2))
    })
    .await;
    let engine = engine();
    let dir = temp_dir("split1");

    let gid = add_uri(
        &engine,
        &server.url("file.bin"),
        &dir,
        "file.bin",
        json!({
            "split": "1",
            "max-connection-per-server": "8",
            "min-split-size": "4M",
            "piece-length": "1M",
            "timeout": "10",
        }),
    );
    let status = wait_terminal(&engine, &gid, Duration::from_secs(120)).await;

    assert_eq!(status["status"], "complete", "{status}");
    assert_eq!(server.metrics.max_active(), 1, "split=1 必须串行");
    assert_eq!(
        file_sha256(&dir.join("file.bin")),
        sha256(&data),
        "文件内容必须与服务端一致"
    );
    let pieces = server.metrics.pieces();
    assert_no_overlap(&pieces);
    assert_exact_cover(&pieces, data.len() as u64);

    std::fs::remove_dir_all(&dir).ok();
}

/// 16MiB + `min-split-size=4M`：planned = min(4, 16/4, 16) = 4。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn min_split_size_sets_worker_count() {
    let data = make_data(16 * MIB);
    let server = Server::start(Arc::clone(&data), |req, _, data| {
        Plan::range(data, req.range).with_slow(64 * 1024, Duration::from_millis(4))
    })
    .await;
    let engine = engine();
    let dir = temp_dir("minsplit");

    let gid = add_uri(
        &engine,
        &server.url("file.bin"),
        &dir,
        "file.bin",
        json!({
            "split": "4",
            "max-connection-per-server": "4",
            "min-split-size": "4M",
            "piece-length": "1M",
            "timeout": "10",
        }),
    );
    let status = wait_terminal(&engine, &gid, Duration::from_secs(120)).await;

    assert_eq!(status["status"], "complete", "{status}");
    assert_eq!(
        file_sha256(&dir.join("file.bin")),
        sha256(&data),
        "文件内容必须与服务端一致"
    );
    let max_active = server.metrics.max_active();
    assert!(max_active <= 4, "并发超过 min-split 允许的 4：{max_active}");
    assert!(max_active >= 2, "min-split-size=4M 应切成多段：{max_active}");
    let pieces = server.metrics.pieces();
    assert_no_overlap(&pieces);
    assert_exact_cover(&pieces, data.len() as u64);

    std::fs::remove_dir_all(&dir).ok();
}

/// 16MiB 默认 `min-split-size=20M`（< 2*min_split）：不切割，单连接完成。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_min_split_uses_single_connection() {
    let data = make_data(16 * MIB);
    let server = Server::start(Arc::clone(&data), |_, _, data| Plan::full(data)).await;
    let engine = engine();
    let dir = temp_dir("short");

    let gid = add_uri(
        &engine,
        &server.url("file.bin"),
        &dir,
        "file.bin",
        json!({"split": "4", "max-connection-per-server": "4", "timeout": "10"}),
    );
    let status = wait_terminal(&engine, &gid, Duration::from_secs(120)).await;

    assert_eq!(status["status"], "complete", "{status}");
    assert_eq!(
        file_sha256(&dir.join("file.bin")),
        sha256(&data),
        "文件内容必须与服务端一致"
    );
    // 探测 1 次 + 单连接 1 次；且没有任何非探测 Range 请求。
    assert_eq!(server.metrics.total_requests(), 2, "短文件不应切割");
    assert!(server.metrics.pieces().is_empty(), "不应出现分片请求");

    std::fs::remove_dir_all(&dir).ok();
}

/// 64MiB / 片长 16M / split=2：首片响应头延迟 5s，stall=1s 触发抢占。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_piece_is_preempted() {
    let data = make_data(64 * MIB);
    let piece = 16 * MIB as u64;
    let delayed = once_flag();
    let server = Server::start(Arc::clone(&data), move |req, _, data| {
        let is_first_piece = req.range == Some((0, Some(piece - 1)));
        if is_first_piece && !delayed.swap(true, Ordering::SeqCst) {
            return Plan::range(data, req.range).with_delay(Duration::from_secs(5));
        }
        Plan::range(data, req.range).with_slow(256 * 1024, Duration::from_millis(2))
    })
    .await;
    let engine = engine();
    let dir = temp_dir("preempt");

    let gid = add_uri(
        &engine,
        &server.url("file.bin"),
        &dir,
        "file.bin",
        json!({
            "split": "2",
            "max-connection-per-server": "2",
            "min-split-size": "4M",
            "piece-length": "16M",
            "dynamic-slicing-stall": "1",
            "dynamic-slicing-victim-cooldown": "1",
            "dynamic-slicing-max-preempts": "2",
            "timeout": "10",
            "max-tries": "1",
        }),
    );
    let status = wait_terminal(&engine, &gid, Duration::from_secs(120)).await;

    assert_eq!(status["status"], "complete", "{status}");
    assert_eq!(
        file_sha256(&dir.join("file.bin")),
        sha256(&data),
        "抢占后整片重下，内容仍须一致"
    );
    let retries = server.metrics.piece_requests(0);
    assert!(retries >= 2, "僵死分片应被重新请求，实际 {retries} 次");
    assert!(retries <= 3, "抢占次数超过 max-preempts=2：{retries}");
    assert!(server.metrics.max_active() <= 2);

    std::fs::remove_dir_all(&dir).ok();
}

/// 服务端忽略 Range（恒 200）：探测后自动降级为单连接，错误码不为 8。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_without_range_falls_back_to_single() {
    let data = make_data(64 * MIB);
    let server = Server::start(Arc::clone(&data), |_, _, data| Plan::full(data)).await;
    let engine = engine();
    let dir = temp_dir("norange");

    let gid = add_uri(
        &engine,
        &server.url("file.bin"),
        &dir,
        "file.bin",
        json!({
            "split": "4",
            "max-connection-per-server": "4",
            "min-split-size": "4M",
            "timeout": "10",
        }),
    );
    let status = wait_terminal(&engine, &gid, Duration::from_secs(120)).await;

    assert_eq!(status["status"], "complete", "{status}");
    assert_ne!(error_code(&status), Some(8), "无已存进度时不应报 8");
    assert_eq!(
        file_sha256(&dir.join("file.bin")),
        sha256(&data),
        "文件内容必须与服务端一致"
    );
    assert!(server.metrics.pieces().is_empty(), "降级后不应再发 Range");

    std::fs::remove_dir_all(&dir).ok();
}

/// 探测 206 → 后续全部 416：只重探一次，最终 8，不形成重试风暴。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_then_416_replans_once() {
    let data = make_data(64 * MIB);
    let total = data.len() as u64;
    let server = Server::start(Arc::clone(&data), move |req, _, data| {
        let is_probe = req.range == Some((0, Some(0)));
        if is_probe {
            Plan::range(data, req.range)
        } else {
            Plan::unsatisfiable(total)
        }
    })
    .await;
    let engine = engine();
    let dir = temp_dir("416");

    let gid = add_uri(
        &engine,
        &server.url("file.bin"),
        &dir,
        "file.bin",
        json!({
            "split": "2",
            "max-connection-per-server": "2",
            "min-split-size": "4M",
            "piece-length": "16M",
            "timeout": "10",
            "max-tries": "3",
            "retry-wait": "0",
        }),
    );
    let status = wait_terminal(&engine, &gid, Duration::from_secs(60)).await;

    assert_eq!(status["status"], "error", "{status}");
    assert_eq!(error_code(&status), Some(8), "416 无法重规划应报 CANNOT_RESUME");
    assert!(
        server.metrics.total_requests() <= 6,
        "重探次数失控：{} 次请求",
        server.metrics.total_requests()
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// 探测后 Range 失效且尚无完成字节：降级单连接并完成。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn range_lost_without_progress_falls_back() {
    let data = make_data(64 * MIB);
    let server = Server::start(Arc::clone(&data), move |req, _, data| {
        let is_probe = req.range == Some((0, Some(0)));
        if is_probe {
            Plan::range(data, req.range)
        } else {
            Plan::full(data)
        }
    })
    .await;
    let engine = engine();
    let dir = temp_dir("rangelost0");

    let gid = add_uri(
        &engine,
        &server.url("file.bin"),
        &dir,
        "file.bin",
        json!({
            "split": "2",
            "max-connection-per-server": "2",
            "min-split-size": "4M",
            "piece-length": "16M",
            "timeout": "10",
            "max-tries": "2",
        }),
    );
    let status = wait_terminal(&engine, &gid, Duration::from_secs(120)).await;

    assert_eq!(status["status"], "complete", "{status}");
    assert_ne!(error_code(&status), Some(8), "降级后不应报 CANNOT_RESUME");
    assert_eq!(
        file_sha256(&dir.join("file.bin")),
        sha256(&data),
        "降级单连接后内容仍须一致"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// 探测后 Range 失效但已有完成分片：不得清空进度重下，必须报 8。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn range_lost_with_progress_fails_cannot_resume() {
    let data = make_data(64 * MIB);
    let served = Arc::new(AtomicUsize::new(0));
    let server = Server::start(Arc::clone(&data), move |req, _, data| {
        if req.range == Some((0, Some(0))) {
            return Plan::range(data, req.range);
        }
        let index = served.fetch_add(1, Ordering::SeqCst);
        if index < 2 {
            Plan::range(data, req.range)
        } else {
            Plan::full(data)
        }
    })
    .await;
    let engine = engine();
    let dir = temp_dir("rangelost1");

    let gid = add_uri(
        &engine,
        &server.url("file.bin"),
        &dir,
        "file.bin",
        json!({
            "split": "2",
            "max-connection-per-server": "2",
            "min-split-size": "4M",
            "piece-length": "16M",
            "timeout": "10",
            "max-tries": "1",
        }),
    );
    let status = wait_terminal(&engine, &gid, Duration::from_secs(60)).await;

    assert_eq!(status["status"], "error", "{status}");
    assert_eq!(
        error_code(&status),
        Some(8),
        "已有进度时失去 Range 只能报 8，不能静默重下"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// 永久挂起（不回响应头）：必须在 timeout*max-tries 的量级内失败，不挂死。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hung_server_times_out() {
    let data = make_data(16 * MIB);
    let server = Server::start(Arc::clone(&data), move |req, _, data| {
        if req.range == Some((0, Some(0))) {
            Plan::range(data, req.range)
        } else {
            Plan::hang()
        }
    })
    .await;
    let engine = engine();
    let dir = temp_dir("hang");

    let started = Instant::now();
    let gid = add_uri(
        &engine,
        &server.url("file.bin"),
        &dir,
        "file.bin",
        json!({
            "split": "2",
            "max-connection-per-server": "2",
            "min-split-size": "4M",
            "piece-length": "1M",
            "timeout": "1",
            "max-tries": "2",
            "retry-wait": "0",
        }),
    );
    let status = wait_terminal(&engine, &gid, Duration::from_secs(60)).await;

    assert_eq!(status["status"], "error", "{status}");
    assert_eq!(error_code(&status), Some(2), "应为超时错误码");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "挂起场景耗时过长：{:?}",
        started.elapsed()
    );

    std::fs::remove_dir_all(&dir).ok();
}
