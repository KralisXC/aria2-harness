//! 下载引擎子进程守护（P0-F 第一层：Windows Job Object）。
//!
//! 背景：应用被任务管理器强杀 / 崩溃时 `RunEvent::Exit` 根本不会跑，
//! 常规的"退出钩子里 kill 子进程"必然失效 → `aria2c.exe` 残留，占住 6800 端口、
//! 锁住 `target\debug\aria2c.exe` 导致后续 `cargo build` 报 `Access denied`。
//!
//! 根治办法：把 sidecar 加入一个设置了 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的
//! **Job Object**。只要父进程的 Job 句柄被关闭（正常退出、崩溃、被强杀都会造成），
//! 内核即自动 kill Job 内全部进程 —— 不依赖任何用户态清理逻辑。
//!
//! 非 Windows 平台为 no-op，保证跨平台编译。

use crate::error::AppError;

#[cfg(windows)]
pub fn attach_to_job(child_pid: u32) -> Result<(), AppError> {
    use std::ffi::c_void;

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};
    use windows::core::PCWSTR;

    unsafe {
        // 1) 创建匿名 Job 对象（无安全属性）。
        let job: HANDLE = CreateJobObjectW(None, PCWSTR::null())
            .map_err(|e| AppError::Internal(format!("CreateJobObjectW 失败：{e}")))?;

        // 2) 设置限制：Job 句柄关闭 → 杀光 Job 内所有进程。
        //    `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` 依赖 Win32_System_Threading feature。
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Err(e) = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) {
            // 保留：句柄释放失败无需处理（即将返回错误，句柄随进程退出由内核回收）。
            let _ = CloseHandle(job);
            return Err(AppError::Internal(format!("SetInformationJobObject 失败：{e}")));
        }

        // 3) 打开子进程句柄（AssignProcessToJobObject 需要 SET_QUOTA + TERMINATE）。
        let process = match OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, child_pid) {
            Ok(handle) => handle,
            Err(e) => {
                let _ = CloseHandle(job);
                return Err(AppError::Internal(format!("OpenProcess(pid={child_pid}) 失败：{e}")));
            }
        };

        // 4) 把子进程加入 Job，随即关闭这份进程句柄（Job 成员关系已建立，无需保留）。
        let assign = AssignProcessToJobObject(job, process);
        let _ = CloseHandle(process);
        if let Err(e) = assign {
            let _ = CloseHandle(job);
            return Err(AppError::Internal(format!("AssignProcessToJobObject 失败：{e}")));
        }

        // 关键：**故意不关闭 job 句柄**。它是"守护契约"的载体 ——
        // 句柄随本进程存活；本进程一旦消失（含崩溃），内核关闭它并触发 KILL_ON_JOB_CLOSE。
        // 若此处 CloseHandle(job)，正常运行时引擎会被立刻杀掉（严重错误）。
        Ok(())
    }
}

/// 非 Windows 平台：不提供 Job Object，直接视为成功（no-op）。
#[cfg(not(windows))]
pub fn attach_to_job(_child_pid: u32) -> Result<(), AppError> {
    Ok(())
}
