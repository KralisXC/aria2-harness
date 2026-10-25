// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! 手写 tao + wry（WebView2）外壳：替代原 Tauri 壳，驱动既有 Vue 前端。
//!
//! - 窗口：tao `Window`；WebView：wry `WebViewBuilder`（加载 Vite dev server :1420）。
//! - IPC：wry `with_ipc_handler` 收 `{"id","cmd","args"}` → 路由分发业务函数 →
//!   `evaluate_script("window.__ipcResolve(id, result)")` 回传；错误以
//!   `{"__err": {"code": "...", "message": "..."}}` 标记（`AppError` 手写 `Serialize`）。
//! - 事件：宿主 `emit` 经 tao `EventLoopProxy` 投递自定义事件，主循环在主线程内
//!   `evaluate_script("window.__ipcEmit(event, payload)")` —— 避免跨线程操作 WebView。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aria2_harness_lib as lib;
use aria2_harness_lib::error::AppError;
use aria2_harness_lib::host::HostContext;
use serde_json::{json, Value};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::platform::run_return::EventLoopExtRunReturn;
#[cfg(windows)]
use tao::platform::windows::WindowExtWindows;
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
use tao::window::WindowBuilder;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use wry::{WebViewBuilder, WebViewBuilderExtWindows};

/// Vite dev server 地址（端口固定 1420，见 vite.config.ts 的 strictPort）。
/// **仅 Debug 构建使用**；Release 走 `web_ui` 内置静态服务器托管 exe 同级 `dist/`。
const DEV_URL: &str = "http://localhost:1420";

/// 选择前端入口：Debug = Vite（带 HMR）；Release = 内置静态服务器（便携包双击即用）。
///
/// 结果**只计算一次**：窗口销毁后重建会再次请求入口，而 `web_ui::start()` 每次都绑定
/// 新端口并新起一个线程 —— 缓存后重建窗口复用同一个本地服务器，避免端口/线程泄漏。
fn frontend_url() -> String {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| {
        #[cfg(debug_assertions)]
        {
            DEV_URL.to_string()
        }
        #[cfg(not(debug_assertions))]
        {
            aria2_harness_lib::web_ui::start().unwrap_or_else(|| {
                // 便携包缺少 dist/ 时仍尝试 dev server，便于排障（正常不会走到这里）。
                tracing::error!("未能托管本地前端资源，回退到 {DEV_URL}");
                DEV_URL.to_string()
            })
        }
    })
    .clone()
}

/// 窗口操作（必须在主线程执行：tao `Window` 不是 `Send`）。
#[derive(Clone, Copy)]
enum WindowAction {
    Minimize,
    Maximize,
    Close,
    /// 进入系统原生拖拽循环（`-webkit-app-region` 在 WebView2 下不生效时的兜底）。
    Drag,
}

/// 自定义用户事件：由任意线程投递到主循环，在主线程内操作 WebView。
enum UiEvent {
    /// 向前端派发事件（事件名 + 载荷）。
    Emit(String, Value),
    /// 回传一次 IPC 调用结果（请求 id + 结果；错误以 `{"__err": {code,message}}` 标记）。
    Resolve(u64, Value),
    /// 无边框窗口控制（请求 id + 动作），由主循环直接操作 `Window`。
    Window(u64, WindowAction),
    /// 托盘菜单动作（菜单项 id），同样只能在主线程操作窗口。
    Tray(String),
    /// 唤起主窗口（托盘左键 / 菜单 / 二次启动唤醒事件共用）：
    /// 窗口还在就聚焦，已被销毁就重建 —— 一律只在主线程执行。
    Show,
    /// 新窗口页面加载完成：此时才把窗口显示出来并聚焦，避免 WebView2 首帧白底闪烁。
    PageLoaded,
    /// 空闲巡检（每 30s 一次）：判断是否需要挂起 WebView2 渲染。
    IdleCheck,
    /// 空闲巡检结论：`true` = 无活跃任务、可以挂起。
    IdleSuspend(bool),
}

// ---------------------------------------------------------------------------
// 空闲判定（第三层：WebView2 级挂起）
// ---------------------------------------------------------------------------

/// 无操作多久后允许挂起渲染。只在**窗口失焦**时生效（见 `UiEvent::IdleCheck`），
/// 保证用户一回到窗口就能通过焦点事件立刻恢复，不会出现"看得见却已冻结"的界面。
const IDLE_SUSPEND_AFTER: Duration = Duration::from_secs(300);
/// 空闲巡检周期。线程只是 sleep，醒来做一次零成本判断，CPU 开销可忽略。
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// 最近一次用户操作（收到任意 IPC 请求）的时间戳（epoch 毫秒）。
/// 前端在无活跃任务时会自行停表，因此"长时间无 IPC"≈"用户没有在操作本应用"。
static LAST_ACTIVITY_MS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 记录一次用户操作（由 IPC 入口调用）。
fn touch_activity() {
    LAST_ACTIVITY_MS.store(now_ms(), Ordering::Relaxed);
}

/// 距上次用户操作的时长；时间戳异常（未初始化 / 时钟回拨）时按"刚刚操作过"处理。
fn idle_for() -> Duration {
    let last = LAST_ACTIVITY_MS.load(Ordering::Relaxed);
    let now = now_ms();
    if last == 0 || now <= last {
        Duration::ZERO
    } else {
        Duration::from_millis(now - last)
    }
}

/// WebView2 级挂起 / 恢复：`wry::WebView::set_visible` 在 Windows 下直接调用
/// `ICoreWebView2Controller::SetIsVisible`，比"隐藏窗口"更彻底 —— 渲染进程会停止
/// 合成与绘制，而窗口与消息泵仍在，恢复时无需重建页面。
fn set_webview_suspended(webview: Option<&wry::WebView>, suspended: bool) {
    let Some(webview) = webview else { return };
    if let Err(err) = webview.set_visible(!suspended) {
        tracing::warn!("切换 WebView 可见性失败（suspended={suspended}）：{err}");
    }
}

/// 后台空闲巡检线程：每 30s 向主循环投递一次 `IdleCheck`。
/// 事件循环退出后 `send_event` 失败，线程随之结束（不泄漏）。
fn spawn_idle_monitor(proxy: EventLoopProxy<UiEvent>) {
    let spawned = std::thread::Builder::new()
        .name("harness-idle".to_string())
        .spawn(move || loop {
            std::thread::sleep(IDLE_CHECK_INTERVAL);
            if proxy.send_event(UiEvent::IdleCheck).is_err() {
                break;
            }
        });
    if let Err(err) = spawned {
        tracing::error!("空闲巡检线程启动失败：{err}");
    }
}

/// IPC 命令名 -> 窗口动作。`None` 表示走常规业务路由。
fn window_action(cmd: &str) -> Option<WindowAction> {
    match cmd {
        "window-minimize" => Some(WindowAction::Minimize),
        "window-maximize" => Some(WindowAction::Maximize),
        "window-close" => Some(WindowAction::Close),
        "window-drag" => Some(WindowAction::Drag),
        _ => None,
    }
}

/// 退出前的白块兜底：WebView2 销毁前若仍可见，会先露出未渲染的白底 + 系统粗边框。
/// 顺序：强制纯黑 → 隐藏 WebView 与窗口 → 恢复普通尺寸。
fn prepare_exit(window: Option<&tao::window::Window>, webview: Option<&wry::WebView>) {
    if let Some(webview) = webview {
        if let Err(err) = webview.set_background_color((0, 0, 0, 255)) {
            tracing::warn!("退出前设置 WebView 底色失败：{err}");
        }
        if let Err(err) = webview.set_visible(false) {
            tracing::warn!("退出前隐藏 WebView 失败：{err}");
        }
    }
    if let Some(window) = window {
        if window.is_maximized() {
            window.set_maximized(false);
        }
        window.set_visible(false);
    }
}

// ---------------------------------------------------------------------------
// 主窗口生命周期：关闭即销毁（释放 WebView2 进程），唤醒即重建
// ---------------------------------------------------------------------------
//
// 为什么不再"隐藏到托盘"：窗口即使物理移出屏幕，WebView2 的渲染进程仍可能被系统
// 降级/冻结，唤回时露出白屏。Motrix 的做法更彻底 —— 关闭就销毁窗口与 WebView，
// 把 WebView2 渲染进程的内存完全还给系统；托盘唤醒时重建一个全新窗口，天然不可能
// 白屏。代价是每次唤醒要重新加载一次前端（本地静态服务器，实测百毫秒级）。

/// 上次关闭时记下的窗口几何：重建时原位还原。
#[derive(Clone, Copy, Default)]
struct WindowState {
    /// 关闭前窗口位置；读不到时回退系统默认位置。
    pos: Option<(i32, i32)>,
    /// 关闭前是否最大化（最大化时位置无意义，重建后直接最大化）。
    was_maximized: bool,
}

/// WebView2 浏览器参数。
///
/// wry 会显式设置 `AdditionalBrowserArguments`，而 WebView2 在显式设置该选项时会
/// 忽略 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` 环境变量，因此必须从 builder 侧
/// 无条件下发（环境变量只作双保险）。开发期可用 `ARIA2_HARNESS_WEBVIEW_ARGS` 追加。
fn webview_browser_args() -> String {
    // 刻意**不**带 `--disable-background-timer-throttling` / `--disable-renderer-backgrounding`
    // / `--disable-backgrounding-occluded-windows`：这三项会让窗口隐藏或失焦后继续全速渲染，
    // 与"空闲即休眠"冲突。需要排障时用 `ARIA2_HARNESS_WEBVIEW_ARGS` 临时加回。
    let mut args = String::from(
        "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
         --autoplay-policy=no-user-gesture-required",
    );
    let extra = std::env::var("ARIA2_HARNESS_WEBVIEW_ARGS").unwrap_or_default();
    let extra = extra.trim();
    if !extra.is_empty() {
        args.push(' ');
        args.push_str(extra);
    }
    args
}

/// 内置 WebView2 Fixed Version 的目录名：便携包与安装版同名，都在 exe 同级。
const WEBVIEW2_RUNTIME_DIR: &str = "WebView2Runtime";
/// WebView2 官方环境变量：指向含 `msedgewebview2.exe` 的 Fixed Version 根目录。
const WEBVIEW2_FOLDER_ENV: &str = "WEBVIEW2_BROWSER_EXECUTABLE_FOLDER";

/// 在候选根目录里定位真正含 `msedgewebview2.exe` 的那一层。
///
/// 官方 Fixed Version 压缩包解压后会多一层版本目录
/// （`Microsoft.WebView2.FixedVersionRuntime.<版本>.x64`），而打包时我们把它摊平到
/// `WebView2Runtime` 下；两种布局都要认，故「根目录 → 一层子目录」都查。
fn locate_webview2_runtime(base: &Path) -> Option<PathBuf> {
    if base.join("msedgewebview2.exe").is_file() {
        return Some(base.to_path_buf());
    }
    for entry in std::fs::read_dir(base).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() && path.join("msedgewebview2.exe").is_file() {
            return Some(path);
        }
    }
    None
}

/// 绑定内置 WebView2 Fixed Version；找不到时返回 `None`（回退系统 WebView2）。
///
/// 必须在任何 WebView2 组件创建**之前**调用：WebView2Loader 只在首次
/// `CreateCoreWebView2EnvironmentWithOptions` 时读取该环境变量。
/// 之所以放在 `main` 最早期而不是建窗前：edition 2024 里 `set_var` 是 unsafe，
/// 其安全前提是"没有其他线程并发读写环境变量"，越早调用越干净。
fn bind_fixed_webview2() -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // 便携版与安装版同构：{exe 同级}\WebView2Runtime。
            roots.push(dir.join(WEBVIEW2_RUNTIME_DIR));
        }
    }
    // 安装版兜底：安装目录默认为 {ProgramFiles}\Aria2-Harness
    // （自定义路径由上面 exe 同级分支覆盖）。ProgramFiles 从环境取值，不写死盘符。
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        roots.push(PathBuf::from(program_files).join("Aria2-Harness").join(WEBVIEW2_RUNTIME_DIR));
    }
    // 开发期：直接读仓库内的解压目录（正式包没有该路径，自然跳过）。
    #[cfg(debug_assertions)]
    roots.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join(WEBVIEW2_RUNTIME_DIR),
    );

    for root in roots {
        if let Some(runtime) = locate_webview2_runtime(&root) {
            // 安全：调用点在 main 最早期，尚未创建任何线程，不存在并发读写环境变量的竞争。
            unsafe { std::env::set_var(WEBVIEW2_FOLDER_ENV, &runtime) };
            return Some(runtime);
        }
    }
    None
}

/// 弹出原生错误对话框。
///
/// Release 是 `windows_subsystem = "windows"`，没有控制台，`eprintln` 与 `tracing`
/// 都到不了用户眼前；闪退时必须靠这一层把原因"摆到桌面上"，用户才能截图反馈。
fn show_error(title: &str, message: &str) {
    #[cfg(windows)]
    {
        use windows::core::HSTRING;
        // 安全：MessageBoxW 只读传入的宽字符串，且 HSTRING 在调用期间保持存活。
        unsafe {
            MessageBoxW(
                None,
                &HSTRING::from(message),
                &HSTRING::from(title),
                MB_OK | MB_ICONERROR,
            );
        }
    }
    #[cfg(not(windows))]
    eprintln!("[{title}] {message}");
}

/// 全局 panic 兜底：任何线程 panic 都弹框展示文件、行号与原因。
///
/// 必须作为 `main` 的第一条语句安装 —— 否则 DPI、单实例、WebView2 绑定等早期步骤
/// 一旦 panic，进程仍是"静默消失"，用户只能看到窗口一闪。
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "未知 panic（无文本负载）".to_string());
        let location = info
            .location()
            .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()))
            .unwrap_or_else(|| "未知位置".to_string());
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();

        eprintln!("[panic] {location} ({thread}) {payload}");
        show_error(
            "Aria2-Harness 发生严重错误",
            &format!(
                "线程：{thread}\n位置：{location}\n\n{payload}\n\n\
                 请把此窗口截图反馈给开发者。"
            ),
        );
    }));
}

/// WebView2 用户数据目录：显式落到可写位置并提前建好。
///
/// 系统默认会把数据目录建在 exe 同级；安装版位于 `Program Files`，普通用户不可写，
/// 环境创建直接失败。`create_dir_all` 必须显式执行：WebView2 不会替我们建多级目录。
fn webview2_data_dir() -> PathBuf {
    let dir = lib::paths::webview2_data_dir();
    if let Err(err) = std::fs::create_dir_all(&dir) {
        show_error(
            "Aria2-Harness 启动失败",
            &format!("无法创建 WebView2 数据目录：\n{}\n\n{err}", dir.display()),
        );
    }
    dir
}

/// 构建主窗口 + WebView（整窗全新创建，可在事件循环内反复调用）。
///
/// 必须在主线程执行：`tao::Window` / `wry::WebView` 都不是 `Send`，状态只能由调用方
/// 以主线程局部变量的形式持有（`Option<Window>` / `Option<WebView>`）—— 不能塞进
/// `Arc<Mutex<...>>`。
///
/// 窗口先以不可见状态创建，等 `PageLoaded` 再显示：WebView2 首帧未绘制时是纯白，
/// 直接显示会闪一下白底。同时把 WebView 底色设成与页面一致的深色，双保险。
fn create_main_window(
    target: &EventLoopWindowTarget<UiEvent>,
    proxy: &EventLoopProxy<UiEvent>,
    req_tx: &UnboundedSender<IpcRequest>,
    state: WindowState,
) -> (tao::window::Window, wry::WebView) {
    let mut window_builder = WindowBuilder::new()
        .with_title(WINDOW_TITLE)
        .with_inner_size(tao::dpi::LogicalSize::new(1000.0, 720.0))
        // 窗口 + 任务栏图标：与托盘同源。
        .with_window_icon(window_icon())
        // L3 表现层：无边框 + 可缩放。透明合成在 Win11 下会与 DWM 原生圆角冲突
        // 产生白边，因此采用方案 A：关闭透明，背景交给页面纯色，圆角交给系统。
        .with_decorations(false)
        .with_transparent(false)
        .with_resizable(true)
        // 先隐藏，等页面加载完成再显示（见 `UiEvent::PageLoaded`）。
        .with_visible(false);
    if state.was_maximized {
        window_builder = window_builder.with_maximized(true);
    } else if let Some((x, y)) = state.pos {
        window_builder = window_builder.with_position(tao::dpi::PhysicalPosition::new(x, y));
    }
    let window = window_builder.build(target).expect("创建窗口失败");

    // 图标装配分两层，顺序不可颠倒：
    //   1) exe 资源段（`assets/app.rc` 由 build.rs 用 windres 编译）—— 任务栏 / Alt-Tab /
    //      资源管理器读的是这一份，且是按 `SM_CXICON` 取原生尺寸，不会把 256px 大图硬缩放；
    //   2) tao 运行期图标 —— 仅当构建机没有 windres、资源段为空时的兜底。
    #[cfg(windows)]
    if !apply_native_icons(&window) {
        tracing::warn!("exe 资源段未取到图标，回退到 tao 运行期窗口图标");
        window.set_taskbar_icon(window_icon());
    }

    let page_proxy = proxy.clone();
    let ipc_tx = req_tx.clone();
    // 用户数据目录随 WebContext 显式传给 wry（等价于 WebView2 的 user data folder）：
    // 绝不能让 WebView2 自己选到 Program Files 下的 exe 同级目录。
    let data_dir = webview2_data_dir();
    tracing::info!("[webview2] 用户数据目录：{}", data_dir.display());
    let mut web_context = wry::WebContext::new(Some(data_dir));
    let webview = WebViewBuilder::new_with_web_context(&mut web_context)
        .with_url(frontend_url())
        // 与窗口一致：关闭 WebView 透明，避免与 DWM 圆角合成出现白边。
        .with_transparent(false)
        .with_additional_browser_args(&webview_browser_args())
        // 页面加载完成 → 回主线程显示窗口（避免首帧白底闪烁）。
        .with_on_page_load_handler(move |event, _url| {
            if matches!(event, wry::PageLoadEvent::Finished) {
                let _ = page_proxy.send_event(UiEvent::PageLoaded);
            }
        })
        .with_ipc_handler(move |req: wry::http::Request<String>| {
            // 任意 IPC 请求都视为「用户在操作」：前端空闲时会停表，因此这条时间戳
            // 是 5 分钟无操作挂起判定的唯一依据。
            touch_activity();
            match serde_json::from_str::<Value>(req.body()) {
                Ok(value) => {
                    let id = value.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                    let cmd = value
                        .get("cmd")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let args = value.get("args").cloned().unwrap_or(Value::Null);
                    // 关键路径：路由任务若已退出，IPC 请求会静默丢失（前端永远等不到结果）。
                    if let Err(err) = ipc_tx.send(IpcRequest { id, cmd, args }) {
                        tracing::error!("IPC 请求投递失败（路由任务已退出）：{err}");
                    }
                }
                Err(e) => eprintln!("[ipc] 请求解析失败：{e}"),
            }
        })
        .build(&window);
    let webview = match webview {
        Ok(webview) => webview,
        Err(err) => {
            // 绝不允许静默 unwrap：把错误原样展示出来，用户截图即可定位。
            let runtime = std::env::var(WEBVIEW2_FOLDER_ENV)
                .unwrap_or_else(|_| "（未绑定，使用系统 WebView2）".to_string());
            show_error(
                "WebView2 初始化失败",
                &format!(
                    "WebView2 初始化失败：{err}\n\n\
                     浏览器运行时：{runtime}\n\
                     用户数据目录：{}\n\n\
                     请把此窗口截图反馈给开发者。",
                    webview2_data_dir().display()
                ),
            );
            std::process::exit(1);
        }
    };

    // 首帧底色与默认深色主题一致（#0d1517）：即便首帧尚未绘制，也不会露出刺眼白底。
    if let Err(err) = webview.set_background_color((13, 21, 23, 255)) {
        tracing::warn!("设置 WebView 底色失败：{err}");
    }

    (window, webview)
}

/// 显示主窗口：窗口还在就聚焦，已被销毁就重建。
///
/// 所有唤回入口（托盘左键、托盘菜单、二次启动唤醒事件）都收敛到这里。
fn show_main_window(
    target: &EventLoopWindowTarget<UiEvent>,
    proxy: &EventLoopProxy<UiEvent>,
    req_tx: &UnboundedSender<IpcRequest>,
    state: WindowState,
    alive: &AtomicBool,
    window: &mut Option<tao::window::Window>,
    webview: &mut Option<wry::WebView>,
    source: &str,
) {
    if let Some(existing) = window.as_ref() {
        if existing.is_minimized() {
            existing.set_minimized(false);
        }
        if !existing.is_visible() {
            existing.set_visible(true);
        }
        // 唤回时同步恢复 WebView 渲染（可能此前被最小化 / 空闲挂起过）。
        set_webview_suspended(webview.as_ref(), false);
        existing.set_focus();
        return;
    }

    tracing::info!(
        "重建主窗口 source={source} pos={:?} maximized={}",
        state.pos,
        state.was_maximized
    );
    let (new_window, new_webview) = create_main_window(target, proxy, req_tx, state);
    *window = Some(new_window);
    *webview = Some(new_webview);

    // 前端回来了：恢复 UI 内提示（不再弹系统 Toast）。
    alive.store(true, Ordering::Relaxed);
    lib::set_window_hidden(false);

    // 兜底显示：静态资源缺失 / 加载失败时 `PageLoaded` 可能永远不来，
    // 1.2s 后无条件补一次（已显示时是幂等的聚焦操作）。
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1200));
        let _ = proxy.send_event(UiEvent::PageLoaded);
    });
}

/// 关闭到托盘：**销毁**窗口与 WebView，只留下进程在后台跑任务。
///
/// `tao` 的 `Window::drop` 会向窗口线程投递销毁消息，由消息循环真正 `DestroyWindow`；
/// 因此这里只需把 `Option` 置空，窗口与 WebView2 渲染进程都会被系统回收（实测内存
/// 大幅下降）。位置与最大化态先存进 `state`，唤醒重建时原位还原。
fn destroy_main_window(
    state: &mut WindowState,
    alive: &AtomicBool,
    window: &mut Option<tao::window::Window>,
    webview: &mut Option<wry::WebView>,
) {
    let Some(existing) = window.as_ref() else { return };
    state.was_maximized = existing.is_maximized();
    state.pos = existing.outer_position().ok().map(|p| (p.x, p.y));

    // 后台态：完成 / 失败改走系统原生 Toast；前端已随页面销毁，宿主按"无 UI"处理。
    lib::set_window_hidden(true);
    alive.store(false, Ordering::Relaxed);

    // 顺序不能反：先关 WebView2 控制器（释放渲染进程与内存），再销毁宿主窗口。
    drop(webview.take());
    drop(window.take());
    tracing::info!("窗口已关闭并销毁（WebView2 进程释放，下载任务继续在后台运行）");
}

// ---------------------------------------------------------------------------
// 应用图标：编译期嵌入 ICO，窗口 / 任务栏 / 托盘同源
// ---------------------------------------------------------------------------

/// 应用图标（窗口 / 任务栏 / 托盘 / exe 资源段**同一份文件**）。
///
/// `assets/app.ico` 由 `build.rs` 用 windres 编进 exe 资源段（`assets/app.rc`），
/// 运行期 tao / tray-icon 也从这里解码 —— 单一来源才能保证四处视觉完全一致。
/// 内含 16/20/24/32/40/48/64/128/256 九个尺寸，`decode_ico` 按目标尺寸挑最接近的一条。
#[cfg(windows)]
const APP_ICON: &[u8] = include_bytes!("../assets/app.ico");

/// 从 ICO 字节解出**最接近 `target` 尺寸**条目的 RGBA 位图。
///
/// 刻意不再一律取 256×256：tao / tray-icon 会直接把 RGBA 做成 HICON，交给系统把
/// 256 像素的大图缩到任务栏（32px）或通知区（16~24px）时会糊成一团白色色块。
#[cfg(windows)]
fn decode_ico(bytes: &[u8], target: u32) -> Option<(Vec<u8>, u32, u32)> {
    let dir = ico::IconDir::read(std::io::Cursor::new(bytes)).ok()?;
    let entry = dir
        .entries()
        .iter()
        .min_by_key(|entry| entry.width().abs_diff(target))?;
    let image = entry.decode().ok()?;
    Some((image.rgba_data().to_vec(), image.width(), image.height()))
}

/// 窗口图标（tao 运行期路径）——取最接近 32px 的条目。
#[cfg(windows)]
fn window_icon() -> Option<tao::window::Icon> {
    let (rgba, width, height) = decode_ico(APP_ICON, 32)?;
    match tao::window::Icon::from_rgba(rgba, width, height) {
        Ok(icon) => Some(icon),
        Err(err) => {
            tracing::error!("窗口图标数据非法，降级为系统默认图标：{err}");
            None
        }
    }
}

/// 托盘图标（tray-icon）——取 32 原生尺寸。
#[cfg(windows)]
fn build_tray_icon() -> Option<tray_icon::Icon> {
    let (rgba, width, height) = decode_ico(APP_ICON, 32)?;
    match tray_icon::Icon::from_rgba(rgba, width, height) {
        Ok(icon) => Some(icon),
        Err(err) => {
            tracing::error!("托盘图标数据非法：{err}");
            None
        }
    }
}

/// 非 Windows 平台无窗口图标（本项目当前仅发布 Windows 版，保留可编译性）。
#[cfg(not(windows))]
fn window_icon() -> Option<tao::window::Icon> {
    None
}

/// 应用图标在 exe 资源段中的 ID（与 `assets/app.rc` 的 `1 ICON "app.ico"` 对应）。
#[cfg(windows)]
const APP_ICON_RESOURCE_ID: usize = 1;

/// 从**当前 exe 的资源段**按指定像素尺寸取出 HICON。
///
/// 这是 Windows 原生取图标的通路：任务栏、Alt-Tab、资源管理器都优先走它，
/// 且尺寸由 `cx/cy` 精确指定（`LoadImageW` 会在 ICO 里挑最接近的那一条），
/// 从而绕开 `CreateIconIndirect` 把 256×256 位图交给系统成缩放的失真路径。
#[cfg(windows)]
fn resource_icon(size: i32) -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
    use windows::core::PCWSTR;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{LoadImageW, HICON, IMAGE_ICON, LR_DEFAULTCOLOR};

    // 资源 ID 是序数而不是字符串名，按 MAKEINTRESOURCEW 约定把整数直接当地址传。
    let name = PCWSTR(APP_ICON_RESOURCE_ID as *const u16);
    let module = unsafe { GetModuleHandleW(None) }.ok()?;
    let handle = unsafe {
        LoadImageW(
            Some(module.into()),
            name,
            IMAGE_ICON,
            size,
            size,
            LR_DEFAULTCOLOR,
        )
    }
    .ok()?;
    Some(HICON(handle.0))
}

/// 把窗口图标（`WM_SETICON` 的 BIG/SMALL）与窗口类图标全部指向 exe 资源段里的原生尺寸图标。
///
/// 返回 `false` 表示资源段里没有图标（例如构建机缺 `windres`），调用方应回退到 tao 运行期图标。
#[cfg(windows)]
fn apply_native_icons(window: &tao::window::Window) -> bool {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SendMessageW, SetClassLongPtrW, GCLP_HICON, GCLP_HICONSM, ICON_BIG,
        ICON_SMALL, SM_CXICON, SM_CXSMICON, WM_SETICON,
    };

    let big_size = match unsafe { GetSystemMetrics(SM_CXICON) } {
        n if n > 0 => n,
        _ => 32,
    };
    let small_size = match unsafe { GetSystemMetrics(SM_CXSMICON) } {
        n if n > 0 => n,
        _ => 16,
    };
    let (Some(big), Some(small)) = (resource_icon(big_size), resource_icon(small_size)) else {
        return false;
    };

    let hwnd = HWND(window.hwnd() as _);
    unsafe {
        SendMessageW(
            hwnd,
            WM_SETICON,
            Some(WPARAM(ICON_BIG as usize)),
            Some(LPARAM(big.0 as isize)),
        );
        SendMessageW(
            hwnd,
            WM_SETICON,
            Some(WPARAM(ICON_SMALL as usize)),
            Some(LPARAM(small.0 as isize)),
        );
        // 类图标是 Explorer 在窗口图标缺失时的兜底来源（实测未设置时 GCLP_HICON 为 0）。
        SetClassLongPtrW(hwnd, GCLP_HICON, big.0 as isize);
        SetClassLongPtrW(hwnd, GCLP_HICONSM, small.0 as isize);
    }
    true
}

/// 创建系统托盘 + 右键菜单，菜单事件统一转投主循环（tao 的 proxy）。
///
/// 菜单触发必须在主线程操作 `Window`，因此这里只做"转发"，
/// 真正的显示 / 暂停 / 退出动作都在 `Event::UserEvent(UiEvent::Tray(_))` 分支执行。
#[cfg(windows)]
fn build_tray(proxy: EventLoopProxy<UiEvent>) -> Option<tray_icon::TrayIcon> {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};

    let show = MenuItem::with_id("show", "显示主界面", true, None);
    let pause_all = MenuItem::with_id("pause_all", "暂停所有任务", true, None);
    let separator = PredefinedMenuItem::separator();
    let quit = MenuItem::with_id("quit", "退出软件", true, None);
    let menu = match Menu::with_items(&[&show, &pause_all, &separator, &quit]) {
        Ok(menu) => menu,
        Err(err) => {
            tracing::error!("创建托盘菜单失败：{err}");
            return None;
        }
    };

    let icon = match build_tray_icon() {
        Some(icon) => icon,
        None => {
            tracing::error!("生成托盘图标失败（RGBA 数据非法）");
            return None;
        }
    };

    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let id = event.id().0.clone();
        let _ = menu_proxy.send_event(UiEvent::Tray(id));
    }));

    // 左键单击直接唤回主界面（菜单只在右键弹出，符合桌面下载器习惯）。
    let click_proxy = proxy.clone();
    tray_icon::TrayIconEvent::set_event_handler(Some(
        move |event: tray_icon::TrayIconEvent| {
            if let tray_icon::TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left,
                button_state: tray_icon::MouseButtonState::Up,
                ..
            } = event
            {
                let _ = click_proxy.send_event(UiEvent::Tray("show".to_string()));
            }
        },
    ));

    match tray_icon::TrayIconBuilder::new()
        .with_tooltip("Aria2-Harness")
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()
    {
        Ok(tray) => Some(tray),
        Err(err) => {
            tracing::error!("创建系统托盘失败（最小化到托盘将退化为普通窗口）：{err}");
            None
        }
    }
}

/// wry/tao 版宿主实现：`emit` 只投递事件，真正的 WebView 操作在主循环内完成。
struct WryHost {
    proxy: EventLoopProxy<UiEvent>,
    alive: Arc<AtomicBool>,
}

impl HostContext for WryHost {
    fn emit(&self, event: &str, payload: Value) {
        // 事件循环已退出时 send_event 会失败，忽略即可（进程即将结束）。
        let _ = self
            .proxy
            .send_event(UiEvent::Emit(event.to_string(), payload));
    }

    fn frontend_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn show_window(&self) {
        // 与托盘 / 二次启动共用同一条主线程通道：最小化则还原，已销毁则重建。
        let _ = self.proxy.send_event(UiEvent::Show);
    }
}

/// 一条待处理的 IPC 请求。
struct IpcRequest {
    id: u64,
    cmd: String,
    args: Value,
}

/// 单条 IPC 命令的执行上界。
///
/// 路由任务是**串行**的：任何一条命令无限期挂起，后面的命令（含 `list_plugins` 这类
/// 纯内存查询）都会一起变哑，前端只能看到"超时"而没有任何错误码。因此在此统一兜底：
/// 上界取 6s —— 既覆盖 `add_download` 最坏路径（插件拦截 3s + RPC），又小于前端 8s
/// 的等待，保证 UI 拿到的是真实错误而不是干等。
const IPC_TIMEOUT: Duration = Duration::from_secs(6);

/// 原生文件夹选择器的等待上界。
///
/// 它是**模态交互**：用户可能在目录树里翻找几分钟，6s 兜底必然误杀。
/// 单独放宽到 5 分钟，超时上限仍然存在（防止 COM 真的挂死堵住串行路由）。
const PICK_DIRECTORY_TIMEOUT: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// 单实例门禁 + 二次启动唤醒：命名事件（窗口销毁后依然存在的 IPC 通道）
// ---------------------------------------------------------------------------

/// 主窗口标题。
const WINDOW_TITLE: &str = "Aria2-Harness";

/// 唤醒事件名：**同时**承担单实例锁与二次启动唤醒通道。
///
/// 为什么不再用「互斥量 + `FindWindowW` + `SendMessageW`」：Motrix 式关闭会把主窗口
/// 彻底销毁，`FindWindowW` 找不到句柄，双击桌面图标就会静默无反应。命名事件是内核
/// 对象，生命周期独立于窗口 —— 窗口没了照样能唤醒。
#[cfg(windows)]
const WAKEUP_EVENT_NAME: &str = "Global\\Aria2Harness_Wakeup_v1";

#[cfg(windows)]
enum SingleInstance {
    /// 本进程是主实例；`wake` 为唤醒事件句柄（创建失败时为 `None`，仅影响二次启动唤醒）。
    Primary {
        wake: Option<windows::Win32::Foundation::HANDLE>,
    },
    /// 已有主实例：唤醒事件已置位、前台权限已移交，本进程应立即退出。
    Secondary,
}

/// 创建命名自动重置事件，用「对象是否已存在」判定单实例。
#[cfg(windows)]
fn acquire_single_instance() -> SingleInstance {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::{
        CreateEventW, OpenEventW, SetEvent, EVENT_MODIFY_STATE,
    };
    use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

    let name = HSTRING::from(WAKEUP_EVENT_NAME);
    let event = match unsafe { CreateEventW(None, false, false, &name) } {
        Ok(handle) => handle,
        Err(err) => {
            // 创建失败（受限令牌等）时放行：宁可多开，也不能让软件彻底无法启动。
            eprintln!("[single-instance] 创建唤醒事件失败，跳过单实例检测：{err}");
            return SingleInstance::Primary { wake: None };
        }
    };

    // 必须紧跟 CreateEventW 读取，中间不能插入其它 Win32 调用（会覆盖 last error）。
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        // 本进程才是"刚拿到用户输入"的进程：先放开前台权限（ASFW_ANY）再置位事件。
        // 顺序不能反 —— 主实例收到唤醒后会立刻抢前台。
        unsafe {
            let _ = AllowSetForegroundWindow(u32::MAX);
            if let Ok(target) = OpenEventW(EVENT_MODIFY_STATE, false, &name) {
                let _ = SetEvent(target);
            }
        }
        return SingleInstance::Secondary;
    }

    // 句柄刻意不关闭：事件对象必须活到进程结束。`HANDLE` 没有 Drop 实现，只要不调
    // CloseHandle 就已"泄漏到进程退出"。
    SingleInstance::Primary { wake: Some(event) }
}

/// 后台线程等待唤醒事件：二次启动置位 → 主线程重建 / 唤起窗口。
#[cfg(windows)]
fn spawn_wake_listener(wake: windows::Win32::Foundation::HANDLE, proxy: EventLoopProxy<UiEvent>) {
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{WaitForSingleObject, INFINITE};

    // `HANDLE` 不是 `Send`：只把句柄值搬进线程（对象所有权仍在主实例，永不关闭）。
    let raw = wake.0 as isize;
    let spawned = std::thread::Builder::new()
        .name("harness-wake".to_string())
        .spawn(move || {
            // SAFETY: 句柄值来自 CreateEventW，且对象需活到进程结束；跨线程只做等待。
            let event = HANDLE(raw as *mut core::ffi::c_void);
            loop {
                if unsafe { WaitForSingleObject(event, INFINITE) } != WAIT_OBJECT_0 {
                    break; // 句柄失效：停止监听，不影响主流程
                }
                if proxy.send_event(UiEvent::Show).is_err() {
                    break; // 事件循环已退出
                }
            }
        });
    if let Err(err) = spawned {
        tracing::error!("唤醒监听线程启动失败：{err}");
    }
}

fn main() {
    // 第一件事：装 panic 钩子。DPI / 单实例 / WebView2 绑定等后续任何一步崩溃，
    // 都能弹出带文件与行号的原生对话框，而不是"双击后无声消失"。
    install_panic_hook();

    // 增量 46：进程级 DPI 感知，必须早于任何窗口 / WebView 创建（含 tao 的 EventLoop 初始化）。
    //
    // `PER_MONITOR_AWARE_V2` 让窗口跨显示器（150% 笔记本 ⇄ 100% 外接屏）时由 DWM 直接
    // 按目标屏重新栅格化，而不是把位图拉伸 → 不再"变糊"。tao 自身也会设置感知级别，
    // 但顺序不保证；这里先设一次，失败（如已被更早的代码锁定）不影响后续逻辑。
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        if let Err(err) =
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
        {
            eprintln!("[dpi] 进程 DPI 感知设置被忽略（通常表示已被更早设置）：{err}");
        }
    }

    // WebView2 后台节流：不再注入 `--disable-background-timer-throttling` 等参数。
    // 之前的写法让隐藏/遮挡态仍全速渲染，是空闲 CPU 的第二大来源；现在恢复系统默认
    // 节流（最小化 / 遮挡时 Chromium 自行降频），与"空闲即休眠"策略一致。
    // 需要临时关闭节流排障时，用 `ARIA2_HARNESS_WEBVIEW_ARGS` 追加同名参数即可。

    // 内置 WebView2 Fixed Version：必须在创建任何 WebView 组件与后台线程之前绑定。
    // 精简版 Win11 不含系统 WebView2，内置运行时让"双击秒开"不再依赖联网安装。
    let webview2_runtime = bind_fixed_webview2();

    // 单实例门禁：必须位于一切初始化之前（窗口 / 引擎 / 数据库 / RPC 端口）。
    // 唤醒通道用命名事件而非窗口消息：主窗口在托盘态已被销毁，事件对象却常驻。
    #[cfg(windows)]
    let wake = match acquire_single_instance() {
        SingleInstance::Secondary => {
            eprintln!("[single-instance] 已有实例在运行，已发出唤醒信号，本进程退出");
            std::process::exit(0);
        }
        SingleInstance::Primary { wake } => wake,
    };

    let mut builder = EventLoopBuilder::<UiEvent>::with_user_event();
    let mut event_loop = builder.build();
    let proxy = event_loop.create_proxy();
    let alive = Arc::new(AtomicBool::new(true));

    // 唤醒监听线程：窗口销毁后依然有效（事件对象独立于窗口生命周期）。
    #[cfg(windows)]
    if let Some(wake) = wake {
        spawn_wake_listener(wake, proxy.clone());
    }

    // 空闲巡检：进程启动即开始计时，5 分钟无操作且无任务时挂起 WebView2 渲染。
    touch_activity();
    spawn_idle_monitor(proxy.clone());

    // 1) 注入宿主实现并启动插件系统 + 后台服务引导（日志 / 全局槽位 / 插件 / WS 循环）。
    let host: Arc<dyn HostContext> = Arc::new(WryHost {
        proxy: proxy.clone(),
        alive: alive.clone(),
    });
    lib::bootstrap(host);
    // 日志已就绪：把实际绑定的运行时写进 app.log，排障时一眼看出用的是内置还是系统 WebView2。
    match webview2_runtime.as_deref() {
        Some(dir) => tracing::info!("[webview2] 已绑定内置 Fixed Version：{}", dir.display()),
        None => tracing::warn!(
            "未找到内置 WebView2 Fixed Version（{WEBVIEW2_RUNTIME_DIR}），回退系统 WebView2"
        ),
    }

    // 2) IPC 请求通道 + 异步路由任务（业务命令跑在共享 tokio 运行时上）。
    let (req_tx, req_rx) = unbounded_channel::<IpcRequest>();
    let router_proxy = proxy.clone();
    lib::rt().spawn(async move {
        route(router_proxy, req_rx).await;
    });

    // 3) 系统托盘：必须常驻到事件循环结束（`_tray` 在 main 作用域内持有）。
    //    创建失败时仅记日志：窗口功能不受影响，只是"关闭即退出"。
    #[cfg(windows)]
    let _tray = build_tray(proxy.clone());

    // 4) 主窗口 + WebView：关闭即销毁、唤醒即重建。
    //    状态只能由主线程局部变量持有 —— `tao::Window` / `wry::WebView` 都不是 `Send`。
    let mut window: Option<tao::window::Window> = None;
    let mut webview: Option<wry::WebView> = None;
    // 上次关闭时的几何：跨"销毁 → 重建"保留，唤醒时原位还原。
    let mut save = WindowState::default();
    {
        // `EventLoop` 对 `EventLoopWindowTarget` 实现了 `Deref`，这里取窗口目标建窗。
        let target: &EventLoopWindowTarget<UiEvent> = &event_loop;
        show_main_window(
            target,
            &proxy,
            &req_tx,
            save,
            &alive,
            &mut window,
            &mut webview,
            "startup",
        );
    }

    // 5) 事件循环：主线程内处理"发事件 / 回传结果"，退出时统一优雅关闭。
    //    闭包按值捕获 `loop_alive`（`alive` 自身留作收尾用），避免 Arc 被 move 走。
    let loop_alive = alive.clone();
    event_loop.run_return(move |event, target, control_flow| {
        // 红线：tao 0.37 的 `ControlFlow` 默认值是 `Poll`，且每次 `run_return` 都会重置回
        // `Poll`（见 tao 源码 `event_loop.rs` 的 `impl Default for ControlFlow`）。不显式设置
        // 时 Windows 消息泵会在每轮结束后 `PostMessageW(PROCESS_NEW_EVENTS_MSG_ID)` 自激，
        // 主线程永远拿得到自己投递的消息 —— 表现为空闲 8%~10% 的纯空转。
        // 这里每个事件都先重置为 `Wait`（有事件才醒来），退出分支再覆盖成 `ExitWithCode`。
        *control_flow = ControlFlow::Wait;
        match event {
        Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } => {
            // 用户点击标题栏 X：默认"关闭即销毁"（引擎与任务继续跑），
            // 只有在设置里选择「直接退出」才走完整退出流程。
            if lib::close_to_tray() {
                destroy_main_window(&mut save, &loop_alive, &mut window, &mut webview);
            } else {
                prepare_exit(window.as_ref(), webview.as_ref());
                loop_alive.store(false, Ordering::Relaxed);
                *control_flow = ControlFlow::ExitWithCode(0);
            }
        }
        Event::UserEvent(UiEvent::Emit(event, payload)) => {
            if let Some(webview) = webview.as_ref() {
                // 事件可能在 ipc.ts 注册全局函数之前到达：用可选链兜底，避免脚本报错。
                let script = format!("window.__ipcEmit?.({}, {});", json!(event), payload);
                if let Err(err) = webview.evaluate_script(&script) {
                    tracing::error!("向前端派发事件「{event}」失败：{err}");
                }
            }
        }
        Event::UserEvent(UiEvent::Resolve(id, result)) => {
            if let Some(webview) = webview.as_ref() {
                let script = format!("window.__ipcResolve?.({id}, {result});");
                if let Err(err) = webview.evaluate_script(&script) {
                    tracing::error!("向前端回传 IPC 结果（id={id}）失败：{err}");
                }
            }
        }
        Event::UserEvent(UiEvent::Window(id, action)) => {
            // 先结算 IPC 调用（前端不 await 窗口命令，但保持 Promise 不悬挂）。
            if let Some(webview) = webview.as_ref() {
                let script = format!("window.__ipcResolve?.({id}, null);");
                if let Err(err) = webview.evaluate_script(&script) {
                    tracing::error!("窗口命令（id={id}）回传失败：{err}");
                }
            }
            match action {
                WindowAction::Close => {
                    // 与标题栏 X 保持同一语义：托盘模式只销毁，直接退出模式才结束进程。
                    if lib::close_to_tray() {
                        destroy_main_window(&mut save, &loop_alive, &mut window, &mut webview);
                    } else {
                        prepare_exit(window.as_ref(), webview.as_ref());
                        loop_alive.store(false, Ordering::Relaxed);
                        *control_flow = ControlFlow::ExitWithCode(0);
                    }
                }
                WindowAction::Minimize => {
                    if let Some(window) = window.as_ref() {
                        window.set_minimized(true);
                    }
                    // 最小化即挂起渲染：最小化窗口没有任何可见内容，继续绘制纯属浪费。
                    set_webview_suspended(webview.as_ref(), true);
                }
                WindowAction::Maximize => {
                    // 切换语义：双击标题栏 / 点击最大化按钮都走这里。
                    if let Some(window) = window.as_ref() {
                        let next = !window.is_maximized();
                        window.set_maximized(next);
                    }
                }
                WindowAction::Drag => {
                    // 原生拖拽循环会阻塞到鼠标释放，必须放在主线程且最后执行。
                    if let Some(window) = window.as_ref() {
                        if let Err(err) = window.drag_window() {
                            tracing::error!("窗口拖拽失败：{err}");
                        }
                    }
                }
            }
        }
        Event::UserEvent(UiEvent::Tray(id)) => match id.as_str() {
            "show" => {
                // 窗口已被销毁 → 重建；仍在（最小化等）→ 直接拉回前台。
                show_main_window(
                    target,
                    &proxy,
                    &req_tx,
                    save,
                    &loop_alive,
                    &mut window,
                    &mut webview,
                    "tray-show",
                );
            }
            "pause_all" => {
                // 引擎调用是异步的：投递到共享运行时，绝不阻塞主线程消息泵。
                lib::rt().spawn(async {
                    match lib::pause_all().await {
                        Ok(()) => tracing::info!("托盘：已暂停所有任务"),
                        Err(err) => tracing::error!("托盘「暂停所有任务」失败：{err}"),
                    }
                });
            }
            "quit" => {
                prepare_exit(window.as_ref(), webview.as_ref());
                loop_alive.store(false, Ordering::Relaxed);
                *control_flow = ControlFlow::ExitWithCode(0);
            }
            other => tracing::warn!("未知托盘菜单项：{other}"),
        },
        // 二次启动唤醒（命名事件 → 监听线程 → 本事件）：窗口已销毁则重建。
        Event::UserEvent(UiEvent::Show) => {
            show_main_window(
                target,
                &proxy,
                &req_tx,
                save,
                &loop_alive,
                &mut window,
                &mut webview,
                "wakeup",
            );
        }
        // 最小化 / 恢复 / 焦点变化：同步 WebView2 的可见性。
        // 最小化（含 Win+D、任务栏最小化）→ SetIsVisible(false) 让渲染进程休眠；
        // 恢复或重新获得焦点 → 立即打开，避免白屏。
        Event::WindowEvent {
            event: WindowEvent::Resized(_) | WindowEvent::Focused(_),
            ..
        } => {
            let minimized = window.as_ref().map(|w| w.is_minimized()).unwrap_or(false);
            set_webview_suspended(webview.as_ref(), minimized);
        }
        // 空闲巡检（第三层）：5 分钟无操作 + 无活跃任务 + 窗口失焦 才挂起。
        Event::UserEvent(UiEvent::IdleCheck) => {
            let Some(win) = window.as_ref() else { return };
            // 前置判断零成本：窗口已销毁到托盘（无渲染进程）/ 最小化（尺寸事件已处理）/
            // 仍有焦点 → 一律不挂起，绝不让用户面对"看得见但已冻结"的界面。
            if win.is_minimized() || win.is_focused() || idle_for() < IDLE_SUSPEND_AFTER {
                return;
            }
            // 引擎状态查询是异步的，绝不阻塞主线程消息泵。
            let idler = proxy.clone();
            lib::rt().spawn(async move {
                let idle = match lib::get_global_stat().await {
                    Ok(stat) => stat.get("numActive").and_then(Value::as_str) == Some("0"),
                    // 状态未知时按"有任务在跑"处理：宁可不挂起，也不黑屏。
                    Err(_) => false,
                };
                let _ = idler.send_event(UiEvent::IdleSuspend(idle));
            });
        }
        Event::UserEvent(UiEvent::IdleSuspend(true)) => {
            // 复核：等待引擎应答期间窗口可能已被重新聚焦 / 取消最小化。
            let still_idle = window
                .as_ref()
                .map(|w| !w.is_focused() && !w.is_minimized())
                .unwrap_or(false);
            if still_idle && idle_for() >= IDLE_SUSPEND_AFTER {
                set_webview_suspended(webview.as_ref(), true);
            }
        }
        Event::UserEvent(UiEvent::IdleSuspend(false)) => {}
        // 页面加载完成：把隐藏创建的窗口显示出来（避免 WebView2 首帧白底闪烁）。
        Event::UserEvent(UiEvent::PageLoaded) => {
            if let Some(window) = window.as_ref() {
                if window.is_minimized() {
                    window.set_minimized(false);
                }
                if !window.is_visible() {
                    window.set_visible(true);
                }
                set_webview_suspended(webview.as_ref(), false);
                window.set_focus();
            }
        }
        _ => {}
        }
    });

    // 6) 优雅退出：停插件监听 → 插件收敛 → RPC 落盘 → 通知核心服务收尾。
    //    （窗口 / WebView 随闭包一起析构，此处只剩业务侧收尾。）
    alive.store(false, Ordering::Relaxed);
    lib::shutdown();
}

/// IPC 路由任务：串行消费请求，分发到业务函数，把结果经 `EventLoopProxy` 回主线程。
async fn route(proxy: EventLoopProxy<UiEvent>, mut rx: UnboundedReceiver<IpcRequest>) {
    while let Some(req) = rx.recv().await {
        // 窗口控制必须在主线程执行（tao Window 非 Send），投递后由事件循环直接操作，
        // 不进入异步业务路由，避免占用 6s 超时预算。
        if let Some(action) = window_action(&req.cmd) {
            if let Err(err) = proxy.send_event(UiEvent::Window(req.id, action)) {
                tracing::error!("窗口命令投递失败（事件循环已退出）：{err}");
            }
            continue;
        }
        // 更新确认（beta7）：暂停任务 → 打开下载页 → 回传 → 走托盘「退出」同一套优雅退出。
        // 必须在此专案处理（而不是 dispatch）：退出动作只能在主线程事件循环完成，
        // 且回传后流程不再返回，避免前端 Promise 悬挂。
        if req.cmd == "quit_for_update" {
            if let Err(err) = handle_quit_for_update(&proxy, req.id, &req.args).await {
                let result = json!({ "__err": err });
                if let Err(send_err) = proxy.send_event(UiEvent::Resolve(req.id, result)) {
                    tracing::error!("quit_for_update 回传失败：{send_err}");
                }
            }
            continue;
        }
        // 失败时编码为 `{"__err": {code, message}}` —— `AppError` 手写 `Serialize`，
        // `json!` 会直接把它摊成这两个字段，前端 `ipc.ts` 据此构造 `IpcError`。
        // 文件夹选择器是模态交互，用独立的超长预算，避免用户挑选途中被 6s 兜底误杀。
        let budget = if req.cmd == "pick_directory" {
            PICK_DIRECTORY_TIMEOUT
        } else {
            IPC_TIMEOUT
        };
        let result = match tokio::time::timeout(budget, dispatch(&req.cmd, req.args)).await {
            Ok(done) => done.unwrap_or_else(|e| json!({ "__err": e })),
            Err(_) => {
                // 兜底：宁可让这一条命令失败，也绝不把整条 IPC 通道永久堵死。
                tracing::error!(
                    "IPC 命令「{}」执行超过 {}s，已中断（防止串行路由被永久占用）",
                    req.cmd,
                    budget.as_secs()
                );
                json!({ "__err": AppError::Internal(format!(
                    "IPC 命令「{}」执行超时（{}s），已中断",
                    req.cmd,
                    budget.as_secs()
                )) })
            }
        };
        if let Err(err) = proxy.send_event(UiEvent::Resolve(req.id, result)) {
            tracing::error!("IPC 结果回传事件循环失败（事件循环已退出）：{err}");
        }
    }
}

/// 更新前的安全退出（beta7）：先暂停任务，再打开下载页，最后走托盘「退出」同一套流程。
///
/// 为什么不能直接强杀：覆盖安装会结束 aria2c，未刷盘的分片进度全指望 `.aria2` 控制文件；
/// 先 `forcePauseAll` 让引擎把进度落盘，退出时 `lib::shutdown()` 再写 `data/aria2.session`，
/// 用户装完新版本一打开，任务还在。
async fn handle_quit_for_update(
    proxy: &EventLoopProxy<UiEvent>,
    id: u64,
    args: &Value,
) -> Result<(), AppError> {
    let url = args
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    // 只放行 http(s)：这个入口的载荷来自远端清单，不能被放大成任意本地执行。
    if !lib::update_check::is_http_url(&url) {
        return Err(AppError::InvalidArgs(format!("更新地址非法：{url}")));
    }

    // 「能暂停任务最好」：引擎未就绪 / 卡死都不能把退出流程拖住，最多等 3s。
    match tokio::time::timeout(Duration::from_secs(3), lib::pause_all()).await {
        Ok(Ok(())) => tracing::info!("更新退出：已暂停所有任务"),
        Ok(Err(err)) => tracing::warn!("更新退出：暂停任务失败（继续退出）：{err}"),
        Err(_) => tracing::warn!("更新退出：暂停任务超时（3s），继续退出"),
    }

    // 先拉起下载页再回传：进程退出后用户仍拿得到安装包。
    lib::open_external(url)?;

    if let Err(err) = proxy.send_event(UiEvent::Resolve(id, json!(true))) {
        tracing::error!("更新退出：IPC 结果回传失败（事件循环已退出）：{err}");
        return Ok(());
    }
    // 留一点时间让前端收起弹窗，再进入与托盘「退出」完全一致的优雅退出路径。
    tokio::time::sleep(Duration::from_millis(250)).await;
    if let Err(err) = proxy.send_event(UiEvent::Tray("quit".into())) {
        tracing::error!("更新退出：无法投递退出事件：{err}");
    }
    Ok(())
}

/// 把 IPC 命令名分发到对应业务函数（命令名 / 参数名与既有 Tauri 命令保持一致）。
async fn dispatch(cmd: &str, args: Value) -> Result<Value, AppError> {
    match cmd {
        "add_download" => {
            let url = take_str(&args, "url")?;
            // 前端 `dir: null` 时透传 None，由后端回退默认下载目录。
            let dir = args.get("dir").and_then(|v| v.as_str()).map(str::to_string);
            // 单任务选项（referer 等）：可选对象，缺失即视为无。
            let options = args
                .get("options")
                .and_then(|v| v.as_object().cloned())
                .filter(|map| !map.is_empty());
            Ok(json!(lib::add_download(url, dir, options).await?))
        }
        // 拖拽 `.torrent`：前端读字节转 base64（WebView 不暴露真实路径）。
        "add_torrent" => {
            let path = args.get("path").and_then(|v| v.as_str()).map(str::to_string);
            let data = args.get("data").and_then(|v| v.as_str()).map(str::to_string);
            let name = args.get("name").and_then(|v| v.as_str()).map(str::to_string);
            Ok(json!(lib::add_torrent(path, data, name).await?))
        }
        "get_tasks" => Ok(json!(lib::get_tasks().await?)),
        "get_task" => {
            let gid = take_str(&args, "gid")?;
            Ok(json!(lib::get_task(gid).await?))
        }
        "get_active_tasks" => Ok(json!(lib::get_active_tasks().await?)),
        "get_engine_status" => Ok(json!(lib::get_engine_status().await?)),
        // 窗口回到前台：WS 断开时打断退避立即重连。
        "reconnect_engine" => Ok(json!(lib::reconnect_engine().await?)),
        "change_global_option" => {
            let options = args
                .get("options")
                .and_then(|v| v.as_object().cloned())
                .ok_or_else(|| AppError::InvalidArgs("change_global_option：缺少 options".to_string()))?;
            lib::change_global_option(options).await?;
            Ok(Value::Null)
        }
        // beta8 配置同步：UI 连上后端后强制全量下发一次（dir / split / 并发 / 限速 / 代理…）。
        "sync_engine_config" => {
            lib::sync_engine_config().await?;
            Ok(Value::Null)
        }
        "get_global_option" => Ok(lib::get_global_option().await?),
        // 设置 → 网络：代理连通性测试（TCP 握手，3s 超时，返回耗时毫秒）。
        "test_proxy" => {
            let host = take_str(&args, "host")?;
            let raw_port = args
                .get("port")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| AppError::InvalidArgs("test_proxy：缺少 port".to_string()))?;
            let port = u16::try_from(raw_port)
                .map_err(|_| AppError::InvalidArgs(format!("test_proxy：端口 {raw_port} 非法")))?;
            Ok(json!(lib::test_proxy(host, port).await?))
        }
        // 设置 → 网络：展示本机接管 API 的实际端口与随机 Token。
        "get_api_info" => Ok(lib::api_info()),
        // 设置 → 网络：浏览器扩展目录（安装引导里一键复制）。
        "get_browser_extension_info" => Ok(lib::browser_extension_info()),
        // 浏览器接管：读取暂存任务（前端 mount 时补回挂机期间收到的那一单）。
        "get_pending_intercept" => Ok(lib::pending_intercept()),
        // 浏览器接管：用户取消（或确认成功后）清空宿主侧暂存。
        "cancel_intercept" => {
            lib::cancel_intercept();
            Ok(Value::Null)
        }
        // 设置 → 网络：宿主本地配置（HTTP/1.1 兼容模式等，跨重启保留）。
        "get_host_config" => Ok(lib::get_host_config()),
        // 运行期关键目录（插件页 / 设置页展示；避免前端写死构建机绝对路径）。
        "get_app_paths" => Ok(lib::app_paths()),
        "set_host_config" => {
            let config = args
                .get("config")
                .and_then(|v| v.as_object().cloned())
                .ok_or_else(|| AppError::InvalidArgs("set_host_config：缺少 config".to_string()))?;
            lib::set_host_config(config)?;
            Ok(Value::Null)
        }
        // 设置 → 外观：关闭窗口时隐藏到托盘 / 直接退出（仅进程内开关）。
        "set_close_behavior" => {
            lib::set_close_behavior(&take_str(&args, "mode")?)?;
            Ok(Value::Null)
        }
        // 设置 → 通用：开机自启（HKCU Run 注册表项）。
        "get_autostart" => Ok(json!(lib::autostart_enabled()?)),
        "set_autostart" => {
            let enabled = args
                .get("enabled")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| AppError::InvalidArgs("set_autostart：缺少 enabled".to_string()))?;
            lib::set_autostart(enabled)?;
            Ok(Value::Null)
        }
        "pick_directory" => Ok(json!(lib::pick_directory().await?)),
        "pause_download" => {
            lib::pause_download(take_str(&args, "gid")?).await?;
            Ok(Value::Null)
        }
        "resume_download" => {
            lib::resume_download(take_str(&args, "gid")?).await?;
            Ok(Value::Null)
        }
        "remove_download" => {
            lib::remove_download(take_str(&args, "gid")?).await?;
            Ok(Value::Null)
        }
        // 任务卡片右键菜单「强制删除」：语义与 remove_download 一致（forceRemove 优先）。
        "force_remove" => {
            lib::force_remove(take_str(&args, "gid")?).await?;
            Ok(Value::Null)
        }
        // 删除任务并清理本地文件（含 .aria2 进度文件；清理失败只记日志）。
        "remove_task_and_files" => {
            lib::remove_task_and_files(take_str(&args, "gid")?).await?;
            Ok(Value::Null)
        }
        "list_plugins" => Ok(json!(lib::list_plugins())),
        "toggle_plugin" => {
            let id = take_str(&args, "id")?;
            let enabled = args
                .get("enabled")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| AppError::InvalidArgs("toggle_plugin：缺少 enabled".to_string()))?;
            lib::toggle_plugin(id, enabled)?;
            Ok(Value::Null)
        }
        "grant_plugin_permissions" => {
            lib::grant_plugin_permissions(take_str(&args, "id")?)?;
            Ok(Value::Null)
        }
        "update_plugin_settings" => {
            let id = take_str(&args, "id")?;
            let values = args
                .get("values")
                .and_then(|v| v.as_object().cloned())
                .ok_or_else(|| {
                    AppError::InvalidArgs("update_plugin_settings：缺少 values".to_string())
                })?;
            lib::update_plugin_settings(id, values)?;
            Ok(Value::Null)
        }
        "get_metrics" => lib::get_metrics(),
        "get_global_stat" => Ok(lib::get_global_stat().await?),
        "get_trackers" => Ok(lib::get_trackers().await?),
        "refresh_trackers" => Ok(lib::refresh_trackers().await?),
        "get_history_tasks" => {
            let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);
            let offset = args.get("offset").and_then(|v| v.as_i64()).unwrap_or(0);
            Ok(json!(lib::get_history_tasks(limit, offset).await?))
        }
        "clear_history" => Ok(json!(lib::clear_history().await?)),
        "reveal_in_dir" => {
            lib::reveal_in_dir(take_str(&args, "path")?)?;
            Ok(Value::Null)
        }
        // 增量 46：更新通知里的"前往下载"，交给系统默认浏览器（只放行 http/https）。
        "open_external" => {
            lib::open_external(take_str(&args, "url")?)?;
            Ok(Value::Null)
        }
        other => Err(AppError::InvalidArgs(format!("未知命令：{other}"))),
    }
}

/// 从 args 中取必需字符串参数（缺失即报可读错误）。
fn take_str(args: &Value, key: &str) -> Result<String, AppError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| AppError::InvalidArgs(format!("缺少参数：{key}")))
}
