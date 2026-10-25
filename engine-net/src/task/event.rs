//! 任务事件（`aria2.onDownload*` 的 engine-net 侧来源）。
//!
//! service 在 6800 上把这里的事件转发成 aria2 兼容的通知；GID 归属由 service 过滤，
//! 因此这里只负责「发生了什么」，不做路由判断。

use crate::task::registry::TaskSnapshot;

/// 与 aria2 通知一一对应的事件类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskEventKind {
    /// `aria2.onDownloadStart`
    Start,
    /// `aria2.onDownloadPause`
    Pause,
    /// `aria2.onDownloadStop`
    Stop,
    /// `aria2.onDownloadProgress`
    Progress,
    /// `aria2.onDownloadComplete`
    Complete,
    /// `aria2.onDownloadError`
    Error,
    /// `aria2.onBtDownloadComplete`（Phase 2 不产生 BT 任务，仅保留映射）
    BtComplete,
}

impl TaskEventKind {
    /// aria2 通知方法名。
    pub fn method(self) -> &'static str {
        match self {
            TaskEventKind::Start => "aria2.onDownloadStart",
            TaskEventKind::Pause => "aria2.onDownloadPause",
            TaskEventKind::Stop => "aria2.onDownloadStop",
            TaskEventKind::Progress => "aria2.onDownloadProgress",
            TaskEventKind::Complete => "aria2.onDownloadComplete",
            TaskEventKind::Error => "aria2.onDownloadError",
            TaskEventKind::BtComplete => "aria2.onBtDownloadComplete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskEvent {
    pub gid: String,
    pub kind: TaskEventKind,
    /// `onDownloadProgress` 的实时快照；其它事件为 `None`。
    ///
    /// 快照经 `rpc::types::status_to_value` 序列化后，长度/速度/连接数等数值字段
    /// **全部是字符串**（与老版 aria2 一致），前端不会解析出 `NaN` 或 0。
    pub progress: Option<TaskSnapshot>,
}

impl TaskEvent {
    pub fn new(gid: impl Into<String>, kind: TaskEventKind) -> Self {
        Self {
            gid: gid.into(),
            kind,
            progress: None,
        }
    }

    pub fn with_progress(gid: impl Into<String>, snapshot: TaskSnapshot) -> Self {
        Self {
            gid: gid.into(),
            kind: TaskEventKind::Progress,
            progress: Some(snapshot),
        }
    }
}
