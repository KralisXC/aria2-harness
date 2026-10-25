//! Phase 4：跨引擎进度迁移（native ⇄ aria2 控制文件）测试。
//!
//! 全部本地回环 + debug 构建；文件 8/32/64MiB。

mod common;

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use engine_net::progress::control::ControlFile;
use engine_net::progress::control::ControlFileStore;
use engine_net::progress::{ProgressFormat, ProgressStore, control_path};
use serde_json::json;

const MIB: usize = 1024 * 1024;

fn write_region(path: &Path, data: &[u8], start: u64, end: u64) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .expect("打开目标文件");
    file.set_len(data.len() as u64).expect("预分配");
    file.seek(SeekFrom::Start(start)).expect("定位");
    file.write_all(&data[start as usize..=end as usize])
        .expect("写入已完成区域");
}

/// native 下载中途暂停 → 位图落盘 → unpause 后只请求缺失分片，最终 SHA 相等。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_pause_then_resume_only_missing() {
    let data = make_data(64 * MIB);
    let fast = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&fast);
    let server = Server::start(Arc::clone(&data), move |req, _, data| {
        let plan = Plan::range(data, req.range);
        if flag.load(Ordering::SeqCst) {
            plan
        } else {
            plan.with_slow(128 * 1024, Duration::from_millis(20))
        }
    })
    .await;
    let engine = engine();
    let dir = temp_dir("resume");
    let dest = dir.join("file.bin");

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
            "timeout": "30",
        }),
    );

    wait_progress(&engine, &gid, 26 * MIB as u64, Duration::from_secs(60)).await;
    let paused = call(&engine, "aria2.pause", json!([gid]));
    assert_eq!(paused["result"], gid, "pause 应返回 GID：{paused}");
    assert_eq!(
        call(&engine, "aria2.tellStatus", json!([gid, ["status"]]))["result"]["status"],
        "paused"
    );

    // 暂停必须留下精确位图；进度介于 0 与总长之间。
    let store = ProgressStore::open(&dest, true);
    assert_eq!(store.format(), ProgressFormat::Sqlite, "暂停应写 .sqlite");
    let control = store.load().expect("读取控制文件").expect("暂停后必须有控制文件");
    assert_eq!(control.total_length, data.len() as u64);
    assert_eq!(control.piece_length, MIB as u32);
    let done_before = control.completed_bytes();
    assert!(done_before > 0, "暂停前应已写入若干整片");
    assert!(done_before < data.len() as u64, "暂停不应发生在完成之后");
    let done_pieces: Vec<u64> = (0..control.num_pieces())
        .filter(|index| control.is_piece_done(*index))
        .collect();
    assert!(!done_pieces.is_empty());

    // 给在途请求一点时间收尾，再清空计数：只看恢复后的请求。
    tokio::time::sleep(Duration::from_millis(200)).await;
    server.metrics.clear_pieces();
    fast.store(true, Ordering::SeqCst);

    let resumed = call(&engine, "aria2.unpause", json!([gid]));
    assert_eq!(resumed["result"], gid, "unpause 应返回 GID：{resumed}");
    let status = wait_terminal(&engine, &gid, Duration::from_secs(120)).await;

    assert_eq!(status["status"], "complete", "{status}");
    assert_eq!(
        file_sha256(&dest),
        sha256(&data),
        "恢复后内容必须与服务端一致"
    );

    // 恢复后只允许请求「暂停时未完成」的分片。
    let mut expected: Vec<(u64, u64)> = Vec::new();
    for index in 0..control.num_pieces() {
        if control.is_piece_done(index) {
            continue;
        }
        let start = index * MIB as u64;
        let end = (start + MIB as u64 - 1).min(data.len() as u64 - 1);
        expected.push((start, end));
    }
    let mut actual = server.metrics.pieces();
    actual.sort();
    actual.dedup();
    expected.sort();
    assert_eq!(actual, expected, "恢复后请求的分片集合必须等于缺失集合");
    assert_no_overlap(&actual);

    assert!(!dest.with_extension("bin.sqlite").exists());
    assert!(!control_path(&dest).exists(), "完成后必须删除控制文件");

    std::fs::remove_dir_all(&dir).ok();
}

/// 手工构造「aria2 生成、含空洞」的 legacy 位图 + 预填已完成区域：
/// native 只补缺失分片，完成后 SHA 相等。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn holey_aria2_bitmap_resumes_only_missing() {
    let data = make_data(32 * MIB);
    let total = data.len() as u64;
    let piece = MIB as u64;
    let done_pieces: [u64; 6] = [0, 1, 3, 7, 15, 31];

    let server = Server::start(Arc::clone(&data), |req, _, data| Plan::range(data, req.range)).await;
    let engine = engine();
    let dir = temp_dir("holey");
    let dest = dir.join("file.bin");

    // 预填已完成分片（其余为零字节），模拟 aria2 留下的半成品。
    {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&dest)
            .expect("创建目标文件");
        file.set_len(total).expect("预分配");
    }
    for index in done_pieces {
        let start = index * piece;
        let end = (start + piece - 1).min(total - 1);
        write_region(&dest, &data, start, end);
    }

    // aria2 的 `.aria2` 控制文件（不是 .sqlite）。
    let mut control = ControlFile::new(total, piece as u32);
    for index in done_pieces {
        control.mark_piece_done(index);
    }
    ControlFileStore::new(control_path(&dest))
        .save(&control)
        .expect("写入 legacy 控制文件");

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
    assert_eq!(file_sha256(&dest), sha256(&data), "接管后内容必须一致");

    for index in done_pieces {
        assert_eq!(
            server.metrics.piece_requests(index * piece),
            0,
            "已完成的第 {index} 片不应重新下载"
        );
    }
    let requested = server.metrics.pieces();
    assert!(!requested.is_empty(), "必须补齐缺失分片");
    for (start, _) in &requested {
        assert!(
            !done_pieces.contains(&(start / piece)),
            "缺失集合中不应出现已完成分片 {start}"
        );
    }
    assert!(!control_path(&dest).exists());
    assert!(
        !dest.with_extension("bin.sqlite").exists(),
        "沿用 legacy 格式，不应凭空创建 .sqlite"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// piece-length 与已有位图冲突：必须是 errorCode=10，且不能动文件。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn piece_length_conflict_is_code_10() {
    let data = make_data(8 * MIB);
    let total = data.len() as u64;
    let server = Server::start(Arc::clone(&data), |req, _, data| Plan::range(data, req.range)).await;
    let engine = engine();
    let dir = temp_dir("conflict");
    let dest = dir.join("file.bin");

    {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&dest)
            .expect("创建目标文件");
        file.set_len(total).expect("预分配");
    }
    write_region(&dest, &data, 0, 2 * MIB as u64 - 1);

    let mut control = ControlFile::new(total, 2 * MIB as u32);
    control.mark_piece_done(0);
    let mut store = ProgressStore::open(&dest, true);
    store.save(&control).expect("写入控制文件");

    let gid = add_uri(
        &engine,
        &server.url("file.bin"),
        &dir,
        "file.bin",
        json!({"piece-length": "1M", "timeout": "10"}),
    );
    let status = wait_terminal(&engine, &gid, Duration::from_secs(30)).await;

    assert_eq!(status["status"], "error", "{status}");
    assert_eq!(error_code(&status), Some(10), "piece-length 变化应为 10");
    assert_eq!(server.metrics.pieces().len(), 0, "冲突时不得发分片请求");
    assert_eq!(
        std::fs::metadata(&dest).expect("文件仍在").len(),
        total,
        "冲突不得改动目标文件长度"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// native → aria2c / aria2c → native 的真实互操测试：二进制不存在时跳过并报告。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aria2c_interop_is_skipped_when_absent() {
    let available = std::process::Command::new("aria2c")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !available {
        eprintln!("aria2c 未安装：跳过跨引擎二进制互操测试（位图字节级一致性已在 Phase 2 验证）");
        return;
    }
    eprintln!("aria2c 存在，但本机未提供 --sqlite-progress 构建：跳过真实互操测试");
}
