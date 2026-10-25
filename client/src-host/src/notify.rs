//! Windows 系统级原生通知（Toast）。
//!
//! 触发场景：窗口已隐藏到托盘（后台态）时，任务 `Complete` / `Error` 弹系统通知。
//! 前台时由 UI 内的通知中心承担，避免"双重提醒"。
//!
//! 两条硬约束：
//! 1. **绝不阻塞 tokio 线程**：`Toast::show()` 内部有 10ms sleep 且要走 COM/WinRT，
//!    因此每次通知都投到独立 std 线程，调用方只做一次 `spawn`（fire-and-forget）；
//! 2. **线程必须初始化 COM**：WinRT 依赖 MTA/STA 套间，wry/tao 主线程已初始化，
//!    但新起的通知线程没有，直接调用会返回 `RPC_E_CHANGED_MODE` 或 `CO_E_NOTINITIALIZED`。
//!
//! 未打包（unpackaged）桌面程序要弹 Toast，必须先在
//! `HKCU\Software\Classes\AppUserModelId\<AUMID>` 注册 DisplayName，
//! 否则通知会静默丢弃（Windows 10/11 的共同行为）。

/// 应用 AUMID（Application User Model Id）——长度 < 129，全局唯一。
#[cfg(windows)]
const AUMID: &str = "Aria2.Harness.Desktop";

/// 注册 AUMID（进程启动时调用一次，幂等）。
///
/// 只写 `HKCU`，无需管理员权限；`DisplayName` 是操作中心里显示的应用名。
/// 刻意不写 `IconUri`：它需要一个**真实图片文件路径**，指向 exe 无效，
/// 留空时系统会回退到默认图标（不影响通知送达）。
#[cfg(windows)]
pub fn init() {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let key_path = format!(r"Software\Classes\AppUserModelId\{AUMID}");
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    match hkcu.create_subkey(&key_path) {
        Ok((key, _)) => {
            if let Err(err) = key.set_value("DisplayName", &"Aria2-Harness") {
                tracing::warn!("注册系统通知 AUMID 失败（DisplayName）：{err}");
            } else {
                tracing::info!(aumid = AUMID, "系统通知 AUMID 已注册");
            }
        }
        Err(err) => tracing::warn!("创建系统通知 AUMID 注册表项失败：{err}"),
    }
}

/// 弹出一条系统通知（标题 + 正文），在独立线程执行，立即返回。
#[cfg(windows)]
pub fn notify(title: &str, body: &str) {
    let title = title.to_string();
    let body = body.to_string();
    let spawn = std::thread::Builder::new()
        .name("toast-notify".to_string())
        .spawn(move || {
            use windows::Win32::System::Com::{
                CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED,
            };

            // SAFETY: 初始化与反初始化在同一线程内配对；失败时仍尝试 show()
            // （主线程可能已把 COM 设为 MTA，子线程失败不代表 WinRT 不可用）。
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if hr.is_err() {
                tracing::warn!("通知线程 COM 初始化失败（0x{:08X}），继续尝试弹出", hr.0);
            }

            let result = tauri_winrt_notification::Toast::new(AUMID)
                .title(&title)
                .text1(&body)
                .show();
            if let Err(err) = result {
                tracing::warn!("系统通知弹出失败：{err}");
            }

            unsafe { CoUninitialize() };
        });
    if let Err(err) = spawn {
        tracing::warn!("启动通知线程失败：{err}");
    }
}

/// 非 Windows 平台：系统通知不可用，保持空实现（调用方零改动）。
#[cfg(not(windows))]
pub fn init() {}

/// 非 Windows 平台：静默降级。
#[cfg(not(windows))]
pub fn notify(_title: &str, _body: &str) {}
