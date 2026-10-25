pub mod metrics;
mod plugin;
// 增量 51：并发自动降级（403/429 等拒绝类错误按档位重试）。
mod degradation;
// 增量 27（P2-A）：全局错误模型。UI bin（`main.rs`）与核心服务 bin
// （`bin/aria2_service.rs`）都要经 `aria2_harness_lib::error::AppError` 引用，故必须 `pub`。
pub mod error;
// 阶段三：宿主抽象层（`HostContext` + 进程级槽位），插件系统与窗口框架解耦。
pub mod host;
// 增量 23：Job Object 守护逻辑由核心服务（aria2-service）复用，故对外可见。
pub mod process_guard;
// 增量 23：UI ⇄ 核心服务的命名管道客户端（仅 UI 侧使用）。
mod service_client;
// 增量 23：UI 与核心服务共用的 IPC 协议（同一 package 的第二个 bin 直接引用本模块）。
pub mod service_protocol;
// 增量 28（P2-B）：任务历史的 SQLite 持久化层（纯存储，不持有全局槽位）。
pub mod db;
// 增量 29（P1-G）：BT Tracker 列表的定时更新与注入（aria2 自拉 + 内部 gid 隔离）。
pub mod tracker;
// 增量 40：系统休眠 / 网络切换感知（仅 Windows）。
#[cfg(windows)]
pub mod power;
// 自研 aria2 JSON-RPC 客户端（取代第三方 `aria2-ws`，见模块头部的竞态说明）。
pub mod rpc_client;
// C-3 Phase 2：`EngineClient` 抽象（aria2c @6801 / 内嵌 engine-net）。
pub mod engine;
// C-3 Phase 2：协议路由 + 6800 对外 JSON-RPC 服务端（service 使用）。
pub mod router;
// 便携版目录解析：HOME 布局自适应（data / logs / plugins / downloads 都挂在其下）。
pub mod paths;
// Release 专用内置静态服务器：托管 exe 同级 `dist/`，让便携包双击即用。
#[cfg(not(debug_assertions))]
pub mod web_ui;
// 浏览器扩展接管的本地 HTTP API（仅绑定 127.0.0.1，后台任务）。
mod browser_api;
// 系统级原生通知（Windows Toast；其他平台为空实现）。
mod notify;
// 增量 46：轻量自动更新检查（启动 5s 后拉取远端 version.json）。
pub mod update_check;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex as StdMutex, OnceLock};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::runtime::Runtime;
use tokio::sync::Mutex;

use error::AppError;
use host::HostContext;
use rpc_client::{Client, Notification};
use service_protocol::ServiceRequest;

// 插件列表视图类型对外可见（`list_plugins` 的返回类型，供 IPC 路由序列化）。
pub use plugin::manager::PluginInfo;

/// aria2 RPC 监听端口。核心服务启动引擎时由 UI 通过命名管道下发该端口。
pub const RPC_PORT: &str = "6800";
/// aria2 的 WebSocket JSON-RPC 端点，与 HTTP RPC 共用同一个端口。
const RPC_WS_URL: &str = "ws://127.0.0.1:6800/jsonrpc";
/// 推送给前端的事件名（前端用 `listen("aria2://event")` 订阅）。
const EVENT_NAME: &str = "aria2://event";
/// 前置拦截（onBeforeAddDownload）等待插件的超时上界；超时则原样放行。
const INTERCEPT_TIMEOUT: Duration = Duration::from_secs(3);
/// 前端事件的最小发送间隔。每条事件最终都会落成一次同步 `evaluate_script`，
/// 事件密集时会长时间占用主线程消息泵；窗口内的同 GID 事件合并为一条即可
/// （前端收到任一事件都会重拉 `tellStatus`，中间态不携带额外信息）。
const EVENT_THROTTLE: Duration = Duration::from_millis(500);
/// 单次任务状态查询（`tellStatus` / `tellStopped`）的上界：引擎挂死时不能让落库任务悬着。
const RECORD_TIMEOUT: Duration = Duration::from_secs(5);
/// 与核心服务做启动握手的最大尝试次数。
/// 用户裁决红线：连不上**绝不崩溃、绝不死循环** —— 有限次退避重试后停在"未就绪"态并向前端报错。
const SERVICE_HANDSHAKE_ATTEMPTS: usize = 5;
/// 握手失败后的重试间隔（5 次即约 8s 的"等 Service 起来"窗口）。
const SERVICE_HANDSHAKE_BACKOFF: Duration = Duration::from_secs(2);
/// 后台服务的唯一名字：SCM 服务名与计划任务名共用（安装包 / UI 必须严格一致）。
const SERVICE_NAME: &str = "Aria2-Harness-Service";

/// 默认下载目录：Windows 系统「下载」文件夹（便携版同样如此，不再落到 exe 同级）。
/// 由 Service（拼 `--dir`）与 UI（`add_download` 的默认目录）共用。
pub fn default_download_dir() -> std::path::PathBuf {
    paths::downloads_dir()
}

/// 会话文件路径：aria2 在此保存/恢复任务列表（--save-session / --input-file）。
pub fn session_path() -> std::path::PathBuf {
    paths::session_path()
}

/// tellActive / tellWaiting / tellStopped 请求的字段白名单，作为前后端的数据契约。
/// 插件系统的 ctx.getTask / getTasks 复用同一份白名单。
pub(crate) const TASK_KEYS: [&str; 13] = [
    "gid",
    "status",
    "totalLength",
    "completedLength",
    "downloadSpeed",
    "connections",
    "errorCode",
    "errorMessage",
    "dir",
    "files",
    // C-3 扩展字段：分片百分比 / 活跃连接数 / ETA（beta4 前端依赖）。
    "pieceProgress",
    "activeWorkers",
    "eta",
];

/// 全局共享状态：WebSocket RPC 客户端 + 核心服务回传的 RPC 密钥 + 服务未就绪原因。
///
/// 增量 23 起 UI **不再持有引擎子进程句柄** —— 引擎由核心服务（aria2-service）负责
/// 拉起 / 监控 / 回收，UI 只通过命名管道下指令、用回传的密钥连 WS RPC。
pub(crate) struct Aria2State {
    /// 已建立的 WS 客户端；未连接时为 `None`。`Client` 内部是 `Arc`，clone 很廉价。
    client: Mutex<Option<Client>>,
    /// RPC 密钥：由**核心服务生成**并经命名管道回传（杜绝硬编码，也不再由 UI 自造）。
    /// 之所以要内部可变：握手发生在 `setup` 之后的异步任务里。
    secret: StdMutex<String>,
    /// 核心服务未就绪时的可读原因（握手失败 / 服务报错）。命令入口会原样回传前端 ——
    /// 这是"前端零改动前提下向前端报错"的主通道。
    service_error: StdMutex<Option<String>>,
}

impl Aria2State {
    fn new() -> Self {
        Self {
            client: Mutex::new(None),
            secret: StdMutex::new(String::new()),
            service_error: StdMutex::new(None),
        }
    }
}

// ---------------------------------------------------------------------------
// 进程级全局槽位（`OnceLock`）
// ---------------------------------------------------------------------------
//
// 阶段三移除 Tauri 依赖后，命令与插件系统不再通过框架句柄（`State<T>`）取用共享对象，
// 一律改走这三个进程级单例，于是插件侧不再需要任何框架句柄。

/// 共享 tokio 运行时（唯一的异步执行底座：命令、WS 循环、插件派发、热重载阻塞池共用）。
static RT: OnceLock<Runtime> = OnceLock::new();
/// 引擎连接状态 + 服务报错。
static STATE: OnceLock<Aria2State> = OnceLock::new();
/// 前置拦截会话状态机。
static INTERCEPTOR: OnceLock<plugin::interceptor::Interceptor> = OnceLock::new();
/// 插件管理器。
static PLUGINS: OnceLock<plugin::PluginManager> = OnceLock::new();
/// 任务历史数据库（增量 28）。`None` = 打开/迁移失败，历史功能降级：
/// **存历史失败绝不能阻断下载主链路**，只有历史命令返回 `DB` 码。
static DB: OnceLock<Option<db::Db>> = OnceLock::new();
/// 历史对账是否已跑过（增量 28）：只在**首次**连通引擎后对账一次，
/// 避免引擎抖动导致每次重连都重复全量写入。
static RECONCILED_ONCE: AtomicBool = AtomicBool::new(false);
/// 关闭窗口行为：`true` = 隐藏到系统托盘（默认），`false` = 直接退出。
///
/// 由前端设置页通过 IPC 下发；外壳在处理 `CloseRequested` / `window-close`
/// 时读取此开关，决定"隐藏"还是"走完整退出流程"。
static CLOSE_TO_TRAY: AtomicBool = AtomicBool::new(true);

/// 窗口是否处于**后台隐藏**态（隐藏到托盘时为 `true`）。
///
/// 由外壳在 `set_visible(false/true)` 的同一处调用 [`set_window_hidden`] 维护。
/// 用途：后台时任务终态改弹系统 Toast，前台时交给 UI 内通知中心（避免双重提醒）。
static WINDOW_HIDDEN: AtomicBool = AtomicBool::new(false);

/// 更新窗口后台态（外壳主线程调用）。
pub fn set_window_hidden(hidden: bool) {
    WINDOW_HIDDEN.store(hidden, Ordering::Relaxed);
}

/// 当前是否后台隐藏（通知循环读取）。
pub fn window_hidden() -> bool {
    WINDOW_HIDDEN.load(Ordering::Relaxed)
}

/// 共享 tokio 运行时（懒加载；首次访问即创建）。
pub fn rt() -> &'static Runtime {
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("创建 tokio 运行时失败")
    })
}

/// 引擎连接状态单例。
pub(crate) fn state() -> &'static Aria2State {
    STATE
        .get()
        .expect("Aria2State 尚未初始化（应先调用 bootstrap）")
}

/// 前置拦截会话单例。
pub(crate) fn interceptor() -> &'static plugin::interceptor::Interceptor {
    INTERCEPTOR
        .get()
        .expect("Interceptor 尚未初始化（应先调用 bootstrap）")
}

/// 插件管理器单例。
pub(crate) fn plugins() -> &'static plugin::PluginManager {
    PLUGINS
        .get()
        .expect("PluginManager 尚未初始化（应先调用 bootstrap）")
}

/// 任务历史数据库句柄；不可用（启动时打开/迁移失败）时返回 `None`，调用方自行降级。
pub(crate) fn db() -> Option<&'static db::Db> {
    DB.get().and_then(|slot| slot.as_ref())
}

/// 数据库未就绪时的统一错误（前端拿到 `DB` 码）。
fn db_unavailable() -> AppError {
    AppError::Db {
        context: "任务历史数据库不可用（启动时打开/迁移失败，详见 logs/app.log）".to_string(),
        source: None,
    }
}

/// 向前端派发事件：宿主未注入或前端已销毁时静默丢弃（退出期不向已销毁窗口 PostMessage）。
fn emit(event: &str, payload: Value) {
    if let Some(host) = host::host() {
        if host.frontend_alive() {
            host.emit(event, payload);
        }
    }
}

/// 引导核心服务：命名管道握手 → `start_engine` → 取回 rpc-secret → 启动 WS 循环。
///
/// 降级策略（用户裁决红线）：
/// - **连不上服务**：只做有限次（`SERVICE_HANDSHAKE_ATTEMPTS`）退避重试，用尽即停止 ——
///   绝不崩溃、绝不忙等死循环；
/// - **服务可达但启动失败**（`ok=false`）：立即停止（服务都起不来，重试无意义）；
/// - 全程只记录日志 + 发 `disconnected` 事件。
///
/// 报错通道（前端零改动）：① `aria2://event` 的 `disconnected`（前端徽标转为未连接）；
/// ② 各引擎相关命令经 [`client_or_err`] 原样抛出可读原因，前端 toast 出"请启动 Service"。
async fn bootstrap_service() {
    let port: u16 = RPC_PORT.parse().unwrap_or(6800);
    let mut last_err = String::new();
    #[cfg(not(debug_assertions))]
    let mut spawned = false;

    for attempt in 1..=SERVICE_HANDSHAKE_ATTEMPTS {
        match service_client::request(&ServiceRequest::StartEngine { port }).await {
            Ok(resp) if resp.ok => {
                let secret = resp.rpc_secret.clone().unwrap_or_default();
                if secret.is_empty() {
                    last_err = "核心服务未回传 rpc-secret".to_string();
                } else {
                    let state = state();
                    if let Ok(mut guard) = state.service_error.lock() {
                        *guard = None;
                    }
                    if let Ok(mut guard) = state.secret.lock() {
                        *guard = secret.clone();
                    }
                    println!(
                        "[service] 引擎已由核心服务拉起（pid={:?}），开始连接 RPC",
                        resp.engine_pid
                    );
                    run_ws_loop(secret).await;
                    return;
                }
            }
            Ok(resp) => {
                last_err = resp
                    .error
                    .unwrap_or_else(|| "核心服务返回未知错误".to_string());
                break;
            }
            Err(err) => {
                // 增量 27：`service_client` 现在抛 `AppError`；此处只要可读文案（指引在下面统一拼）。
                last_err = err.to_string();
                // 便携包：首次连不上说明核心服务没起，直接提权拉起同级 aria2-service.exe。
                // 只在 Release 生效 —— 开发期由 start_dev.ps1 负责启动，避免每次调试都弹 UAC。
                #[cfg(not(debug_assertions))]
                if attempt == 1 && !spawned {
                    spawned = true;
                    // 先做"同级文件是否存在"的硬检查：静默安装型缺陷（用户只复制了
                    // aria2-harness.exe）下，schtasks 会返回成功但任务里的程序根本不存在，
                    // 最终只能报"管道连不上"，用户看不到真正原因。
                    let service_exe = std::env::current_exe()
                        .ok()
                        .and_then(|path| path.parent().map(|dir| dir.join("aria2-service.exe")));
                    if !service_exe.as_ref().is_some_and(|path| path.is_file()) {
                        let shown = service_exe
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "aria2-service.exe".to_string());
                        last_err = format!(
                            "未找到核心服务程序 {shown}（应与 aria2-harness.exe 位于同一目录；\
                             请重新运行安装包，或使用完整便携包）"
                        );
                        tracing::error!("[service] {last_err}");
                        break;
                    }
                    // 三保险：SCM 系统服务（最稳）→ 计划任务（免 UAC，兼容老安装）
                    // → ShellExecuteW runas（兜底，会弹一次 UAC）。
                    if start_service_from_scm() {
                        tracing::info!("[service] 已通过系统服务（SCM）拉起核心服务（免 UAC）");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    } else if start_service_from_task() {
                        tracing::info!("[service] 已通过计划任务拉起核心服务（免 UAC）");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    } else {
                        // 便携版无计划任务：退回 ShellExecuteW runas（会弹一次 UAC）。
                        match spawn_service_elevated() {
                            Ok(()) => {
                                tracing::info!("[service] 已提权启动核心服务，等待其就绪");
                                tokio::time::sleep(Duration::from_secs(2)).await;
                            }
                            Err(e) => {
                                // 必须覆盖 last_err：否则最终提示只剩"管道连不上"，
                                // UAC 被取消 / 文件缺失这类真正原因会被吞掉。
                                tracing::error!("[service] 提权启动核心服务失败：{e}");
                                last_err = e.to_string();
                            }
                        }
                    }
                }
                if attempt < SERVICE_HANDSHAKE_ATTEMPTS {
                    tokio::time::sleep(SERVICE_HANDSHAKE_BACKOFF).await;
                }
            }
        }
    }

    let message = format!(
        "下载服务未就绪：{last_err}；请重新运行安装包（或使用完整便携包），\
         以管理员身份运行 aria2-service.exe 后重试。"
    );
    if let Ok(mut guard) = state().service_error.lock() {
        *guard = Some(message.clone());
    }
    // Release 下没有控制台，`eprintln!` 等于把根因扔进黑洞 —— 必须写进日志文件。
    tracing::error!("[service] {message}");
    // 窗口在托盘 / 后台时用户看不到徽标，必须弹系统通知，给出明确可见的错误。
    notify::notify(
        "下载引擎未启动",
        &format!("{last_err}（详见 data\\logs\\app.log 与 service.log）"),
    );
    emit(EVENT_NAME, json!({ "type": "disconnected" }));
}

/// 通过安装包注册的计划任务拉起核心服务（免 UAC）。
///
/// 任务由 Inno 安装包以管理员身份创建，运行级别为 `HIGHEST`；
/// 普通权限的 UI 调用 `schtasks /run` 时由任务计划服务代为提权，
/// Windows 不再弹 UAC。任务不存在时返回 `false`（便携版走 UAC 兜底）。
#[cfg(all(windows, not(debug_assertions)))]
fn start_service_from_scm() -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // 用 System32 下的 sc.exe 而不是 PATH 查找：避免被同目录/用户 PATH 里的同名程序劫持。
    let sc = std::env::var_os("SystemRoot")
        .map(|root| std::path::PathBuf::from(root).join("System32").join("sc.exe"))
        .unwrap_or_else(|| std::path::PathBuf::from("sc.exe"));
    let output = match std::process::Command::new(&sc)
        .args(["start", SERVICE_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(output) => output,
        Err(err) => {
            tracing::warn!("[service] 调用 sc.exe 失败：{err}");
            return false;
        }
    };

    // sc 用进程退出码回传 Win32 错误码：0 = 已启动，1056 = 已在运行（同样可用）。
    // 1060 = 服务未注册 → 交给计划任务 / UAC 兜底。
    match output.status.code() {
        Some(0) => true,
        Some(1056) => {
            tracing::info!("[service] 系统服务已处于运行状态");
            true
        }
        Some(code) => {
            tracing::warn!(code, "[service] 系统服务启动失败（未注册或被禁用），转入后备方案");
            false
        }
        None => false,
    }
}

#[cfg(all(windows, not(debug_assertions)))]
fn start_service_from_task() -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    std::process::Command::new("schtasks")
        .args(["/run", "/tn", SERVICE_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// 便携包：以管理员权限拉起与宿主同级的 `aria2-service.exe`（UAC 由系统弹出）。
///
/// 仅在 Release 编译。`ShellExecuteW` 返回值 > 32 视为成功（Win32 HINSTANCE 语义），
/// 用户取消 UAC 会得到 `SE_ERR_ACCESSDENIED (5)`，此时保留上层"服务未就绪"提示。
#[cfg(all(windows, not(debug_assertions)))]
fn spawn_service_elevated() -> Result<(), AppError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.join("aria2-service.exe")))
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            AppError::Service(
                "未找到 aria2-service.exe（应与 aria2-harness.exe 位于同一目录）".to_string(),
            )
        })?;

    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    let file: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = result.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(AppError::Service(format!(
            "提权启动核心服务失败（ShellExecuteW={code}，用户取消 UAC 时为 5）"
        )))
    }
}

/// 把 crate 的通知枚举转成前端可直接消费的 JSON。
fn notification_payload(msg: &Notification) -> Value {
    match msg {
        Notification::Aria2 { gid, event } => {
            let name = match event {
                rpc_client::Event::Start => "start",
                rpc_client::Event::Pause => "pause",
                rpc_client::Event::Stop => "stop",
                rpc_client::Event::Complete => "complete",
                rpc_client::Event::Error => "error",
                rpc_client::Event::BtComplete => "bt_complete",
            };
            json!({ "type": "aria2", "event": name, "gid": gid })
        }
        Notification::WebSocketConnected => json!({ "type": "ws_connected" }),
        Notification::WebsocketClosed => json!({ "type": "ws_closed" }),
    }
}

// ---------------------------------------------------------------------------
// 增量 28（P2-B）：任务历史落库
// ---------------------------------------------------------------------------

/// 事件驱动的终态落库：状态由通知直接给出（complete / error），无需额外 RPC 即可降级。
fn record_terminal(client: Client, gid: String, status: &'static str) {
    // 增量 29（P1-G）：内部任务（tracker 自拉）不进历史库。
    if tracker::is_internal(&gid) {
        return;
    }
    spawn_record(client, gid, Some(status), None, None);
}

/// 后台态终态通知：异步查文件名 → 弹系统 Toast。
///
/// **fire-and-forget**：与落库同理，查询失败/超时只降级为 GID 文案，绝不阻塞通知循环。
/// 文件名优先取 `files[0].path` 的末段（BT 任务此时 path 已确定为具体文件）。
fn notify_terminal(client: Client, gid: String, failed: bool) {
    rt().spawn(async move {
        let keys = Some(vec!["files".to_string()]);
        metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
        let name = match tokio::time::timeout(RECORD_TIMEOUT, client.tell_status(&gid, keys)).await
        {
            Ok(Ok(detail)) => detail
                .get("files")
                .and_then(Value::as_array)
                .and_then(|files| files.first())
                .and_then(|file| file.get("path"))
                .and_then(Value::as_str)
                .and_then(last_path_segment)
                .unwrap_or_else(|| gid.clone()),
            Ok(Err(err)) => {
                metrics::METRICS.rpc_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(gid = %gid, "通知取文件名失败，降级为 GID：{err}");
                gid.clone()
            }
            Err(_) => {
                tracing::warn!(gid = %gid, "通知取文件名超时，降级为 GID");
                gid.clone()
            }
        };

        notify::notify(if failed { "下载失败" } else { "下载完成" }, &name);
    });
}

/// 取路径末段（兼容 `\` 与 `/`）；空路径返回 `None`（由调用方回退 GID）。
fn last_path_segment(path: &str) -> Option<String> {
    path.rsplit(['\\', '/'])
        .find(|segment| !segment.is_empty())
        .map(str::to_string)
}

/// 判断一条任务状态 Map 是否为内部任务（tracker 自拉）。增量 29（P1-G）。
fn is_internal_task(task: &Map<String, Value>) -> bool {
    task.get("gid")
        .and_then(Value::as_str)
        .map(tracker::is_internal)
        .unwrap_or(false)
}

/// `add_download` 路径落库：此时本地已知 url / dir，用于 `tellStatus` 兜底。
///
/// `fallback_status = "waiting"`：**保证 add 时必有一行**（用户裁决 A「add 时插入」）。
/// 即便 `tellStatus` 失败/超时，也会用本地 url/dir 建一条 `waiting` 记录；后续终态写入纠正。
fn record_added(client: Client, gid: String, url: String, dir: String) {
    spawn_record(client, gid, Some("waiting"), Some(url), Some(dir));
}

/// 后台落库：查 `tellStatus` → 幂等 upsert。
///
/// **fire-and-forget**：调用方（命令路径 / 通知循环）绝不 `.await` —— 否则一次
/// 引擎卡顿会把 IPC 响应或事件派发一起拖住。
fn spawn_record(
    client: Client,
    gid: String,
    fallback_status: Option<&'static str>,
    fallback_url: Option<String>,
    fallback_dir: Option<String>,
) {
    rt().spawn(async move {
        let Some(db) = db() else { return };
        let keys: Option<Vec<String>> = Some(TASK_KEYS.iter().map(|k| k.to_string()).collect());

        // 降级记录：`tellStatus` 失败/超时时才会被采用，只要有 `fallback_status` 即可建。
        // - add 路径：status="waiting" + 本地 url/dir（保证「add 时插入」）；
        // - 事件路径：status=终态 + url/dir 留空 —— SQL 侧空串保护（`upsert_sync` 的 CASE）
        //   不会冲掉 add 时已写入的值。
        let degraded = fallback_status.map(|status| {
            db::TaskRecord::minimal(
                &gid,
                status,
                fallback_url.as_deref().unwrap_or(""),
                fallback_dir.as_deref().unwrap_or(""),
            )
        });

        metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
        let record = match tokio::time::timeout(
            RECORD_TIMEOUT,
            client.tell_status(&gid, keys),
        )
        .await
        {
            Ok(Ok(detail)) => db::TaskRecord::from_status_map(
                &detail,
                fallback_url.as_deref(),
                fallback_dir.as_deref(),
            ),
            Ok(Err(err)) => {
                metrics::METRICS.rpc_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(gid = %gid, "查询任务状态失败，改用降级记录：{err}");
                degraded
            }
            Err(_) => {
                tracing::warn!(gid = %gid, "查询任务状态超时（{}s），改用降级记录", RECORD_TIMEOUT.as_secs());
                degraded
            }
        };

        // 既查不到状态、又无降级依据（事件路径拿不到 gid 之外的任何东西时不会走到这里，
        // 因为 `fallback_status` 必非空）—— 兜底跳过而不是写入无主空行。
        let Some(record) = record else {
            tracing::warn!(gid = %gid, "任务落库跳过（状态查询失败且无本地兜底信息）");
            return;
        };

        if let Err(err) = db.upsert_task(record).await {
            // best-effort：失败只落日志（符合 §6.4「禁止裸 let _ 吞错」），
            // 绝不把存储故障冒泡成下载链路故障。
            tracing::warn!(gid = %gid, "任务落库失败：{err}");
        }
    });
}

/// 首次连通引擎后的历史对账（增量 28，用户裁决 A）：
/// 把 aria2 中**已停止**的任务批量补写进库 —— 覆盖"UI 未运行期间后台引擎自行下完"
/// 的记录空洞。幂等：重复执行与既有行等价（同 gid 覆盖终态字段）。
fn reconcile_history(client: Client) {
    rt().spawn(async move {
        let Some(db) = db() else { return };
        let keys: Option<Vec<String>> = Some(TASK_KEYS.iter().map(|k| k.to_string()).collect());

        metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
        // 与 `get_tasks` 同口径：一次拉上限 1000 条已停止任务。
        let tasks = match tokio::time::timeout(
            RECORD_TIMEOUT,
            client.tell_stopped(0, 1000, keys),
        )
        .await
        {
            Ok(Ok(tasks)) => tasks,
            Ok(Err(err)) => {
                metrics::METRICS.rpc_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!("历史对账查询失败：{err}");
                return;
            }
            Err(_) => {
                tracing::warn!("历史对账查询超时（{}s）", RECORD_TIMEOUT.as_secs());
                return;
            }
        };

        // 增量 29（P1-G）：内部任务（tracker 自拉）不进历史库。
        let records: Vec<db::TaskRecord> = tasks
            .iter()
            .filter(|task| !is_internal_task(task))
            .filter_map(|task| db::TaskRecord::from_status_map(task, None, None))
            .collect();
        let count = records.len();
        match db.upsert_many(records).await {
            Ok(_) => tracing::info!("历史对账完成：aria2 已停止任务 {count} 条并入历史库"),
            Err(err) => tracing::warn!("历史对账写入失败：{err}"),
        }
    });
}

/// 后台常驻任务：把引擎事件转发到前端 / 插件总线。
///
/// 增量 36：连接与重连**下沉到** [`rpc_client::Client`] 的守护任务（指数退避 1s→30s），
/// 本函数只负责消费通知：
/// - 连上 → 向前端发 `connected`、首次连通做历史对账；
/// - 断开 → 向前端发 `disconnected`，客户端会自动重连，无需在此重建句柄。
///
/// 这样插件总线只订阅**一次**：重连不会再叠加新的 dispatcher 任务（旧实现每次重连
/// 都会 `spawn` 一个，导致同一事件被派发多次）。
async fn run_ws_loop(secret: String) {
    use std::collections::HashMap;
    use tokio::time::Instant;

    /// 到点把攒下的事件推给前端（仅由 [`run_ws_loop`] 调用）。
    fn flush(pending: &mut Vec<Notification>, last_emit: &mut Option<Instant>) {
        if pending.is_empty() {
            return;
        }
        for msg in coalesce(std::mem::take(pending)) {
            emit(EVENT_NAME, notification_payload(&msg));
        }
        *last_emit = Some(Instant::now());
    }

    /// 同一 GID 的事件只保留最后一条（位置沿用首次出现处）。连接状态事件全部保留。
    fn coalesce(events: Vec<Notification>) -> Vec<Notification> {
        let mut slots: HashMap<String, usize> = HashMap::new();
        let mut out: Vec<Notification> = Vec::with_capacity(events.len());
        for msg in events {
            if let Notification::Aria2 { gid, .. } = &msg {
                if let Some(&index) = slots.get(gid) {
                    out[index] = msg;
                    continue;
                }
                slots.insert(gid.clone(), out.len());
            }
            out.push(msg);
        }
        out
    }

    let client = Client::start(RPC_WS_URL, secret);
    *state().client.lock().await = Some(client.clone());

    // 插件事件总线：与前端事件同源、零额外轮询。
    // 传**共享订阅者单元**而非启动快照，热重载后新实例即时可见。
    let subscribers = plugins().subs_cell();
    rt().spawn(plugin::dispatcher::run(
        client.subscribe_notifications(),
        subscribers,
    ));

    let mut notif = client.subscribe_notifications();

    // 前端事件节流（见 `EVENT_THROTTLE`）：`pending` 攒窗口内的事件，`last_emit`
    // 记录上次真正 emit 的时刻。落库 / 系统通知 / 自动降级等副作用仍在**收包时立即**
    // 执行，只有"推给前端"这一步被限频。
    let mut pending: Vec<Notification> = Vec::new();
    let mut last_emit: Option<Instant> = None;

    loop {
        // 有待发事件时最多等到「节流窗口结束」；无待发事件时永久阻塞等通知 ——
        // 空闲期既没有轮询也没有定时器，IPC 通道彻底静默。
        let deadline = if pending.is_empty() {
            None
        } else {
            Some(last_emit.map_or_else(Instant::now, |at| at + EVENT_THROTTLE))
        };
        let received = match deadline {
            Some(deadline) => tokio::select! {
                recv = notif.recv() => Some(recv),
                _ = tokio::time::sleep_until(deadline) => None,
            },
            None => Some(notif.recv().await),
        };

        let mut closed = false;
        if let Some(received) = received {
            match received {
                Ok(Notification::WebSocketConnected) => {
                    pending.push(Notification::WebSocketConnected);
                    // 引擎若被服务重新拉起，`rpc-secret` 会轮换：每次连上后都重新对齐，
                    // 否则后续调用会持续 `Unauthorized`（E2E 强杀引擎实测）。fire-and-forget，
                    // 绝不阻塞通知循环。
                    //
                    // 对齐后立即把 config.json 里的全局设置重放给引擎：覆盖安装 / 引擎重启
                    // 会丢失 `changeGlobalOption` 的运行时值（下载目录、限速、代理等）。
                    let restore_client = client.clone();
                    rt().spawn(async move {
                        resync_secret(restore_client.clone()).await;
                        apply_saved_globals(&restore_client).await;
                    });
                    // 增量 28：首次连通即与 aria2 对账，补齐"UI 未运行期间自行下完"的记录。
                    // 用 `swap` 保证只跑一次：引擎抖动会反复重连，不能每次都全量写库。
                    if !RECONCILED_ONCE.swap(true, Ordering::Relaxed) {
                        reconcile_history(client.clone());
                    }
                }
                Ok(Notification::WebsocketClosed) => {
                    pending.push(Notification::WebsocketClosed);
                }
                Ok(Notification::Aria2 { gid, event }) => {
                    // 增量 29（P1-G）：内部任务（tracker 自拉）对 UI 完全隐身 ——
                    // 既不派发事件、也不落库（对账/列表查询同理）。
                    if !tracker::is_internal(&gid) {
                        // 增量 28：终态任务落库。fire-and-forget —— 绝不在通知循环里
                        // `.await`，否则一次查询就会把后续所有事件派发一起拖住。
                        let hidden = window_hidden();
                        match event {
                            rpc_client::Event::Complete => {
                                record_terminal(client.clone(), gid.clone(), "complete");
                                if hidden {
                                    notify_terminal(client.clone(), gid.clone(), false);
                                }
                            }
                            rpc_client::Event::Error => {
                                // 增量 51：拒绝类错误优先走自动降级（同 GID 降档重试）。
                                // 增量 52：「文件被占用」（errorCode=17）与并发无关，单线程
                                // 任务同样要重试，因此入口口径放宽到「已登记」。
                                // `on_error` 内部若判定不可降级，会自行补终态落库与通知。
                                if degradation::is_registered(&gid) {
                                    degradation::on_error(client.clone(), gid.clone(), hidden);
                                } else {
                                    record_terminal(client.clone(), gid.clone(), "error");
                                    if hidden {
                                        notify_terminal(client.clone(), gid.clone(), true);
                                    }
                                }
                            }
                            _ => {}
                        }
                        pending.push(Notification::Aria2 { gid, event });
                    }
                }
                // 通道彻底关闭（进程退出期）才结束循环。
                Err(tokio::sync::broadcast::error::RecvError::Closed) => closed = true,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    // 红线：落后只是"跳过若干条旧消息"，通道本身仍可用，**绝不 break**。
                    eprintln!("[aria2-rpc] 通知通道积压，丢弃 {n} 条");
                }
            }
        }

        if !pending.is_empty()
            && last_emit.map_or(true, |at| at.elapsed() >= EVENT_THROTTLE)
        {
            flush(&mut pending, &mut last_emit);
        }

        if closed {
            // 退出前把剩余事件发完，避免前端漏掉最后的终态。
            flush(&mut pending, &mut last_emit);
            break;
        }
    }
}

/// 与核心服务重新对齐 `rpc-secret`（每次连上后调用，只读取状态、不拉起引擎）。
///
/// 引擎崩溃后由服务重新拉起时密钥会重新生成；旧密钥会让所有调用返回
/// `Unauthorized`，表现为"连接正常但什么都做不了"。拿不到新密钥时沿用旧值。
async fn resync_secret(client: Client) {
    match service_client::request(&ServiceRequest::GetStatus).await {
        Ok(resp) if resp.ok => {
            if let Some(secret) = resp.rpc_secret.filter(|s| !s.is_empty()) {
                client.update_secret(secret);
                tracing::info!("已与核心服务对齐 rpc-secret（重连鉴权）");
            }
        }
        Ok(resp) => tracing::warn!("对齐 rpc-secret 失败：{:?}", resp.error),
        Err(err) => tracing::warn!("对齐 rpc-secret 失败：{err}"),
    }
}

/// 取出已连接的客户端；未就绪则返回可读错误。
/// 插件系统的宿主桥（ctx.getTask / getTasks）复用同一入口。
///
/// 增量 23：核心服务未就绪时**优先**抛出服务层面的原因（含"请启动 Service"指引），
/// 这样前端一行不改，也能在 toast 里看到明确的操作提示。
pub(crate) async fn client_or_err(state: &Aria2State) -> Result<Client, AppError> {
    if let Ok(guard) = state.service_error.lock() {
        if let Some(message) = guard.as_ref() {
            // 服务层面的原因优先：前端据此给 `SERVICE_UNAVAILABLE`，提示"请启动 Service"。
            return Err(AppError::Service(message.clone()));
        }
    }
    match state.client.lock().await.clone() {
        // 句柄常驻（守护任务负责重连），因此"是否可用"以连接态为准：
        // 断线期间仍按 `ARIA2_DISCONNECTED` 报错，语义与旧实现（句柄置空）保持一致。
        Some(client) if client.is_connected() => Ok(client),
        _ => Err(AppError::Aria2Disconnected(
            "aria2 RPC 尚未连接，请稍候重试".to_string(),
        )),
    }
}

/// 广播 `onBeforeAddDownload` 并同步等待插件改写结果（微内核前置拦截）。
///
/// 时序（关键：`begin` 必须先于 `dispatch`，否则插件回报的"完成"会落到空会话上）：
/// 1. 收集启用且订阅了该 hook 的插件（按 priority 升序）；
/// 2. 取得会话串行锁，建立拦截会话（无订阅者时直接零等待放行）；
/// 3. 向插件总线**非阻塞**投递事件（payload 含 `url`）；
/// 4. 等待全部插件回报完成或 `INTERCEPT_TIMEOUT` 超时；
/// 5. 取出经优先级仲裁后的胜出 URL；无人改写则返回原 URL。
#[tracing::instrument(skip_all, fields(url = %url))]
async fn intercept_url(url: String) -> String {
    let subscribers = plugins().subscribers();
    // 仅纳入**启用且订阅了拦截 hook** 的插件：与 `dispatch` 的跳过规则保持一致，
    // 否则被禁用的拦截器会被计入待回报集合、却永不回报，导致每次 add 都空等 3s 超时。
    // `subscribers()` 已按 priority 升序，故此列表即为优先级序。
    let interceptors: Vec<(String, i64)> = subscribers
        .iter()
        .filter(|s| {
            s.enabled.load(Ordering::Relaxed)
                && s.hooks.contains(plugin::dispatcher::HOOK_BEFORE_ADD)
        })
        .map(|s| (s.plugin_id.clone(), s.priority))
        .collect();
    if interceptors.is_empty() {
        return url;
    }

    let interceptor = interceptor();
    // 串行化拦截会话：避免并发 add_download 共用同一会话互相覆盖。
    // 串行锁必须**有界等待**：IPC 路由是单任务串行消费，一旦这里无限期挂起，后续所有
    // 命令（get_task / list_plugins …）都会排在后面一起变哑，前端只能看到超时。
    // 上界取「单次拦截预算 3s + 2s 余量」；超时与拦截超时同样处理：记录后原样放行。
    const SERIAL_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
    let _serial = match tokio::time::timeout(SERIAL_LOCK_TIMEOUT, interceptor.lock()).await {
        Ok(guard) => guard,
        Err(_) => {
            eprintln!(
                "[plugin] 等待拦截串行锁超时（{}s），原样放行：{url}",
                SERIAL_LOCK_TIMEOUT.as_secs()
            );
            return url;
        }
    };

    let Some(rx) = interceptor.begin(interceptors) else {
        return url;
    };

    plugin::dispatcher::dispatch(
        &subscribers,
        plugin::dispatcher::HOOK_BEFORE_ADD,
        json!({ "hook": plugin::dispatcher::HOOK_BEFORE_ADD, "url": url }),
    );

    if tokio::time::timeout(INTERCEPT_TIMEOUT, rx).await.is_err() {
        metrics::METRICS.intercept_timeout.fetch_add(1, Ordering::Relaxed);
        eprintln!(
            "[plugin] onBeforeAddDownload 等待超时（{}s），原样放行",
            INTERCEPT_TIMEOUT.as_secs()
        );
    }

    match interceptor.finish() {
        Some(new_url) if new_url.trim() != url => {
            println!("[plugin] 前置拦截改写 URL：{url} -> {new_url}");
            new_url
        }
        _ => url,
    }
}

// ---------------------------------------------------------------------------
// 业务命令（原 `#[tauri::command]`，改为普通 pub 异步函数供 IPC 路由分发器调用）
// ---------------------------------------------------------------------------

/// 判断下载目录是否为网络共享盘（UNC 路径，以 `\\` 或 `//` 开头）。
///
/// 注意：插件侧 `guard_path` 出于安全禁止 UNC，但这里处理的是**用户显式选择**
/// 的下载目录，属于合法的 SMB 使用场景，不能沿用该限制。
fn is_unc_path(path: &str) -> bool {
    let p = path.trim_start();
    p.starts_with(r"\\") || p.starts_with("//")
}

/// 网络共享盘降级：SMB 上多连接并发收益极低，且远端句柄/锁竞争会放大 RST
/// 与重连风暴，因此命中 UNC 时强制单连接。
///
/// `split` / `max-connection-per-server` **不是** aria2 的全局可变更选项
///（`changeGlobalOption` 会按 `getChangeGlobalOption` 白名单过滤掉），
/// 只能在 `addUri` 的 per-task options 中下发。
fn downgrade_connections_for_unc(dir: &str, options: &mut Map<String, Value>) {
    if !is_unc_path(dir) {
        return;
    }
    options.insert("split".to_string(), json!("1"));
    options.insert("max-connection-per-server".to_string(), json!("1"));
    println!("[download] 目标目录是网络共享盘，已降级为单连接：{dir}");
}

/// 从 URL 中提取主机名（小写，去掉 userinfo / 端口）。
fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = authority.split(':').next().unwrap_or(authority);
    host.trim().to_ascii_lowercase()
}

/// 内置防盗链规则：主机后缀 → 站点级 Referer。
///
/// 目标是"无感"：用户粘贴直链即可下载，不必理解 Referer 是什么。
/// 规则只覆盖裸请求会被 WAF / CDN 拒绝的站点；匹配按完整主机后缀，
/// `evilqq.com` 这类仿冒域不会命中。
const REFERER_RULES: &[(&str, &str)] = &[
    ("gtimg.cn", "https://qq.com/"),
    ("qq.com", "https://qq.com/"),
    ("bilibili.com", "https://www.bilibili.com/"),
    ("bilivideo.com", "https://www.bilibili.com/"),
    ("weibo.com", "https://weibo.com/"),
    ("sinaimg.cn", "https://weibo.com/"),
    ("github.com", "https://github.com/"),
];

/// 按内置规则推断站点级 Referer；未命中返回 `None`。
fn guess_referer(url: &str) -> Option<&'static str> {
    let host = url_host(url);
    if host.is_empty() {
        return None;
    }
    REFERER_RULES
        .iter()
        .find(|(domain, _)| host == *domain || host.ends_with(&format!(".{domain}")))
        .map(|(_, referer)| *referer)
}

/// 宿主本地配置路径：`<HOME>/config.json`，与引擎全局选项解耦，跨重启保留。
fn host_config_path() -> std::path::PathBuf {
    paths::home().join("config.json")
}

/// 读取宿主配置原始键值；文件缺失或损坏时返回空表（调用方使用各字段默认值）。
fn read_host_config() -> Map<String, Value> {
    std::fs::read_to_string(host_config_path())
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

/// 布尔配置项读取，缺省值由调用方给出。
fn host_config_bool(key: &str, default: bool) -> bool {
    read_host_config()
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

/// 文件分片数（`split`）的默认值。
///
/// 与 `aria2_service.rs` 的 `--split=64` 保持同步；用户可在设置页调整，
/// 由宿主写入 `<HOME>/config.json` 的 `split_size`。
pub const DEFAULT_SPLIT_SIZE: u32 = 64;

/// 单服务器连接数（`max-connection-per-server`）的默认值。
///
/// 保守值 16：再高极易触发 CDN 反爬。引擎侧上限已解封到 128（见
/// `aria2-1.37.0/src/OptionHandlerFactory.cc`），但日常仍建议保持 16。
pub const DEFAULT_MAX_CONNECTIONS_PER_SERVER: u32 = 16;

/// 并发选项统一上限：与解封后的引擎保持一致，只做安全边界，不再按 16 夹取。
pub const CONCURRENCY_CAP: u32 = 128;

/// 读取文件分片数（1..=128），缺省 64。
///
/// 兼容旧配置：`split_size` 尚未写入时，把历史的 `max_connections_per_server`
/// 当作分片数（旧版本那个字段存的就是「单任务最大连接数（分片数）」）。
pub fn configured_split() -> u32 {
    let config = read_host_config();
    config
        .get("split_size")
        .and_then(Value::as_u64)
        .or_else(|| config.get("max_connections_per_server").and_then(Value::as_u64))
        .map(|value| (value as u32).clamp(1, CONCURRENCY_CAP))
        .unwrap_or(DEFAULT_SPLIT_SIZE)
}

/// 读取单服务器连接数（1..=128），缺省 16。
///
/// 旧配置没有 `split_size`，当时的 `max_connections_per_server` 是分片数而非
/// 单服连接数，不能直接沿用 —— 回退默认 16，避免用户在不知情下被拉到高危值。
pub fn configured_connections() -> u32 {
    let config = read_host_config();
    if config.get("split_size").is_none() {
        return DEFAULT_MAX_CONNECTIONS_PER_SERVER;
    }
    config
        .get("max_connections_per_server")
        .and_then(Value::as_u64)
        .map(|value| (value as u32).clamp(1, CONCURRENCY_CAP))
        .unwrap_or(DEFAULT_MAX_CONNECTIONS_PER_SERVER)
}

/// 把用户配置的并发数写成**单任务 options**（`split` 与单服连接数完全独立）。
///
/// `changeGlobalOption` 只影响"此后新建任务继承的全局副本"，且部分选项在引擎
/// 重启后丢失；任务级 options 才是每次 `addUri` 真正生效的裁决点。
///
/// 调用方已显式下发时不覆盖（浏览器扩展 / 插件可以按任务定制）；
/// 随后的 UNC / 防盗链降级会再强制覆盖为 1，优先级最高。
pub fn apply_task_concurrency(options: &mut Map<String, Value>) {
    let split = configured_split();
    let connections = configured_connections();
    options
        .entry("split".to_string())
        .or_insert_with(|| json!(split.to_string()));
    options
        .entry("max-connection-per-server".to_string())
        .or_insert_with(|| json!(connections.to_string()));
}

/// 被宿主强制降级为单连接的 GID（命中防盗链 / 老旧 CDN 规则）。
///
/// 仅用于 UI 的「单线程降级」提示：aria2 的 `tellStatus` 没有这类语义，
/// 只能在宿主侧记一份。进程重启即失效，与当前会话任务同生命周期。
static SINGLE_THREAD_GIDS: LazyLock<StdMutex<std::collections::HashSet<String>>> =
    LazyLock::new(|| StdMutex::new(std::collections::HashSet::new()));

fn mark_single_thread(gid: &str) {
    if let Ok(mut set) = SINGLE_THREAD_GIDS.lock() {
        // 防御性上限：标签丢失无妨，内存不能随任务数无限增长。
        if set.len() > 4096 {
            set.clear();
        }
        set.insert(gid.to_string());
    }
}

fn is_marked_single_thread(gid: &str) -> bool {
    SINGLE_THREAD_GIDS
        .lock()
        .map(|set| set.contains(gid))
        .unwrap_or(false)
}

/// 给任务状态补上 `isDegraded` / `currentConcurrency` 字段（增量 51）。
///
/// 降级只对本次 GID 生效：新任务从用户设置值重新开始，因此这里只读 GID 记录，
/// 不回写任何全局配置。记录会保留到任务被显式删除，所以"降到 1 后成功"的标签仍在。
fn annotate_degradation(task: &mut Map<String, Value>) {
    let gid = task
        .get("gid")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    match degradation::status(&gid) {
        Some((degraded, current)) => {
            task.insert("isDegraded".to_string(), json!(degraded));
            task.insert("currentConcurrency".to_string(), json!(current));
        }
        None => {
            task.insert("isDegraded".to_string(), json!(false));
        }
    }
}

/// 给任务状态补上 `isSingleThread` 字段（前端据此显示橙色降级标签）。
fn annotate_single_thread(task: &mut Map<String, Value>) {
    let flag = task
        .get("gid")
        .and_then(Value::as_str)
        .is_some_and(is_marked_single_thread);
    task.insert("isSingleThread".to_string(), json!(flag));
}

/// 前端读取宿主配置（设置页初始化）：返回带默认值的完整对象。
pub fn get_host_config() -> Value {
    let mut config = read_host_config();
    // 下载目录：**始终**回显经过安全解析的绝对路径。历史版本的 systemprofile /
    // 相对路径不允许再泄漏到设置页与新建任务弹窗。
    let stored_dir = config
        .get("download_dir")
        .and_then(Value::as_str)
        .map(str::to_string);
    let resolved_dir = paths::resolve_download_dir(stored_dir.as_deref());
    config.insert(
        "download_dir".to_string(),
        json!(resolved_dir.to_string_lossy().to_string()),
    );
    config
        .entry("enable_auto_referer".to_string())
        .or_insert(json!(true));
    config
        .entry("enable_http1_compat".to_string())
        .or_insert(json!(false));
    // 文件分片数 / 单服连接数：两者独立，旧配置由访问器负责迁移语义。
    config.insert("split_size".to_string(), json!(configured_split()));
    config.insert(
        "max_connections_per_server".to_string(),
        json!(configured_connections()),
    );
    Value::Object(config)
}

/// 从 `config.json` 解析**真实**下载目录（非法 / systemprofile / 相对路径自动纠正）。
pub fn configured_download_dir() -> std::path::PathBuf {
    let stored = read_host_config()
        .get("download_dir")
        .and_then(Value::as_str)
        .map(str::to_string);
    paths::resolve_download_dir(stored.as_deref())
}

/// 把 `config.json` 里持久化的引擎全局选项**强制全量**下发给 aria2。
///
/// `changeGlobalOption` 只作用于当前 aria2c 进程：覆盖安装 / 引擎崩溃重启后，
/// 下载目录、分片数、单服并发、限速、代理、UA 等全部回到服务启动默认值 ——
/// 这正是"设置被重置 / UI 正确但底层下错地方"的根因。
///
/// 因此每次 WS 连上、以及 UI 启动后显式调用 `sync_engine_config`，都执行一次
/// 幂等全量下发。`dir` / `split` / `max-connection-per-server` 无条件下发，
/// 其余只处理**用户显式保存过**的键（从未保存过的键绝不下发，避免把服务自动
/// 探测的系统代理等覆盖成空）。
async fn apply_saved_globals(client: &Client) {
    let config = read_host_config();
    let mut options = Map::new();

    fn text<'a>(config: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
        config.get(key).and_then(Value::as_str)
    }

    // 下载目录：每次都重放安全解析后的绝对路径。即便从未保存过、或旧配置
    // 里残留 systemprofile / 相对路径，也会被纠正为用户真实下载夹。
    let stored_dir = text(&config, "download_dir").map(str::to_string);
    let resolved_dir = paths::resolve_download_dir(stored_dir.as_deref());
    options.insert(
        "dir".to_string(),
        json!(resolved_dir.to_string_lossy().to_string()),
    );
    // 分片 / 单服并发：无条件全量下发，保证引擎全局副本与设置页显示一致
    // （单任务仍由 `apply_task_concurrency` 再兜一层）。
    options.insert("split".to_string(), json!(configured_split().to_string()));
    options.insert(
        "max-connection-per-server".to_string(),
        json!(configured_connections().to_string()),
    );
    // 允许空串：referer / header / 代理清空是用户的显式意图（空串即关闭）。
    for (config_key, engine_key) in [
        ("proxy", "all-proxy"),
        ("referer", "referer"),
        ("header", "header"),
        ("max_overall_download_limit", "max-overall-download-limit"),
        ("max_download_limit", "max-download-limit"),
    ] {
        if let Some(value) = text(&config, config_key) {
            options.insert(engine_key.to_string(), json!(value));
        }
    }
    // UA 只在非空时下发：空值代表"沿用引擎默认伪装"，写空反而会让 WAF 拦截。
    if let Some(ua) = text(&config, "user_agent").filter(|value| !value.trim().is_empty()) {
        options.insert("user-agent".to_string(), json!(ua));
    }
    for (config_key, engine_key) in [
        ("dynamic_slicing_stall", "dynamic-slicing-stall"),
        ("dynamic_slicing_max_preempts", "dynamic-slicing-max-preempts"),
    ] {
        if let Some(value) = config.get(config_key).and_then(Value::as_u64) {
            options.insert(engine_key.to_string(), json!(value.to_string()));
        }
    }

    let count = options.len();
    metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
    match client.change_global_option(options).await {
        Ok(()) => tracing::info!("已从 config.json 全量下发 {count} 项全局设置"),
        Err(err) => {
            metrics::METRICS.rpc_errors.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("全局设置下发失败（下次连接重试）：{err}");
        }
    }
}

/// UI 连上后端后**显式**触发一次全量配置下发（beta8 配置同步红线）。
pub async fn sync_engine_config() -> Result<(), AppError> {
    let client = client_or_err(state()).await?;
    apply_saved_globals(&client).await;
    Ok(())
}

/// 前端写入宿主配置：只合并给定字段，未知字段原样保留。
pub fn set_host_config(patch: Map<String, Value>) -> Result<(), AppError> {
    let mut config = read_host_config();
    for (key, value) in patch {
        config.insert(key, value);
    }
    let path = host_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AppError::fs(format!("创建配置目录失败（{}）", parent.display()), e))?;
    }
    let body = serde_json::to_string_pretty(&Value::Object(config))
        .map_err(|e| AppError::Internal(format!("序列化宿主配置失败：{e}")))?;
    std::fs::write(&path, body)
        .map_err(|e| AppError::fs(format!("写入宿主配置失败（{}）", path.display()), e))
}

/// 隐形防盗链的全局开关（默认开启）。
///
/// 属隐含配置：不在 UI 暴露；`<HOME>/config.json` 写入
/// `{"enable_auto_referer": false}` 即可关闭。文件缺失或解析失败一律按开启处理。
fn auto_referer_enabled() -> bool {
    host_config_bool("enable_auto_referer", true)
}

/// 用户手动开启的「HTTP/1.1 兼容模式」（默认关闭）。
fn http1_compat_enabled() -> bool {
    host_config_bool("enable_http1_compat", false)
}

/// 已知对 HTTP/2 帧格式 / 大分片 Range 处理不完整的老旧 CDN。
const LEGACY_CDN_DOMAINS: &[&str] = &["gtimg.cn"];

fn is_legacy_cdn(url: &str) -> bool {
    let host = url_host(url);
    LEGACY_CDN_DOMAINS
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

/// 引擎全局 Referer 是否已被用户显式设置（设置 → 网络 → 自定义 Referer）。
///
/// `aria2.addUri` 以**全局选项副本**为基底再叠加任务选项，任务级猜测值会
/// 覆盖全局设置。故注入前先查一次，保证"用户显式设置 > 智能猜测"。
/// 查询失败时按"未设置"处理：宁可让下载多带一个站点 Referer，也不要让
/// 本可成功的直链因缺少来源被 403。
async fn global_referer_present(client: &Client) -> bool {
    match client.call("aria2.getGlobalOption", vec![]).await {
        Ok(Value::Object(map)) => map
            .get("referer")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty()),
        _ => false,
    }
}

/// 添加一个下载任务，返回 aria2 分配的 GID。
///
/// 目录解析放在 Rust 端：前端未传（`None`）或传空串时回退到默认下载目录，
/// 并在下发前用 `create_dir_all` 确保目录真实存在，避免 aria2c 因目录缺失而报错。
#[tracing::instrument(skip_all, fields(url = %url, dir = ?dir))]
pub async fn add_download(
    url: String,
    dir: Option<String>,
    extra_options: Option<Map<String, Value>>,
) -> Result<String, AppError> {
    let client = client_or_err(state()).await?;

    // 微内核前置拦截：把原始 URL 交给插件总线改写（宿主不含任何站点解析逻辑）。
    let url = intercept_url(url).await;

    // 前端未传、传空、传相对路径或历史遗留的 systemprofile 路径时，强制兜底到
    // 当前用户的系统「下载」文件夹。相对路径在 LocalSystem 下会解析到 System32。
    let target_dir = paths::resolve_download_dir(dir.as_deref())
        .to_string_lossy()
        .into_owned();
    std::fs::create_dir_all(&target_dir)
        .map_err(|e| AppError::fs(format!("创建下载目录失败（{target_dir}）"), e))?;

    // 增量 28：`url` 与 `target_dir` 会被下面的 RPC 参数取走，先各留一份给落库
    //（此时它们是本地已知量，不需要额外查询即可建立历史行）。
    let url_for_db = url.clone();
    let dir_for_db = target_dir.clone();

    // 选项直接以 kebab-case 键值对下发（`dir`），无需中间结构体。
    let mut options = Map::new();
    options.insert("dir".to_string(), json!(target_dir));
    // 单任务级选项（如 referer 防盗链）：原样并入 aria2 选项表。
    // `dir` 由宿主统一裁决（默认目录 + UNC 降级），不允许被单任务覆盖。
    if let Some(extra) = extra_options {
        for (key, value) in extra {
            if key != "dir" {
                options.insert(key, value);
            }
        }
    }
    // 并发：调用方没显式给时，用设置页的「单任务最大连接数（分片数）」补齐。
    // 必须在 extras 之后 —— 显式值优先；随后的 UNC / 防盗链降级则反过来强制覆盖。
    // 不依赖 changeGlobalOption：全局副本既不能表达"按任务定制"，引擎重启后也会丢。
    apply_task_concurrency(&mut options);
    // 增量 42：SMB / UNC 目录自动降级为单连接（宿主策略，强制覆盖）。
    downgrade_connections_for_unc(&target_dir, &mut options);
    // 隐形防盗链引擎（优先级由高到低）：
    // 1. 浏览器扩展 / 调用方显式传入 —— 已在 `options` 里，直接尊重，绝不覆盖；
    // 2. 设置页的全局自定义 Referer —— 由引擎全局选项承接，注入前先探测；
    // 3. 内置规则猜测 —— 仅在以上都没有时兜底。
    if !options.contains_key("referer") && auto_referer_enabled() {
        if let Some(referer) = guess_referer(&url) {
            if !global_referer_present(&client).await {
                options.insert("referer".to_string(), json!(referer));
                tracing::info!(
                    "[download] 已自动为 {} 注入 Referer: {}",
                    url_host(&url),
                    referer
                );
            }
        }
    }

    // HTTP/1.1 兼容：全局开关命中，或目标属于已知的老旧 CDN（腾讯 gtimg）。
    // 后者同时降为单连接 —— 老旧 CDN 对大分片 Range 的边界处理经常有 bug，
    // 多连接会把"能下的文件"切成"必 403 的请求"。
    let legacy_cdn = is_legacy_cdn(&url);
    if !options.contains_key("enable-http2") && (http1_compat_enabled() || legacy_cdn) {
        options.insert("enable-http2".to_string(), json!("false"));
        if !legacy_cdn {
            tracing::info!("[download] 已按 HTTP/1.1 兼容模式下发 enable-http2=false");
        }
    }
    if legacy_cdn {
        // 防盗链 / 老旧 CDN：**强制覆盖**为单连接（调用方显式传了更大的拆分也不采纳）。
        // 这类站点对并发 Range 请求常直接回 403，多连接会把"能下的文件"切碎成必失败的请求。
        options.insert("split".to_string(), json!("1"));
        options.insert("max-connection-per-server".to_string(), json!("1"));
        tracing::info!(
            "[download] {} 命中老旧 CDN 兼容规则：已强制 HTTP/1.1 + 单连接",
            url_host(&url)
        );
    }

    metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
    let retry_options = options.clone();
    let gid = rpc_bounded("addUri", client.add_uri(vec![url], options)).await?;
    // 增量 51：登记并发档位，供 onDownloadError 时自动降级重试。
    degradation::register(&gid, &url_for_db, &dir_for_db, &retry_options);
    // 记下"被动降级"的任务：前端据此显示 [单线程降级] 标签。
    if legacy_cdn {
        mark_single_thread(&gid);
    }

    // 落库：fire-and-forget，绝不 `.await` 在命令路径上（否则一次引擎卡顿会拖住 IPC 响应）。
    record_added(client.clone(), gid.clone(), url_for_db, dir_for_db);
    Ok(gid)
}

/// 添加 BT 种子任务（拖拽 `.torrent` 文件）。
///
/// 数据来源二选一：
/// - `data`：前端 `FileReader` 读出的 base64（WebView 不暴露真实文件路径，这是主通道）；
/// - `path`：本地文件路径（留给 CLI / 未来原生拖拽，读取后由本函数编码）。
///
/// `name` 仅用于历史记录与日志展示 —— `addTorrent` 的返回值只有 GID，
/// 引擎侧文件名要等元数据解析完成后才能确定。
pub async fn add_torrent(
    path: Option<String>,
    data: Option<String>,
    name: Option<String>,
) -> Result<String, AppError> {
    let client = client_or_err(state()).await?;

    let (torrent_b64, source) = match data.filter(|value| !value.trim().is_empty()) {
        Some(b64) => (
            b64.trim().to_string(),
            name.unwrap_or_else(|| "torrent".to_string()),
        ),
        None => {
            let path = path.ok_or_else(|| {
                AppError::InvalidArgs("add_torrent：缺少种子数据（data）或路径（path）".to_string())
            })?;
            let bytes = std::fs::read(&path)
                .map_err(|e| AppError::fs(format!("读取种子文件失败（{path}）"), e))?;
            let source = name
                .or_else(|| {
                    std::path::Path::new(&path)
                        .file_name()
                        .map(|value| value.to_string_lossy().to_string())
                })
                .unwrap_or_else(|| "torrent".to_string());
            (base64_encode(&bytes), source)
        }
    };

    let target_dir = paths::downloads_dir().to_string_lossy().into_owned();
    std::fs::create_dir_all(&target_dir)
        .map_err(|e| AppError::fs(format!("创建下载目录失败（{target_dir}）"), e))?;

    // aria2.addTorrent(torrent, uris, options)：torrent 必须是 **base64 编码的种子字节**；
    // uris 传空数组（无 Web Seed 时的标准写法）。
    let mut options = Map::new();
    options.insert("dir".to_string(), json!(target_dir));

    metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
    let value = rpc_bounded(
        "addTorrent",
        client.call(
            "addTorrent",
            vec![json!(torrent_b64), json!([]), json!(options)],
        ),
    )
    .await?;
    let gid = value
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::Internal("addTorrent 返回的 GID 非法".to_string()))?
        .to_string();

    record_added(
        client.clone(),
        gid.clone(),
        format!("torrent:{source}"),
        target_dir,
    );
    tracing::info!(gid = %gid, torrent = %source, "BT 种子任务已添加");
    Ok(gid)
}

/// 标准 Base64 编码（RFC 4648，带 `=` 填充）。
///
/// 刻意不引入 `base64` 依赖：仅此一处使用，20 行实现足够，且避免为单个工具函数
/// 扩大依赖面（发布前收敛体积）。
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(triple >> 18) as usize & 0x3F] as char);
        out.push(TABLE[(triple >> 12) as usize & 0x3F] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(triple >> 6) as usize & 0x3F] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[triple as usize & 0x3F] as char
        } else {
            '='
        });
    }
    out
}

/// 汇总 active + waiting + stopped 三类任务（字段集合由 `TASK_KEYS` 约定）。
pub async fn get_tasks() -> Result<Vec<Map<String, Value>>, AppError> {
    let client = client_or_err(state()).await?;
    let keys: Option<Vec<String>> = Some(TASK_KEYS.iter().map(|k| k.to_string()).collect());

    let mut tasks: Vec<Map<String, Value>> = Vec::new();
    metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
    tasks.extend(
        rpc_bounded("tellActive", client.tell_active(keys.clone())).await?,
    );
    metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
    tasks.extend(
        rpc_bounded(
            "tellWaiting",
            client.tell_waiting(0, 1000, keys.clone()),
        )
        .await?,
    );
    metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
    tasks.extend(
        rpc_bounded("tellStopped", client.tell_stopped(0, 1000, keys)).await?,
    );
    // 增量 29（P1-G）：内部任务（tracker 自拉）不进 UI 列表。
    tasks.retain(|task| !is_internal_task(task));
    for task in &mut tasks {
        annotate_single_thread(task);
        annotate_degradation(task);
    }
    Ok(tasks)
}

/// 获取单个任务详情（带 `TASK_KEYS` 白名单），供事件驱动的单 gid 增量更新使用。
pub async fn get_task(gid: String) -> Result<Map<String, Value>, AppError> {
    let client = client_or_err(state()).await?;
    let keys: Option<Vec<String>> = Some(TASK_KEYS.iter().map(|k| k.to_string()).collect());
    metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
    let mut task = rpc_bounded("tellStatus", client.tell_status(&gid, keys)).await?;
    annotate_single_thread(&mut task);
    annotate_degradation(&mut task);
    Ok(task)
}

/// 仅拉取活跃任务，供前端 1s 高频轮询更新进度（比全量 get_tasks 便宜）。
pub async fn get_active_tasks() -> Result<Vec<Map<String, Value>>, AppError> {
    let client = client_or_err(state()).await?;
    let keys: Option<Vec<String>> = Some(TASK_KEYS.iter().map(|k| k.to_string()).collect());
    metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
    let mut tasks = rpc_bounded("tellActive", client.tell_active(keys)).await?;
    // 增量 29（P1-G）：内部任务（tracker 自拉）不进 UI 列表。
    tasks.retain(|task| !is_internal_task(task));
    for task in &mut tasks {
        annotate_single_thread(task);
        annotate_degradation(task);
    }
    Ok(tasks)
}

/// 每个 GID 最近一次「暂停」完成的时刻。
///
/// `forcePause` 返回只代表请求已受理：aria2 关闭 `.aria2` 控制文件与部分文件
/// 是异步的，句柄可能还被杀软扫描占用。此时立刻 `unpause` 会撞上 Windows 的
/// 文件占用（errorCode=17，UI 表现为「无法打开已有文件」）。
static LAST_PAUSE: LazyLock<StdMutex<HashMap<String, std::time::Instant>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));
/// 暂停 → 继续之间的最小静默期（句柄回收窗口）。
const RESUME_SETTLE: Duration = Duration::from_millis(400);

/// 暂停任务（对 active / waiting / paused 状态均可用 `forcePause`）。
pub async fn pause_download(gid: String) -> Result<(), AppError> {
    let client = client_or_err(state()).await?;
    metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
    rpc_bounded("pause", client.force_pause(&gid)).await?;
    // 记录暂停时刻，供 resume_download 补足句柄回收窗口（表满即整体清空）。
    if let Ok(mut map) = LAST_PAUSE.lock() {
        if map.len() >= 1024 {
            map.clear();
        }
        map.insert(gid, std::time::Instant::now());
    }
    Ok(())
}

/// 暂停所有任务（托盘菜单「暂停所有任务」）。
///
/// 走 `forcePauseAll`：对 active / waiting 一并生效，不依赖前端任务列表；
/// 引擎未就绪时返回可读错误，由托盘动作记录日志，绝不 panic。
pub async fn pause_all() -> Result<(), AppError> {
    let client = client_or_err(state()).await?;
    metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
    rpc_bounded("forcePauseAll", client.force_pause_all()).await
}

/// 关闭窗口时是否隐藏到托盘（外壳主循环读取）。
pub fn close_to_tray() -> bool {
    CLOSE_TO_TRAY.load(Ordering::Relaxed)
}

/// 设置关闭行为：`tray` = 最小化到托盘，`quit` = 直接退出。
pub fn set_close_behavior(mode: &str) -> Result<(), AppError> {
    match mode {
        "tray" => {
            CLOSE_TO_TRAY.store(true, Ordering::Relaxed);
            Ok(())
        }
        "quit" => {
            CLOSE_TO_TRAY.store(false, Ordering::Relaxed);
            Ok(())
        }
        other => Err(AppError::InvalidArgs(format!(
            "未知关闭行为：{other}（仅支持 tray / quit）"
        ))),
    }
}

/// 继续已暂停的任务。
pub async fn resume_download(gid: String) -> Result<(), AppError> {
    let client = client_or_err(state()).await?;
    // 新一轮续传：重置「文件被占用」重试预算（每次继续都重新拥有 3 次机会）。
    degradation::reset_file_io_attempts(&gid);
    // 暂停后立刻继续（用户手快 / 全部继续）：补足 400ms 静默期，避免 aria2
    // 重新打开句柄时被 Windows 占用检查拦下（errorCode=17）。
    let paused_at = LAST_PAUSE.lock().ok().and_then(|mut map| map.remove(&gid));
    if let Some(at) = paused_at {
        let settle = RESUME_SETTLE.saturating_sub(at.elapsed());
        if !settle.is_zero() {
            tracing::info!(
                gid = %gid,
                wait_ms = settle.as_millis() as u64,
                "暂停后立即继续：等待文件句柄释放后再 unpause"
            );
            tokio::time::sleep(settle).await;
        }
    }
    metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
    rpc_bounded("unpause", client.unpause(&gid)).await
}

/// 删除任务。
///
/// 活动/等待态用 `forceRemove`（立即移除、不做 tracker 注销等耗时动作）；
/// 已停止（complete/error/removed）的条目 `forceRemove` 会报错，回退用
/// `removeDownloadResult` 把它从结果列表里清除。
///
/// aria2 语义里 `forceRemove` 只把任务置为 `removed`，结果记录仍留在
/// `tellStopped` 中；UI 的「删除」要求卡片立刻消失，因此成功后再补一次
/// `removeDownloadResult`（engine-net 与 aria2c 都会把记录从结果表摘掉）。
pub async fn remove_download(gid: String) -> Result<(), AppError> {
    let client = client_or_err(state()).await?;
    // 增量 51：任务被显式删除后不再保留降级记录（GID 可能被后续任务复用）。
    degradation::forget(&gid);
    if let Ok(mut map) = LAST_PAUSE.lock() {
        map.remove(&gid);
    }
    metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
    match rpc_bounded("forceRemove", client.force_remove(&gid)).await {
        Ok(()) => {
            // 尽力而为：任务已经从活动列表消失，结果记录清理失败不影响删除结果。
            metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
            let _ = rpc_bounded("removeDownloadResult", client.remove_download_result(&gid)).await;
            Ok(())
        }
        Err(_) => {
            metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
            rpc_bounded("removeDownloadResult", client.remove_download_result(&gid)).await
        }
    }
}

/// 强制删除（任务卡片右键菜单）：立即中断并移除任务。
///
/// 与 [`remove_download`] 共用同一实现 —— 活动/等待态走 `forceRemove`，
/// 已停止条目回退 `removeDownloadResult`（否则恒报 "GID not found"）。
/// 单独保留命令名是为了让前端语义（右键强制删除）与后端动作一一对应。
pub async fn force_remove(gid: String) -> Result<(), AppError> {
    remove_download(gid).await
}

/// 删除任务并清理本地文件（前端「同时删除本地文件」）。
///
/// 顺序不可调换：`tellStatus` 在任务被移除后就查不到路径了，所以必须**先取路径**、
/// 再移除任务（active 任务由 `forceRemove` 中止并释放句柄）、最后删物理文件。
///
/// 文件清理是「尽力而为」：被占用 / 已被 auto-sort 插件移走 / 路径不存在都只记日志 ——
/// 任务记录此时已经删掉，绝不能因为文件清理失败把错误抛回前端。
///
/// 单文件任务取 `files[0].path`；BT 多文件任务会清理 `files` 列出的全部文件。
pub async fn remove_task_and_files(gid: String) -> Result<(), AppError> {
    let (file_paths, task_dir) = match get_task(gid.clone()).await {
        Ok(task) => {
            let paths = task
                .get("files")
                .and_then(|value| value.as_array())
                .map(|files| {
                    files
                        .iter()
                        .filter_map(|file| file.get("path").and_then(|path| path.as_str()))
                        .map(str::trim)
                        .filter(|path| !path.is_empty())
                        .map(PathBuf::from)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let dir = task
                .get("dir")
                .and_then(|value| value.as_str())
                .map(PathBuf::from);
            (paths, dir)
        }
        Err(err) => {
            // 任务已不在引擎里（例如刚被其它入口删掉）：仍尝试移除，路径缺失时只删任务记录。
            tracing::warn!(gid = %gid, "删除任务前读取路径失败，将只移除任务记录：{err}");
            (Vec::new(), None)
        }
    };

    // 先移除任务：active / waiting 走 forceRemove，立即中止并释放 Windows 文件句柄。
    remove_download(gid.clone()).await?;

    if file_paths.is_empty() {
        return Ok(());
    }

    // forceRemove 返回后句柄释放可能仍有延迟：native 引擎的 abort 只是取消请求，
    // 协程要到下一次 await 才真正 drop（Windows 上删除会撞 os error 32）。
    // 这里给足窗口：6 次 × 150ms ≈ 0.9s，远大于一次 event-loop 调度。
    const DELETE_ATTEMPTS: usize = 6;
    let mut pending = file_paths.clone();
    for attempt in 1..=DELETE_ATTEMPTS {
        let mut retry: Vec<PathBuf> = Vec::new();
        for path in pending {
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    tracing::info!(gid = %gid, path = %path.display(), "已删除任务文件");
                    remove_progress_files(&path);
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    // 已不存在：被 auto-sort 插件移走 / 用户手工删除，不算失败。
                    tracing::debug!(gid = %gid, path = %path.display(), "任务文件已不存在，跳过");
                    remove_progress_files(&path);
                }
                Err(err) if attempt < DELETE_ATTEMPTS => {
                    tracing::debug!(
                        gid = %gid,
                        path = %path.display(),
                        "删除任务文件失败，稍后重试：{err}"
                    );
                    retry.push(path);
                }
                Err(err) => {
                    tracing::warn!(
                        gid = %gid,
                        path = %path.display(),
                        "删除任务文件失败（已忽略，任务本身已移除）：{err}"
                    );
                    // 文件仍在时离开前再试一次清理控制文件（尽力而为）。
                    remove_progress_files(&path);
                }
            }
        }
        if retry.is_empty() {
            break;
        }
        pending = retry;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    // 清理删空后的父目录；下载根目录与盘符根永远保留。
    cleanup_empty_parents(&file_paths, task_dir.as_deref());
    Ok(())
}

/// 删除 `<file>.aria2` / `<file>.sqlite` 进度文件（存在才删，失败仅日志）。
fn remove_progress_files(path: &Path) {
    // SQLite 的 WAL 伴生文件也要一并清理，否则残留会干扰下一次续传。
    for suffix in [".aria2", ".sqlite", ".sqlite-wal", ".sqlite-shm"] {
        let mut raw = path.as_os_str().to_owned();
        raw.push(suffix);
        let sidecar = PathBuf::from(raw);
        match std::fs::remove_file(&sidecar) {
            Ok(()) => tracing::info!(path = %sidecar.display(), "已删除进度文件"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(path = %sidecar.display(), "删除进度文件失败（已忽略）：{err}")
            }
        }
    }
}

/// 清理删除文件后留下的空目录（`remove_dir` 对非空目录会失败，天然安全）。
fn cleanup_empty_parents(files: &[PathBuf], task_dir: Option<&Path>) {
    let downloads = paths::downloads_dir();
    for file in files {
        let Some(parent) = file.parent() else {
            continue;
        };
        // 盘符根 / UNC 共享根：绝不动。
        if parent.parent().is_none() {
            continue;
        }
        // 任务下载根目录用户还在用，不能被当成"空目录"删掉。
        if same_path(&downloads, parent) || task_dir.is_some_and(|dir| same_path(dir, parent)) {
            continue;
        }
        match std::fs::remove_dir(parent) {
            Ok(()) => tracing::info!(dir = %parent.display(), "已清理空目录"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            // 非空目录 / 被占用：保留即可。
            Err(_) => {}
        }
    }
}

/// 路径比较：Windows 大小写不敏感，统一分隔符并去掉尾斜杠后再比。
fn same_path(a: &Path, b: &Path) -> bool {
    let normalize = |path: &Path| {
        path.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    normalize(a) == normalize(b)
}

/// 模块 5（本轮**未实现**）：`.torrent` 文件关联状态查询。
///
/// TODO(P5)：
/// 1. 注册 `HKCU\Software\Classes\.torrent` → `Aria2Harness.Torrent`；
/// 2. 写 `...\Aria2Harness.Torrent\shell\open\command` = `"<exe>" --open-torrent "%1"`；
/// 3. 外壳启动时解析 `--open-torrent <路径>`，直接调用 [`add_torrent`]；
/// 4. 卸载时清理上述键（幂等）。
///
/// 本轮只留函数坑位，**不写注册表**（用户裁决）。
pub fn associate_torrent_registered() -> bool {
    false
}

/// 动态修改 aria2 全局选项（供后续设置页使用）。
///
/// `options` 为 kebab-case 键值对，例如 `{"max-concurrent-downloads":"5"}`；
/// 值统一用字符串语义，原样作为 `aria2.changeGlobalOption` 的第一个参数。
///
/// 增量 36：自研客户端不再把返回值反序列化成 `()`，因此不会出现旧版
/// `aria2-ws` 那种"引擎返回字符串 `OK`、包装层期望 `unit`"的误报；
/// 真正失败时服务端会回 `error` 对象，客户端转成 [`rpc_client::RpcError::Server`]。
pub async fn change_global_option(options: Map<String, Value>) -> Result<(), AppError> {
    let client = client_or_err(state()).await?;
    metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
    rpc_bounded("changeGlobalOption", client.change_global_option(options)).await
}

/// 读取引擎当前全局选项（设置页初始化用）。
///
/// `aria2.getGlobalOption` 返回一个巨大的 kebab-case 键值对象（数百项），
/// 前端只取其中十余项渲染，故原样透传、不做服务端裁剪：
/// 裁剪逻辑放 UI 层，后续加设置项时无需再动 Rust。
pub async fn get_global_option() -> Result<Value, AppError> {
    let client = client_or_err(state()).await?;
    metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
    let mut value =
        rpc_bounded("getGlobalOption", client.call("aria2.getGlobalOption", vec![])).await?;
    // 引擎的 dir 可能来自启动 cwd 或旧配置重放：返回前统一安全解析，
    // 前端「新建任务」弹窗直接拿这个值做默认目录。
    if let Some(object) = value.as_object_mut() {
        let raw = object
            .get("dir")
            .and_then(Value::as_str)
            .map(str::to_string);
        let resolved = paths::resolve_download_dir(raw.as_deref());
        object.insert("dir".to_string(), json!(resolved.to_string_lossy().to_string()));
    }
    Ok(value)
}

/// 有界 RPC：命令路径上的引擎调用必须有超时上界。
///
/// 自研客户端内部已带 [`rpc_client::RPC_TIMEOUT`]，这里再套一层是**双保险**：
/// 一是覆盖"客户端整体不可用"这类非等待型故障，二是把错误统一计入 `rpc_errors`。
/// 关键是让"单条命令再坏也只坏这一条"，绝不把串行 IPC 通道永久堵死。
async fn rpc_bounded<T, F>(method: &str, fut: F) -> Result<T, AppError>
where
    F: std::future::Future<Output = Result<T, rpc_client::RpcError>>,
{
    match tokio::time::timeout(RECORD_TIMEOUT, fut).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => {
            metrics::METRICS.rpc_errors.fetch_add(1, Ordering::Relaxed);
            Err(AppError::from(err))
        }
        Err(_) => {
            metrics::METRICS.rpc_errors.fetch_add(1, Ordering::Relaxed);
            Err(AppError::Internal(format!(
                "aria2 响应超时（{}s）：{method}",
                RECORD_TIMEOUT.as_secs()
            )))
        }
    }
}

/// 查询当前引擎（WS RPC）是否已连接。
///
/// 前端 `onMounted` 主动调用一次以对齐初始状态：`connected`/`ws_connected` 事件可能在
/// 前端注册监听器**之前**就已被发射（时序竞争），仅依赖事件会让 UI 长期停留在"连接中"。
pub async fn get_engine_status() -> Result<bool, AppError> {
    Ok(state().client.lock().await.is_some())
}

/// 立即触发一次引擎重连（窗口回到前台时调用）。
///
/// 已连接时不做任何事；未连接则打断指数退避（最长 30s），立即重新握手。
/// 返回重连发起后的连接快照（调用瞬间的状态，前端仍以 WS 事件为准）。
pub async fn reconnect_engine() -> Result<bool, AppError> {
    let guard = state().client.lock().await;
    let Some(client) = guard.as_ref() else {
        return Ok(false);
    };
    let connected = client.is_connected();
    if !connected {
        client.reconnect_now();
    }
    Ok(connected)
}

/// 代理连通性测试（设置 → 网络）：对 `host:port` 做一次 TCP 握手，返回耗时毫秒。
///
/// 只验证"能不能连上代理端口"，不发起真实 HTTP 代理请求 —— 这正是用户点「测试」
/// 想知道的（地址填错 / 代理没开），也是唯一不依赖目标站点可达性的判据。
/// 3s 超时：连不上的代理通常表现为静默丢包，不能把设置页卡住。
pub async fn test_proxy(host: String, port: u16) -> Result<u64, AppError> {
    let host = host.trim();
    if host.is_empty() {
        return Err(AppError::InvalidArgs("代理地址不能为空".to_string()));
    }

    let started = std::time::Instant::now();
    let connect = tokio::net::TcpStream::connect((host, port));
    match tokio::time::timeout(Duration::from_secs(3), connect).await {
        Ok(Ok(_stream)) => Ok(started.elapsed().as_millis() as u64),
        Ok(Err(err)) => Err(AppError::Network(format!("{host}:{port} 无法连接：{err}"))),
        Err(_) => Err(AppError::Network(format!(
            "{host}:{port} 连接超时（3s），请检查地址与代理是否已启动"
        ))),
    }
}

/// 读取全局可观测性计数器的快照（原始 JSON，前端渲染留待后续）。
///
/// 该命令无入参、无锁，直接读取 `AtomicU64`（`Relaxed`），不会阻塞事件循环。
pub fn get_metrics() -> Result<Value, AppError> {
    Ok(metrics::snapshot())
}

/// 全局统计（L3 表现层新增）：上下行速度 / 各类任务计数。
///
/// `aria2.getGlobalStat` 的字段全部是字符串，原样透传给前端，
/// 由前端统一做数值转换与单位格式化。
pub async fn get_global_stat() -> Result<Value, AppError> {
    let client = client_or_err(state()).await?;
    metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
    rpc_bounded(
        "getGlobalStat",
        client.call("aria2.getGlobalStat", vec![]),
    )
    .await
}

/// 当前注入的 Tracker 列表与最近更新元数据。
pub async fn get_trackers() -> Result<Value, AppError> {
    tracker::snapshot().await
}

/// 手动同步 Tracker（自拉 → 解析 → 注入），返回同步后的快照。
pub async fn refresh_trackers() -> Result<Value, AppError> {
    tracker::refresh().await?;
    tracker::snapshot().await
}

/// 查询历史任务（增量 28 / P2-B）：**只返回已结束**（complete/error/removed）的任务，分页。
///
/// 返回 `{ items, total }`：带 `total` 前端才能渲染分页器与"共 N 条"。
/// 数据库不可用时返回 `DB` 码（下载主链路不受影响）。
pub async fn get_history_tasks(limit: i64, offset: i64) -> Result<db::TaskPage, AppError> {
    let database = db().ok_or_else(db_unavailable)?;
    database.history(limit, offset).await
}

/// 清空历史（增量 28 / P2-B）：**只删已结束**任务，active/waiting/paused 的行一律保留。
/// 返回删除条数。
pub async fn clear_history() -> Result<usize, AppError> {
    let database = db().ok_or_else(db_unavailable)?;
    database.clear_history().await
}

/// 读取当前已加载插件列表（供插件管理 UI 展示）。
pub fn list_plugins() -> Vec<PluginInfo> {
    plugins().list()
}

/// 启用/禁用指定插件。
///
/// 禁用为**挂起**语义：插件线程与 QuickJS 运行时仍存活，但 HookDispatcher 不再向其派发事件，
/// 保留热启用的即时性（无需重载脚本）。
///
/// 若该插件存在**新增未授权权限**，启用会被拒绝（返回 `Err`），需前端弹窗确认后走
/// `grant_plugin_permissions`。
pub fn toggle_plugin(id: String, enabled: bool) -> Result<(), AppError> {
    plugins().set_enabled(&id, enabled).map_err(AppError::from)
}

/// 用户在权限确认弹窗中确认后，强制刷新授权快照并启用该插件（P0-A 授权可解释性闭环）。
pub fn grant_plugin_permissions(id: String) -> Result<(), AppError> {
    plugins().grant_permissions(&id).map_err(AppError::from)
}

/// 保存插件设置（JSON Schema 表单提交）。
///
/// 校验、持久化（registry.json）与运行时热更新统一由 `PluginManager` 完成；
/// `ctx.settings.get()` 在下一次 hook 调用时即可读到新值，无需重载插件。
pub fn update_plugin_settings(id: String, values: Map<String, Value>) -> Result<(), AppError> {
    plugins()
        .update_settings(&id, values.into_iter().collect())
        .map_err(AppError::from)
}

/// 在系统文件管理器中定位文件（替代 `@tauri-apps/plugin-opener` 的 `revealItemInDir`）。
///
/// 把前端传来的路径整理成 Explorer 能直接消费的 Windows 路径。
///
/// 两个实测坑：
/// - **正斜杠**：`std::fs::metadata` 接受 `C:/a/b.mp4`，但 Explorer 的 `/select,`
///   解析不了正斜杠，会**静默回退到默认文件夹**（用户看到的就是"只弹出桌面"）。
/// - **`\\?\` 长路径前缀**：Explorer 不认；去掉后普通文件名的路径长度完全够用。
fn normalize_reveal_path(path: &str) -> String {
    let trimmed = path.trim().trim_matches('"').trim();
    let stripped = trimmed.strip_prefix(r"\\?\").unwrap_or(trimmed);
    if cfg!(windows) {
        stripped.replace('/', "\\")
    } else {
        stripped.to_string()
    }
}

/// 引号内的尾部反斜杠补成偶数，否则 `"C:\"` 里的 `\"` 会被 Explorer 当成转义引号。
fn explorer_quote(path: &str) -> String {
    let trailing = path.chars().rev().take_while(|c| *c == '\\').count();
    if trailing % 2 == 1 {
        format!("\"{path}\\\"")
    } else {
        format!("\"{path}\"")
    }
}

/// 向上找到第一个真实存在的目录（文件被移走时的兜底落点）。
fn nearest_existing_dir(path: &str) -> Option<String> {
    let mut candidate = std::path::Path::new(path).parent();
    while let Some(dir) = candidate {
        if dir.is_dir() {
            return Some(dir.to_string_lossy().into_owned());
        }
        candidate = dir.parent();
    }
    None
}

/// 启动 Explorer：`select = true` 时 `/select,"<path>"`（逗号后**紧贴**引号，
/// 中间不能有空格），否则直接打开目录。失败时降级到 `cmd /C` 代理。
fn spawn_explorer(select: bool, path: &str) -> Result<(), AppError> {
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;

    let quoted = explorer_quote(path);
    let arg = if select {
        format!("/select,{quoted}")
    } else {
        quoted
    };

    let mut command = std::process::Command::new("explorer.exe");
    command.raw_arg(arg.clone());
    tracing::info!("[open] 构造的命令行: {:?}", command);

    match command.spawn() {
        Ok(_) => Ok(()),
        Err(first) => {
            tracing::warn!(
                error = %first,
                "[open] explorer.exe 直接启动失败，改用 cmd /C 兜底"
            );
            // cmd /C 之后是**一整条命令行**，必须用 raw_arg 原样拼接：
            // 拆成多个 args 会被空格切开，`/select,` 与路径分离后只会打开父目录。
            let mut fallback = std::process::Command::new("cmd.exe");
            fallback.raw_arg(format!("/C explorer.exe {arg}"));
            tracing::info!("[open] 兜底命令行: {:?}", fallback);
            fallback
                .spawn()
                .map(|_| ())
                .map_err(|e| AppError::fs(format!("打开文件夹失败（{path}）"), e))
        }
    }
}

/// 路径由前端给出（`files[0].path`，缺失时退回任务 `dir`）。
///
/// 处理链路：
/// 1. 记录原始路径，并归一化成 Explorer 可识别的反斜杠绝对路径；
/// 2. 目标存在 → 文件 `/select,"<path>"` 选中，目录直接打开；
/// 3. 目标不存在（被 auto-sort 等插件移走 / 尚未落盘）→ 退到最近的已存在
///    父目录 —— 绝不让 Explorer 因非法路径回退到「桌面」。
pub fn reveal_in_dir(path: String) -> Result<(), AppError> {
    tracing::info!("[open] 收到原始路径: {:?}", path);

    let target = normalize_reveal_path(&path);
    if target.is_empty() {
        return Err(AppError::InvalidArgs("reveal_in_dir：路径为空".to_string()));
    }

    let meta = match std::fs::metadata(&target) {
        Ok(meta) => meta,
        Err(error) => {
            if let Some(dir) = nearest_existing_dir(&target) {
                tracing::warn!(
                    path = %target,
                    dir = %dir,
                    error = %error,
                    "[open] 目标不存在，改为打开最近的已存在父目录"
                );
                return spawn_explorer(false, &dir);
            }
            tracing::warn!(
                path = %target,
                error = %error,
                "[open] 目标及其父目录均不存在"
            );
            return Err(AppError::fs(
                format!("文件或目录不存在（可能已被移动）：{target}"),
                error,
            ));
        }
    };

    if meta.is_dir() {
        spawn_explorer(false, &target)
    } else {
        spawn_explorer(true, &target)
    }
}

/// 用系统默认浏览器打开外部链接（增量 46：更新通知的"前往下载"）。
///
/// **只放行 http / https**：这个入口的上游是远端 `version.json`，若清单被替换成
/// `file:///C:/Windows/...` 或自定义协议，就等于给了外部一份"任意本地执行"的入口。
#[cfg(windows)]
pub fn open_external(url: String) -> Result<(), AppError> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let url = url.trim().to_string();
    if !update_check::is_http_url(&url) {
        return Err(AppError::InvalidArgs(format!(
            "只允许打开 http(s) 链接：{url}"
        )));
    }

    let verb: Vec<u16> = "open\0".encode_utf16().collect();
    let target: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    // Safety: 两个缓冲区都是 NUL 结尾且在调用期间存活；没有额外的参数 / 目录。
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = result.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(AppError::Service(format!(
            "打开浏览器失败（ShellExecuteW={code}）"
        )))
    }
}

/// 非 Windows 平台没有"默认浏览器"的统一入口，直接拒绝（本项目产物仅 Windows）。
#[cfg(not(windows))]
pub fn open_external(_url: String) -> Result<(), AppError> {
    Err(AppError::Internal("当前平台不支持打开外部链接".to_string()))
}

// ---------------------------------------------------------------------------
// 设置页基础设施：本机 API 信息 / 开机自启
// ---------------------------------------------------------------------------

/// 本机接管 API 的端口与 Token（设置 → 网络展示、一键复制）。
///
/// `port == 0` 表示绑定尚未完成（或失败），前端据此显示"启动中/不可用"，
/// 不应把它当成真实端口写入扩展配置。
pub fn api_info() -> Value {
    let port = browser_api::port();
    let url = if port == 0 {
        String::new()
    } else {
        format!("http://127.0.0.1:{port}")
    };
    json!({ "port": port, "url": url, "token": browser_api::token() })
}

/// 浏览器扩展暂存的任务（悬浮确认弹窗的数据源）。
///
/// 前端在 mount 时主动查一次：窗口被销毁后重建、或事件在页面加载前丢失，
/// 都能从这里把挂机期间收到的任务补回来 —— 这是"挂机 20 分钟也不丢"的核心兜底。
pub fn pending_intercept() -> Value {
    browser_api::pending().unwrap_or(Value::Null)
}

/// 用户取消接管：清空暂存（扩展侧早已收到 200 并取消了浏览器下载）。
pub fn cancel_intercept() {
    browser_api::clear_pending();
}

/// 运行期关键目录（设置 → 集成 / 插件页展示用；不在前端写死任何绝对路径）。
pub fn app_paths() -> Value {
    json!({
        "home": paths::home().to_string_lossy(),
        "data": paths::data_dir().to_string_lossy(),
        "plugins": paths::plugins_dir().to_string_lossy(),
        "downloads": configured_download_dir().to_string_lossy(),
    })
}

/// 浏览器扩展目录（设置 → 网络「安装浏览器扩展」引导用）。
///
/// 与 `dist` 一样属于**只读资源**，永远跟随 exe（安装版在 Program Files，
/// 便携版在解压目录）；开发期 `target\debug` 下没有，回退到 HOME（工程根）。
/// `exists=false` 时前端提示用户重新安装完整包。
pub fn browser_extension_info() -> Value {
    let candidates = [
        paths::app_dir().join("browser-extension"),
        paths::home().join("browser-extension"),
    ];
    let dir = candidates
        .iter()
        .find(|dir| dir.join("manifest.json").is_file())
        .unwrap_or(&candidates[0]);
    json!({
        "path": dir.to_string_lossy(),
        "exists": dir.join("manifest.json").is_file(),
    })
}

/// 注册表自启项：`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`。
#[cfg(windows)]
const AUTOSTART_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// 自启值名（固定，便于卸载/排查）。
#[cfg(windows)]
const AUTOSTART_VALUE: &str = "Aria2Harness";

/// 查询当前用户是否已开启开机自启。
///
/// 只读写 `HKCU`（当前用户），**不需要管理员权限**，也不会污染系统级 Run 键。
#[cfg(windows)]
pub fn autostart_enabled() -> Result<bool, AppError> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu.open_subkey(AUTOSTART_KEY).map_err(|e| {
        AppError::Internal(format!("打开开机自启注册表项失败：{e}"))
    })?;
    match key.get_value::<String, _>(AUTOSTART_VALUE) {
        Ok(_) => Ok(true),
        // 值不存在 = 未开启（正常态，不是错误）。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(AppError::Internal(format!(
            "读取开机自启注册表值失败：{e}"
        ))),
    }
}

/// 开启 / 关闭开机自启。
///
/// 写入的是**当前可执行文件**路径（带引号，兼容含空格路径）。
/// 开发模式下 `current_exe()` 指向 `target\debug\aria2-harness.exe`，
/// 这是预期行为：发布包安装后重新开关一次即可指向正式程序。
#[cfg(windows)]
pub fn set_autostart(enabled: bool) -> Result<(), AppError> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey(AUTOSTART_KEY)
        .map_err(|e| AppError::Internal(format!("打开开机自启注册表项失败：{e}")))?;
    if enabled {
        let exe = std::env::current_exe()
            .map_err(|e| AppError::Internal(format!("获取当前程序路径失败：{e}")))?;
        let value = format!("\"{}\"", exe.display());
        key.set_value(AUTOSTART_VALUE, &value)
            .map_err(|e| AppError::Internal(format!("写入开机自启注册表失败：{e}")))?;
        tracing::info!("已开启开机自启：{value}");
    } else {
        match key.delete_value(AUTOSTART_VALUE) {
            Ok(()) => tracing::info!("已关闭开机自启"),
            // 值本来就不存在：关闭操作视为成功（幂等）。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(AppError::Internal(format!(
                    "删除开机自启注册表值失败：{e}"
                )));
            }
        }
    }
    Ok(())
}

/// 非 Windows 平台没有 Run 注册表键（宿主当前只发行 Windows 版）。
#[cfg(not(windows))]
pub fn autostart_enabled() -> Result<bool, AppError> {
    Ok(false)
}

#[cfg(not(windows))]
pub fn set_autostart(_enabled: bool) -> Result<(), AppError> {
    Err(AppError::Internal(
        "开机自启仅在 Windows 平台可用".to_string(),
    ))
}

/// 调起系统原生「选择文件夹」对话框（设置页的默认下载目录）。
///
/// 红线：
/// - 对话框是**模态交互**，会一直阻塞到用户点击，因此必须放进 `spawn_blocking`，
///   绝不能占用 tokio 事件循环；
/// - COM 的 `CoInitializeEx` / `CoUninitialize` 在同一阻塞线程内成对出现；
/// - 用户取消返回 `Ok(None)`，与真正的失败（`Err`）区分开，前端无需弹错。
#[cfg(windows)]
pub async fn pick_directory() -> Result<Option<String>, AppError> {
    tokio::task::spawn_blocking(pick_directory_blocking)
        .await
        .map_err(|e| AppError::Internal(format!("目录选择线程异常退出：{e}")))?
}

#[cfg(windows)]
fn pick_directory_blocking() -> Result<Option<String>, AppError> {
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize,
    };
    use windows::Win32::UI::Shell::{
        FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
    };

    /// `HRESULT_FROM_WIN32(ERROR_CANCELLED)`：用户在对话框里点了「取消」。
    const ERROR_CANCELLED_HRESULT: i32 = 0x8007_04C7u32 as i32;

    // 对话框要求 STA。若该阻塞线程已被当作 MTA 初始化（RPC_E_CHANGED_MODE），
    // 继续执行即可 —— COM 会走代理，功能不受影响。
    let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();

    let result = (|| -> Result<Option<String>, AppError> {
        unsafe {
            let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL)
                .map_err(|e| AppError::Internal(format!("创建目录选择器失败：{e}")))?;
            dialog
                .SetOptions(FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
                .map_err(|e| AppError::Internal(format!("配置目录选择器失败：{e}")))?;

            if let Err(err) = dialog.Show(None) {
                if err.code().0 == ERROR_CANCELLED_HRESULT {
                    return Ok(None);
                }
                return Err(AppError::Internal(format!("目录选择器异常：{err}")));
            }

            let item = dialog
                .GetResult()
                .map_err(|e| AppError::Internal(format!("读取所选目录失败：{e}")))?;
            let raw = item
                .GetDisplayName(SIGDN_FILESYSPATH)
                .map_err(|e| AppError::Internal(format!("解析目录路径失败：{e}")))?;
            // 该字符串由 COM 任务分配器持有，读完必须立刻释放。
            let path = wide_ptr_to_string(raw);
            CoTaskMemFree(Some(raw.0 as *const core::ffi::c_void));
            if path.is_empty() {
                Ok(None)
            } else {
                Ok(Some(path))
            }
        }
    })();

    if initialized {
        unsafe { CoUninitialize() };
    }
    result
}

/// 读取 NUL 结尾的宽字符串（COM 返回的 `PWSTR`）。
///
/// # Safety
/// `ptr` 必须指向以 NUL 结尾的 UTF-16 缓冲区，且其生命周期覆盖本次调用。
#[cfg(windows)]
unsafe fn wide_ptr_to_string(ptr: windows::core::PWSTR) -> String {
    if ptr.0.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while unsafe { *ptr.0.add(len) } != 0 {
        len += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr.0, len) })
}

/// 非 Windows 平台没有 Shell 文件夹选择器（宿主当前只发行 Windows 版）。
#[cfg(not(windows))]
pub async fn pick_directory() -> Result<Option<String>, AppError> {
    Err(AppError::Internal(
        "目录选择器仅在 Windows 平台可用".to_string(),
    ))
}

// ---------------------------------------------------------------------------
// 启动 / 退出流程
// ---------------------------------------------------------------------------

/// 进程级启动：初始化日志与全局槽位 → 注入宿主 → 加载插件 → 启动后台服务引导。
///
/// 由外壳（`main.rs`）在窗口与 WebView 创建后调用一次。
pub fn bootstrap(host: Arc<dyn HostContext>) {
    // 可观测性底座：必须早于任何事件源初始化，否则早期日志会丢失。
    metrics::init_tracing();

    // 系统通知：注册 AUMID（幂等，仅 HKCU），否则后台 Toast 会被系统静默丢弃。
    notify::init();

    // 增量 28（P2-B）：任务历史库（建目录 → 开库 → WAL → 版本化迁移）。
    // 打开/迁移失败**一律降级**（槽位存 `None`）—— 存历史失败绝不能阻断下载主链路，
    // 历史命令会返回 `DB` 码提示用户。
    let db_path = db::db_path();
    let db_slot = match db::Db::open(&db_path) {
        Ok(database) => {
            tracing::info!(path = %db_path, "任务历史库已就绪");
            Some(database)
        }
        Err(err) => {
            tracing::warn!(path = %db_path, "任务历史库不可用，历史功能降级：{err}");
            None
        }
    };
    let _ = DB.set(db_slot);

    // 全局槽位（幂等）
    let _ = STATE.set(Aria2State::new());
    let _ = INTERCEPTOR.set(plugin::interceptor::Interceptor::default());
    host::install_host(host.clone());

    // 插件系统：先加载所有插件（快速），再拉起引擎。
    let bridge = plugin::hostapi::spawn_worker(host.clone());
    let manager = plugin::PluginManager::load_all(&host, bridge);
    manager.notify_app_start();
    let _ = PLUGINS.set(manager);

    // 增量 21：启动插件目录监听（独立 std 线程 + 300ms 防抖），
    // 支持新增插件 / 改 main.js / 改 manifest 后无需重启即生效。
    plugin::manager::watch_plugins(host.clone());

    // 增量 29（P1-G）：Tracker 列表定时更新（每日一次，落地 data/tracker.json 节流）。
    // 与引擎连接解耦：调度循环自检"是否到期 + 引擎是否就绪"，就绪才执行自拉 + 注入。
    rt().spawn(tracker::run_scheduler());

    // 浏览器扩展接管 API：仅回环绑定，独立 tokio 任务，绝不阻塞 UI / IPC 主链路。
    browser_api::start();

    // 增量 40：注册系统休眠/网络变化通知（回调只投递事件，动作在消费任务里执行）。
    #[cfg(windows)]
    power::start();

    // 增量 23：引擎不再由 UI 拉起 —— 改为向核心服务（aria2-service）握手，
    // 由服务拉起 aria2c 并回传 rpc-secret；随后在同一任务里启动 WS 循环。
    rt().spawn(bootstrap_service());

    // 增量 46：自动更新检查（延迟 5s、单次、失败静默），只影响提示，不影响主链路。
    update_check::spawn_check();
}

/// 优雅退出：停监听 → 插件收敛 → RPC 落盘 → 通知核心服务收尾。
///
/// 由外壳在事件循环结束后调用一次。
pub fn shutdown() {
    // 0) 注销系统通知，避免回调在退出期继续投递事件。
    #[cfg(windows)]
    power::stop();

    // 停止插件目录监听（幂等），再让插件优雅退出（先给 onShutdown 机会，再有界等待收敛）
    plugin::manager::stop_watching();
    if let Some(manager) = PLUGINS.get() {
        manager.shutdown();
    }

    // 优雅停机：先 RPC 落盘（saveSession）+ 有序关闭（shutdown），再留出落盘余量。
    // 直接 kill 会强杀进程导致 session 丢失（--save-session-interval 往往来不及触发）。
    rt().block_on(async {
        let client = state().client.lock().await.clone();
        if let Some(client) = client {
            // 先关掉自动重连：退出流程里引擎被关掉后，不该再拉起重连循环。
            client.stop_reconnecting();
            // saveSession 是同步写盘的：RPC 返回时文件已经写好，因此这一步成功后基本不会丢。
            let graceful = async {
                metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
                if let Err(err) = client.save_session().await {
                    metrics::METRICS.rpc_errors.fetch_add(1, Ordering::Relaxed);
                    eprintln!("[aria2] saveSession 失败：{err}");
                }
                metrics::METRICS.rpc_calls.fetch_add(1, Ordering::Relaxed);
                if let Err(err) = client.shutdown().await {
                    metrics::METRICS.rpc_errors.fetch_add(1, Ordering::Relaxed);
                    eprintln!("[aria2] shutdown 失败：{err}");
                }
            };
            // 整体限时 1.5s，避免引擎无响应时卡死退出流程
            if tokio::time::timeout(Duration::from_millis(1500), graceful)
                .await
                .is_err()
            {
                eprintln!("[aria2] 优雅停机超时，进入兜底流程");
            }
        }
        // 给引擎一点时间完成进程收尾（含响应 shutdown 后的资源释放）。
        // 注意：aria2.shutdown 走的是 TimedHaltCommand，**固定延迟 3s** 才真正停机
        // （RpcMethodImpl.cc:1357 `goingShutdown`），远超 2s 退出预算，因此这里
        // 只留极短窗口，真正的兜底由核心服务的强杀负责（见 aria2_service.rs）。
        tokio::time::sleep(Duration::from_millis(120)).await;
    });

    // 兜底：UI 已不持有子进程句柄，故改为通知核心服务收尾（服务侧幂等：
    // 先等引擎自行退出，超时才强杀，并让服务自身退出）。
    rt().block_on(async {
        match service_client::request(&ServiceRequest::Shutdown).await {
            Ok(_) => println!("[service] 已通知核心服务关闭引擎"),
            Err(err) => eprintln!("[service] 通知核心服务关闭失败（忽略）：{err}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{base64_encode, last_path_segment};

    /// RFC 4648 标准向量：覆盖 1/2/3 字节（含 `=` 与 `==` 两种填充）。
    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    /// 路径末段：Windows 反斜杠 / POSIX 斜杠 / 混合分隔符。
    #[test]
    fn last_segment_handles_both_separators() {
        assert_eq!(last_path_segment(r"C:\downloads\a.zip").as_deref(), Some("a.zip"));
        assert_eq!(last_path_segment("/tmp/a.zip").as_deref(), Some("a.zip"));
        assert_eq!(last_path_segment(r"E:\mixed/a.zip").as_deref(), Some("a.zip"));
        assert_eq!(last_path_segment(""), None);
        assert_eq!(last_path_segment(r"C:\downloads\"), Some("downloads".to_string()));
    }
}
