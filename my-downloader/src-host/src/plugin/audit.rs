//! 插件审计日志（`<HOME>\data\logs\plugin-audit.log`）。
//!
//! 红线对齐：`dispatch` 运行在 tokio 事件循环线程上，**绝不允许做阻塞式文件 I/O**。
//! 因此这里用一条常驻写盘线程 + 有界 `sync_channel`：调用方只做 `try_send`（非阻塞），
//! 磁盘写入全部发生在审计线程内；队列满时静默丢弃审计（审计可丢，主流程不可卡）。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::OnceLock;

/// 审计日志目录（与 app.log 同目录，随便携包 HOME 走）。
fn audit_dir() -> std::path::PathBuf {
    crate::paths::logs_dir()
}
/// 审计日志文件名。
const AUDIT_FILE: &str = "plugin-audit.log";
/// 审计队列容量（满则丢弃，保证调用方永不阻塞）。
const AUDIT_CAP: usize = 1024;
/// 单文件空间上限：超过即轮转为 `plugin-audit.log.1`（旧 `.1` 直接丢弃）。
const AUDIT_MAX_BYTES: u64 = 10 * 1024 * 1024;

static SINK: OnceLock<SyncSender<String>> = OnceLock::new();

/// 获取（首次调用时惰性创建）审计写入通道。
fn sink() -> &'static SyncSender<String> {
    SINK.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<String>(AUDIT_CAP);
        let spawn = std::thread::Builder::new()
            .name("plugin-audit".to_string())
            .spawn(move || {
                let dir = audit_dir();
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    eprintln!("[plugin][审计] 创建日志目录失败（{}）：{e}", dir.display());
                }
                let path = dir.join(AUDIT_FILE);
                let rotated = dir.join(format!("{AUDIT_FILE}.1"));
                let open = |target: &Path| {
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(target)
                        .map_err(|e| {
                            eprintln!(
                                "[plugin][审计] 打开日志文件失败（{}）：{e}",
                                target.display()
                            )
                        })
                        .ok()
                };

                let mut file = open(&path);
                // 以现有体积为起点计数（进程重启不重置限额语义）。
                let mut written = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

                for line in rx.iter() {
                    // 轮转：**写前判断**，保证单文件体积不超过上限（容许最后一条略微超出行宽）。
                    if written + line.len() as u64 > AUDIT_MAX_BYTES {
                        // 先交还句柄，Windows 下带句柄的 rename 会失败。
                        drop(file.take());
                        if let Err(e) = std::fs::remove_file(&rotated) {
                            if e.kind() != std::io::ErrorKind::NotFound {
                                eprintln!("[plugin][审计] 删除旧轮转文件失败：{e}");
                            }
                        }
                        if let Err(e) = std::fs::rename(&path, &rotated) {
                            eprintln!("[plugin][审计] 轮转日志失败（将继续追加当前文件）：{e}");
                        }
                        file = open(&path);
                        written = 0;
                    }
                    // 借用收敛为一次表达式：失败分支要在借用结束后才能把句柄置空。
                    let write_result = match file.as_mut() {
                        Some(f) => f.write_all(line.as_bytes()),
                        None => continue,
                    };
                    match write_result {
                        Ok(()) => written += line.len() as u64,
                        Err(_) => {
                            // 写失败（如磁盘满）后放弃该文件句柄，后续行直接丢弃，绝不 panic。
                            file = None;
                            written = 0;
                        }
                    }
                }
            });
        if let Err(e) = spawn {
            eprintln!("[plugin][审计] 启动审计线程失败：{e}");
        }
        tx
    })
}

/// 把一条审计记录归一为**恰好一行**（清洗内容中的 `\r`/`\n`，只保留末尾唯一换行符）。
///
/// 背景：QuickJS 的异常堆栈本身带 `\n`，直接落盘会把"一条事件"劈成多行，
/// 破坏审计文件"一行一条"的下游可解析性。这里用可见符号 ` ↵ ` 折叠内容里的换行。
///
/// 注意终止符语义：内容换行**不能**一并删成空格/空串，否则相邻记录会粘连成一行；
/// 因此先剥掉尾部换行，再把内容中残留的换行折叠，最后统一补回唯一的 `\n`。
fn sanitize(record: &str) -> String {
    let content = record.trim_end_matches(|c| c == '\r' || c == '\n');
    format!("{}\n", content.replace('\r', "").replace('\n', " ↵ "))
}

/// 把一行审计投递到写盘线程；非阻塞，队列满/断开一律静默丢弃。
fn enqueue(line: String) {
    match sink().try_send(sanitize(&line)) {
        Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
    }
}

/// 记录一次事件丢弃（`时间 - 插件ID - 事件名`）。
pub fn log_dropped_event(plugin_id: &str, hook: &str) {
    enqueue(format!(
        "{} - {} - {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        plugin_id,
        hook
    ));
}

/// 记录一次事件总线"落后丢弃"（`时间 - LAGGED - 跳过 N 条`）。
///
/// broadcast 通道 receiver 落后过多时会跳过消息（`RecvError::Lagged(n)`）；
/// 这类"静默漏事件"必须留痕，否则插件行为缺失将无从追溯。
pub fn log_lagged(skipped: u64) {
    enqueue(format!(
        "{} - LAGGED - 跳过 {skipped} 条\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
}

/// 记录一次**越权拒绝**（`时间 - DENY - 插件ID - 操作名`）。
///
/// 这是安全审计里最重要的一类行：插件试图调用未在 manifest 声明的能力。
/// 插件 ID / 操作名取不到或为空时退化为 `<unknown>`，**绝不 panic**、绝不阻断主流程。
pub fn log_deny(plugin_id: &str, op: &str) {
    let id = plugin_id.trim();
    let id = if id.is_empty() { "<unknown>" } else { id };
    let op = op.trim();
    let op = if op.is_empty() { "<unknown>" } else { op };

    enqueue(format!(
        "{} - DENY - {} - {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        id,
        op
    ));
}

/// 记录一次拦截**结果迟到被丢弃**（`时间 - LATE - 插件ID - 结果迟到被丢弃`）。
///
/// 超时收尾后仍有插件回报改写时触发 —— 让"有插件想改写但太慢"这件事可追溯，
/// 而不是像以前那样静默忽略。
pub fn log_late_result(plugin_id: &str) {
    let id = plugin_id.trim();
    let id = if id.is_empty() { "<unknown>" } else { id };
    enqueue(format!(
        "{} - LATE - {} - 结果迟到被丢弃\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        id
    ));
}

/// 记录一次拦截**仲裁落败**（`时间 - LOCKED - 插件ID - 原因`）。
///
/// 与 `DENY` 区分：插件是**已授权**的，只是输掉了优先级仲裁，属仲裁事件而非安全违规，
/// 因此**不计入** `plugin_deny` 安全指标，只留独立审计行。
pub fn log_intercept_locked(plugin_id: &str, reason: &str) {
    let id = plugin_id.trim();
    let id = if id.is_empty() { "<unknown>" } else { id };
    let reason = reason.trim();
    let reason = if reason.is_empty() { "<unknown>" } else { reason };
    enqueue(format!(
        "{} - LOCKED - {} - {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        id,
        reason
    ));
}

/// 记录一次**熔断自动禁用**（`时间 - CIRCUIT_BREAKER - 插件ID - 连续 N 次异常，自动禁用`）。
///
/// 仅内存生效（不持久化）：应用重启后插件按用户开关恢复，计数清零。
pub fn log_circuit_breaker(plugin_id: &str, consecutive: usize) {
    let id = plugin_id.trim();
    let id = if id.is_empty() { "<unknown>" } else { id };
    enqueue(format!(
        "{} - CIRCUIT_BREAKER - {} - 连续 {} 次异常，自动禁用\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        id,
        consecutive
    ));
}

/// 记录一次**热重载成功**（`时间 - RELOAD_OK - 插件ID - 重新加载成功`）。
pub fn log_reload_ok(plugin_id: &str) {
    let id = plugin_id.trim();
    let id = if id.is_empty() { "<unknown>" } else { id };
    enqueue(format!(
        "{} - RELOAD_OK - {} - 重新加载成功\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        id
    ));
}

/// 记录一次**热重载失败**（`时间 - RELOAD_FAILED - 插件ID - 原因`）。
///
/// 失败一律保留旧版本（或标记加载失败），**绝不自动重试**（避免死循环）。
pub fn log_reload_failed(plugin_id: &str, reason: &str) {
    let id = plugin_id.trim();
    let id = if id.is_empty() { "<unknown>" } else { id };
    enqueue(format!(
        "{} - RELOAD_FAILED - {} - {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        id,
        reason
    ));
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    /// 带栈帧换行的真实形态：折叠后**恰好只有末尾一个换行符**，内容不丢失。
    #[test]
    fn sanitize_collapses_multiline_to_single_physical_line() {
        let raw = "2026-10-18 15:21:27 - RELOAD_FAILED - com.example.hello - 入口脚本执行失败：Error: expecting ';'\n    at eval_script:4:6\n";
        let out = sanitize(raw);

        assert_eq!(out.matches('\n').count(), 1, "一条审计只能有一个物理换行");
        assert!(out.ends_with('\n'), "末尾终止符必须保留");
        assert!(out.contains(" ↵ "), "内容换行应折叠为可见符号");
        assert!(
            out.contains("at eval_script:4:6"),
            "折叠不得丢失内容"
        );
    }

    /// 单行记录必须原样保留（清洗不得引入多余空格或改变内容）。
    #[test]
    fn sanitize_keeps_single_line_record_intact() {
        assert_eq!(sanitize("DENY - com.a - downloads:read\n"), "DENY - com.a - downloads:read\n");
    }

    /// CRLF：`\r` 直接剔除，`\n` 折叠为 ` ↵ `，避免出现 ` ↵  ↵ ` 的双符号。
    #[test]
    fn sanitize_normalizes_crlf() {
        assert_eq!(sanitize("x\r\ny\r\n"), "x ↵ y\n");
    }

    /// 相邻两条记录不得粘连：第二次清洗的末尾终止符独立存在。
    #[test]
    fn sanitize_prevents_record_concatenation() {
        let first = sanitize("A - 1\n");
        let second = sanitize("B - 2\n");
        assert_eq!(format!("{first}{second}"), "A - 1\nB - 2\n");
    }
}
