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
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::AppError;
use crate::host::HostContext;

use super::interceptor::ResolveOutcome;
use super::manifest::Limits;

/// 能力令牌：默认拒绝，需在 manifest.permissions 中显式声明。
pub const CAP_NOTIFY: &str = "notify";
pub const CAP_DOWNLOADS_READ: &str = "downloads:read";
/// 下载目录写权限（`ctx.fs.*`）。授权范围严格限定在下载根目录内。
pub const CAP_FS_WRITE_DOWNLOADS: &str = "fs:write:downloads";
/// 前置拦截改写权（`ctx.resolveUrl`）。默认拒绝，需显式声明，防插件静默劫持下载源。
pub const CAP_DOWNLOADS_INTERCEPT: &str = "downloads:intercept";
/// beta10：出站 HTTP 能力（`ctx.httpGet`）。
pub const CAP_NET_HTTP: &str = "net:http";
/// beta10：批量派发下载任务（`ctx.addTasksAndWait`）。
pub const CAP_DOWNLOADS_WRITE: &str = "downloads:write";
/// beta10：执行 FFmpeg / FFprobe（`ctx.exec`）。
pub const CAP_EXEC_FFMPEG: &str = "exec:ffmpeg";
/// rc1 · 模块一：执行插件自带的可执行文件（非 ffmpeg）。
///
/// 双重闸门：manifest 必须声明 + 用户必须已授权（授权快照见 [`PluginHostContext::granted`]）。
/// 安装期另有红色安全确认（拒绝即中止安装）。
pub const CAP_EXEC_LOCAL: &str = "exec:local";

/// 普通宿主调用的默认同步等待上界（getTask / getTasks）。
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_millis(1500);
/// `tasks.addAndWait` 在插件侧声明的任务预算之外额外留出的收尾时间。
const TASK_CALL_GRACE: Duration = Duration::from_secs(15);
/// `tasks.addAndWait` 的**硬等待上界**（beta12.2）。
///
/// 插件线程在宿主桥上是同步阻塞的：若宿主工作线程崩溃 / 死锁，声明的 30 分钟
/// 预算会把插件线程一起拖死。120 秒兜底既覆盖正常分片批量，又能在异常时快速解冻。
const TASK_CALL_HARD_CAP: Duration = Duration::from_secs(120);
/// 宿主机并发处理上限：长任务（批量下载）不得堵塞其他插件的短调用。
const HOST_WORKER_CONCURRENCY: usize = 8;

/// 宿主同步调用的一次请求。
pub struct HostCall {
    pub plugin_id: String,
    pub op: String,
    pub payload: String,
    pub reply: Sender<String>,
    /// 插件侧等待上界（由 `HostBridge::call*` 决定）。
    pub timeout: Duration,
}

/// 插件线程侧的宿主桥句柄。
#[derive(Clone)]
pub struct HostBridge {
    tx: tokio::sync::mpsc::UnboundedSender<HostCall>,
}

impl HostBridge {
    /// 同步阻塞等待宿主结果；超时/失败一律返回空串（JS 侧视为 null/空）。
    pub fn call(&self, plugin_id: &str, op: &str, payload: &str) -> String {
        self.call_with_timeout(plugin_id, op, payload, DEFAULT_CALL_TIMEOUT)
    }

    /// 指定等待上界的同步调用（长任务专用）。
    pub fn call_with_timeout(
        &self,
        plugin_id: &str,
        op: &str,
        payload: &str,
        timeout: Duration,
    ) -> String {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<String>();
        let call = HostCall {
            plugin_id: plugin_id.to_string(),
            op: op.to_string(),
            payload: payload.to_string(),
            reply: reply_tx,
            timeout,
        };
        if self.tx.send(call).is_err() {
            return String::new();
        }
        reply_rx.recv_timeout(timeout).unwrap_or_default()
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
    let semaphore = Arc::new(tokio::sync::Semaphore::new(HOST_WORKER_CONCURRENCY));
    crate::rt().spawn(async move {
        while let Some(call) = rx.recv().await {
            // beta10：每次调用独立 spawn + 信号量。
            // 旧实现串行 consume，一个 `addTasksAndWait`（最长 30 分钟）会把其他插件的
            // getTask 全部堵死 —— 这是 HLS 插件落地前必须解决的架构问题。
            let Ok(permit) = semaphore.clone().acquire_owned().await else {
                break;
            };
            let host = host.clone();
            crate::rt().spawn(async move {
                let result = serve(&host, &call).await;
                // 通道回执：插件侧 `recv_timeout` 超时后会 drop receiver，此处 send 失败
                // 属预期竞态（插件已放弃等待），记 debug 便于排查"插件侧等不到回执"的问题。
                if call.reply.send(result).is_err() {
                    tracing::debug!(
                        "插件宿主调用回执未送达（插件侧已超时/退出）：plugin={} op={}",
                        call.plugin_id,
                        call.op
                    );
                }
                drop(permit);
            });
        }
    });
    HostBridge { tx }
}

/// 执行一次需要引擎的能力调用（权限校验已在插件侧完成）。
async fn serve(_host: &Arc<dyn HostContext>, call: &HostCall) -> String {
    // beta10：调用预算仅用于诊断（插件侧 `recv_timeout` 才是真正的等待上界）。
    tracing::debug!(
        "插件宿主调用：plugin={} op={} 预算={:?}",
        call.plugin_id,
        call.op,
        call.timeout
    );
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
            } else if call.op == "tasks.addAndWait" {
                json!({ "ok": false, "error": format!("引擎尚未就绪：{e}") }).to_string()
            } else {
                String::new()
            };
        }
    };
    let keys: Option<Vec<String>> = Some(crate::TASK_KEYS.iter().map(|k| k.to_string()).collect());

    match call.op.as_str() {
        // beta10：插件批量下载（HLS 分片）。长任务在独立 tokio 任务里跑，
        // 不占用 IPC 串行分发器，也不阻塞其他插件的短调用。
        "tasks.addAndWait" => {
            super::tasks::add_and_wait(
                &client,
                &call.plugin_id,
                &call.payload,
                super::tasks::HARD_MAX_TASKS,
            )
            .await
        }
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
    /// 用户**已授权**的权限快照（rc1 · 模块一）。
    ///
    /// 与 `permissions`（manifest 声明）分离：声明不等于授权，`exec:local` 必须两者都有。
    /// 用户在前端确认授权后由管理器原地刷新，无需重载插件。
    pub granted: Arc<std::sync::RwLock<HashSet<String>>>,
    /// 阶段三：宿主抽象（发事件 / 探活前端），取代原先的 `tauri::AppHandle`。
    pub host: Arc<dyn HostContext>,
    pub bridge: HostBridge,
    /// manifest 声明的资源上限（beta10：http / 批量任务共用）。
    pub limits: Limits,
    /// beta10：插件根目录（`ctx.exec` 默认 cwd 与自带 `bin/` 的解析根）。
    pub plugin_dir: PathBuf,
}

impl PluginHostContext {
    pub fn allows(&self, cap: &str) -> bool {
        self.permissions.contains(cap)
    }

    /// 处理一次来自插件的 `__harness_host(op, payload)` 调用。
    pub fn respond(&self, op: &str, payload: &str) -> String {
        match op {
            "log" => {
                // beta11 · B3：`ctx.log(level, msg)` 与单参 `ctx.log(msg)` 都要兼容
                // —— 后者由 JS 侧归为 info；直接调 __harness_host 时（msg 缺省）
                // 取 level 字段原文作为消息并归为 info。
                let (level, msg) = parse_log(payload);
                let entry = super::logs::push(&self.plugin_id, &level, &msg);
                println!("[plugin:{}][{level}] {msg}", self.plugin_id);
                if self.frontend_alive() {
                    self.host.emit(
                        "plugin://log",
                        json!({ "pluginId": self.plugin_id, "entry": entry }),
                    );
                }
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
                // beta10：虚拟任务状态事件走专用通道（前端按任务 id 合并渲染），
                // 其余自定义事件保持原样转发到 plugin://emit。
                if name == "plugin-task" {
                    if let Some(task) = super::plugin_tasks::apply_event(&self.plugin_id, &data) {
                        if self.frontend_alive() {
                            self.host.emit(
                                "plugin://task",
                                json!({ "pluginId": self.plugin_id, "task": task }),
                            );
                        }
                    }
                    return String::new();
                }
                if self.frontend_alive() {
                    self.host.emit(
                        "plugin://emit",
                        json!({ "pluginId": self.plugin_id, "name": name, "payload": data }),
                    );
                }
                String::new()
            }
            // beta12 · Step 2：插件自建虚拟任务卡（onHolo / onAction 没有宿主任命的 id）。
            // 只创建**本插件命名空间**内的任务；状态推进仍走 `emit("plugin-task")`。
            "task.create" => {
                let title = get_str(payload, "title");
                let url = get_str(payload, "url");
                let task = super::plugin_tasks::create(&self.plugin_id, &title, &url);
                if self.frontend_alive() {
                    self.host.emit(
                        "plugin://task",
                        json!({ "pluginId": self.plugin_id, "task": task }),
                    );
                }
                json!({ "ok": true, "id": task.id }).to_string()
            }
            // beta12 · Step 2：只读下载根目录（onHolo 拿不到 task.dir）。
            // beta12.3：改为**继承全局设置**的用户配置下载目录，而不是系统默认下载夹；
            // 否则插件在自定义目录下拼出的路径全都会被沙箱判越权。
            "paths.downloadDir" => {
                let path = crate::configured_download_dir();
                json!({ "ok": true, "path": path.to_string_lossy() }).to_string()
            }
            // beta12 · 模块 E：插件业务意图 → 宿主事件总线（无权限要求，仅事件）。
            "dispatchTask" => {
                let value: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
                let intent = value
                    .get("intent")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if intent.is_empty() {
                    return err_json("intent 不能为空");
                }
                if intent.len() > 128 {
                    return err_json("intent 过长（上限 128 字节）");
                }
                let data = value.get("payload").cloned().unwrap_or(Value::Null);
                // 日志只记 intent：payload 可能含页面上下文 / Cookie，绝不写入日志。
                let entry =
                    super::logs::push(&self.plugin_id, "info", &format!("dispatchTask: {intent}"));
                if self.frontend_alive() {
                    self.host.emit(
                        "plugin://dispatch",
                        json!({ "pluginId": self.plugin_id, "intent": intent, "payload": data }),
                    );
                    self.host.emit(
                        "plugin://log",
                        json!({ "pluginId": self.plugin_id, "entry": entry }),
                    );
                }
                json!({ "ok": true, "intent": intent }).to_string()
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
            // beta10：同步出站 HTTP（HLS 清单 / AES key）。
            "http.get" => {
                if !self.allows(CAP_NET_HTTP) {
                    self.audit_deny(op);
                    return deny_json();
                }
                super::net::http_get(payload, self.limits.max_http_bytes)
            }
            // beta10：批量派发下载并等待（长任务，独立 tokio 任务 + 信号量）。
            "tasks.addAndWait" => {
                if !self.allows(CAP_DOWNLOADS_WRITE) {
                    self.audit_deny(op);
                    return deny_json();
                }
                let declared = serde_json::from_str::<Value>(payload)
                    .ok()
                    .and_then(|v| v.get("timeoutMs").and_then(Value::as_u64))
                    .unwrap_or(600_000)
                    .clamp(30_000, 1_800_000);
                let timeout =
                    (Duration::from_millis(declared) + TASK_CALL_GRACE).min(TASK_CALL_HARD_CAP);
                let reply = self.bridge.call_with_timeout(
                    &self.plugin_id,
                    op,
                    payload,
                    timeout,
                );
                if reply.is_empty() {
                    // beta12.2：宿主桥超时 / 工作线程失联 —— 必须返回结构化错误，
                    // 否则 JS 的 hostResult 只会看到 "宿主无响应"，丢失上下文。
                    let msg = format!(
                        "addTasksAndWait 等待超时（{} 秒），已放弃等待；下载可能仍在后台进行",
                        TASK_CALL_HARD_CAP.as_secs()
                    );
                    tracing::warn!("[plugin:{}] {msg}", self.plugin_id);
                    super::logs::push(&self.plugin_id, "warn", &msg);
                    return json!({ "ok": false, "error": msg }).to_string();
                }
                reply
            }
            // beta10：执行 FFmpeg / FFprobe。
            "exec.run" => {
                // rc1 · 模块一：权限判定统一交给 exec.rs（含安全日志）。
                // `allow_local` 需要「manifest 声明」+「用户已授权」同时成立。
                let allow_local = self.allows(CAP_EXEC_LOCAL)
                    && self
                        .granted
                        .read()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .contains(CAP_EXEC_LOCAL);
                let grant = super::exec::ExecGrant {
                    allow_ffmpeg: self.allows(CAP_EXEC_FFMPEG),
                    allow_local,
                };
                // beta11 · FF-2：stderr 逐行转成 `plugin://exec` 事件（前端按插件过滤）。
                let sink: Option<std::sync::Arc<dyn Fn(&str) + Send + Sync>> =
                    if self.frontend_alive() {
                        let host = self.host.clone();
                        let plugin_id = self.plugin_id.clone();
                        let program = get_str(payload, "program");
                        Some(std::sync::Arc::new(move |line: &str| {
                            host.emit(
                                "plugin://exec",
                                json!({
                                    "pluginId": plugin_id,
                                    "program": program,
                                    "line": line,
                                }),
                            );
                        }))
                    } else {
                        None
                    };
                super::exec::run(payload, &self.plugin_dir, &self.plugin_id, grant, sink)
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
            "fs.writeText" => {
                if !self.allows(CAP_FS_WRITE_DOWNLOADS) {
                    self.audit_deny(op);
                    return deny_json();
                }
                let path = get_str(payload, "path");
                let content = get_str(payload, "content");
                match fs_write_text(&path, &content) {
                    Ok(path) => {
                        println!(
                            "[plugin:{}] fs.writeText：{}（{} 字节）",
                            self.plugin_id,
                            path.display(),
                            content.len()
                        );
                        ok_json()
                    }
                    Err(e) => {
                        eprintln!("[plugin:{}] fs.writeText 拒绝：{e}", self.plugin_id);
                        err_json(&e)
                    }
                }
            }
            "fs.readText" => {
                if !self.allows(CAP_FS_WRITE_DOWNLOADS) {
                    self.audit_deny(op);
                    return deny_json();
                }
                let path = get_str(payload, "path");
                match fs_read_text(&path) {
                    Ok((content, truncated)) => json!({
                        "ok": true,
                        "content": content,
                        "truncated": truncated,
                    })
                    .to_string(),
                    Err(e) => err_json(&e),
                }
            }
            "fs.exists" => {
                if !self.allows(CAP_FS_WRITE_DOWNLOADS) {
                    self.audit_deny(op);
                    return deny_json();
                }
                let path = get_str(payload, "path");
                match guard_path(&path) {
                    Ok(path) => json!({ "ok": true, "exists": path.exists() }).to_string(),
                    Err(e) => err_json(&e),
                }
            }
            "fs.remove" => {
                if !self.allows(CAP_FS_WRITE_DOWNLOADS) {
                    self.audit_deny(op);
                    return deny_json();
                }
                let path = get_str(payload, "path");
                match fs_remove(&path) {
                    Ok(path) => {
                        println!("[plugin:{}] fs.remove：{}", self.plugin_id, path.display());
                        ok_json()
                    }
                    Err(e) => err_json(&e),
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

/// 解析 `log` 调用的 `(level, msg)`。
///
/// 注意：`parse_pair` 对缺省 key 会返回空串，无法区分"没传 msg"与"msg 为空"，
/// 因此日志单独解析：**msg 缺省 / 空白时把 level 字段原文当作消息，级别归为 info**
/// （兼容 `ctx.log(msg)` 单参调用）。
fn parse_log(payload: &str) -> (String, String) {
    let value: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    let level = value
        .get("level")
        .and_then(Value::as_str)
        .unwrap_or("info")
        .trim()
        .to_string();
    let msg = value
        .get("msg")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if msg.trim().is_empty() {
        ("info".to_string(), level)
    } else {
        (level, msg)
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

/// 下载根目录（beta12.3：继承全局设置里的用户配置下载目录），真实路径解析后词法归一。
///
/// 注意：Windows 上 `canonicalize` 会返回 `\\?\` 主动式前缀，这里剥离后再比对，
/// 否则分量前缀匹配会失败。目录通常已由引擎启动流程创建；不存在时退回字面量。
pub(super) fn download_root() -> PathBuf {
    let root = crate::configured_download_dir();
    let root = root.as_path();
    std::fs::canonicalize(root)
        .map(|p| strip_verbatim(&p))
        .unwrap_or_else(|_| normalize_lexical(root))
}

/// 额外放行的下载根目录（用户在设置里指定的自定义下载目录）。
///
/// beta10 回归发现：`add_download` 允许任务目录落在任意安全路径，而插件沙箱此前
/// 只认系统「下载」文件夹，自定义目录下的 HLS 任务会直接「路径越权」失败。
/// 宿主在派发插件任务前把该目录登记进来；判定仍与下载根目录同一套归一 + 真实路径复核。
static EXTRA_ROOTS: OnceLock<RwLock<Vec<PathBuf>>> = OnceLock::new();
/// 只服务"当前正在跑的插件任务目录"，无需无限增长；超限时淘汰最旧的一条。
const EXTRA_ROOTS_MAX: usize = 8;

/// 登记一个允许插件写入的额外根目录（仅接受盘符下级及以上的绝对路径）。
pub fn allow_extra_root(dir: &str) {
    let raw = Path::new(dir.trim());
    if !raw.is_absolute() {
        return;
    }
    let normalized = std::fs::canonicalize(raw)
        .map(|p| strip_verbatim(&p))
        .unwrap_or_else(|_| normalize_lexical(raw));
    // 盘符根目录（`D:\`）不放行：等于把整个盘登记成可写。
    if normalized.parent().is_none() {
        return;
    }
    let lock = EXTRA_ROOTS.get_or_init(|| RwLock::new(Vec::new()));
    let mut roots = lock.write().unwrap_or_else(|poisoned| poisoned.into_inner());
    if roots.iter().any(|existing| existing == &normalized) {
        return;
    }
    if roots.len() >= EXTRA_ROOTS_MAX {
        roots.remove(0);
    }
    roots.push(normalized);
}

/// 插件可写的全部根目录（全局配置的下载目录 + 已登记的额外目录）。
pub(super) fn allowed_roots() -> Vec<PathBuf> {
    let mut roots = vec![download_root()];
    if let Some(lock) = EXTRA_ROOTS.get() {
        roots.extend(lock.read().unwrap_or_else(|p| p.into_inner()).iter().cloned());
    }
    roots
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
pub(super) fn normalize_lexical(path: &Path) -> PathBuf {
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
pub(super) fn is_within(root: &Path, candidate: &Path) -> bool {
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

/// 真实路径归一化：存在则 canonicalize 并剥离 `\\?\` 前缀，否则退回词法归一。
pub(super) fn normalize_existing(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .map(|p| strip_verbatim(&p))
        .unwrap_or_else(|_| normalize_lexical(path))
}

/// beta12.3：把候选路径归一到 `root` 内，越权 / 异常写法返回 `None`（**不触碰文件系统**）。
///
/// 供「删除插件任务并清理产物」使用：只允许严格位于下载根目录之内（根目录本身也不许删），
/// 并对已存在的最深祖先做真实路径复核，拦截软链接逃逸。
pub(crate) fn guarded_download_path(root: &Path, input: &str) -> Option<PathBuf> {
    let raw = input.trim();
    if raw.is_empty() || raw.contains('\0') || raw.starts_with(r"\\") || raw.starts_with("//") {
        return None;
    }
    let root = normalize_existing(root);
    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let normalized = normalize_lexical(&joined);
    if normalized == root || !is_within(&root, &normalized) {
        return None;
    }
    // 软链接逃逸：对已存在的最深祖先 canonicalize 后再比对一次。
    if let Some(existing) = nearest_existing(&normalized) {
        let real = std::fs::canonicalize(&existing)
            .map(|p| strip_verbatim(&p))
            .unwrap_or(existing);
        if !is_within(&root, &real) {
            return None;
        }
    }
    Some(normalized)
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
pub(super) fn guard_path(input: &str) -> Result<PathBuf, String> {
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

    if !allowed_roots().iter().any(|root| is_within(root, &normalized)) {
        return Err(format!("路径越权（不在下载目录内）：{input}"));
    }

    // 软链接逃逸：对已存在的最深祖先 canonicalize 后再比对一次。
    if let Some(existing) = nearest_existing(&normalized) {
        let real = std::fs::canonicalize(&existing)
            .map(|p| strip_verbatim(&p))
            .unwrap_or(existing);
        if !allowed_roots().iter().any(|root| is_within(root, &real)) {
            return Err(format!("路径越权（真实路径指向下载目录外）：{input}"));
        }
    }

    Ok(normalized)
}

// ---------------------------------------------------------------------------
// beta10：扩展 fs 能力（FFmpeg concat 清单 / AES key / 临时目录清理）
// ---------------------------------------------------------------------------

/// 单次文本读写的上限（8MB）：与 http 响应体上限一致。
const FS_TEXT_LIMIT: usize = 8 * 1024 * 1024;

fn fs_write_text(path: &str, content: &str) -> Result<PathBuf, String> {
    if content.len() > FS_TEXT_LIMIT {
        return Err(format!("写入内容超过上限（{} 字节）", FS_TEXT_LIMIT));
    }
    let target = guard_path(path)?;
    if target == download_root() {
        return Err("不能覆盖下载根目录".to_string());
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建父目录失败：{e}"))?;
    }
    std::fs::write(&target, content).map_err(|e| format!("写入文件失败：{e}"))?;
    Ok(target)
}

fn fs_read_text(path: &str) -> Result<(String, bool), String> {
    let target = guard_path(path)?;
    let metadata = std::fs::metadata(&target)
        .map_err(|e| format!("读取文件失败（{}）：{e}", target.display()))?;
    if !metadata.is_file() {
        return Err(format!("不是文件：{}", target.display()));
    }
    let limited = metadata.len() as usize > FS_TEXT_LIMIT;
    let bytes = std::fs::read(&target)
        .map_err(|e| format!("读取文件失败（{}）：{e}", target.display()))?;
    let slice = if limited {
        &bytes[..FS_TEXT_LIMIT]
    } else {
        &bytes[..]
    };
    Ok((String::from_utf8_lossy(slice).into_owned(), limited))
}

fn fs_remove(path: &str) -> Result<PathBuf, String> {
    let target = guard_path(path)?;
    if target == download_root() {
        return Err("不能删除下载根目录".to_string());
    }
    if !target.exists() {
        return Ok(target);
    }
    if target.is_dir() {
        std::fs::remove_dir_all(&target).map_err(|e| format!("删除目录失败：{e}"))?;
    } else {
        std::fs::remove_file(&target).map_err(|e| format!("删除文件失败：{e}"))?;
    }
    Ok(target)
}

/// 判断 `rename` 失败是否因源与目标不在同一卷（需要 copy 降级）。
pub(super) fn is_cross_device(err: &std::io::Error) -> bool {
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
    use super::{allow_extra_root, guard_path, guarded_download_path};

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
        denied(r"D:\other\outside.txt");
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
        // beta12.3：沙箱根目录改为全局配置的下载目录，测试也必须按同一来源准备。
        std::fs::create_dir_all(crate::configured_download_dir()).unwrap();

        allowed("file.txt");
        allowed("sub/file.txt");
        allowed(r"sub\deep\nested.bin");
        assert!(guard_path("sub/deep/file.bin").is_ok());
    }

    #[test]
    fn extra_root_unlocks_custom_download_dir() {
        // beta10 回归：用户把任务目录设到非默认位置时，宿主登记后插件才可写。
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("hls");
        std::fs::create_dir_all(&sub).unwrap();
        let input = sub.to_str().unwrap();

        assert!(guard_path(input).is_err(), "未登记前必须拒绝");
        allow_extra_root(input);
        assert!(guard_path(input).is_ok(), "登记后必须放行");
    }

    /// beta12.3：删除任务产物时的路径归一 —— 只放行严格位于下载根内的路径。
    #[test]
    fn guarded_download_path_accepts_only_paths_inside_root() {
        let root = tempfile::tempdir().unwrap();
        let inside_dir = root.path().join("hls").join("task-1");
        std::fs::create_dir_all(&inside_dir).unwrap();
        let inside_file = inside_dir.join("a.mp4");
        std::fs::write(&inside_file, b"x").unwrap();

        // 合法：绝对路径 / 相对路径都归一到根目录内
        assert_eq!(
            guarded_download_path(root.path(), inside_file.to_str().unwrap()),
            Some(inside_file.clone())
        );
        assert_eq!(
            guarded_download_path(root.path(), "hls/task-1/a.mp4"),
            Some(inside_file)
        );

        // 根目录本身不允许（避免误删整个下载夹）
        assert!(guarded_download_path(root.path(), root.path().to_str().unwrap()).is_none());

        // 越界：另一棵目录树 / 上溯穿越 / UNC 一律拒绝
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("evil.mp4");
        std::fs::write(&outside_file, b"x").unwrap();
        assert!(
            guarded_download_path(root.path(), outside_file.to_str().unwrap()).is_none(),
            "根目录外的路径必须拒绝"
        );
        assert!(guarded_download_path(root.path(), "../evil.mp4").is_none());
        assert!(guarded_download_path(root.path(), r"\\server\share\evil.mp4").is_none());
        assert!(guarded_download_path(root.path(), "").is_none());
    }
}
