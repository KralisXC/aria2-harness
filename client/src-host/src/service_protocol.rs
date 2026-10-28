//! 核心服务（`aria2-service`）↔ UI 之间的 **命名管道 IPC 协议**（增量 23）。
//!
//! 设计要点：
//! - **按行分帧**：一次连接 = 一个请求 + 一个响应，双方都以 `\n` 结束一条 JSON。
//!   `serde_json::to_string` 不会产出裸换行（字符串内的 `\n` 会被转义），因此
//!   "一条消息恰好一个物理行"是可保证的。
//! - **强类型**：请求/响应都是 serde 结构，UI 与 Service 共用同一份定义
//!   （同一 package 的第二个 bin 直接 `use aria2_harness_lib::service_protocol::*`），
//!   不存在"字段名写歪了但编译通过"的风险。
//! - **无状态**：协议不带会话 ID、不带序号；所有状态都存在 Service 进程内。
//!
//! 红线对齐：本模块只做"纯数据 + 纯函数"，不触碰任何引擎句柄、不持有锁。

use serde::{Deserialize, Serialize};

use crate::error::AppError;

/// 默认命名管道名。Windows 命名管道的完整名必须以 `\\.\pipe\` 开头。
pub const DEFAULT_PIPE_NAME: &str = r"\\.\pipe\aria2-service";

/// 覆盖管道名的环境变量（多实例调试 / 打包后隔离）。
pub const PIPE_NAME_ENV: &str = "ARIA2_SERVICE_PIPE";

/// 解析生效的管道名：环境变量优先（去空白后非空才采纳），否则用默认值。
pub fn pipe_name() -> String {
    std::env::var(PIPE_NAME_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_PIPE_NAME.to_string())
}

/// UI → Service 的管理指令。
///
/// `#[serde(tag = "cmd")]` 让 JSON 形如 `{"cmd":"start_engine","port":6800}`，
/// 与任务书 §2 约定的报文一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ServiceRequest {
    /// 启动 aria2c 引擎。`port` 为 RPC 监听端口（由 UI 指定，Service 照单执行）。
    ///
    /// 返回的 `rpc_secret` 由 **Service 生成**（而不是 UI），这样密钥的生命周期与
    /// 引擎一致：服务重启 → 引擎重启 → 密钥自然轮换。
    StartEngine { port: u16 },
    /// 查询引擎状态（是否在运行、进程 PID）。
    GetStatus,
    /// 原地更新 6800 对外密钥（热更新，不重启引擎、不断开连接）。
    ///
    /// 仅代理档（`auto` / `native`）支持；回滚档（`--engine=aria2`）的密钥是
    /// aria2c 启动参数，在线修改必然要求重启引擎，服务会明确拒绝。
    SetSecret { secret: String },
    /// 优雅关闭引擎并让 Service 自身退出。**幂等**：重复调用不报错。
    Shutdown,
}

/// Service → UI 的统一响应。
///
/// 无论成功失败都是同一个结构，避免 UI 侧两套解析分支。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceResponse {
    /// 指令是否成功。
    #[serde(default)]
    pub ok: bool,
    /// 失败原因（`ok=false` 时必定非空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `start_engine` 回传的 RPC 密钥；UI 用它连接 aria2 的 WS RPC。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_secret: Option<String>,
    /// 引擎当前是否在运行。
    #[serde(default)]
    pub engine_running: bool,
    /// 引擎进程 PID（未运行时为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_pid: Option<u32>,
}

impl ServiceResponse {
    /// 成功且无附加数据。
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            rpc_secret: None,
            engine_running: false,
            engine_pid: None,
        }
    }

    /// `start_engine` 成功：带回密钥与 PID。
    pub fn engine_started(secret: String, pid: u32) -> Self {
        Self {
            ok: true,
            error: None,
            rpc_secret: Some(secret),
            engine_running: true,
            engine_pid: Some(pid),
        }
    }

    /// `get_status` 结果。
    pub fn engine_status(running: bool, pid: Option<u32>) -> Self {
        Self {
            ok: true,
            error: None,
            rpc_secret: None,
            engine_running: running,
            engine_pid: pid,
        }
    }

    /// `get_status` 结果（带当前引擎的 RPC 密钥）。
    ///
    /// 用于**重连后重新鉴权**：引擎由服务重新拉起时密钥会轮换，UI 侧的
    /// WebSocket 客户端若继续用旧密钥，会一直收到 `Unauthorized`。
    /// 该指令是只读的（不会触发拉起引擎）。
    pub fn engine_status_with_secret(
        running: bool,
        pid: Option<u32>,
        secret: Option<String>,
    ) -> Self {
        Self {
            ok: true,
            error: None,
            rpc_secret: secret,
            engine_running: running,
            engine_pid: pid,
        }
    }

    /// 失败响应。
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            rpc_secret: None,
            engine_running: false,
            engine_pid: None,
        }
    }
}

/// 编码请求：单行 JSON + 结尾 `\n`。
pub fn encode_request(req: &ServiceRequest) -> Result<String, AppError> {
    let mut line = serde_json::to_string(req).map_err(|e| AppError::Serde(e))?;
    line.push('\n');
    Ok(line)
}

/// 解析一行请求 JSON（容忍首尾空白）。
pub fn decode_request(line: &str) -> Result<ServiceRequest, AppError> {
    serde_json::from_str(line.trim()).map_err(|e| AppError::Internal(format!("请求反序列化失败：{e}")))
}

/// 编码响应：单行 JSON + 结尾 `\n`。
pub fn encode_response(resp: &ServiceResponse) -> Result<String, AppError> {
    let mut line = serde_json::to_string(resp).map_err(|e| AppError::Internal(format!("响应序列化失败：{e}")))?;
    line.push('\n');
    Ok(line)
}

/// 解析一行响应 JSON（容忍首尾空白）。
pub fn decode_response(line: &str) -> Result<ServiceResponse, AppError> {
    serde_json::from_str(line.trim()).map_err(|e| AppError::Internal(format!("响应反序列化失败：{e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 帧完整性：编码结果必须是"唯一一个物理行"（只允许末尾那个 `\n`）。
    /// 这是按行分帧协议的生命线 —— 一旦序列化结果里出现裸换行，对端就会
    /// 把一条请求劈成两条，产生"半个 JSON"这种最难排查的错。
    #[test]
    fn encoded_frames_are_single_physical_lines() {
        let reqs = [
            ServiceRequest::StartEngine { port: 6800 },
            ServiceRequest::GetStatus,
            ServiceRequest::SetSecret {
                secret: "new-secret".to_string(),
            },
            ServiceRequest::Shutdown,
        ];
        for req in reqs {
            let line = encode_request(&req).unwrap();
            assert_eq!(line.matches('\n').count(), 1, "请求报文含多余换行：{line:?}");
            assert!(line.ends_with('\n'));
            assert!(!line.trim_end_matches('\n').contains('\n'));
        }

        let resps = [
            ServiceResponse::ok(),
            ServiceResponse::engine_started("a-b-c".to_string(), 4321),
            ServiceResponse::engine_status(true, Some(4321)),
            ServiceResponse::err("boom \"quoted\"\nwith newline"),
        ];
        for resp in resps {
            let line = encode_response(&resp).unwrap();
            assert_eq!(line.matches('\n').count(), 1, "响应报文含多余换行：{line:?}");
            assert!(line.ends_with('\n'));
        }
    }

    /// 请求往返：编码 → 解码必须等价。
    #[test]
    fn request_round_trip() {
        let req = ServiceRequest::StartEngine { port: 6800 };
        assert_eq!(decode_request(&encode_request(&req).unwrap()).unwrap(), req);

        let req = ServiceRequest::GetStatus;
        assert_eq!(decode_request(&encode_request(&req).unwrap()).unwrap(), req);

        let req = ServiceRequest::SetSecret {
            secret: "n3w-s3cret".to_string(),
        };
        assert_eq!(decode_request(&encode_request(&req).unwrap()).unwrap(), req);

        let req = ServiceRequest::Shutdown;
        assert_eq!(decode_request(&encode_request(&req).unwrap()).unwrap(), req);
    }

    /// 响应往返：成功 / 失败 / 带密钥三种形态都必须等价。
    #[test]
    fn response_round_trip() {
        for resp in [
            ServiceResponse::ok(),
            ServiceResponse::engine_started("secret".to_string(), 99),
            ServiceResponse::engine_status(false, None),
            ServiceResponse::err("引擎启动失败"),
        ] {
            assert_eq!(decode_response(&encode_response(&resp).unwrap()).unwrap(), resp);
        }
    }

    /// 线上报文格式锁定：`cmd` 必须是 snake_case，且 UI/Service 两侧不会漂移。
    #[test]
    fn wire_format_is_locked() {
        let line = encode_request(&ServiceRequest::StartEngine { port: 6800 }).unwrap();
        assert_eq!(line, "{\"cmd\":\"start_engine\",\"port\":6800}\n");

        let line = encode_request(&ServiceRequest::SetSecret {
            secret: "abc".to_string(),
        })
        .unwrap();
        assert_eq!(line, "{\"cmd\":\"set_secret\",\"secret\":\"abc\"}\n");

        let line = encode_response(&ServiceResponse::engine_started("s".repeat(3), 7)).unwrap();
        assert_eq!(
            line,
            "{\"ok\":true,\"rpc_secret\":\"sss\",\"engine_running\":true,\"engine_pid\":7}\n"
        );

        // 失败响应必须带 error，且 ok=false
        let resp = decode_response("{\"ok\":false,\"error\":\"x\"}\n").unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.error.as_deref(), Some("x"));

        // 冗余字段（未知字段）应被忽略，方便后续协议增量扩展而不破坏旧 UI
        let resp = decode_response("{\"ok\":true,\"future_field\":123}\n").unwrap();
        assert!(resp.ok);
    }

    /// 非法输入必须被拒绝，而不是解析出"看起来能跑"的假对象。
    #[test]
    fn malformed_input_is_rejected() {
        assert!(decode_request("").is_err());
        assert!(decode_request("not json").is_err());
        assert!(decode_request("{\"cmd\":\"unknown_cmd\"}").is_err());
        assert!(decode_request("{\"port\":6800}").is_err());
        assert!(decode_response("{").is_err());

        // 大小写必须严格遵守（snake_case）
        assert!(decode_request("{\"cmd\":\"StartEngine\",\"port\":1}").is_err());
    }

    /// 管道名解析：环境变量优先，空值回退默认。
    #[test]
    fn pipe_name_env_override() {
        // 直接校验默认值常量，避免污染进程级环境变量（测试可能并行）。
        assert!(DEFAULT_PIPE_NAME.starts_with(r"\\.\pipe\"));
        assert!(!PIPE_NAME_ENV.is_empty());
    }
}
