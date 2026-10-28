//! `ctx.exec` 的宿主实现（beta10 · 模块 A4）：执行 FFmpeg / FFprobe。
//!
//! 依赖模型：**插件自包含 + 全局共享池**（beta12.1）。
//!
//! ```text
//! data/plugins/
//! ├── .shared/com.harness.shared-ffmpeg/
//! │   ├── manifest.json            ← type=shared
//! │   └── ffmpeg.exe + DLL         ← 官方插件共用（beta12.3 平铺布局）
//! │   └── bin/ffmpeg.exe           ← 兼容旧格式（beta12.2 布局，次优先）
//! └── hls-downloader/
//!     ├── manifest.json
//!     ├── main.js
//!     └── bin/ffmpeg.exe             ← 可选：单插件覆盖共享版本
//! ```
//!
//! 宿主只负责：白名单 → 解析 `bin/` → `.shared/<共享id>/` → PATH → 受控 cwd →
//! 超时杀树 → 输出截断。**不做** FFmpeg 下载、版本探测、UI 安装提示。
//!
//! ## 两级授权（1.0.0-rc1 · 模块一）
//!
//! - `exec:ffmpeg`：`ffmpeg` / `ffprobe`，解析顺序 `插件 bin/ → .shared/ → PATH`（原语义不变）。
//! - `exec:local`：插件自带的其它可执行文件，**只允许** `<plugin>/bin/<exe>` 或
//!   `<plugin>/<exe>`，绝不查共享包、绝不查 PATH（防跨插件目录读取 / 路径穿越）。
//!
//! 两条权限都必须先由 manifest 声明、再由用户在安装或启用时确认（授权快照），
//! 二者缺一即拒绝并写安全日志。
//!
//! 安全边界（全部不可由插件放宽）：
//! - 程序白名单仅 `ffmpeg` / `ffprobe`（或持有 `exec:local` 时的插件自带程序）；
//! - shell 程序（`cmd` / `powershell` / `bash` …）即使持有 `exec:local` 也拒绝；
//! - 默认 `cwd` = 插件根目录；显式 cwd 只允许插件根目录或下载根目录内；
//! - 绝对路径参数同样只允许上述两个根目录；
//! - 绝不经过 shell，参数以数组传递；`env_clear()`；Windows `CREATE_NO_WINDOW`；
//! - 超时后杀整棵进程树（Windows Job Object `KILL_ON_JOB_CLOSE`）；
//! - stdout / stderr 各截断 1MB，按 UTF-8 替换非法序列。

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::hostapi::{allowed_roots, guard_path, is_within, normalize_existing};

/// 单次 exec 的最短预算。
const MIN_TIMEOUT_MS: u64 = 1_000;
/// 单次 exec 的最长预算（30 分钟，与 `limits.taskTimeoutMs` 默认值一致）。
const MAX_TIMEOUT_MS: u64 = 1_800_000;
/// stdout / stderr 各自的截断上限。
const OUTPUT_CAP: usize = 1024 * 1024;
/// stderr 流式读取的分块大小（beta11 · FF-1：逐行推给前端）。
const STDERR_CHUNK: usize = 4096;
/// `exec:ffmpeg` 白名单程序。
const ALLOWED_PROGRAMS: [&str; 2] = ["ffmpeg", "ffprobe"];

/// 即使持有 `exec:local` 也一律拒绝的 shell / 脚本宿主。
///
/// "绝不经过 shell" 是硬约束：允许插件自带 `cmd.exe` 等价于允许任意命令拼接，
/// 且会让参数数组的隔离语义失效。
const DENIED_PROGRAMS: [&str; 5] = ["cmd", "powershell", "pwsh", "bash", "sh"];

/// 单次 `ctx.exec` 的授权快照（由 `hostapi` 依 manifest + 用户授权计算，插件无法伪造）。
#[derive(Clone, Copy, Debug, Default)]
pub struct ExecGrant {
    /// `exec:ffmpeg`：允许 `ffmpeg` / `ffprobe`。
    pub allow_ffmpeg: bool,
    /// `exec:local`：允许插件自带目录内的其它可执行文件。
    pub allow_local: bool,
}

/// 运行中的子进程登记表：`plugin_id -> [pid]`（beta11 · B4 取消级联）。
///
/// 只登记本模块 spawn 出来的 FFmpeg / FFprobe；`kill_plugin` 据此强杀。
static RUNNING: OnceLock<Mutex<HashMap<String, Vec<u32>>>> = OnceLock::new();

fn running() -> &'static Mutex<HashMap<String, Vec<u32>>> {
    RUNNING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 登记一个正在运行的子进程。
fn register(plugin_id: &str, pid: u32) {
    let mut guard = running().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let list = guard.entry(plugin_id.to_string()).or_default();
    if !list.contains(&pid) {
        list.push(pid);
    }
}

/// 注销一个已退出 / 超时的子进程。
fn unregister(plugin_id: &str, pid: u32) {
    let mut guard = running().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let empty = match guard.get_mut(plugin_id) {
        Some(list) => {
            list.retain(|item| *item != pid);
            list.is_empty()
        }
        None => false,
    };
    if empty {
        guard.remove(plugin_id);
    }
}

/// 强杀某插件登记的全部子进程，返回**已发送终止请求**的进程数。
///
/// 调用点：取消虚拟任务、禁用插件、卸载插件、宿主关闭（见 `manager`）。
/// 临时分片目录的清理仍由插件的 `onTaskCancel` 负责（宿主不猜路径）。
pub fn kill_plugin(plugin_id: &str) -> usize {
    let pids: Vec<u32> = {
        let mut guard = running().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.remove(plugin_id).unwrap_or_default()
    };
    if pids.is_empty() {
        return 0;
    }
    #[cfg(windows)]
    {
        pids.iter().filter(|pid| terminate_pid(**pid)).count()
    }
    #[cfg(not(windows))]
    {
        // 项目只发布 Windows；其他平台保持可编译并以 0 表示"未处理"。
        let _ = pids;
        0
    }
}

/// Windows：`TerminateProcess` 单个进程（不加 Job 也能杀 FFmpeg 本身）。
#[cfg(windows)]
fn terminate_pid(pid: u32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, pid) else {
            return false;
        };
        let killed = TerminateProcess(handle, 1).is_ok();
        let _ = CloseHandle(handle);
        killed
    }
}

/// 执行一次 `ctx.exec`，返回 JSON 字符串。
///
/// `plugin_dir` 由插件管理器注入（插件根目录），既作为默认 cwd，
/// 也作为自带 `bin/` 的解析根；`plugin_id` 用于进程登记（取消时强杀）；
/// `grant` 是本次调用的授权快照（`exec:ffmpeg` / `exec:local`）；
/// `sink`（beta11 · FF-1）在 stderr 每读到一行时回调（前端实时进度）。
pub fn run(
    payload: &str,
    plugin_dir: &Path,
    plugin_id: &str,
    grant: ExecGrant,
    sink: Option<Arc<dyn Fn(&str) + Send + Sync>>,
) -> String {
    let opts: Value = serde_json::from_str(payload).unwrap_or(Value::Null);

    let program = opts
        .get("program")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if program.is_empty() {
        return err("缺少 program 参数");
    }

    // 授权归属先判定：ffmpeg / ffprobe 属 `exec:ffmpeg`，其余程序属 `exec:local`。
    // 权限不足时不在此返回业务错误，统一经 `deny_exec` 留痕（审计 + 插件日志）。
    let is_ffmpeg = ALLOWED_PROGRAMS.contains(&program.as_str());
    if is_ffmpeg && !grant.allow_ffmpeg {
        return deny_exec(plugin_id, &program, "未声明或未授权 exec:ffmpeg");
    }
    if !is_ffmpeg {
        if DENIED_PROGRAMS.contains(&program.as_str()) {
            return deny_exec(
                plugin_id,
                &program,
                "禁止经由 shell 执行（参数以数组传递、不经过 shell 是宿主硬约束）",
            );
        }
        if !grant.allow_local {
            return deny_exec(plugin_id, &program, "未声明或未授权 exec:local");
        }
    }

    let args: Vec<String> = opts
        .get("args")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    // 默认 cwd = 插件根目录（自包含依赖的关键：插件无需知道宿主安装位置）。
    let cwd = match resolve_cwd(opts.get("cwd").and_then(Value::as_str), plugin_dir) {
        Ok(path) => path,
        Err(msg) => return err(&msg),
    };
    if !cwd.is_dir() {
        return err(&format!("工作目录不存在：{}", cwd.display()));
    }

    // 绝对路径参数逐个复核：只允许插件目录或下载目录内。
    for arg in &args {
        let raw = Path::new(arg);
        if raw.is_absolute() && !allowed_path(raw, plugin_dir) {
            return err(&format!("参数路径越权：{arg}"));
        }
    }

    // beta11 · FF-3：`.tmp` 防覆盖约定的两个路径同样走下载目录白名单。
    let tmp_path = match optional_guarded_path(&opts, "tmpPath") {
        Ok(path) => path,
        Err(msg) => return err(&msg),
    };
    let output_path = match optional_guarded_path(&opts, "outputPath") {
        Ok(path) => path,
        Err(msg) => return err(&msg),
    };

    let timeout_ms = opts
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .unwrap_or(MAX_TIMEOUT_MS)
        .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);

    let exe = match if is_ffmpeg {
        // ffmpeg / ffprobe：插件 bin/ → 共享包 → PATH（原语义）。
        resolve_program(&program, plugin_dir)
    } else {
        // 其它程序：只允许插件自身目录，绝不查共享包 / PATH。
        resolve_local_program(&program, plugin_dir)
    } {
        Ok(path) => path,
        Err(msg) => return err(&msg),
    };

    let mut result = match spawn_and_wait(
        &exe,
        &args,
        &cwd,
        Duration::from_millis(timeout_ms),
        plugin_id,
        sink,
    ) {
        Ok(value) => value,
        Err(msg) => return err(&msg),
    };

    // 成功改名 / 失败清理：只在插件声明了 tmpPath 时介入（其它调用零变化）。
    let mut finalized = false;
    let mut cleaned = false;
    if let Some(tmp) = tmp_path.as_deref() {
        let succeeded = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
        if succeeded && tmp.exists() {
            if let Some(out) = output_path.as_deref() {
                if out.exists() {
                    // 绝不覆盖：让插件换一个带时间戳的输出名。
                    return err("目标文件已存在，请使用带时间戳的输出名");
                }
                if let Some(parent) = out.parent() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        cleaned = std::fs::remove_file(tmp).is_ok();
                        return json!({
                            "ok": false,
                            "error": format!("创建输出目录失败：{e}"),
                            "finalized": false,
                            "cleaned": cleaned,
                        })
                        .to_string();
                    }
                }
                match move_file(tmp, out) {
                    Ok(()) => finalized = true,
                    Err(msg) => {
                        cleaned = std::fs::remove_file(tmp).is_ok();
                        return json!({
                            "ok": false,
                            "error": msg,
                            "finalized": false,
                            "cleaned": cleaned,
                        })
                        .to_string();
                    }
                }
            }
        } else if tmp.exists() && std::fs::remove_file(tmp).is_ok() {
            // 失败 / 超时 / 非 0：宿主负责清掉半成品 .tmp。
            cleaned = true;
        }
    }

    if let Some(object) = result.as_object_mut() {
        object.insert("finalized".to_string(), Value::Bool(finalized));
        object.insert("cleaned".to_string(), Value::Bool(cleaned));
    }
    result.to_string()
}

/// 解析可选的受控路径字段（缺失 / 空白 → `None`）。
fn optional_guarded_path(opts: &Value, key: &str) -> Result<Option<PathBuf>, String> {
    let Some(raw) = opts
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    else {
        return Ok(None);
    };
    guard_path(raw).map(Some)
}

/// 同卷 `rename`；跨卷降级 `copy` + 删源（与 `ctx.fs.move` 同一策略）。
fn move_file(src: &Path, dest: &Path) -> Result<(), String> {
    match std::fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(e) if super::hostapi::is_cross_device(&e) => {
            std::fs::copy(src, dest).map_err(|e| format!("跨卷复制失败：{e}"))?;
            std::fs::remove_file(src)
                .map_err(|e| format!("跨卷移动已复制但删除源文件失败：{e}"))?;
            Ok(())
        }
        Err(e) => Err(format!("输出改名失败：{e}")),
    }
}

/// 解析白名单程序：插件自带 `bin/` → 共享包 `.shared/<共享id>/` → PATH。
pub fn resolve_program(program: &str, plugin_dir: &Path) -> Result<PathBuf, String> {
    let exe_name = if cfg!(windows) {
        format!("{program}.exe")
    } else {
        program.to_string()
    };

    // 1) 插件自包含：<plugin>/bin/<program>.exe（单插件可覆盖共享版本）
    let bundled = plugin_dir.join("bin").join(&exe_name);
    if bundled.is_file() {
        return Ok(bundled);
    }
    // 2) 共享包：包根平铺的 <shared>/<program>.exe（beta12.3 主布局），
    //    其次兼容旧格式 <shared>/bin/<program>.exe。
    if let Some(root) = plugin_dir.parent() {
        if let Some(found) = resolve_in_shared(&root.join(".shared"), &exe_name) {
            return Ok(found);
        }
    }
    // 3) 系统 PATH（开发者本机已装 FFmpeg 时的兜底）
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(&exe_name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    Err(format!(
        "{exe_name} 未找到：请检查插件 bin/{exe_name}、共享包 data/plugins/.shared/<共享id>/{exe_name}（优先）或 data/plugins/.shared/<共享id>/bin/{exe_name}（兼容旧布局），或系统 PATH 中已安装"
    ))
}

/// 按目录名顺序遍历共享包：先 `<shared>/<exe>`（beta12.3 平铺布局），
/// 再 `<shared>/bin/<exe>`（兼容 beta12.2 旧布局）。
///
/// 仅扫描 `.shared/` 下的**一级子目录**（备份 / 临时点目录跳过），不递归下钻。
fn resolve_in_shared(shared_root: &Path, exe_name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(shared_root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| !name.starts_with('.'))
                    .unwrap_or(false)
        })
        .collect();
    dirs.sort();
    for dir in dirs {
        // beta12.3：平铺优先（与插件包内 bin/ 的平铺语义一致），旧 bin/ 兜底。
        let flat = dir.join(exe_name);
        if flat.is_file() {
            return Some(flat);
        }
        let bundled = dir.join("bin").join(exe_name);
        if bundled.is_file() {
            return Some(bundled);
        }
    }
    None
}

/// 解析插件自带程序（`exec:local`）：只允许 `<plugin>/bin/<exe>` 或 `<plugin>/<exe>`。
///
/// 三重防护：
/// 1. 程序名本身不得含路径分隔符 / 盘符 / `..`（否则 `bin/../../x` 可直接穿越）；
/// 2. 只在这两个固定位置查找，**绝不**扫描 `.shared/` 与系统 `PATH`；
/// 3. 命中后再用 `normalize_existing` 复核仍在插件根目录内（软链接 / 大小写兜底）。
pub fn resolve_local_program(program: &str, plugin_dir: &Path) -> Result<PathBuf, String> {
    if program.contains('/') || program.contains('\\') || program.contains(':') {
        return Err(format!("程序名非法：`{program}`（不得包含路径分隔符或盘符）"));
    }
    if program.contains("..") {
        return Err(format!("程序名非法：`{program}`（不得包含 `..`）"));
    }

    let exe_name = if cfg!(windows) {
        format!("{program}.exe")
    } else {
        program.to_string()
    };
    let root = normalize_existing(plugin_dir);

    for candidate in [
        plugin_dir.join("bin").join(&exe_name),
        plugin_dir.join(&exe_name),
    ] {
        if !candidate.is_file() {
            continue;
        }
        let normalized = normalize_existing(&candidate);
        if !is_within(&root, &normalized) {
            return Err(format!("程序路径越权：{}", candidate.display()));
        }
        return Ok(normalized);
    }

    Err(format!(
        "{exe_name} 未找到：持有 exec:local 的插件只能使用自身目录内的程序（bin/{exe_name} 或 {exe_name}），不支持共享包或系统 PATH"
    ))
}

/// 显式 cwd：绝对路径只允许插件根 / 下载根；相对路径按下载根解析（与 fs / 分片目录一致）。
fn resolve_cwd(input: Option<&str>, plugin_dir: &Path) -> Result<PathBuf, String> {
    let Some(text) = input.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(plugin_dir.to_path_buf());
    };
    let raw = Path::new(text);
    if raw.is_absolute() {
        let normalized = normalize_existing(raw);
        if is_within(&normalize_existing(plugin_dir), &normalized) {
            return Ok(normalized);
        }
        // 下载根目录内的绝对路径交给统一白名单校验（含软链接复核）。
        return guard_path(text);
    }
    guard_path(text)
}

/// 绝对路径参数是否落在允许的根目录内（插件根 / 下载根 / 已登记的额外目录）。
fn allowed_path(raw: &Path, plugin_dir: &Path) -> bool {
    let normalized = normalize_existing(raw);
    if is_within(&normalize_existing(plugin_dir), &normalized) {
        return true;
    }
    allowed_roots()
        .iter()
        .any(|root| is_within(root, &normalized))
}

fn err(msg: &str) -> String {
    json!({ "ok": false, "error": msg }).to_string()
}

/// 越权拒绝的统一出口：审计日志 + 插件日志 + tracing + 指标计数。
///
/// 拒绝必须留痕（用户安全确认的前提是"拒绝可追溯"），因此所有授权失败
/// 都汇聚到此函数，避免各处文案分叉或漏写审计。
fn deny_exec(plugin_id: &str, program: &str, reason: &str) -> String {
    let msg = format!("拒绝执行 `{program}`：{reason}");
    eprintln!("[plugin:{plugin_id}] {msg}");
    super::audit::log_deny(plugin_id, "exec.run");
    super::logs::push(plugin_id, "error", &msg);
    tracing::warn!(plugin = plugin_id, program = program, "{}", msg);
    crate::metrics::METRICS
        .plugin_deny
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    err(&msg)
}

/// 启动子进程并等待结束；超时杀整棵进程树。
fn spawn_and_wait(
    exe: &Path,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    plugin_id: &str,
    sink: Option<Arc<dyn Fn(&str) + Send + Sync>>,
) -> Result<Value, String> {
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // 环境隔离：只保留 Windows 运行必需的最小变量。
    cmd.env_clear();
    if let Some(root) = std::env::var_os("SystemRoot") {
        cmd.env("SystemRoot", root);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：绝不弹出控制台窗口。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("启动 {} 失败：{e}", exe.display()))?;
    let pid = child.id();

    // Windows：把子进程放进「句柄关闭即杀树」的 Job Object。
    #[cfg(windows)]
    let mut job = JobGuard::attach(pid);

    // beta11 · B4：登记 pid，取消 / 禁用 / 卸载时可强杀。
    register(plugin_id, pid);

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_reader = std::thread::spawn(move || read_stream(stdout, None));
    let err_reader = std::thread::spawn(move || read_stream(stderr, sink));

    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(e) => {
                unregister(plugin_id, pid);
                let _ = child.kill();
                return Err(format!("等待进程失败：{e}"));
            }
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    if timed_out {
        #[cfg(windows)]
        if let Some(job) = job.take() {
            job.terminate();
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    // 正常结束也要关闭 Job 句柄（此时 Job 内已无存活进程）。
    #[cfg(windows)]
    drop(job);

    // 进程已收敛（正常 / 超时）：注销登记，避免取消时误杀复用的 pid。
    unregister(plugin_id, pid);

    let (stdout_text, stdout_truncated) = out_reader.join().unwrap_or_default();
    let (stderr_text, stderr_truncated) = err_reader.join().unwrap_or_default();

    let code = status.as_ref().and_then(|s| s.code()).unwrap_or(-1);
    Ok(json!({
        "ok": !timed_out && code == 0,
        "code": code,
        "stdout": stdout_text,
        "stderr": stderr_text,
        "timedOut": timed_out,
        "truncated": stdout_truncated || stderr_truncated,
        "elapsedMs": started.elapsed().as_millis(),
    }))
}

/// 读取管道，最多 `OUTPUT_CAP` 字节；返回 (文本, 是否被截断)。
///
/// beta11 · FF-1：给出 `sink` 时按 `\n` 切行逐条回调（stderr 实时进度），
/// 累计推送量同样以 `OUTPUT_CAP` 为上界，避免长任务把前端事件通道灌满。
fn read_stream<R: Read>(
    reader: Option<R>,
    sink: Option<Arc<dyn Fn(&str) + Send + Sync>>,
) -> (String, bool) {
    let Some(mut reader) = reader else {
        return (String::new(), false);
    };
    let mut bytes: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut pending: Vec<u8> = Vec::new();
    let mut streamed: usize = 0;
    let mut buffer = [0u8; STDERR_CHUNK];

    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if bytes.len() < OUTPUT_CAP {
            let take = (OUTPUT_CAP - bytes.len()).min(read);
            bytes.extend_from_slice(&buffer[..take]);
            if take < read {
                truncated = true;
            }
        } else {
            truncated = true;
        }

        let Some(sink) = sink.as_ref() else {
            continue;
        };
        pending.extend_from_slice(&buffer[..read]);
        while let Some(pos) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=pos).collect();
            if streamed >= OUTPUT_CAP {
                // 已到推送上限：继续丢弃行（保持 pending 有界），不再发事件。
                continue;
            }
            let text = String::from_utf8_lossy(&line);
            let text = text.trim_end_matches(['\r', '\n']);
            streamed = streamed.saturating_add(text.len());
            sink(text);
        }
    }

    if let Some(sink) = sink {
        let tail = String::from_utf8_lossy(&pending);
        let tail = tail.trim_end_matches(['\r', '\n']);
        if !tail.is_empty() && streamed < OUTPUT_CAP {
            sink(tail);
        }
    }

    (String::from_utf8_lossy(&bytes).into_owned(), truncated)
}

/// Windows Job Object：`KILL_ON_JOB_CLOSE`，关闭句柄即杀光 Job 内全部进程。
#[cfg(windows)]
struct JobGuard(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl JobGuard {
    fn attach(pid: u32) -> Option<Self> {
        use std::ffi::c_void;

        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };
        use windows::core::PCWSTR;

        unsafe {
            let job = CreateJobObjectW(None, PCWSTR::null()).ok()?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .is_err()
            {
                let _ = CloseHandle(job);
                return None;
            }
            let process = match OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) {
                Ok(handle) => handle,
                Err(_) => {
                    let _ = CloseHandle(job);
                    return None;
                }
            };
            let assigned = AssignProcessToJobObject(job, process).is_ok();
            let _ = CloseHandle(process);
            if !assigned {
                let _ = CloseHandle(job);
                return None;
            }
            Some(Self(job))
        }
    }

    /// 关闭句柄 → 内核按 `KILL_ON_JOB_CLOSE` 杀掉整棵树。
    fn terminate(self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
impl Drop for JobGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{resolve_cwd, resolve_local_program, resolve_program, run, kill_plugin, ExecGrant};

    /// 仅持有 `exec:ffmpeg`（大多数既有插件）。
    fn grant_ffmpeg() -> ExecGrant {
        ExecGrant { allow_ffmpeg: true, allow_local: false }
    }

    /// 仅持有 `exec:local`。
    fn grant_local() -> ExecGrant {
        ExecGrant { allow_ffmpeg: false, allow_local: true }
    }

    /// 1.0.0-rc1 · 模块一：未声明 `exec:local` 时，非 ffmpeg 程序必须拒绝并点明权限。
    #[test]
    fn rejects_local_program_without_permission() {
        let result = run(
            r#"{"program":"customtool","args":[]}"#,
            Path::new("."),
            "com.test.exec",
            ExecGrant::default(),
            None,
        );
        assert!(
            result.contains("\"ok\":false") && result.contains("exec:local"),
            "未授权 exec:local 必须拒绝并点明权限：{result}"
        );
    }

    /// 未声明 `exec:ffmpeg` 时，ffmpeg 同样拒绝（授权收紧不得漏口）。
    #[test]
    fn rejects_ffmpeg_without_permission() {
        let result = run(
            r#"{"program":"ffmpeg","args":["-version"]}"#,
            Path::new("."),
            "com.test.exec",
            ExecGrant::default(),
            None,
        );
        assert!(
            result.contains("\"ok\":false") && result.contains("exec:ffmpeg"),
            "未授权 exec:ffmpeg 必须拒绝并点明权限：{result}"
        );
    }

    /// beta11 · D-2：shell 程序即使持有 `exec:local` 也一律拒绝（不经过 shell 是硬约束）。
    #[test]
    fn rejects_shell_programs() {
        for program in ["cmd", "powershell", "bash"] {
            let payload = format!(r#"{{"program":"{program}","args":["-c","echo hi"]}}"#);
            let result = run(&payload, Path::new("."), "com.test.exec", grant_local(), None);
            assert!(
                result.contains("\"ok\":false"),
                "{program} 必须被拒绝：{result}"
            );
            assert!(
                result.contains("shell"),
                "{program} 的拒绝文案应点明 shell 硬约束：{result}"
            );
        }
    }

    #[test]
    fn rejects_empty_program() {
        let result = run(r#"{"args":[]}"#, Path::new("."), "com.test.exec", grant_ffmpeg(), None);
        assert!(result.contains("\"ok\":false"));
    }

    /// beta11 · D-3：缺少 ffmpeg 时必须是结构化错误（不是 panic / 空返回）。
    ///
    /// 本机 PATH 里若恰好装了 ffmpeg 就无法构造"缺失"场景，此时跳过（与
    /// `bundled_hls_plugin_loads_and_claims_m3u8` 同一策略）。
    #[test]
    fn missing_binary_returns_structured_error() {
        let dir = tempfile::tempdir().unwrap();
        if resolve_program("ffmpeg", dir.path()).is_ok() {
            eprintln!("跳过：本机 PATH 中存在 ffmpeg，无法构造缺失场景");
            return;
        }
        let result = run(
            r#"{"program":"ffmpeg","args":["-version"]}"#,
            dir.path(),
            "com.test.exec",
            grant_ffmpeg(),
            None,
        );
        assert!(
            result.contains("\"ok\":false") && result.contains("bin/"),
            "缺 ffmpeg 必须返回结构化错误且指向 bin/：{result}"
        );
    }

    /// 1.0.0-rc1：持有 `exec:local` 但插件目录内没有该程序 → 结构化错误且不泄漏 PATH。
    #[test]
    fn missing_local_program_reports_own_dir_only() {
        let dir = tempfile::tempdir().unwrap();
        let result = run(
            r#"{"program":"customtool","args":[]}"#,
            dir.path(),
            "com.test.exec",
            grant_local(),
            None,
        );
        assert!(
            result.contains("\"ok\":false") && result.contains("自身目录"),
            "缺失程序文案必须限定插件自身目录：{result}"
        );
    }

    /// 1.0.0-rc1：`exec:local` 只认插件根目录内的可执行文件。
    #[test]
    fn resolve_local_program_only_inside_plugin_dir() {
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("com.test.tool");
        std::fs::create_dir_all(plugin.join("bin")).unwrap();
        let exe = if cfg!(windows) { "customtool.exe" } else { "customtool" };

        // 共享包里同名程序绝不能被 `exec:local` 取用。
        let shared = root.path().join(".shared").join("com.harness.shared-tool");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join(exe), b"shared").unwrap();
        assert!(
            resolve_local_program("customtool", &plugin).is_err(),
            "exec:local 不得回落到 .shared/"
        );

        // 插件 bin/ 命中。
        let bundled = plugin.join("bin").join(exe);
        std::fs::write(&bundled, b"plugin").unwrap();
        assert_eq!(resolve_local_program("customtool", &plugin).unwrap(), bundled);
    }

    /// 1.0.0-rc1：程序名不得携带路径分隔符 / 盘符 / `..`（防目录穿越）。
    #[test]
    fn resolve_local_program_rejects_traversal_names() {
        let dir = Path::new("plugins/com.test.tool");
        for program in ["../evil", "bin/evil", r"bin\evil", r"C:\Windows\notepad", ".."] {
            assert!(
                resolve_local_program(program, dir).is_err(),
                "程序名 `{program}` 必须被拒绝"
            );
        }
    }

    /// beta11 · B4：（无登记时）强杀是幂等空操作，绝不容许 panic。
    #[test]
    fn kill_plugin_without_running_process_is_noop() {
        assert_eq!(kill_plugin("com.test.exec.not-running"), 0);
        assert_eq!(kill_plugin("com.test.exec.not-running"), 0);
    }

    #[test]
    fn resolve_prefers_plugin_bin() {
        let dir = std::env::temp_dir().join(format!("harness-plugin-{}", std::process::id()));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let name = if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        };
        let marker = bin.join(name);
        std::fs::write(&marker, b"stub").unwrap();

        let resolved = resolve_program("ffmpeg", &dir).unwrap();
        assert_eq!(resolved, marker, "必须优先使用插件自带 bin/");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// beta12.3：共享包布局 —— 插件 `bin/` > `.shared/<id>/` 平铺 > `.shared/<id>/bin/`。
    #[test]
    fn resolve_shared_package_layout_and_order() {
        let root = tempfile::tempdir().unwrap();
        let exe = if cfg!(windows) { "tool.exe" } else { "tool" };
        let plugin = root.path().join("com.test.plugin");
        std::fs::create_dir_all(&plugin).unwrap();

        // 共享包：同时放根部 tool.exe（应优先）与 bin/tool.exe（兼容旧布局兜底）
        let shared = root.path().join(".shared").join("com.harness.shared-tool");
        std::fs::create_dir_all(shared.join("bin")).unwrap();
        let shared_bin = shared.join("bin").join(exe);
        std::fs::write(&shared_bin, b"shared-bin").unwrap();
        let shared_flat = shared.join(exe);
        std::fs::write(&shared_flat, b"shared-flat").unwrap();

        // 1) 插件无 bin/ → 平铺命中优先（beta12.3）
        assert_eq!(resolve_program("tool", &plugin).unwrap(), shared_flat);

        // 2) 插件自带 bin/ 仍优先于共享包（单插件覆盖）
        std::fs::create_dir_all(plugin.join("bin")).unwrap();
        let plugin_bin = plugin.join("bin").join(exe);
        std::fs::write(&plugin_bin, b"plugin-bin").unwrap();
        assert_eq!(resolve_program("tool", &plugin).unwrap(), plugin_bin);

        // 3) 删除插件 bin 与平铺文件后 → 回退共享包 bin/（兼容旧格式）
        std::fs::remove_file(&plugin_bin).unwrap();
        std::fs::remove_file(&shared_flat).unwrap();
        assert_eq!(resolve_program("tool", &plugin).unwrap(), shared_bin);
    }

    /// beta12.3：错误文案必须同时指向平铺 `.shared/<共享id>/<exe>`（优先）与旧 bin/。
    #[test]
    fn missing_program_hint_mentions_shared_package_layout() {
        match resolve_program("ffmpeg-not-exist", Path::new(".")) {
            Ok(path) => panic!("不该找到：{}", path.display()),
            Err(msg) => {
                assert!(
                    msg.contains(".shared/<共享id>/ffmpeg-not-exist.exe")
                        || msg.contains(".shared/<共享id>/ffmpeg-not-exist"),
                    "错误文案应指向共享包平铺路径：{msg}"
                );
                assert!(
                    msg.contains(".shared/<共享id>/bin/"),
                    "错误文案应保留旧 bin/ 布局提示：{msg}"
                );
                assert!(msg.contains("优先"), "错误文案应说明平铺优先：{msg}");
            }
        }
    }

    #[test]
    fn missing_program_reports_bilingual_hint() {
        match resolve_program("ffmpeg-not-exist", Path::new(".")) {
            Ok(path) => panic!("不该找到：{}", path.display()),
            Err(msg) => assert!(msg.contains("bin/"), "错误文案应指向 bin/：{msg}"),
        }
    }

    #[test]
    fn default_cwd_is_plugin_dir() {
        let dir = Path::new("plugins/hls-downloader");
        assert_eq!(resolve_cwd(None, dir).unwrap(), dir);
        assert_eq!(resolve_cwd(Some("   "), dir).unwrap(), dir);
    }

    #[test]
    fn absolute_cwd_outside_roots_is_rejected() {
        let outside = if cfg!(windows) {
            r"C:\Windows\System32"
        } else {
            "/etc"
        };
        assert!(
            resolve_cwd(Some(outside), Path::new("plugins/x")).is_err(),
            "插件不得把 cwd 指向系统目录"
        );
    }
}
