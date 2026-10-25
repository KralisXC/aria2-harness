//! 插件 -> 宿主能力实现（ctx 的 Rust 侧）。
//!
//! L1 能力默认拒绝：JS 全局被裁剪到只剩预注入的 `harness`，唯一出口是
//! `__harness_host(op, payloadJson)`；未在 manifest 声明对应权限的调用一律拒绝并审计。
//!
//! 同步桥：log/notify/emit/settings 在插件线程内直接完成；只有需要访问引擎的
//! getTask/getTasks 才经 `HostBridge` 转发到 tokio 侧，且有 1.5s 明确上界。

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::AppError;
use crate::host::HostContext;

use super::interceptor::ResolveOutcome;

/// 能力令牌：默认拒绝，需在 manifest.permissions 中显式声明。
pub const CAP_NOTIFY: &str = "notify";
pub const CAP_DOWNLOADS_READ: &str = "downloads:read";
/// 下载目录写权限（`ctx.fs.mkdir` / `ctx.fs.move`）。授权范围严格限定在下载根目录内。
pub const CAP_FS_WRITE_DOWNLOADS: &str = "fs:write:downloads";
/// 前置拦截改写权（`ctx.resolveUrl`）。默认拒绝，需显式声明，防插件静默劫持下载源。
pub const CAP_DOWNLOADS_INTERCEPT: &str = "downloads:intercept";

/// 宿主同步调用的一次请求（仅 getTask/getTasks 使用）。
pub struct HostCall {
    pub plugin_id: String,
    pub op: String,
    pub payload: String,
    pub reply: Sender<String>,
}

/// 插件线程侧的宿主桥句柄。
#[derive(Clone)]
pub struct HostBridge {
    tx: tokio::sync::mpsc::UnboundedSender<HostCall>,
}

impl HostBridge {
    /// 同步阻塞等待宿主结果；超时/失败一律返回空串（JS 侧视为 null/空）。
    pub fn call(&self, plugin_id: &str, op: &str, payload: &str) -> String {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<String>();
        let call = HostCall {
            plugin_id: plugin_id.to_string(),
            op: op.to_string(),
            payload: payload.to_string(),
            reply: reply_tx,
        };
        if self.tx.send(call).is_err() {
            return String::new();
        }
        reply_rx
            .recv_timeout(Duration::from_millis(1500))
            .unwrap_or_default()
    }
}

#[cfg(test)]
impl HostBridge {
    /// 测试专用：直接以发送端构造桥（正常路径只能由 [`spawn_worker`] 创建）。
    ///
    /// 仅 `cfg(test)` 可见，生产二进制零影响；供 `wasm_runtime` 的单测用假回执
    /// 验证 `ctx.getTask` 的完整往返。
    pub(crate) fn for_test(tx: tokio::sync::mpsc::UnboundedSender<HostCall>) -> Self {
        Self { tx }
    }
}

/// 启动宿主侧 RPC 服务任务，返回插件线程可用的桥。
///
/// 阶段三：宿主实现改由进程级槽位注入（[`HostContext`]），此处仅把它透传给
/// 需要引擎的能力调用（`serve`）。`host` 作为启动参数保留，便于后续扩展宿主侧能力。
pub fn spawn_worker(host: Arc<dyn HostContext>) -> HostBridge {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<HostCall>();
    crate::rt().spawn(async move {
        while let Some(call) = rx.recv().await {
            let result = serve(&host, &call).await;
            // 通道回执：插件侧 `recv_timeout(1.5s)` 超时后会 drop receiver，此处 send 失败
            // 属预期竞态（插件已放弃等待），记 debug 便于排查"插件侧等不到回执"的问题。
            if call.reply.send(result).is_err() {
                tracing::debug!(
                    "插件宿主调用回执未送达（插件侧已超时/退出）：plugin={} op={}",
                    call.plugin_id,
                    call.op
                );
            }
        }
    });
    HostBridge { tx }
}

/// 执行一次需要引擎的能力调用（权限校验已在插件侧完成）。
async fn serve(_host: &Arc<dyn HostContext>, call: &HostCall) -> String {
    // 阶段三：引擎连接状态改走进程级全局访问器，插件侧不再持有框架句柄。
    let state = crate::state();
    let client = match crate::client_or_err(state).await {
        Ok(client) => client,
        Err(e) => {
            // 启动期时序：`onAppStart` 由插件管理器在宿主 bootstrap 后立即广播，
            // 而引擎连接是异步的 —— 此时 getTask/getTasks 必然拿到
            // `Aria2Disconnected("aria2 RPC 尚未连接…")`。这是"还没准备好"而不是
            // 插件出错，降为 info，避免控制台出现刺眼的失败日志。
            if matches!(e, crate::error::AppError::Aria2Disconnected(_)) {
                tracing::info!(
                    "[plugin:{}] {} 跳过：引擎尚未就绪（启动期正常时序）",
                    call.plugin_id,
                    call.op
                );
            } else {
                eprintln!("[plugin:{}] {} 失败：{e}", call.plugin_id, call.op);
            }
            return if call.op == "getTasks" {
                "[]".to_string()
            } else {
                String::new()
            };
        }
    };
    let keys: Option<Vec<String>> = Some(crate::TASK_KEYS.iter().map(|k| k.to_string()).collect());

    match call.op.as_str() {
        "getTask" => {
            let gid = serde_json::from_str::<Value>(&call.payload)
                .ok()
                .and_then(|v| v.get("gid").and_then(|g| g.as_str()).map(str::to_string));
            let Some(gid) = gid else {
                return String::new();
            };
            match client.tell_status(&gid, keys).await {
                Ok(map) => serde_json::to_string(&Value::Object(map)).unwrap_or_default(),
                Err(e) => {
                    eprintln!("[plugin:{}] tellStatus 失败：{e}", call.plugin_id);
                    String::new()
                }
            }
        }
        "getTasks" => match client.tell_active(keys).await {
            Ok(list) => serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string()),
            Err(e) => {
                eprintln!("[plugin:{}] tellActive 失败：{e}", call.plugin_id);
                "[]".to_string()
            }
        },
        _ => String::new(),
    }
}

/// 注入 ctx 时随插件一起捕获的能力上下文。
#[derive(Clone)]
pub struct PluginHostContext {
    pub plugin_id: String,
    pub permissions: Arc<HashSet<String>>,
    /// 设置快照：由插件管理器持有同一 `Arc`，保存后无需重载即可读到新值。
    pub settings: Arc<std::sync::RwLock<HashMap<String, Value>>>,
    /// 阶段三：宿主抽象（发事件 / 探活前端），取代原先的 `tauri::AppHandle`。
    pub host: Arc<dyn HostContext>,
    pub bridge: HostBridge,
}

impl PluginHostContext {
    pub fn allows(&self, cap: &str) -> bool {
        self.permissions.contains(cap)
    }

    /// 处理一次来自插件的 `__harness_host(op, payload)` 调用。
    pub fn respond(&self, op: &str, payload: &str) -> String {
        match op {
            "log" => {
                let (level, msg) = parse_pair(payload, "level", "info", "msg", "");
                println!("[plugin:{}][{level}] {msg}", self.plugin_id);
                String::new()
            }
            "notify" => {
                if !self.allows(CAP_NOTIFY) {
                    self.audit_deny(op);
                    return String::new();
                }
                let (title, body) = parse_pair(payload, "title", "插件通知", "body", "");
                // 退出期主窗口可能已销毁：emit 前先判定，避免 PostMessage 无效句柄报错。
                if self.frontend_alive() {
                    self.host.emit(
                        "plugin://notify",
                        json!({ "pluginId": self.plugin_id, "title": title, "body": body }),
                    );
                }
                println!("[plugin:{}] 通知：{title} — {body}", self.plugin_id);
                String::new()
            }
            "emit" => {
                let value: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
                let name = value
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let data = value.get("payload").cloned().unwrap_or(Value::Null);
                if self.frontend_alive() {
                    self.host.emit(
                        "plugin://emit",
                        json!({ "pluginId": self.plugin_id, "name": name, "payload": data }),
                    );
                }
                String::new()
            }
            "settings.get" => {
                let key = serde_json::from_str::<Value>(payload)
                    .ok()
                    .and_then(|v| v.get("key").and_then(|k| k.as_str()).map(str::to_string));
                let settings = self
                    .settings
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match key.and_then(|k| settings.get(&k).cloned()) {
                    Some(v) => serde_json::to_string(&v).unwrap_or_default(),
                    None => String::new(),
                }
            }
            "settings.all" => {
                let settings = self
                    .settings
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                serde_json::to_string(&*settings).unwrap_or_else(|_| "{}".to_string())
            }
            "getTask" | "getTasks" => {
                if !self.allows(CAP_DOWNLOADS_READ) {
                    self.audit_deny(op);
                    return String::new();
                }
                self.bridge.call(&self.plugin_id, op, payload)
            }
            "resolveUrl" => {
                if !self.allows(CAP_DOWNLOADS_INTERCEPT) {
                    self.audit_deny(op);
                    return deny_json();
                }
                let url = get_str(payload, "url");
                if url.trim().is_empty() {
                    return err_json("空 URL，已忽略");
                }
                match crate::interceptor().resolve(&self.plugin_id, &url) {
                    ResolveOutcome::Accepted => ok_json(),
                    ResolveOutcome::Superseded => {
                        // 仲裁落败（非安全违规）：独立审计行，不计入 plugin_deny。
                        super::audit::log_intercept_locked(&self.plugin_id, "已被其它插件抢先改写");
                        err_json("该请求的 URL 已被其它插件改写，本次改写被忽略")
                    }
                    ResolveOutcome::LowerPriority => {
                        super::audit::log_intercept_locked(
                            &self.plugin_id,
                            "仍有更高优先级插件正在处理",
                        );
                        err_json("仍有更高优先级插件正在处理，本次改写被忽略")
                    }
                    ResolveOutcome::Late => {
                        super::audit::log_late_result(&self.plugin_id);
                        err_json("拦截会话已结束，本次改写迟到被丢弃")
                    }
                }
            }
            "fs.mkdir" => {
                if !self.allows(CAP_FS_WRITE_DOWNLOADS) {
                    self.audit_deny(op);
                    return deny_json();
                }
                let path = get_str(payload, "path");
                match guard_path(&path)
                    .map_err(|e| AppError::FsDenied(e))
                    .and_then(|p| {
                        std::fs::create_dir_all(&p)
                            .map_err(|e| AppError::fs("创建目录失败", e))
                            .map(|_| p)
                    }) {
                    Ok(p) => {
                        println!("[plugin:{}] fs.mkdir 已就绪：{}", self.plugin_id, p.display());
                        ok_json()
                    }
                    Err(e) => {
                        eprintln!("[plugin:{}] fs.mkdir 拒绝：{e}", self.plugin_id);
                        err_json(&e.to_string())
                    }
                }
            }
            "fs.move" => {
                if !self.allows(CAP_FS_WRITE_DOWNLOADS) {
                    self.audit_deny(op);
                    return deny_json();
                }
                let src = get_str(payload, "src");
                let dest = get_str(payload, "dest");
                match fs_move(&src, &dest).map_err(AppError::from) {
                    Ok((from, to)) => {
                        println!(
                            "[plugin:{}] fs.move：{} -> {}",
                            self.plugin_id,
                            from.display(),
                            to.display()
                        );
                        ok_json()
                    }
                    Err(e) => {
                        eprintln!("[plugin:{}] fs.move 拒绝：{e}", self.plugin_id);
                        err_json(&e.to_string())
                    }
                }
            }
            _ => {
                self.audit_deny(op);
                String::new()
            }
        }
    }

    /// 主窗口是否仍存活（退出期返回 false，避免向已销毁窗口 PostMessage）。
    fn frontend_alive(&self) -> bool {
        self.host.frontend_alive()
    }

    /// 前置拦截的"完成回执"：由插件线程在 `onBeforeAddDownload` 返回后调用，
    /// 让宿主收敛等待（无论插件是否改写过 URL）。同时把该插件从待回报集合移除。
    pub fn intercept_complete(&self) {
        crate::interceptor().complete(&self.plugin_id);
    }

    /// L7 审计：记录每一次被拒绝的能力调用。
    ///
    /// 这里是**唯一收敛点** —— `respond` 内全部 6 处越权分支都调用本函数，
    /// 因此在此处统一落盘 + 计数即可覆盖所有拒绝路径，无需逐点重复。
    fn audit_deny(&self, op: &str) {
        eprintln!(
            "[plugin:{}] 能力调用被拒绝：{op}（manifest 未声明对应权限）",
            self.plugin_id
        );
        super::audit::log_deny(&self.plugin_id, op);
        crate::metrics::METRICS
            .plugin_deny
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

fn parse_pair(payload: &str, k1: &str, d1: &str, k2: &str, d2: &str) -> (String, String) {
    let value: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    let a = value
        .get(k1)
        .and_then(|v| v.as_str())
        .unwrap_or(d1)
        .to_string();
    let b = value
        .get(k2)
        .and_then(|v| v.as_str())
        .unwrap_or(d2)
        .to_string();
    (a, b)
}

// ---------------------------------------------------------------------------
// ctx.fs 下载目录沙箱（L3：路径白名单，防越权 / 穿越 / 软链接逃逸）
// ---------------------------------------------------------------------------

/// 从 payload JSON 取字符串字段（缺失返回空串）。
fn get_str(payload: &str, key: &str) -> String {
    serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|v| v.get(key).and_then(|x| x.as_str()).map(str::to_string))
        .unwrap_or_default()
}

fn ok_json() -> String {
    r#"{"ok":true}"#.to_string()
}

fn err_json(msg: &str) -> String {
    json!({ "ok": false, "error": msg }).to_string()
}

fn deny_json() -> String {
    r#"{"ok":false,"error":"权限不足"}"#.to_string()
}

/// 下载根目录（与 `lib.rs::default_download_dir` 一致），真实路径解析后词法归一。
///
/// 注意：Windows 上 `canonicalize` 会返回 `\\?\` 主动式前缀，这里剥离后再比对，
/// 否则分量前缀匹配会失败。目录通常已由引擎启动流程创建；不存在时退回字面量。
fn download_root() -> PathBuf {
    let root = crate::paths::downloads_dir();
    let root = root.as_path();
    std::fs::canonicalize(root)
        .map(|p| strip_verbatim(&p))
        .unwrap_or_else(|_| normalize_lexical(root))
}

/// 剥离 Windows 的 `\\?\` / `\\?\UNC\` 主动式前缀。
fn strip_verbatim(path: &Path) -> PathBuf {
    if let Some(s) = path.to_str() {
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

/// 词法归一化：消解 `.` 与 `..`（`..` 仅在可回溯时 pop，越界成分自然被丢弃）。
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(n) => out.push(n),
        }
    }
    out
}

/// `candidate` 是否等于 `root` 或以 `root` 为父（分量逐段比对，大小写不敏感）。
fn is_within(root: &Path, candidate: &Path) -> bool {
    let mut r = root.components();
    let mut c = candidate.components();
    loop {
        match (r.next(), c.next()) {
            (Some(a), Some(b)) => {
                let sa = a.as_os_str().to_string_lossy();
                let sb = b.as_os_str().to_string_lossy();
                if !sa.eq_ignore_ascii_case(&sb) {
                    return false;
                }
            }
            (None, _) => return true,
            (Some(_), None) => return false,
        }
    }
}

/// 从 `path` 向上找到第一个真实存在的祖先（用于软链接逃逸复核）。
fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut cur = path;
    loop {
        if cur.exists() {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
}

/// Windows 保留设备名（含带扩展名形式的基名，如 `CON.txt`）。
fn is_reserved_name(name: &str) -> bool {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let base = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    RESERVED.contains(&base.as_str())
}

/// 严格路径白名单校验：返回归一化后的绝对路径，任何越权一律 Err。
///
/// 防御面（按顺序）：
///  1. 空路径 / NUL 字节 / UNC 与设备命名空间（`\\`、`//`）
///  2. 词法归一化 `..`（上溯逃逸）
///  3. 每个分量的 `:`（备用数据流）与 Windows 保留设备名
///  4. 归一化结果必须落在下载根目录内（分量前缀比对，大小写不敏感）
///  5. 对已存在的最深祖先做真实路径复核，拦截软链接指向目录外
fn guard_path(input: &str) -> Result<PathBuf, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("路径为空".to_string());
    }
    if raw.contains('\0') {
        return Err("路径含 NUL 字符".to_string());
    }
    if raw.starts_with(r"\\") || raw.starts_with("//") {
        return Err("禁止 UNC / 设备路径".to_string());
    }

    let path = Path::new(raw);
    let root = download_root();
    // 相对路径一律相对下载根目录解析；绝对路径（含盘符）按原样归一化。
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };

    let mut normalized = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::Prefix(p) => normalized.push(p.as_os_str()),
            Component::RootDir => normalized.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(n) => {
                let name = n.to_string_lossy();
                if name.contains(':') {
                    return Err(format!("禁止备用数据流（含 ':' 的分量）：{name}"));
                }
                if is_reserved_name(&name) {
                    return Err(format!("禁止 Windows 保留设备名：{name}"));
                }
                normalized.push(n);
            }
        }
    }

    if !is_within(&root, &normalized) {
        return Err(format!("路径越权（不在下载目录内）：{input}"));
    }

    // 软链接逃逸：对已存在的最深祖先 canonicalize 后再比对一次。
    if let Some(existing) = nearest_existing(&normalized) {
        let real = std::fs::canonicalize(&existing)
            .map(|p| strip_verbatim(&p))
            .unwrap_or(existing);
        if !is_within(&root, &real) {
            return Err(format!("路径越权（真实路径指向下载目录外）：{input}"));
        }
    }

    Ok(normalized)
}

/// 判断 `rename` 失败是否因源与目标不在同一卷（需要 copy 降级）。
fn is_cross_device(err: &std::io::Error) -> bool {
    #[cfg(windows)]
    {
        // ERROR_NOT_SAME_DEVICE
        err.raw_os_error() == Some(17)
    }
    #[cfg(not(windows))]
    {
        // EXDEV
        err.raw_os_error() == Some(18)
    }
}

/// 白名单内移动文件（同卷 `rename`；目标目录不存在时自动补齐）。
///
/// 跨盘（`ERROR_NOT_SAME_DEVICE` / `EXDEV`）时降级为 `copy` + 删源，
/// 避免 auto-sort 类插件在下载目录与目标盘不同卷时直接失败。
fn fs_move(src: &str, dest: &str) -> Result<(PathBuf, PathBuf), String> {
    let from = guard_path(src)?;
    let to = guard_path(dest)?;
    if !from.exists() {
        return Err(format!("源文件不存在：{}", from.display()));
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目标目录失败：{e}"))?;
    }
    match std::fs::rename(&from, &to) {
        Ok(()) => Ok((from, to)),
        Err(e) if is_cross_device(&e) => {
            if from.is_dir() {
                return Err(format!("跨盘移动目录不受支持：{}", from.display()));
            }
            std::fs::copy(&from, &to).map_err(|e| format!("跨盘复制失败：{e}"))?;
            std::fs::remove_file(&from)
                .map_err(|e| format!("跨盘移动已复制但删除源文件失败：{e}"))?;
            Ok((from, to))
        }
        Err(e) => Err(format!("移动失败：{e}")),
    }
}

// ---------------------------------------------------------------------------
// P0 安全单元测试：路径白名单（guard_path）必须拒绝一切越权写法
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::guard_path;

    /// 断言该输入**必须**被拒绝（返回 Err）。
    #[track_caller]
    fn denied(input: &str) {
        let result = guard_path(input);
        assert!(result.is_err(), "安全漏洞！应被拒绝却通过了：{input:?} -> {result:?}");
    }

    /// 断言该输入**必须**被允许（返回 Ok），用于证明沙箱不是"一律拒绝"。
    #[track_caller]
    fn allowed(input: &str) {
        let result = guard_path(input);
        assert!(result.is_ok(), "误杀！应被允许却被拒绝：{input:?} -> {result:?}");
    }

    #[test]
    fn rejects_parent_dir_traversal() {
        denied(r"..\..\windows\system32\evil.dll");
        denied(r"..\..\..\..\..\evil.txt");
        denied(r"sub\..\..\..\evil.txt");
        denied("../outside.txt");
    }

    #[test]
    fn rejects_unc_and_device_namespace() {
        denied(r"\\server\share\evil.txt");
        denied(r"\\?\C:\Windows\evil.txt");
        denied("//server/share/evil.txt");
    }

    #[test]
    fn rejects_windows_reserved_names() {
        denied("CON");
        denied("NUL");
        denied("aux.txt");
        denied(r"sub\COM1.log");
        denied("LPT9.bin");
    }

    #[test]
    fn rejects_alternate_data_streams() {
        denied("file.txt:stream");
        denied("evil:stream");
        denied(r"sub\report.pdf:evil");
    }

    #[test]
    fn rejects_absolute_paths_outside_root() {
        denied(r"C:\Windows\System32\evil.dll");
        denied(r"C:\other\outside.txt");
        // 与下载根同盘但不同前缀的"伪包含"目录也必须拒绝
        denied(r"C:\Users\Public\Downloads_outside\x.txt");
    }

    #[test]
    fn rejects_empty_and_nul() {
        denied("");
        denied("   ");
        denied("bad\0name.txt");
    }

    #[test]
    fn allows_paths_inside_download_root() {
        // 前置：确保下载根目录真实存在，以便 `canonicalize` 复核通过（与运行时一致）。
        std::fs::create_dir_all(crate::paths::downloads_dir()).unwrap();

        allowed("file.txt");
        allowed("sub/file.txt");
        allowed(r"sub\deep\nested.bin");
        assert!(guard_path("sub/deep/file.bin").is_ok());
    }
}
