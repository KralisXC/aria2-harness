//! 运行期目录解析（便携版 HOME 布局自适应）。
//!
//! 优先级：
//! 1. `ARIA2_HARNESS_HOME` 环境变量（显式覆盖，测试 / 多实例用）；
//! 2. **exe 同级存在 `dist/index.html`** → 该目录即 HOME（绿色免安装包布局）；
//!    但若 exe 位于 `Program Files` 之类系统安装目录（普通用户**不可写**），
//!    HOME 改为 `%LOCALAPPDATA%\Aria2-Harness`（安装包布局）；
//! 3. 开发期回退：仓库根（由编译期清单目录反推，不写死绝对路径）。
//!
//! 所有可写目录（data / logs / plugins / downloads）都挂在 HOME 下，
//! 便携包解压到任何盘符都能就地读写，绝不依赖构建机的绝对路径。
//!
//! **为什么按目录位置而不是"试写探测"判定**：服务进程是提权的、UI 是普通权限，
//! 同一个 `Program Files` 目录对两者可写性不同 —— 试写会让两个进程算出不同的 HOME，
//! 下载目录与数据库直接错位。按路径前缀判断与令牌无关，两边结果恒定一致。
//!
//! `dist` 等**只读资源**永远从 exe 所在目录取（[`app_dir`]），与可写 HOME 解耦：
//! 安装包场景下资源在 Program Files、数据在 LocalAppData，二者本就不在同一处。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 开发期根目录（仅当 exe 同级没有 `dist/index.html` 时生效）。
///
/// 由编译期清单目录反推仓库根（`my-downloader/src-host` → 仓库根），
/// 源码里不写死任何构建机的绝对路径。
fn dev_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}
/// HOME 覆盖环境变量。
const HOME_ENV: &str = "ARIA2_HARNESS_HOME";

/// 探测 HOME 目录（进程内只算一次）。
fn detect_home() -> PathBuf {
    if let Ok(raw) = std::env::var(HOME_ENV) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let exe_dir = app_dir();
    // 便携包标志：前端资源与 exe 同级。开发期 target\debug 下没有 dist，走回退。
    if exe_dir.join("dist").join("index.html").is_file() {
        if !is_system_install(&exe_dir) {
            return exe_dir;
        }
        // Program Files 安装：数据一律进**当前登录用户**的 LOCALAPPDATA。
        //
        // 服务进程以 LocalSystem 运行时，自身环境里的 LOCALAPPDATA 指向
        // `...\system32\config\systemprofile\AppData\Local` —— 若沿用它，UI 与
        // 服务/引擎会各自算出不同的 HOME，`config.json` 与 data 目录彻底脱节
        // （beta7 实测：UI 显示正确，底层却把文件落到 systemprofile）。
        if let Some(local) = install_user_local_appdata() {
            return local.join("Aria2-Harness");
        }
        return exe_dir;
    }
    dev_root()
}

/// exe 所在目录 —— **只读资源**（dist / aria2c.exe）的唯一来源。
///
/// 与 [`home`] 分离的原因：安装包场景下资源在 `Program Files`、可写数据在
/// `%LOCALAPPDATA%`；若用 HOME 推 `dist`，安装版会直接找不到前端。
pub fn app_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| dev_root())
}

/// 安装版 HOME 基目录（`%LOCALAPPDATA%`），**必须与 UI 进程算出同一个值**。
///
/// IP 进程（普通用户）直接读环境变量即可；服务进程（LocalSystem）的环境变量指向
/// systemprofile，必须改从交互式用户的 WTS 令牌环境块里取 `LOCALAPPDATA`。
/// 取不到时返回 `None`（调用方回退 exe 目录），**绝不**退回 SYSTEM 的 LOCALAPPDATA。
#[cfg(windows)]
fn install_user_local_appdata() -> Option<PathBuf> {
    if is_local_system() {
        if let Some(local) = interactive_env_value("LOCALAPPDATA=") {
            return Some(local);
        }
        return interactive_user_profile()
            .map(|profile| profile.join("AppData").join("Local"));
    }
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
}

#[cfg(not(windows))]
fn install_user_local_appdata() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
}

/// exe 是否位于系统安装目录（`Program Files` / `Program Files (x86)`）。
///
/// 纯字符串前缀比较，与进程令牌无关 —— 提权服务与普通权限 UI 必须得到同一答案。
fn is_system_install(dir: &Path) -> bool {
    let dir = dir.to_string_lossy().to_lowercase();
    ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
        .iter()
        .filter_map(|key| std::env::var_os(key))
        .map(|base| base.to_string_lossy().to_lowercase())
        .filter(|base| !base.is_empty())
        .any(|base| dir.starts_with(&format!("{base}\\")) || dir == base)
}

/// 应用根目录（便携包 = exe 所在目录；开发期 = 仓库根）。
pub fn home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(detect_home).as_path()
}

/// 前端静态资源目录（仅 Release 的内置静态服务器使用）。
///
/// 永远跟随 exe 所在目录（安装包布局下资源在 Program Files，数据在 LocalAppData）。
pub fn web_root() -> PathBuf {
    app_dir().join("dist")
}

/// 数据目录：**所有内部状态都下沉到这里**（SQLite / 会话 / tracker / 日志 / 错误页）。
///
/// 红线：程序根目录只允许出现 `data` 与 `plugins` 两个文件夹。
pub fn data_dir() -> PathBuf {
    home().join("data")
}

/// 日志目录（应用日志 + 服务日志 + 插件审计日志）：`data/logs`。
pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

/// 错误页目录（内置错误页 / 诊断页资源）：`data/error_pages`。
pub fn error_pages_dir() -> PathBuf {
    data_dir().join("error_pages")
}

/// 插件目录（扫描 / 热重载 / registry.json）。
///
/// 刻意**不**下沉到 `data`：用户需要直接往这里拖放插件，保持根目录可见。
pub fn plugins_dir() -> PathBuf {
    home().join("plugins")
}

/// 默认下载目录：Windows 系统「下载」文件夹（`FOLDERID_Downloads`）。
///
/// 便携版同样指向系统下载夹，绝不与 exe 混在一起。
/// 解析失败（极少见：无用户配置文件 / 被组策略禁用）时回退 `%USERPROFILE%\Downloads`，
/// 再失败才退回 HOME 下的 `downloads`（仅保证功能不崩）。
///
/// **LocalSystem 特判**：后台服务由 SCM 以 SYSTEM 身份启动时，`dirs::download_dir()`
/// 解析到的是 `C:\Windows\system32\config\systemprofile\Downloads` —— 用户既看不见
/// 也没权限接管。此时必须改用当前控制台登录用户的 `USERPROFILE`（WTS token +
/// `CreateEnvironmentBlock`），避免默认目录泄漏到 UI 的"新建任务"弹窗。
pub fn downloads_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if is_local_system() {
            if let Some(profile) = interactive_user_profile() {
                return profile.join("Downloads");
            }
            // 极端兜底：无控制台会话（headless / 服务未登录）时用公共下载夹，
            // 至少保证文件落在所有用户可访问的路径，而不是 systemprofile。
            let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
            return PathBuf::from(format!(r"{drive}\Users\Public\Downloads"));
        }
    }
    dirs::download_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Downloads")))
        .unwrap_or_else(|| home().join("downloads"))
}

/// 配置里的下载目录是否可信：必须是绝对路径，且不能落在 LocalSystem 的
/// `systemprofile`（历史版本覆盖安装后可能把该路径写进 `config.json`）。
pub fn is_safe_download_dir(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    if !Path::new(trimmed).is_absolute() {
        return false;
    }
    let lowered = trimmed.to_ascii_lowercase().replace('/', "\\");
    !lowered.contains(r"\systemprofile\") && !lowered.contains(r"\system32\config\")
}

/// 解析下载目录：非法 / 缺失 / 相对路径 / systemprofile 一律回退到当前用户的
/// 系统「下载」文件夹。**所有**读写该配置的入口都必须经过这里。
pub fn resolve_download_dir(raw: Option<&str>) -> PathBuf {
    raw.filter(|value| is_safe_download_dir(value))
        .map(PathBuf::from)
        .unwrap_or_else(downloads_dir)
}

/// 当前进程是否以 LocalSystem（SYSTEM）身份运行。

#[cfg(windows)]
fn is_local_system() -> bool {
    std::env::var("USERNAME")
        .map(|name| name.eq_ignore_ascii_case("SYSTEM"))
        .unwrap_or(false)
}

/// 取当前控制台登录用户的 `USERPROFILE`。
///
/// 通过 WTS 拿到控制台会话的用户令牌，再 `CreateEnvironmentBlock` 读出该用户的
/// 环境变量块 —— 比拼 `C:\Users\<name>` 可靠（兼容漫游配置 / 用户名与文件夹名不一致）。
#[cfg(windows)]
fn interactive_user_profile() -> Option<PathBuf> {
    interactive_env_value("USERPROFILE=")
}

/// 读交互式用户环境块里的指定变量（`key` 需带结尾 `=`）。
#[cfg(windows)]
fn interactive_env_value(key: &str) -> Option<PathBuf> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};

    unsafe {
        let session = WTSGetActiveConsoleSessionId();
        if session == u32::MAX {
            return None;
        }
        let mut token = HANDLE::default();
        if WTSQueryUserToken(session, &mut token).is_err() {
            return None;
        }
        let mut environment: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut profile = None;
        if CreateEnvironmentBlock(&mut environment, Some(token), false).is_ok()
            && !environment.is_null()
        {
            profile = read_env_value(environment.cast::<u16>(), key);
            let _ = DestroyEnvironmentBlock(environment);
        }
        let _ = CloseHandle(token);
        profile
    }
}

/// 在 UTF-16 双零结尾的环境变量块里查找 `KEY=value`。
#[cfg(windows)]
fn read_env_value(block: *const u16, key: &str) -> Option<PathBuf> {
    if block.is_null() {
        return None;
    }
    let mut cursor = block;
    loop {
        // SAFETY: 环境块由 `CreateEnvironmentBlock` 生成，以双零结尾；
        // 调用方保证在本函数返回前不会 `DestroyEnvironmentBlock`。
        let (entry, next) = unsafe {
            let mut len = 0usize;
            while *cursor.add(len) != 0 {
                len += 1;
            }
            if len == 0 {
                return None;
            }
            (
                String::from_utf16_lossy(std::slice::from_raw_parts(cursor, len)),
                cursor.add(len + 1),
            )
        };
        if let Some(value) = entry.strip_prefix(key) {
            return Some(PathBuf::from(value));
        }
        cursor = next;
    }
}

/// aria2 会话文件（`--save-session` / `--input-file`）：`data/aria2.session`。
///
/// 放在 data 里而不是下载目录：会话文件是内部状态，混进用户的下载夹会被误删。
pub fn session_path() -> PathBuf {
    data_dir().join("aria2.session")
}

/// 任务历史数据库路径。
pub fn db_path() -> PathBuf {
    data_dir().join("aria2-harness.db")
}

/// WebView2 用户数据目录 —— **必须显式指定到可写位置**。
///
/// 不指定时 WebView2 会把 `aria2-harness.exe.WebView2` 建在 exe 同级：安装版位于
/// `Program Files`，普通权限进程不可写，`CreateCoreWebView2Environment` 直接失败，
/// 用户看到的就是"双击闪退、没有任何提示"。
///
/// - 便携版：`exe 同级\data\WebView2`
/// - 安装版：`%LOCALAPPDATA%\Aria2-Harness\WebView2`
pub fn webview2_data_dir() -> PathBuf {
    if is_system_install(&app_dir()) {
        home().join("WebView2")
    } else {
        data_dir().join("WebView2")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_systemprofile_and_relative_dirs() {
        assert!(!is_safe_download_dir(
            r"C:\WINDOWS\system32\config\systemprofile\Downloads"
        ));
        assert!(!is_safe_download_dir(
            r"c:/windows/system32/config/systemprofile/Downloads"
        ));
        assert!(!is_safe_download_dir(""));
        assert!(!is_safe_download_dir("Downloads"));
        assert!(is_safe_download_dir(r"C:\Users\alice\Downloads"));
    }

    #[test]
    fn resolve_never_returns_systemprofile() {
        let resolved = resolve_download_dir(Some(
            r"C:\Windows\System32\config\systemprofile\Downloads",
        ));
        assert!(resolved.is_absolute());
        assert!(!resolved
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("systemprofile"));
    }
}
