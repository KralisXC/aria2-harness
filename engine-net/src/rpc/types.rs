//! RPC 数据结构：GID / tellStatus 字段 / JSON-RPC error 包装。
//!
//! 数值字段一律按 aria2 惯例序列化成字符串（`totalLength: "123"`），
//! 便于前端与 aria2c 逐字段比对（§5.4.7）。

use serde_json::{Value, json};

use crate::task::registry::{TaskError, TaskSnapshot, TaskStatus};

pub const JSONRPC_VERSION: &str = "2.0";

/// JSON-RPC 标准错误码。
pub const CODE_PARSE_ERROR: i64 = -32700;
pub const CODE_INVALID_REQUEST: i64 = -32600;
pub const CODE_METHOD_NOT_FOUND: i64 = -32601;
pub const CODE_INVALID_PARAMS: i64 = -32602;
pub const CODE_INTERNAL_ERROR: i64 = -32603;
/// aria2 业务错误（GID 不存在、状态不允许等）。
pub const CODE_TASK: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(CODE_METHOD_NOT_FOUND, format!("方法不存在：{method}"))
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(CODE_INVALID_PARAMS, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(CODE_INTERNAL_ERROR, message)
    }

    fn to_value(&self) -> Value {
        json!({ "code": self.code, "message": self.message })
    }
}

impl From<TaskError> for RpcError {
    fn from(err: TaskError) -> Self {
        RpcError::new(CODE_TASK, err.to_string())
    }
}

/// 成功响应。
pub fn success(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "result": result })
}

/// 失败响应。
pub fn failure(id: &Value, error: &RpcError) -> Value {
    json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "error": error.to_value() })
}

/// `tellStatus.result`（RPC 层做 keys 过滤）。
pub fn status_to_value(snapshot: &TaskSnapshot) -> Value {
    let dir = snapshot
        .dest
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let uris: Vec<Value> = snapshot
        .uris
        .iter()
        .enumerate()
        .map(|(index, uri)| {
            json!({
                "uri": uri,
                // aria2 只区分 used / waiting；Phase 1 单连接，首个即 used。
                "status": if index == 0 { "used" } else { "waiting" },
            })
        })
        .collect();

    let mut value = json!({
        "gid": snapshot.gid,
        "status": snapshot.status.as_aria2(),
        "totalLength": snapshot.total_length.to_string(),
        "completedLength": snapshot.completed_length.to_string(),
        "downloadSpeed": snapshot.download_speed.to_string(),
        "uploadLength": "0",
        "uploadSpeed": "0",
        "connections": snapshot.active_workers.to_string(),
        "pieceLength": snapshot.piece_length.to_string(),
        // C-3 扩展字段（aria2 无这些项，前端按需读取，旧客户端忽略即可）：
        // 每片完成百分比数组 + 实际活跃连接数 + 预计剩余秒数。
        "pieceProgress": snapshot.piece_progress,
        "activeWorkers": snapshot.active_workers,
        "eta": snapshot.eta.to_string(),
        // aria2 的 tellStatus 恒返回 errorCode/errorMessage（成功时 "0"/空串），
        // 见 RpcMethodImpl.cc `gatherStoppedDownload`；A/B 比对必须逐字段一致。
        "errorCode": snapshot.error_code.unwrap_or(0).to_string(),
        "errorMessage": snapshot.error_message.clone().unwrap_or_default(),
        "dir": dir,
        "files": [{
            "index": "1",
            "path": snapshot.dest.to_string_lossy(),
            "selected": "true",
            "length": snapshot.total_length.to_string(),
            "completedLength": snapshot.completed_length.to_string(),
            "uris": uris,
        }],
    });

    // beta8.3：限流降级暂停原因（aria2 无此字段，前端按需读取）。
    if let Some(reason) = &snapshot.pause_reason {
        value["pauseReason"] = json!(reason);
    }
    if snapshot.error_code.is_some() {
        value["status"] = json!(TaskStatus::Error.as_aria2());
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn status_fields_are_aria2_shaped() {
        let snapshot = TaskSnapshot {
            gid: "fffe000000000001".into(),
            status: TaskStatus::Error,
            total_length: 100,
            completed_length: 40,
            download_speed: 0,
            dest: PathBuf::from("C:/tmp/a.bin"),
            uris: vec!["http://a.test/a.bin".into()],
            error_code: Some(8),
            error_message: Some("服务器不支持 Range".into()),
            piece_length: 1024,
            piece_progress: vec![100, 40],
            active_workers: 2,
            eta: 0,
            pause_reason: None,
        };
        let value = status_to_value(&snapshot);
        assert_eq!(value["totalLength"], "100");
        assert_eq!(value["completedLength"], "40");
        assert_eq!(value["errorCode"], "8");
        assert_eq!(value["status"], "error");
        assert_eq!(value["connections"], "2");
        assert_eq!(value["activeWorkers"], 2);
        assert_eq!(value["pieceProgress"], json!([100, 40]));
        assert_eq!(value["eta"], "0");
        assert_eq!(value["files"][0]["uris"][0]["uri"], "http://a.test/a.bin");
        assert_eq!(value["files"][0]["uris"][0]["status"], "used");
    }

    #[test]
    fn pause_reason_is_exposed() {
        let snapshot = TaskSnapshot {
            gid: "fffe000000000002".into(),
            status: TaskStatus::Paused,
            total_length: 100,
            completed_length: 40,
            download_speed: 0,
            dest: PathBuf::from("C:/tmp/b.bin"),
            uris: vec!["http://a.test/b.bin".into()],
            error_code: None,
            error_message: None,
            piece_length: 1024,
            piece_progress: vec![100, 40],
            active_workers: 0,
            eta: 0,
            pause_reason: Some("rate-limit".into()),
        };
        let value = status_to_value(&snapshot);
        assert_eq!(value["status"], "paused");
        assert_eq!(value["pauseReason"], "rate-limit");
    }

    #[test]
    fn success_and_failure_envelopes() {
        let id = json!("1");
        assert_eq!(success(&id, json!("OK"))["result"], "OK");
        let response = failure(&id, &RpcError::method_not_found("aria2.x"));
        assert_eq!(response["error"]["code"], CODE_METHOD_NOT_FOUND);
        assert_eq!(response["jsonrpc"], "2.0");
    }
}
