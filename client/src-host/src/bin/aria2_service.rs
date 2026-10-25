// Prevents additional console window on Windows in release, DO NOT REMOVE!!
// Debug 保留控制台便于看日志；Release 彻底隐身（输出改写进 logs/service.log）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! 核心服务 `aria2-service`（增量 23）—— 双进程模型中的"较高权限"一侧。
//!
//! 职责边界（与 UI 严格分工）：
//! - **它**：拉起 / 监控 / 销毁 `aria2c.exe`；生成 RPC 密钥；通过命名管道对外提供
//!   管理指令（`start_engine` / `get_status` / `shutdown`）。
//! - **UI**：只负责界面与 Tauri 命令转发，运行时永远普通权限，只通过命名管道发指令、
//!   用服务回传的密钥去连 aria2 的 WS RPC。
//!
//! 为什么必须拆：`--file-allocation=falloc` 在 Windows 上走
//! `truncate()` + `SetFileValidData()`（`aria2-1.37.0/src/AbstractDiskWriter.cc`），
//! 而 `SetFileValidData()` 需要 `SE_MANAGE_VOLUME_NAME` 特权（`RequestGroup.cc`
//! 里对应 `util::gainPrivilege(SE_MANAGE_VOLUME_NAME)`）。UI 进程永远普通权限，
//! 所以由高权限服务父进程拉起引擎，子进程继承特权令牌，`falloc` 才能真正"瞬间占位"。
//!
//! 形态说明：本增量**不注册 Windows Service**（不碰 `sc.exe`），就是一个常驻进程，
//! 由 UI（Release 自动 `runas`）或 `start_dev.ps1`（Debug）以管理员权限拉起。
//!
//! 输出去向：Debug 直接打控制台；Release 是 `windows_subsystem = "windows"`（无黑框），
//! 进程入口第一件事就把 stdout / stderr 重定向到 `<HOME>/data/logs/service.log`。

use std::ffi::c_void;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
// `creation_flags` 由该 trait 提供，必须显式引入作用域（仅 Windows）。
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::Duration;

use aria2_harness_lib::error::AppError;
// C-3 Phase 2：6800 对外单入口（Router + WS JSON-RPC 服务端）。
use aria2_harness_lib::engine::{Aria2Client, EngineClient};
#[cfg(feature = "native-http")]
use aria2_harness_lib::engine::NativeClient;
use aria2_harness_lib::process_guard;
use aria2_harness_lib::router::{self, EngineMode, Router};
use aria2_harness_lib::service_protocol::{
    self, decode_request, encode_response, ServiceRequest, ServiceResponse,
};
use aria2_harness_lib::paths;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::Notify;
#[cfg(windows)]
use windows_service::{
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

/// 注册到 SCM 的服务名（与安装包 `sc create` / UI `sc start` 严格一致）。
#[cfg(windows)]
const SERVICE_NAME: &str = "Aria2-Harness-Service";

// SCM 入口回调（由 `define_windows_service!` 生成 `ffi_service_main` 时调用）。
#[cfg(windows)]
windows_service::define_windows_service!(ffi_service_main, service_main);

/// aria2c sidecar 的候选文件名。
/// `tauri-build` 的 `copy_binaries` 会去掉三元组后缀并把 `binaries/` 压平，
/// 所以产物实际是 exe 同级的 `aria2c.exe`；带三元组的是源码目录里的原始名。
const ARIA2_NAMES: [&str; 2] = ["aria2c.exe", "aria2c-x86_64-pc-windows-msvc.exe"];
/// 显式覆盖 aria2c 路径的环境变量（红线：绝不写死绝对路径）。
const ARIA2_PATH_ENV: &str = "ARIA2_SERVICE_ARIA2_PATH";
/// 单次请求的读取上界（防异常客户端把服务挂住）。
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// 启动前 RPC 端口占用探测的连接超时。
const PORT_PROBE_TIMEOUT: Duration = Duration::from_millis(300);
/// 启动后等待 RPC 端口真正可连的上界。
const ENGINE_READY_TIMEOUT: Duration = Duration::from_secs(5);
/// 等待引擎"自行优雅退出"的轮询间隔与次数（合计 300ms）。
/// aria2.shutdown 是 TimedHaltCommand 的固定 3s 延迟停机（RpcMethodImpl.cc:1357），
/// 不可能在 2s 退出预算内自行结束，所以这里只做短暂礼让，随后强杀兜底。
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(30);
const EXIT_POLL_TRIES: usize = 10;
/// `CreateProcess` 的 `CREATE_NO_WINDOW`（0x08000000）。
///
/// **为什么必需**：`aria2c.exe` 是 **Console 子系统**程序，而本服务在 Release 下是
/// **GUI 子系统**、自身没有控制台。按 Windows 的控制台继承规则，无控制台的父进程
/// 拉起 Console 子进程时，系统会为它**新分配一个控制台窗口** —— 这就是实机看到的
/// 那个黑色大窗。`CREATE_NO_WINDOW` 让子进程不分配控制台，且**不影响**已经
/// `Stdio::piped()` 的 stdout/stderr（日志照常进 service.log）。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 命名管道的安全描述符（SDDL）—— 增量 24 修复「UAC 完整性隔离」（对应实机报告的
/// `os error 5 拒绝访问`）。
///
/// **两道闸门，缺一不可**：Medium 完整性的 UI 想连上 High 完整性服务创建的管道，
/// 必须同时通过 **DACL（自主访问控制）** 与 **MAC（强制性完整性控制）** 两关：
///
/// 1. **DACL**：服务以 **High 完整性**（管理员）运行，UI 永远是 **Medium 完整性**
///    （普通权限）。不给安全描述符时，Windows 套用命名管道默认 DACL（只给
///    SYSTEM / Administrators / 创建者完全控制，Everyone 仅可读），Medium 进程的
///    `CreateFile` 需要 `GENERIC_READ|GENERIC_WRITE` → 被拒。
/// 2. **SACL（完整性标签）**：**只放行 DACL 不够**。高完整性进程创建的命名管道，
///    **默认强制性标签（Mandatory Label）随创建者 = High**，Medium 进程的写请求属于
///    "write-up"，会被内核直接拦掉 → 同样是 `os error 5`。
///    （早期判断"显式 SD 时默认标签即 Medium"来自 OpenJDK 线程，已被实机证伪。）
///
/// 本条 SDDL 的含义：
/// - `D:` 部分（自主访问控制 —— 管"能不能开"）
///   - `(A;;GRGW;;;WD)`：**Everyone** 可读可写。改用 `WD` 而非 `AU`（Authenticated
///     Users），是为了排除"当前用户不在 Authenticated Users 组"这类边角情况；
///   - `(A;;GA;;;SY)` / `(A;;GA;;;BA)`：SYSTEM / Administrators 完全控制。
/// - `S:(ML;;NW;;;S-1-16-8192)`：强制性完整性标签（SACL —— 管"能不能写"）。
///   `ML` = Mandatory Label，`NW` = No-Write-Up，`S-1-16-8192` = 中等完整性（Medium）。
///   这里用 Windows 文档明确支持的**完整 SID** 写法（等价于 `ME`），便于日志比对与排错。
const PIPE_SDDL: &str =
    "D:(A;;GRGW;;;WD)(A;;GA;;;SY)(A;;GA;;;BA)S:(ML;;NW;;;S-1-16-8192)";

/// SDDL 字符串 → 安全描述符（系统用 `LocalAlloc` 在堆上分配，生命周期由调用方负责）。
///
/// **关键坑**：解析**含 SACL** 的 SDDL 同样要求 `SeSecurityPrivilege` 已启用。特权未启用
/// 时本调用可能仍然"成功"，但 SACL 部分被静默剥离 —— 所以调用方必须先
/// `enable_security_privilege()`，并用 `GetSecurityInfo` 回读结果验证。
fn sddl_to_descriptor(sddl: &str) -> Option<windows::Win32::Security::PSECURITY_DESCRIPTOR> {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::PSECURITY_DESCRIPTOR;

    let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut descriptor = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
    // Safety: `wide` 是 NUL 结尾的宽字符串、在本函数内一直存活；`descriptor` 是出参，
    // 成功后由系统在堆上分配（是否 `LocalFree` 由调用方决定）。
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(wide.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    };
    if let Err(err) = converted {
        eprintln!("[svc][错误] 解析 SDDL「{sddl}」失败：{err}");
        return None;
    }
    Some(descriptor)
}

/// 构造管道安全属性，返回 `SECURITY_ATTRIBUTES` 的裸指针（供 tokio 的
/// `ServerOptions::create_with_security_attributes_raw` 使用）。
///
/// 指针在整个**进程生命周期**内保持有效：转换得到的 `SECURITY_DESCRIPTOR` 与
/// `SECURITY_ATTRIBUTES` 都是有意"泄漏"的。理由是服务为长驻进程、且 accept 循环会
/// **反复创建**同名管道实例 —— 与其做引用计数/生命周期管理，不如一次持有到底。
///
/// 返回 `None` 表示 SDDL 转换失败（此时退回默认 DACL/SACL，并在调用处打印警告）。
fn pipe_security_attributes() -> Option<*mut c_void> {
    use windows::core::BOOL;
    use windows::Win32::Security::SECURITY_ATTRIBUTES;

    let descriptor = sddl_to_descriptor(PIPE_SDDL)?;
    let descriptor = Box::leak(Box::new(descriptor));
    let attributes = Box::leak(Box::new(SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: BOOL(0), // 管道句柄不继承
    }));
    Some(attributes as *mut SECURITY_ATTRIBUTES as *mut c_void)
}

/// **兜底专用**：仅含强制性完整性标签的 SDDL。用于在建管道**之后**用 `SetSecurityInfo`
/// 单独补设标签 —— 部分 Windows 版本对 `CreateNamedPipe` 的 `SECURITY_ATTRIBUTES` 里的
/// SACL 支持不完整。
const LABEL_SDDL: &str = "S:(ML;;NW;;;S-1-16-8192)";

/// 从 `LABEL_SDDL` 中抽出 SACL 的裸指针（供 `SetSecurityInfo` 兜底使用）。
/// 同样有意"泄漏"：SACL 位于被泄漏的安全描述符内部，进程生命周期内一直有效。
fn label_sacl() -> Option<*const windows::Win32::Security::ACL> {
    use windows::core::BOOL;
    use windows::Win32::Security::{ACL, GetSecurityDescriptorSacl};

    let descriptor = sddl_to_descriptor(LABEL_SDDL)?;
    let descriptor = Box::leak(Box::new(descriptor));

    let mut present = BOOL(0);
    let mut sacl: *mut ACL = std::ptr::null_mut();
    let mut defaulted = BOOL(0);
    // Safety: `descriptor` 指向有效的安全描述符；三个出参都是本地变量。
    let got =
        unsafe { GetSecurityDescriptorSacl(*descriptor, &mut present, &mut sacl, &mut defaulted).is_ok() };
    if !got || present.0 == 0 || sacl.is_null() {
        eprintln!(
            "[svc][错误] `LABEL_SDDL` 中未解析出 SACL（present={}）。",
            present.0
        );
        return None;
    }
    Some(sacl as *const ACL)
}

/// 回读管道对象上**实际生效**的安全描述符（以 SDDL 字符串返回），用于诊断。
///
/// 这是本次排查的"眼睛"：只有它才能证明 SACL 到底有没有落到管道对象上。
fn describe_pipe_security(server: &NamedPipeServer) -> Option<String> {
    use std::os::windows::io::AsRawHandle;

    use windows::core::PWSTR;
    use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree, WIN32_ERROR};
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
        SE_KERNEL_OBJECT,
    };
    use windows::Win32::Security::{
        DACL_SECURITY_INFORMATION, LABEL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };

    let handle = HANDLE(server.as_raw_handle() as *mut c_void);
    // 同时取 DACL 与 LABEL（强制性完整性标签）两部分。
    let info = DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION;

    let mut descriptor = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
    // Safety: `handle` 来自仍然存活的 `NamedPipeServer`；`descriptor` 是出参。
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            info,
            None,
            None,
            None,
            None,
            Some(&mut descriptor),
        )
    };
    if status != WIN32_ERROR(0) || descriptor.0.is_null() {
        eprintln!("[svc][诊断] GetSecurityInfo 失败，Win32 错误码 {}。", status.0);
        return None;
    }

    let mut text = PWSTR::null();
    // Safety: `descriptor` 是有效的安全描述符；`text` 是出参，成功后由系统分配。
    let converted = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            info,
            &mut text,
            None,
        )
    };
    let result = match converted {
        Ok(()) if !text.0.is_null() => {
            // Safety: 转换成功，`text` 指向 NUL 结尾的宽字符串。
            unsafe { text.to_string().ok() }
        }
        Ok(()) => None,
        Err(err) => {
            eprintln!("[svc][诊断] 安全描述符转 SDDL 字符串失败：{err}");
            None
        }
    };

    // 两个缓冲区都由系统 `LocalAlloc` 分配，无论成败都要 `LocalFree`。
    unsafe {
        if !descriptor.0.is_null() {
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        }
        if !text.0.is_null() {
            let _ = LocalFree(Some(HLOCAL(text.0 as *mut c_void)));
        }
    }
    result
}

/// 建管道**之后**用 `SetSecurityInfo` 单独把完整性标签（SACL）写到管道对象上。
/// 返回 Win32 错误码（0 = 成功）。同样要求 `SeSecurityPrivilege` 已启用。
fn apply_label_to_pipe(server: &NamedPipeServer, sacl: *const windows::Win32::Security::ACL) -> u32 {
    use std::os::windows::io::AsRawHandle;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::Authorization::{SE_KERNEL_OBJECT, SetSecurityInfo};
    use windows::Win32::Security::LABEL_SECURITY_INFORMATION;

    let handle = HANDLE(server.as_raw_handle() as *mut c_void);
    // Safety: `handle` 来自仍然存活的管道对象；`sacl` 指向进程生命周期内有效的 SACL。
    let status = unsafe {
        SetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            LABEL_SECURITY_INFORMATION,
            None,
            None,
            None,
            Some(sacl),
        )
    };
    status.0
}

/// 首个管道实例上的安全诊断（增量 24 排查）：打印实际 SDDL；若完整性标签缺失，立刻用
/// `SetSecurityInfo` 兜底补设并再次回读。只在**首次**创建实例时执行一次。
fn diagnose_and_repair_pipe_label(
    server: &NamedPipeServer,
    sacl: Option<*const windows::Win32::Security::ACL>,
) {
    let before = describe_pipe_security(server);
    match &before {
        Some(sddl) => println!("[svc][诊断] 建管道后实际安全描述符：{sddl}"),
        None => eprintln!("[svc][诊断] 无法读取管道安全描述符。"),
    }

    // SDDL 里出现 `S:` 段才说明完整性标签真的挂上了。
    let label_ok = before.as_deref().is_some_and(|sddl| sddl.contains("S:"));
    if label_ok {
        println!("[svc][诊断] 完整性标签已在管道上生效，无需兜底。");
        return;
    }

    println!("[svc][诊断] 未检测到完整性标签，尝试用 SetSecurityInfo 兜底补设……");
    match sacl {
        Some(sacl) => {
            let code = apply_label_to_pipe(server, sacl);
            if code == 0 {
                println!("[svc][诊断] SetSecurityInfo 成功（错误码 0）。");
            } else {
                eprintln!("[svc][诊断] SetSecurityInfo 失败，Win32 错误码 {code}。");
            }
            match describe_pipe_security(server) {
                Some(sddl) => println!("[svc][诊断] 兜底后实际安全描述符：{sddl}"),
                None => eprintln!("[svc][诊断] 兜底后仍无法读取安全描述符。"),
            }
        }
        None => eprintln!("[svc][诊断] 无可用的 SACL 指针，跳过兜底（完整性标签将缺失）。"),
    }
}

/// 启用当前进程令牌中的 `SeSecurityPrivilege`（建管道前必须做）。
///
/// **为什么必须显式启用**：`SeSecurityPrivilege` 即便在管理员令牌里也**默认是禁用**的。
/// 而 `CreateNamedPipe` 在**未启用**该特权时，会**静默丢弃** `SECURITY_ATTRIBUTES` 里的
/// SACL —— 调用返回成功、管道照建，但**完整性标签根本没生效**，Medium 完整性 UI 依旧
/// 被 MIC 拦在门外（`os error 5`）。这是本 Bug 最容易反复踩的坑：加了 SACL 却看不到
/// 任何报错。
///
/// 返回 `false` 表示特权未能启用（调用方**必须**视为致命错误并拒绝启动：SACL 必然无效）。
fn enable_security_privilege() -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE, LUID};
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
        SE_SECURITY_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // Safety: 全部句柄都在本函数内获取并释放；出参都是本地变量。
    unsafe {
        let mut token = HANDLE::default();
        if let Err(err) = OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        ) {
            eprintln!("[svc][错误] OpenProcessToken 失败：{err}");
            return false;
        }

        // `lpsystemname` 传 null = 本机；`lpname` 直接用 windows crate 的
        // `SE_SECURITY_NAME` 常量，避免硬编码字符串。
        let mut luid = LUID::default();
        if let Err(err) = LookupPrivilegeValueW(PCWSTR::null(), SE_SECURITY_NAME, &mut luid) {
            eprintln!("[svc][错误] LookupPrivilegeValueW(SeSecurityPrivilege) 失败：{err}");
            let _ = CloseHandle(token);
            return false;
        }

        let new_state = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        if let Err(err) = AdjustTokenPrivileges(token, false, Some(&new_state), 0, None, None) {
            eprintln!("[svc][错误] AdjustTokenPrivileges 失败：{err}");
            let _ = CloseHandle(token);
            return false;
        }

        // `AdjustTokenPrivileges` 即使**没真正授予**特权也会返回成功（BOOL 为真），
        // 必须再查 `GetLastError` 才能确认。
        let last = GetLastError();
        let _ = CloseHandle(token);
        if last == ERROR_NOT_ALL_ASSIGNED {
            eprintln!(
                "[svc][错误] SeSecurityPrivilege 未能授予（ERROR_NOT_ALL_ASSIGNED=1300）——\
                 当前令牌不含该特权，管道 SACL 必然无效。"
            );
            return false;
        }
        println!("[svc] SeSecurityPrivilege 已启用（AdjustTokenPrivileges 后 GetLastError={}）。", last.0);
        true
    }
}

/// 6800 代理服务端的停机句柄（auto / native 模式才有）。
struct ProxyGuard {
    /// `None` 表示已经打过停机标记（幂等）。
    server: Option<router::RpcServerHandle>,
}

impl ProxyGuard {
    fn new(server: router::RpcServerHandle) -> Self {
        Self {
            server: Some(server),
        }
    }

    /// 同步打标：用于 `EngineHandle::kill()` 这类不能 await 的路径。
    ///
    /// 丢弃句柄 = 丢弃 shutdown 发送端，accept 循环的 `changed()` 立即返回 `Err` 并退出
    /// （端口释放由监听器 drop 完成；连接任务随进程退出/超时自然结束）。
    fn trip(mut self) {
        self.server.take();
    }

    /// 打标并等待 accept 循环退出（端口随之释放）。
    async fn stop(mut self) {
        if let Some(server) = self.server.take() {
            server.shutdown().await;
        }
    }
}

/// 引擎句柄。持有 `Child` 即持有引擎的"守护契约"：
/// 本进程消失 → Job Object 句柄关闭 → 内核自动 kill 引擎，不依赖任何用户态清理。
struct EngineHandle {
    /// UI 连接 6800 使用的 RPC 密钥（服务生成，随引擎重启自然轮换）。
    ///
    /// 代理模式下它是 **6800 的密钥**；`--engine=aria2` 回滚档下它与 aria2c 的
    /// `--rpc-secret` 是同一个值（与 v0.9.4 完全一致）。
    secret: String,
    /// 引擎进程 PID（对外状态查询用）。
    pid: u32,
    child: Child,
    /// 止损检测：退出原因只记录一次（`alive()` 会被状态轮询反复调用）。
    exit_logged: bool,
    /// 6800 代理（仅 auto / native 模式）。
    proxy: Option<ProxyGuard>,
}

impl EngineHandle {
    /// 进程是否仍存活。内部会 `try_wait` 顺带回收僵尸进程。
    ///
    /// 止损检测（C-3 Phase 0）：首次观测到退出时，把退出码写进 service.log。
    /// 这是 Q10 方案 A 的前置 —— 没有退出码，就无法区分「正常收敛」「超时退出」
    /// 与「访问违例（疑似 c-ares/DNS 崩溃）」。
    fn alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                if !self.exit_logged {
                    self.exit_logged = true;
                    log_engine_exit(self.pid, status);
                }
                false
            }
            Err(err) => {
                if !self.exit_logged {
                    self.exit_logged = true;
                    eprintln!("[svc][引擎退出] pid={} 无法获取退出状态：{err}", self.pid);
                }
                false
            }
        }
    }

    /// 强杀并回收（幂等）。仅在确认需要终止时调用。
    fn kill(&mut self) {
        // 6800 代理先停：避免引擎已死却仍有入口把请求转给空引擎。
        if let Some(proxy) = self.proxy.take() {
            proxy.trip();
        }
        // 关键路径：终止/回收子进程失败会导致残留进程锁住端口与目标文件，必须留痕。
        // （服务 bin 无 tracing 订阅者，沿用 eprintln! 风格保证可见。）
        if let Err(err) = self.child.kill() {
            eprintln!("[svc][警告] 终止 aria2c 子进程失败（可能已自行退出）：{err}");
        }
        if let Err(err) = self.child.wait() {
            eprintln!("[svc][警告] 回收 aria2c 子进程失败（可能残留僵尸）：{err}");
        }
    }
}

/// 把引擎退出码翻译成人能读的原因，并落 service.log。
///
/// 只做记录，不做任何自动重启/降级：止血动作必须显式、可审计（红线 R3）。
fn log_engine_exit(pid: u32, status: std::process::ExitStatus) {
    #[cfg(windows)]
    {
        // Windows 没有信号概念：异常终止（如访问违例）也以 >= 0x8000_0000 的退出码表达，
        // `code()` 会把它按 i32 返回，这里再按 u32 还原成 NTSTATUS 形态便于比对。
        let raw = status.code().map(|code| code as u32).unwrap_or(u32::MAX);
        let reason = match raw {
            0 => "正常退出",
            0xC000_0005 => "ACCESS_VIOLATION（访问违例，疑似崩溃）",
            0xC000_00FD => "STACK_OVERFLOW（栈溢出）",
            0xC000_0409 => "STACK_BUFFER_OVERRUN / __fastfail",
            0xC000_0374 => "HEAP_CORRUPTION（堆损坏）",
            0xC000_0135 => "UNHANDLED_EXCEPTION（未处理异常）",
            _ => "未知（非 0 退出）",
        };
        eprintln!("[svc][引擎退出] pid={pid} exit=0x{raw:08X}（{reason}）");
    }
    #[cfg(not(windows))]
    {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        eprintln!("[svc][引擎退出] pid={pid} exit={code}");
    }
}

/// 服务级"退出信号"。
///
/// 用 `AtomicBool` + `Notify` 的组合：`AtomicBool` 负责"状态可查"（accept 循环每轮
/// 先自查），`Notify::notify_one` 负责"可靠唤醒"（`notify_one` 会存下一个 permit，
/// 哪怕叫醒时主循环还没进 `notified()`，下一轮也会立即返回 —— 不存在丢唤醒的竞态）。
struct Stop {
    flag: AtomicBool,
    notify: Notify,
}

impl Stop {
    fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    fn trip(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    fn tripped(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

/// 取共享锁，忽略 poison（锁内只做纯数据处理，panic 后状态依然可用，
/// 没必要让一次 panic 把整个服务拖垮）。
fn lock_engine(engine: &Arc<StdMutex<Option<EngineHandle>>>) -> MutexGuard<'_, Option<EngineHandle>> {
    engine.lock().unwrap_or_else(|e| e.into_inner())
}

/// 核心服务启动前的**提权自检**（增量 24 §1；对应增量 23 报告 §8.1 的"提权地雷"）。
///
/// 为什么必须拒绝启动、而不是降级运行：`--file-allocation=falloc` 依赖
/// `SE_MANAGE_VOLUME_NAME` 才能走通 `SetFileValidData`。非管理员下 aria2 会**退化为
/// 阻塞式填零**（其原文 WARN：`File will be allocated by filling zero, which blocks
/// whole aria2 execution.`）—— 注意这**不是**降级成 `trunc`，而是拖死整个引擎。
/// 与其让用户在大文件上无声踩进性能悬崖，不如直接拒绝启动、逼其右键提权。
///
/// 判定依据：进程令牌的 `TokenElevation`。拿不到令牌时按**最保守**处理（视为未提权）。
#[cfg(windows)]
fn is_elevated() -> bool {
    use std::ffi::c_void;

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }

        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned: u32 = 0;
        let queried = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
        .is_ok();

        let _ = CloseHandle(token);
        queried && elevation.TokenIsElevated != 0
    }
}

/// 非 Windows 平台没有提权概念，直接放行（保证跨平台编译）。
#[cfg(not(windows))]
fn is_elevated() -> bool {
    true
}

/// 未提权时的醒目横幅 + 立即退出（退出码 1，便于脚本判定失败）。
fn abort_not_elevated() -> ! {
    // 不用 ANSI 颜色：Release 下这些输出直接进 service.log，转义序列只会变成乱码。
    eprintln!(
        "================================================================\n\
         [svc][严重] 未以管理员身份运行，拒绝启动！\n\
         [svc][严重] 大文件预分配将退化为「阻塞式填零」，拖死整个引擎。\n\
         [svc] 请右键 aria2-service.exe →「以管理员身份运行」，\n\
         [svc] 或直接运行项目根目录的 start_dev.ps1。\n\
         ================================================================"
    );
    std::process::exit(1)
}

/// 增量 41：DLL 安全加载 —— 把默认 DLL 搜索路径锁死为 `System32`。
///
/// Windows 默认搜索顺序会先查 exe 所在目录与当前工作目录。若服务目录可写
/// （例如被解压到用户目录），攻击者放入同名恶意 DLL 即可劫持本服务（DLL planting）。
/// `SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32)` 影响其后所有
/// `LoadLibrary*` 调用，因此必须在 `main()` 最前面、任何动态加载之前执行。
#[cfg(windows)]
fn harden_dll_search_path() {
    use windows::Win32::System::LibraryLoader::{
        SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32,
    };

    // Safety: 进程启动早期调用一次，参数为合法常量；失败仅告警，不阻止服务运行。
    match unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) } {
        Ok(()) => println!("[svc] DLL 默认搜索路径已锁定为 System32（防劫持）。"),
        Err(err) => eprintln!("[svc][警告] 锁定 DLL 搜索路径失败（继续运行）：{err}"),
    }
}

#[cfg(not(windows))]
fn harden_dll_search_path() {}

/// Release 下把进程 stdout / stderr 重定向到 `<HOME>/data/logs/service.log`（追加写）。
///
/// 为什么用**句柄重定向**而不是逐条替换 `println!`：服务的输出有两个来源 ——
/// 自身诊断，以及 [`drain`] 抽干的 `aria2c` stdout/stderr。逐条改造既啰嗦又易漏；
/// 而 Rust 的 `std::io::stdout()` 是在**首次打印时**才 `GetStdHandle`，
/// 因此在 `main` 最开头 `SetStdHandle` 之后，所有 `println!` / `eprintln!`
/// 自动落盘，调用点零改动。
///
/// 返回值必须存活到进程结束（std 直接持有该句柄，提前 drop 会让后续输出失败）。
/// 任一步失败都静默返回 `None`：日志不可用绝不能拖垮服务。
#[cfg(all(windows, not(debug_assertions)))]
fn redirect_streams_to_log() -> Option<std::fs::File> {
    use std::io::Write;
    use std::os::windows::io::AsRawHandle;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};

    let dir = paths::logs_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("service.log");

    // 轮转：8 MiB 上限，保留一份上一代即可（服务日志是排障用，不需要长期归档）。
    if std::fs::metadata(&path)
        .map(|m| m.len() > 8 * 1024 * 1024)
        .unwrap_or(false)
    {
        let rotated = dir.join("service.log.1");
        let _ = std::fs::remove_file(&rotated);
        let _ = std::fs::rename(&path, &rotated);
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    let handle = HANDLE(file.as_raw_handle() as _);
    // Safety: 句柄来自仍然存活的 `file`；`SetStdHandle` 只登记句柄值，不转移所有权。
    unsafe {
        let _ = SetStdHandle(STD_OUTPUT_HANDLE, handle);
        let _ = SetStdHandle(STD_ERROR_HANDLE, handle);
    }
    let _ = writeln!(
        file,
        "===== aria2-service 启动 pid={} home={} =====",
        std::process::id(),
        paths::home().display()
    );
    Some(file)
}

/// Debug（或非 Windows）下不重定向：开发期保留控制台输出。
#[cfg(not(all(windows, not(debug_assertions))))]
fn redirect_streams_to_log() {}

/// C-3 引擎选择：`--engine=auto|aria2|native`（也支持环境变量 `ARIA2_SERVICE_ENGINE`）。
///
/// 解析与语义统一由 [`aria2_harness_lib::router::EngineMode`] 提供，service 只负责
/// 在启动时校验「本次构建是否具备该引擎」。
static ENGINE_MODE: std::sync::OnceLock<aria2_harness_lib::router::EngineMode> =
    std::sync::OnceLock::new();

fn engine_mode() -> aria2_harness_lib::router::EngineMode {
    *ENGINE_MODE.get_or_init(|| {
        use aria2_harness_lib::router::EngineMode;
        let from_arg = std::env::args()
            .skip(1)
            .find_map(|arg| arg.strip_prefix("--engine=").map(str::to_owned));
        let raw = from_arg.or_else(|| std::env::var("ARIA2_SERVICE_ENGINE").ok());
        match EngineMode::parse(raw.as_deref()) {
            Ok(mode) => mode,
            Err(message) => {
                eprintln!("[svc][严重] {message}");
                std::process::exit(2);
            }
        }
    })
}

/// 打印并校验引擎模式。`native` 在未带 feature 的构建里明确拒绝启动。
fn resolve_engine_mode() -> aria2_harness_lib::router::EngineMode {
    let mode = engine_mode();
    if mode == aria2_harness_lib::router::EngineMode::Native {
        #[cfg(not(feature = "native-http"))]
        {
            eprintln!(
                "[svc][严重] --engine=native 需要以 `--features native-http` 编译；\
                 本次构建不含 engine-net。请改用 --engine=auto 或 aria2。"
            );
            std::process::exit(4);
        }
    }
    println!("[svc] 引擎模式：{}", mode.as_str());
    mode
}

fn main() {
    // 必须最先执行：一旦有任何输出被打印，std 就缓存了旧的（无控制台 = 空的）句柄。
    let _log_guard = redirect_streams_to_log();

    // 可观测性底座：engine-net 调度器的 `[scheduler]` 日志走 tracing，
    // 服务进程若不初始化就全会丢进无底洞（前端排障看不到分片降级原因）。
    aria2_harness_lib::metrics::init_tracing();

    // 安全加载必须在最前面：一旦有 DLL 被 LoadLibrary 加载，再设置就晚了。
    harden_dll_search_path();

    // 提权自检必须在**任何线程被拉起之前**完成：因此这里刻意不用 `#[tokio::main]`
    // —— 多线程 runtime 在 `Builder::build()` 时就已经启动了 worker 线程，
    // 而 `#[tokio::main]` 会把 runtime 的创建放在函数体之前，做不到"零线程"。
    if !is_elevated() {
        abort_not_elevated();
    }

    // C-3：引擎选择开关。放在 SCM 分发之前，确保服务模式与独立进程模式的判定一致。
    resolve_engine_mode();

    // 增量 53：优先作为 Windows 系统服务（SCM）运行。
    //
    // `StartServiceCtrlDispatcherW` 只有在「由 SCM 拉起」时才会阻塞并成功；
    // 双击 / 计划任务 / 命令行启动时它会立即失败，错误码固定为
    // `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT (1063)` —— 此时退回独立进程模式，
    // 行为与旧版完全一致（计划任务、UAC 兜底都还能用）。
    #[cfg(windows)]
    {
        match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
            Ok(()) => return,
            Err(windows_service::Error::Winapi(err)) if err.raw_os_error() == Some(1063) => {
                println!("[svc] 未由 SCM 拉起（1063），进入独立进程模式");
            }
            Err(err) => {
                // 真正被 SCM 拉起却初始化失败：写日志后退出，SCM 会记录服务错误，
                // 绝不能悄悄转成控制台模式（那会让 SCM 永远等不到 RUNNING）。
                eprintln!("[svc][严重] 以系统服务方式启动失败：{err}");
                std::process::exit(1);
            }
        }
    }

    run_console();
}

/// 独立进程模式（旧路径）：自建 runtime，跑命名管道主循环。
fn run_console() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("[svc] 创建 tokio runtime 失败");
    runtime.block_on(async_main(Arc::new(Stop::new())));
}

/// 系统服务模式的 `ServiceMain`。
///
/// 必须在这里完成三件事，缺一就是 1053（服务没有及时响应启动请求）：
/// 1. 注册控制处理器（接收 Stop / Shutdown）；
/// 2. 立刻 `SetServiceStatus(RUNNING)`；
/// 3. 才进入可能长时间阻塞的主循环。
#[cfg(windows)]
fn service_main(_arguments: Vec<std::ffi::OsString>) {
    if let Err(err) = run_as_scm_service() {
        eprintln!("[svc][严重] 服务主循环异常退出：{err}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
fn run_as_scm_service() -> windows_service::Result<()> {
    let stop = Arc::new(Stop::new());
    let handler_stop = Arc::clone(&stop);

    let event_handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                println!("[svc] 收到 SCM 停止指令，开始收敛引擎");
                handler_stop.trip();
                ServiceControlHandlerResult::NoError
            }
            // Interrogate 必须回 NoError，否则 `sc queryex` / 服务管理器会报错。
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    // 先报 RUNNING 再干别的：管道 SDDL 解析、runtime 创建都可能耗时。
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;
    println!("[svc] 已作为 Windows 服务运行（{SERVICE_NAME}）");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("[svc] 创建 tokio runtime 失败");
    runtime.block_on(async_main(Arc::clone(&stop)));

    // 主循环已收敛（引擎已 kill / 回收），向 SCM 报 STOPPED 后进程退出。
    let _ = status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    });
    Ok(())
}

async fn async_main(stop: Arc<Stop>) {
    let pipe = service_protocol::pipe_name();
    println!("[svc] aria2-service 启动中……");
    println!("[svc] 命名管道：{pipe}");

    let engine: Arc<StdMutex<Option<EngineHandle>>> = Arc::new(StdMutex::new(None));
    // `stop` 由调用方传入：SCM 模式下与 `ServiceControlHandler` 共享同一实例，
    // 停止指令才能真正穿透到 accept 循环。

    // 管道安全属性（增量 24 修复「UAC 完整性隔离」）：必须显式给出 DACL，否则默认
    // DACL 会拒绝普通权限（Medium 完整性）的 UI 进程连接 → `os error 5 拒绝访问`。
    // 该指针在进程生命周期内一直有效，accept 循环反复创建实例时复用同一份即可。
    //
    // 注意顺序：`S:(ML;;NW;;;S-1-16-8192)` 这条 SACL 必须**先启用 `SeSecurityPrivilege`**
    // 才能生效 —— 未启用时 `ConvertStringSecurityDescriptorToSecurityDescriptorW` 与
    // `CreateNamedPipe` 都会静默剥离 SACL，管道标签仍是 High。
    // 因此这里把它当作**致命前置条件**：失败直接拒绝启动，绝不带病运行。
    if !enable_security_privilege() {
        eprintln!(
            "================================================================\n\
             [svc][严重] 启用 SeSecurityPrivilege 失败！\n\
             [svc][严重] 管道完整性标签（SACL）必然无法生效，普通权限 UI 无法连接。\n\
             [svc][严重] 拒绝启动。请确认以管理员身份运行本程序。\n\
             ================================================================"
        );
        std::process::exit(1);
    }

    println!("[svc][诊断] 目标管道 SDDL：{PIPE_SDDL}");
    let security_attributes = pipe_security_attributes();
    if security_attributes.is_none() {
        eprintln!(
            "[svc][警告] 未能构造管道安全属性，将退回默认 DACL/SACL —— 普通权限 UI 可能连不上。"
        );
    }
    // 兜底用的 SACL 指针（建管道后若发现标签缺失，由 `SetSecurityInfo` 单设）。
    let label_sacl = label_sacl();
    if label_sacl.is_none() {
        eprintln!("[svc][错误] 未能构造完整性标签 SACL，SetSecurityInfo 兜底将不可用。");
    }

    // 命名管道 accept 循环：一次连接处理一个请求，处理完立刻挂出下一个监听实例。
    let mut first = true;
    loop {
        if stop.tripped() {
            break;
        }

        let mut options = ServerOptions::new();
        options.first_pipe_instance(first);
        // Safety: `security_attributes`（若为 `Some`）指向一个在整个进程生命周期内
        // 都有效的 `SECURITY_ATTRIBUTES`；传入 null 时行为等同 `create`（退回默认 DACL）。
        let created = match security_attributes {
            Some(attrs) => unsafe {
                options.create_with_security_attributes_raw(pipe.as_str(), attrs)
            },
            None => options.create(pipe.as_str()),
        };

        // 首个实例带 `FILE_FLAG_FIRST_PIPE_INSTANCE`：若管道名已存在（另一个
        // aria2-service 正在运行），`create` 立刻失败 —— 天然的"单实例"闸门。
        let server = match created {
            Ok(server) => server,
            Err(err) => {
                if first {
                    eprintln!("[svc] 无法创建命名管道 {pipe}：{err}");
                    eprintln!("[svc] 提示：可能已有另一个 aria2-service 实例在运行，请先关闭它。");
                    std::process::exit(1);
                }
                eprintln!("[svc] 创建命名管道实例失败：{err}");
                break;
            }
        };
        // 首个实例上做一次安全诊断：打印管道**实际生效**的 SDDL，并在完整性标签缺失时
        // 用 `SetSecurityInfo` 兜底补设（部分 Windows 版本对 SECURITY_ATTRIBUTES 里的
        // SACL 支持不完整）。只在首次执行，后续实例复用同一份管道对象的安全信息。
        if first {
            diagnose_and_repair_pipe_label(&server, label_sacl);
        }
        first = false;

        // 等待 UI 连入；期间随时可被 shutdown 指令叫醒。
        tokio::select! {
            result = server.connect() => {
                if let Err(err) = result {
                    eprintln!("[svc] 等待客户端连接失败：{err}");
                    continue;
                }
            }
            _ = stop.notify.notified() => break,
        }

        let engine = engine.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            if let Err(err) = serve_one(server, engine, stop).await {
                eprintln!("[svc] 连接处理结束：{err}");
            }
        });
    }

    // 退出流程：无论走 shutdown 指令还是管道异常，都确保引擎被收敛（幂等）。
    shutdown_engine(&engine).await;
    println!("[svc] aria2-service 已退出");
}

/// 处理单个连接：读一行请求 → 路由 → 写一行响应。
async fn serve_one(
    server: NamedPipeServer,
    engine: Arc<StdMutex<Option<EngineHandle>>>,
    stop: Arc<Stop>,
) -> std::io::Result<()> {
    let mut conn = BufReader::new(server);
    let mut line = String::new();

    match tokio::time::timeout(READ_TIMEOUT, conn.read_line(&mut line)).await {
        // 客户端连上但没发数据就断开：静默收尾。
        Ok(Ok(0)) => return Ok(()),
        Ok(Ok(_)) => {}
        Ok(Err(err)) => {
            if let Err(e) =
                write_response(&mut conn, &ServiceResponse::err(format!("读取请求失败：{err}"))).await
            {
                eprintln!("[svc][警告] 写回「读取请求失败」响应失败（客户端可能已断开）：{e}");
            }
            return Ok(());
        }
        Err(_) => {
            if let Err(e) =
                write_response(&mut conn, &ServiceResponse::err("读取请求超时")).await
            {
                eprintln!("[svc][警告] 写回「读取超时」响应失败（客户端可能已断开）：{e}");
            }
            return Ok(());
        }
    }

    let resp = match decode_request(&line) {
        Ok(req) => route(req, &engine, &stop).await,
        Err(err) => ServiceResponse::err(err.to_string()),
    };
    write_response(&mut conn, &resp).await
}

/// 写出单行响应。编码失败也不允许"无声沉默"——回一条固定的失败报文。
async fn write_response<W>(writer: &mut W, resp: &ServiceResponse) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let line = encode_response(resp)
        .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"响应编码失败\"}\n".to_string());
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await
}

/// 指令路由。
async fn route(
    req: ServiceRequest,
    engine: &Arc<StdMutex<Option<EngineHandle>>>,
    stop: &Arc<Stop>,
) -> ServiceResponse {
    match req {
        ServiceRequest::StartEngine { port } => start_engine(port, engine).await,
        ServiceRequest::GetStatus => {
            let mut guard = lock_engine(engine);
            // 注意：`match` 的 pattern guard 里绑定值是不可变的，无法调用 `&mut self` 的
            // `alive()`（它内部要 `try_wait` 回收僵尸进程），因此存活判定必须写进 arm 体内。
            match guard.as_mut() {
                Some(handle) => {
                    if handle.alive() {
                        // 同时回传当前密钥：引擎被重新拉起后密钥会轮换，
                        // UI 侧重连时需要靠它重新鉴权（只读，不触发拉起）。
                        ServiceResponse::engine_status_with_secret(
                            true,
                            Some(handle.pid),
                            Some(handle.secret.clone()),
                        )
                    } else {
                        ServiceResponse::engine_status(false, None)
                    }
                }
                None => ServiceResponse::engine_status(false, None),
            }
        }
        ServiceRequest::Shutdown => {
            println!("[svc] 收到 shutdown 指令，开始优雅退出");
            shutdown_engine(engine).await;
            stop.trip();
            ServiceResponse::ok()
        }
    }
}

/// 启动引擎（幂等：已在运行则直接交回同一份密钥）。
///
/// 两条路径（C-3 Phase 2 §6.3）：
/// - `--engine=aria2`：**回滚档**，与 v0.9.4 完全一致 —— aria2c 直接监听 `port`，
///   不启 6800 代理，密钥由服务生成并原样交给 UI；
/// - `auto` / `native`：6800 交给 Router 代理，aria2c 降级到内部端口 `port+1`，
///   native 引擎与本进程同址；UI 永远只连 6800。
async fn start_engine(port: u16, engine: &Arc<StdMutex<Option<EngineHandle>>>) -> ServiceResponse {
    // 复用判定 + 陈旧句柄回收：两者必须在同一把锁内完成，否则并发 start 会重复拉起。
    let stale = {
        let mut guard = lock_engine(engine);
        if let Some(handle) = guard.as_mut() {
            if handle.alive() {
                println!("[svc] 引擎已在运行（pid={}），复用现有实例", handle.pid);
                return ServiceResponse::engine_started(handle.secret.clone(), handle.pid);
            }
        }
        guard.take()
    };
    if let Some(mut handle) = stale {
        println!("[svc] 引擎进程已退出（pid={}），回收并重新拉起", handle.pid);
        // 先收代理：端口 6800 必须在我们重新绑定前真正释放。
        if let Some(proxy) = handle.proxy.take() {
            proxy.stop().await;
        }
    }

    let secret = uuid::Uuid::new_v4().to_string();
    match engine_mode() {
        EngineMode::Aria2 => start_direct_engine(port, secret, engine).await,
        EngineMode::Auto | EngineMode::Native => {
            start_proxy_engine(port, secret, &engine_mode(), engine).await
        }
    }
}

/// 回滚档：aria2c 直接对外监听 `port`（与 v0.9.4 逐字节一致）。
async fn start_direct_engine(
    port: u16,
    secret: String,
    engine: &Arc<StdMutex<Option<EngineHandle>>>,
) -> ServiceResponse {
    // 端口占用探测：只报错、不自动 kill —— 占用者可能是用户手动运行的 aria2c，误杀代价更高。
    if port_in_use(port) {
        return ServiceResponse::err(port_busy_message(port));
    }

    let handle = match spawn_engine(port, secret) {
        Ok(handle) => handle,
        Err(err) => return ServiceResponse::err(err.to_string()),
    };

    let pid = handle.pid;
    let resp = ServiceResponse::engine_started(handle.secret.clone(), handle.pid);
    *lock_engine(engine) = Some(handle);

    if !wait_port(port, ENGINE_READY_TIMEOUT).await {
        println!(
            "[svc] 警告：引擎进程（pid={pid}）已创建，但 RPC 端口 {port} 在 {}s 内未就绪；UI 侧会自行重试连接",
            ENGINE_READY_TIMEOUT.as_secs()
        );
    }
    resp
}

/// 代理档：6800 对外（Router），aria2c 退到 `port+1`，native 引擎同进程。
///
/// 密钥双轨：`public_secret` 给 UI 连 6800；`inner_secret` 只用于 6800 → 6801。
async fn start_proxy_engine(
    port: u16,
    public_secret: String,
    mode: &EngineMode,
    engine: &Arc<StdMutex<Option<EngineHandle>>>,
) -> ServiceResponse {
    let Some(inner_port) = port.checked_add(1) else {
        return ServiceResponse::err(format!("RPC 端口 {port} 无法推导内部引擎端口（+1 溢出）"));
    };
    if port_in_use(port) {
        return ServiceResponse::err(port_busy_message(port));
    }
    if port_in_use(inner_port) {
        return ServiceResponse::err(format!(
            "内部引擎端口 {inner_port} 已被占用：请结束占用该端口的进程后重试\
             （本服务不会自动结束它，以免误杀）。"
        ));
    }

    let inner_secret = uuid::Uuid::new_v4().to_string();
    let aria2 = Arc::new(Aria2Client::new(
        format!("ws://127.0.0.1:{inner_port}/jsonrpc"),
        inner_secret.clone(),
    ));

    #[cfg(feature = "native-http")]
    let native_engine = match build_native_engine() {
        Ok(engine) => Some(engine),
        Err(err) => return ServiceResponse::err(err),
    };

    #[cfg(feature = "native-http")]
    let native_client: Option<Arc<dyn EngineClient>> = native_engine
        .as_ref()
        .map(|engine| Arc::new(NativeClient::new(Arc::clone(engine))) as Arc<dyn EngineClient>);
    #[cfg(not(feature = "native-http"))]
    let native_client: Option<Arc<dyn EngineClient>> = None;

    let router = Router::new(
        *mode,
        Arc::clone(&aria2) as Arc<dyn EngineClient>,
        native_client,
    );
    // 6800 先绑：绑定失败时还没拉起 aria2c，无需回滚进程。
    let server = match router::start_server(Arc::clone(&router), port, public_secret.clone()).await {
        Ok(server) => server,
        Err(err) => {
            return ServiceResponse::err(format!(
                "RPC 端口 {port} 绑定失败（6800 单入口未能启动）：{err}"
            ));
        }
    };
    let guard = ProxyGuard::new(server);

    // 引擎进程：失败必须立刻释放刚绑定的 6800，绝不留下"能连但没引擎"的半启动状态。
    let mut handle = match spawn_engine(inner_port, inner_secret) {
        Ok(handle) => handle,
        Err(err) => {
            guard.stop().await;
            return ServiceResponse::err(err.to_string());
        }
    };

    // 事件转发：两个引擎的通知统一进 Router 的广播源，由 6800 分发给各连接。
    let events = router.events();
    router::spawn_aria2_forwarder(aria2.client().clone(), events.clone());
    #[cfg(feature = "native-http")]
    if let Some(native_engine) = native_engine.as_ref() {
        router::spawn_native_forwarder(Arc::clone(native_engine), events.clone());
    }

    handle.proxy = Some(guard);
    // 代理档下 `handle.secret` 必须是**对外**的 6800 密钥：`GetStatus` 会把它
    // 回传给 UI 做重连鉴权（见 `route` 的 GetStatus 分支）。`spawn_engine` 只认识
    // 引擎侧密钥，这里必须覆盖；否则 UI 重连后拿着 inner_secret 调 6800，
    // 表现为"连接正常但所有 RPC 都 Unauthorized"。
    handle.secret = public_secret.clone();
    let pid = handle.pid;
    let resp = ServiceResponse::engine_started(public_secret, pid);
    *lock_engine(engine) = Some(handle);

    println!("[svc] 6800 代理已就绪：对外 {port}，内部 aria2c {inner_port}");
    if !wait_port(port, ENGINE_READY_TIMEOUT).await {
        println!(
            "[svc] 警告：代理服务端（pid={pid} 的引擎）已创建，但 RPC 端口 {port} 在 {}s 内未就绪",
            ENGINE_READY_TIMEOUT.as_secs()
        );
    }
    resp
}

/// 端口占用的统一话术（回滚档 / 代理档共用）。
fn port_busy_message(port: u16) -> String {
    format!(
        "RPC 端口 {port} 已被占用：可能是上次残留的 aria2c，或你手动启动的引擎。\
         请自行结束该进程后重试（本服务不会自动结束它，以免误杀）。"
    )
}

/// 构造进程内 native 引擎（engine-net）。
#[cfg(feature = "native-http")]
fn build_native_engine() -> Result<Arc<engine_net::rpc::methods::Engine>, String> {
    let resolver = engine_net::dns::resolver::DnsResolver::new(
        &engine_net::dns::resolver::DnsConfig::default(),
    )
    .map_err(|err| format!("native 引擎 DNS 初始化失败：{err}"))?;
    let engine = Arc::new(engine_net::rpc::methods::Engine::new(resolver));
    // 与 aria2c 的 `--dir` 对齐（同样读 config.json），保证两条引擎落到同一目录。
    engine.set_global_option(
        "dir",
        &aria2_harness_lib::configured_download_dir().to_string_lossy(),
    );
    // 红线：产品默认 `--sqlite-progress=true`，控制文件写 `<file>.sqlite`。
    engine.set_global_option("sqlite-progress", "true");
    Ok(engine)
}

/// 系统代理的探测来源（只用于启动日志，实机排错时一眼看出走的是哪条路径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProxySource {
    Env,
    Registry,
}

impl ProxySource {
    fn label(self) -> &'static str {
        match self {
            ProxySource::Env => "环境变量",
            ProxySource::Registry => "系统设置",
        }
    }
}

/// 把各种"代理写法"规整成 aria2 能吃的 URI。
///
/// 现场常见的三种形态：
/// 1. `127.0.0.1:7890`（Clash 默认的简单写法）；
/// 2. `http://...` / `https://...`（环境变量里带 scheme）；
/// 3. `http=127.0.0.1:7890;https=...`（Windows 设置 / 组策略的多协议写法）。
///
/// **只接受 HTTP 代理**：aria2 的 `HttpProxyOptionHandler`（OptionHandlerImpl.cc:501）
/// 只认 `http://` / `https://` / `ftp://`，并把 protocol 强制改成 `http` —— 源码里
/// 完全没有 SOCKS 实现。下发 `socks5://...` 会被拼成 `http://socks5://...`，
/// 解析失败直接 `DL_ABORT_EX`，**引擎启动即退出**（比"没代理"糟糕得多）。
/// 因此 SOCKS 一律返回 `None`：宁可直连，也绝不让引擎起不来。
///
/// 认不出、缺端口、带路径（PAC 地址）同样返回 `None`。
fn normalize_proxy(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }

    if value.contains('=') && !value.contains("://") {
        let mut http = None;
        let mut https = None;
        for part in value.split(';') {
            let Some((name, addr)) = part.split_once('=') else {
                continue;
            };
            let slot = match name.trim().to_ascii_lowercase().as_str() {
                "http" => &mut http,
                "https" => &mut https,
                // socks / ftp 等：aria2 不支持，跳过。
                _ => continue,
            };
            if slot.is_none() {
                *slot = Some(addr.trim().to_string());
            }
        }
        return with_proxy_scheme("http", http.or(https)?.as_str());
    }

    if let Some((scheme, rest)) = value.split_once("://") {
        return match scheme.to_ascii_lowercase().as_str() {
            // aria2 会把 https:// 代理也按明文 http 连接（us.protocol = "http"），
            // 这里提前归一，日志里看到的值即引擎实际使用的值。
            "http" | "https" => with_proxy_scheme("http", rest),
            _ => None,
        };
    }
    with_proxy_scheme("http", value)
}

/// 拼 scheme 前先校验 `host:port`：端口必须是纯数字（可带 `user:pass@` 前缀）。
fn with_proxy_scheme(scheme: &str, addr: &str) -> Option<String> {
    let scheme = scheme.to_ascii_lowercase();
    let addr = addr.trim();
    let addr = addr.strip_suffix('/').unwrap_or(addr);
    if addr.is_empty() || addr.starts_with(':') || addr.starts_with('/') {
        return None;
    }
    let (_, port) = addr.rsplit_once(':')?;
    if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if port.parse::<u16>().map(|p| p == 0).unwrap_or(true) {
        return None;
    }
    Some(format!("{scheme}://{addr}"))
}

/// 环境变量里的代理：Clash / V2Ray 在部分安装方式下只写环境变量。
fn detect_proxy_from_env() -> Option<String> {
    const KEYS: [&str; 6] = [
        "ALL_PROXY",
        "all_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ];
    for key in KEYS {
        if let Ok(value) = std::env::var(key) {
            if let Some(proxy) = normalize_proxy(&value) {
                return Some(proxy);
            }
        }
    }
    None
}

/// WinINET 的"系统代理"（Clash「系统代理」开关写的就是这里）。
///
/// `ProxyEnable=0` 时系统可能走 PAC（`AutoConfigURL`）或直连。PAC 无法折算成
/// `--all-proxy`，强行下发只会把流量指到错误的地方，因此直接视为"无代理"。
#[cfg(windows)]
fn detect_proxy_from_registry() -> Option<String> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};
    use winreg::RegKey;

    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(
            r"Software\Microsoft\Windows\CurrentVersion\Internet Settings",
            KEY_READ,
        )
        .ok()?;
    let enabled: u32 = key.get_value("ProxyEnable").unwrap_or(0);
    if enabled == 0 {
        return None;
    }
    let server: String = key.get_value("ProxyServer").ok()?;
    normalize_proxy(&server)
}

#[cfg(not(windows))]
fn detect_proxy_from_registry() -> Option<String> {
    None
}

/// 探测系统代理：环境变量优先（显式配置更可信），其次系统设置。
fn detect_system_proxy() -> Option<(String, ProxySource)> {
    if let Some(proxy) = detect_proxy_from_env() {
        return Some((proxy, ProxySource::Env));
    }
    detect_proxy_from_registry().map(|proxy| (proxy, ProxySource::Registry))
}

/// 组装 aria2c 启动参数。
///
/// 与增量 22 相比，**唯一实质性差异**是 `--file-allocation`：由 `trunc` 恢复为
/// `falloc`。这是本增量的核心目标 —— 高权限服务父进程拉起引擎后，子进程继承
/// `SE_MANAGE_VOLUME_NAME` 特权，`falloc` 的 `SetFileValidData()` 才能成功，
/// 从而做到"秒级物理占位"（类 Steam 行为）。
///
/// 其余参数与增量 22 完全一致，保证 UI 侧的功能与 RPC 行为零变化。
fn build_args(port: u16, secret: &str, proxy: Option<&str>) -> Vec<String> {
    // 启动即读取**本地真实配置**（<HOME>/config.json）：覆盖安装 / 服务重启后
    // 引擎的初始 dir / split / 单服并发必须与设置页一致，而不是硬编码默认值。
    let download_dir = aria2_harness_lib::configured_download_dir()
        .to_string_lossy()
        .into_owned();
    let session = paths::session_path().to_string_lossy().into_owned();
    let mut args = vec![
        "--enable-rpc=true".to_string(),
        "--rpc-listen-all=false".to_string(),
        format!("--rpc-listen-port={port}"),
        format!("--rpc-secret={secret}"),
        // 下载根目录：显式下发绝对路径，避免 aria2 退化到"宿主启动 cwd"。
        format!("--dir={download_dir}"),
        // 并发基线：`--split` 是"一个任务最多切成几片"，`--max-connection-per-server`
        // 是"对同一台服务器最多同时开几条连接"，两者互不推导。
        //
        // 引擎已在本项目内解封：`OptionHandlerFactory.cc` 的 NumberOptionHandler
        // 上限由 16 提到 128。用户设置持久化在 `<HOME>/config.json`
        // （split_size / max_connections_per_server），这里启动即读，随后
        // `lib.rs::apply_task_concurrency` 还会按任务再兜一层。
        format!("--split={}", aria2_harness_lib::configured_split()),
        format!(
            "--max-connection-per-server={}",
            aria2_harness_lib::configured_connections()
        ),
        // 同时进行的任务数：aria2 原生默认同样为 5，设置页下拉框却没有 5 这个档位
        // （预设为 1/2/4/8/16/32/64/128）。不显式下发的话，UI 启动后从
        // getGlobalOption 读回 5，会与前端默认 8 不一致且落到无对应选项的空值。
        "--max-concurrent-downloads=8".to_string(),
        // 默认 UA 伪装：必须伪装成**纯现代浏览器**。任何 `aria2` / `Harness`
        // 关键词都会被 WAF 直接标记为下载器并拒绝（腾讯 CDN 即按此拦截）。
        // 前端设置页的 UA 默认值与此保持同步。
        "--user-agent=Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
         (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36"
            .to_string(),
        // 动态切割：让饥饿的快连接可抢占慢 peer 手中"尚未写入"的 piece。
        // 选项默认即为 true（BooleanOptionHandler 的 initialOption），此处显式下发只为
        // 让实机验证时的 `[svc] 启动引擎：...` 日志能直接比对到该开关。
        "--enable-dynamic-slicing=true".to_string(),
        // 注意：**不要**加 --max-http-pipelining。该选项在 OptionHandlerFactory.cc:1192
        // 被 op->hide() 隐藏，CLI 解析表里查不到，实机直接报
        // `unknown option -- max-http-pipelining=4` 并让引擎启动即退出（RPC 全 10061）。
        // --split 已在上面显式下发（64），这里不再追加第二个 split，避免同一选项重复下发。
        // 日志级别保持默认 notice：验证期曾临时拉到 info 以观测 MSG_NO_SEGMENT_AVAILABLE，
        // 抢占日志本身是 A2_LOG_NOTICE，notice 下依然可见。
        // 恢复物理预占（依赖服务侧高权限）。这是本增量存在的根本理由。
        "--file-allocation=falloc".to_string(),
        // 内存换速度：64 MiB 写缓存，把大量小写入合并成顺序刷盘，
        // 显著降低机械盘 / 高并发下的写放大（aria2 原生默认仅 16 MiB）。
        "--disk-cache=64M".to_string(),
        // 增量 40：BT 局域网加速（LPD）。同网段内的 aria2 节点会互相发现并交换
        // peer，宿舍/办公室场景无需公网端口映射即可提速。
        // 注意：**没有** --enable-upnp —— 本源码树不含任何 UPnP 实现
        // （全树 grep "upnp" 零匹配），强行下发会让 aria2c 报
        // `unknown option -- enable-upnp=true` 并以退出码 28 退出，引擎直接起不来。
        "--bt-enable-lpd=true".to_string(),
        // 任务持久化：启动加载既有会话、退出落盘，并每 15s 周期保存（双保险）。
        format!("--input-file={session}"),
        format!("--save-session={session}"),
        "--save-session-interval=15".to_string(),
        // 断点持久化：每 5s 落盘控制文件（*.aria2），避免强制关闭后进度归零。
        "--auto-save-interval=5".to_string(),
        // P2-C 配套开关：默认开启 HTTP gzip 解码（省流量 / 省磁盘）。
        // 之所以敢默认开：C++ 侧 HttpRequest::createRequest() 已加入 gzip/Range 智能互斥 ——
        // 只要本次请求会带 `Range:`（续传/分段），就不再发送 `Accept-Encoding: gzip`，
        // 从而避免服务端返回 gzip 编码的 206 触发"关闭分段下载"的旧缺陷。
        "--http-accept-gzip=true".to_string(),
        // ------------------------------------------------------------------
        // 增量 47：网络栈诊断（按实机反馈"微软镜像能下、腾讯/Ubuntu 全不可达"）
        // ------------------------------------------------------------------
        // 纯 IPv4 网络里，AAAA 记录可能指向黑洞地址：aria2 先试 IPv6 会一直等到超时，
        // 表现为"域名解析成功但连接不可达"。这里彻底关掉 AAAA 路径。
        "--disable-ipv6=true".to_string(),
        // 异步 DNS：不阻塞事件循环，且允许指定 DNS 服务器。
        "--async-dns=true".to_string(),
        // 国内稳定公共 DNS（阿里 + 腾讯）。系统 DNS 被运营商劫持时，
        // 这里能绕开污染；顺序即优先级。
        "--async-dns-server=223.5.5.5,119.29.29.29".to_string(),
        // 诊断日志：info 级别会记录"正在解析 xxx"与"连接到 x.x.x.x:443"，
        // 这是区分 DNS 污染 / IPv6 黑洞 / 代理拦截的唯一现场证据。
        // 输出由服务统一抽干进 <HOME>/data/logs/service.log（8 MiB 轮转）。
        "--console-log-level=info".to_string(),
        // 代理：优先由系统设置 / 环境变量探测；设置页手动填写的值会由 UI
        // 在连接后通过 changeGlobalOption 覆盖（引擎全局选项不跨进程保留）。
        // 未探测到就**不下发**，避免把空值当成"显式清空代理"。
    ];
    if let Some(proxy) = proxy {
        args.push(format!("--all-proxy={proxy}"));
    }
    args
}

/// 拉起 aria2c 进程。`secret` 由调用方给出：
/// - 回滚档（`--engine=aria2`）：就是 UI 直连用的密钥；
/// - 代理档（auto/native）：这是 **6800 → 6801 的内部密钥**，与 UI 拿到的公开密钥不同。
fn spawn_engine(port: u16, secret: String) -> Result<EngineHandle, AppError> {
    let aria2 = resolve_aria2_path()?;

    // 任务持久化前置：数据目录 / 下载目录 / 会话文件都必须存在，否则 aria2 启动即失败。
    let data_dir = paths::data_dir();
    std::fs::create_dir_all(&data_dir).map_err(|e| {
        AppError::Internal(format!("创建数据目录失败（{}）：{e}", data_dir.display()))
    })?;
    let download_dir = paths::downloads_dir();
    let session = paths::session_path();
    std::fs::create_dir_all(&download_dir).map_err(|e| {
        AppError::Internal(format!("创建下载目录失败（{}）：{e}", download_dir.display()))
    })?;
    // 旧版会话文件在 `<HOME>/downloads/aria2.session`，升级后一次性搬进 data，
    // 避免用户升级后丢失未完成任务的续传进度。
    let legacy_session = paths::home().join("downloads").join("aria2.session");
    if !session.exists() && legacy_session.is_file() {
        if let Err(e) = std::fs::rename(&legacy_session, &session) {
            tracing::warn!("迁移旧会话文件失败（{}）：{e}", legacy_session.display());
        }
    }
    if !session.exists() {
        std::fs::File::create(&session).map_err(|e| {
            AppError::Internal(format!("创建会话文件失败（{}）：{e}", session.display()))
        })?;
    }

    // 探测结果同时写进日志：用户报"某些站点不可达"时，第一眼就能确认
    // 到底有没有代理、走的是哪条来源。
    let proxy = detect_system_proxy();
    match &proxy {
        Some((url, source)) => {
            println!("[svc] 系统代理：{url}（来源：{}）", source.label());
        }
        None => println!(
            "[svc] 系统代理：未检测到（直连）。\
             若使用 Clash/V2Ray，请在「设置 → 网络 → 代理设置」填写其 **HTTP** 代理端口\
             （Clash 混合端口通常 7890，V2Ray HTTP 入站通常 10809）；\
             aria2 不支持 SOCKS 代理，SOCKS 端口无法使用。"
        ),
    }
    let args = build_args(port, &secret, proxy.as_ref().map(|(url, _)| url.as_str()));

    // 关键证据：把完整命令行打出来，实机验证可直接比对 `--file-allocation=falloc`。
    println!("[svc] 启动引擎：{} {}", aria2.display(), args.join(" "));

    let mut command = Command::new(&aria2);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // 关键：不给 Console 子进程分配窗口。不设这个标志，GUI 子系统的服务拉起
    // aria2c 时系统会新开一个黑色控制台（实机反馈的"巨大黑窗"）。
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);

    let mut child = command
        .spawn()
        .map_err(|e| AppError::Internal(format!("启动 aria2c 失败（{}）：{e}", aria2.display())))?;

    let pid = child.id();

    // Job Object 守护：本服务进程消失（正常退出 / 崩溃 / 被强杀）时内核自动回收引擎。
    match process_guard::attach_to_job(pid) {
        Ok(()) => println!("[svc] 已把引擎加入 Job Object（pid={pid}）"),
        Err(err) => eprintln!(
            "[svc] 加入 Job Object 失败（引擎仍可运行，但服务异常退出时可能残留）：{err}"
        ),
    }

    // 持续消费引擎输出，避免 stdout/stderr 管道缓冲写满把引擎阻塞死。
    if let Some(out) = child.stdout.take() {
        drain("aria2", out);
    }
    if let Some(err) = child.stderr.take() {
        drain("aria2", err);
    }

    Ok(EngineHandle {
        secret,
        pid,
        child,
        exit_logged: false,
        // 6800 代理由 `start_engine` 在服务端绑定成功后再挂上。
        proxy: None,
    })
}

/// 以独立 std 线程按行抽干子进程输出（绝不占用 tokio 事件循环）。
fn drain<R: std::io::Read + Send + 'static>(tag: &'static str, reader: R) {
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(reader).lines() {
            match line {
                Ok(text) => println!("[{tag}] {text}"),
                Err(_) => break,
            }
        }
    });
}

/// 解析 aria2c 可执行文件路径。
///
/// 红线：**不写死绝对路径**，全部通过环境变量或相对 exe 的位置推导：
/// 1. `ARIA2_SERVICE_ARIA2_PATH` 环境变量（显式覆盖，打包/多版本切换用）；
/// 2. 与本服务 exe **同级**的目录及其 `binaries/` 子目录
///    （`cargo run` 与打包后命中：`tauri-build` 会把 sidecar 压平拷成 `<exe_dir>/aria2c.exe`）；
/// 3. 相对源码目录回退：`<exe_dir>/../../binaries/<原始名>`
///    （`cargo run` 下 exe 在 `src-host/target/debug`，往上两级即 `src-host`）。
fn resolve_aria2_path() -> Result<PathBuf, AppError> {
    if let Some(explicit) = std::env::var(ARIA2_PATH_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        let path = PathBuf::from(&explicit);
        if path.exists() {
            return Ok(path);
        }
        return Err(AppError::Internal(format!("{ARIA2_PATH_ENV} 指向的路径不存在：{explicit}")));
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
    {
        for name in ARIA2_NAMES {
            candidates.push(exe_dir.join(name));
            candidates.push(exe_dir.join("binaries").join(name));
        }
        if let Some(src_tauri) = exe_dir.parent().and_then(|dir| dir.parent()) {
            for name in ARIA2_NAMES {
                candidates.push(src_tauri.join("binaries").join(name));
            }
        }
    }

    if let Some(hit) = candidates.iter().find(|path| path.exists()) {
        return Ok(hit.clone());
    }

    Err(AppError::Internal(format!(
        "未找到 aria2c 可执行文件，已尝试以下路径：\n{}",
        candidates
            .iter()
            .map(|path| format!("  - {}", path.display()))
            .collect::<Vec<_>>()
            .join("\n")
    )))
}

/// 探测 `127.0.0.1:{port}` 是否已可连接。
fn port_in_use(port: u16) -> bool {
    match format!("127.0.0.1:{port}").parse::<SocketAddr>() {
        Ok(addr) => std::net::TcpStream::connect_timeout(&addr, PORT_PROBE_TIMEOUT).is_ok(),
        Err(_) => false,
    }
}

/// 轮询等待端口就绪，返回是否在 `timeout` 内就绪。
async fn wait_port(port: u16, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if port_in_use(port) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// 收敛引擎进程（幂等）。
///
/// 顺序刻意如此：**先给引擎自行优雅退出的时间窗**（UI 侧在退出流程里已先发过
/// `aria2.shutdown` RPC，正常情况下只需等待），超时后才强杀 + 回收。
/// 等待期间**不持锁**（每轮单独取放），避免把 `start_engine` / `get_status` 堵死。
async fn shutdown_engine(engine: &Arc<StdMutex<Option<EngineHandle>>>) {
    for _ in 0..EXIT_POLL_TRIES {
        let alive = {
            let mut guard = lock_engine(engine);
            match guard.as_mut() {
                Some(handle) => handle.alive(),
                None => false,
            }
        };
        if !alive {
            break;
        }
        tokio::time::sleep(EXIT_POLL_INTERVAL).await;
    }

    // 取出句柄后不再持锁：`proxy.stop()` 需要 await，持锁会堵死 `get_status`。
    let Some(mut handle) = lock_engine(engine).take() else {
        return;
    };
    // 6800 先停：不再接受新的 UI 请求，避免引擎正在收尾时还有流量进来。
    if let Some(proxy) = handle.proxy.take() {
        proxy.stop().await;
    }
    if handle.alive() {
        println!("[svc] 引擎未自行退出，强制终止（pid={}）", handle.pid);
        handle.kill();
    } else {
        println!("[svc] 引擎已优雅退出（pid={}）", handle.pid);
    }
}

#[cfg(test)]
mod proxy_tests {
    use super::normalize_proxy;

    #[test]
    fn accepts_simple_host_port() {
        assert_eq!(
            normalize_proxy("127.0.0.1:7890").as_deref(),
            Some("http://127.0.0.1:7890")
        );
    }

    #[test]
    fn normalizes_https_and_rejects_socks() {
        // aria2 只会明文连代理，https:// 归一成 http://（与引擎内部行为一致）。
        assert_eq!(
            normalize_proxy("https://127.0.0.1:7890").as_deref(),
            Some("http://127.0.0.1:7890")
        );
        // aria2 无 SOCKS 实现：下发会让引擎启动即退出，必须拒绝。
        assert_eq!(normalize_proxy("socks5://127.0.0.1:1080"), None);
        assert_eq!(normalize_proxy("socks://127.0.0.1:1080"), None);
        assert_eq!(normalize_proxy("socks=127.0.0.1:1080"), None);
    }

    #[test]
    fn picks_http_from_multi_protocol_value() {
        let value = "http=127.0.0.1:7890;https=127.0.0.1:7890;ftp=127.0.0.1:7890";
        assert_eq!(
            normalize_proxy(value).as_deref(),
            Some("http://127.0.0.1:7890")
        );
        // 只有 https 时同样可用（统一按 http 连代理）。
        assert_eq!(
            normalize_proxy("https=127.0.0.1:7890").as_deref(),
            Some("http://127.0.0.1:7890")
        );
    }

    #[test]
    fn rejects_pac_urls_and_garbage() {
        // PAC 地址带路径：不能当成代理主机下发。
        assert_eq!(normalize_proxy("http://127.0.0.1:7890/proxy.pac"), None);
        assert_eq!(normalize_proxy("127.0.0.1"), None);
        assert_eq!(normalize_proxy("127.0.0.1:0"), None);
        assert_eq!(normalize_proxy("   "), None);
        assert_eq!(normalize_proxy("ftp://127.0.0.1:21"), None);
    }
}
