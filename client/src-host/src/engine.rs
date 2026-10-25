//! §6.3.1 `EngineClient` 抽象与两个实现。
//!
//! - `Aria2Client`：经 6801 的 WS/HTTP JSON-RPC 驱动产品自带 aria2c（`rpc_client` 平移）。
//! - `NativeClient`：直接调用 service 进程内 engine-net 的 lib API（Q3 选 B）。
//!
//! 刻意不引入 `async-trait`：手写 `Pin<Box<dyn Future + Send>>`，保持 trait 可 `dyn`
//! （service 里按引用持有，不做泛型传染）。

use std::future::Future;
use std::pin::Pin;
#[cfg(feature = "native-http")]
use std::sync::Arc;

use serde_json::Value;

use crate::rpc_client::{Client as Aria2RpcClient, RpcError as Aria2RpcError};

/// `EngineClient::call` 的返回类型（避免 `async-trait` 依赖）。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 统一错误：保留 aria2 errorCode 语义，6800 侧可原样回给前端。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("[{code}] {message}")]
pub struct EngineError {
    pub code: i64,
    pub message: String,
}

impl EngineError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// JSON-RPC `-32603`（内部错误 / 引擎不可用）。
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(-32603, message)
    }

    /// JSON-RPC `-32602`（参数形状不符合 aria2 协议）。
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }
}

impl From<Aria2RpcError> for EngineError {
    fn from(err: Aria2RpcError) -> Self {
        match err {
            Aria2RpcError::Server { code, message } => Self::new(code, message),
            other => Self::internal(other.to_string()),
        }
    }
}

/// 两个引擎共用的最小接口（weilai.md §6.3.1）。
pub trait EngineClient: Send + Sync {
    /// `params` 为 JSON-RPC 的 `params` 数组（不含 `token:`，鉴权由所属引擎客户端负责）。
    fn call<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, EngineError>>;

    /// `"aria2"` | `"native"`，用于 `_engine` 扩展字段与路由审计。
    fn name(&self) -> &'static str;
}

/// 把 JSON-RPC `params` 归一成数组；`null` 视作空数组（与 JSON-RPC 2.0 一致）。
fn params_to_vec(params: Value) -> Result<Vec<Value>, EngineError> {
    match params {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => Ok(items),
        _ => Err(EngineError::invalid_params("params 必须是数组")),
    }
}

/// aria2c 客户端（6801）。
pub struct Aria2Client {
    client: Aria2RpcClient,
}

impl Aria2Client {
    pub fn new(url: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            client: Aria2RpcClient::start(url, secret),
        }
    }

    pub fn from_client(client: Aria2RpcClient) -> Self {
        Self { client }
    }

    /// 供 service 侧做连接状态、secret 轮换与通知订阅。
    pub fn client(&self) -> &Aria2RpcClient {
        &self.client
    }
}

impl EngineClient for Aria2Client {
    fn call<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, EngineError>> {
        Box::pin(async move {
            let params = params_to_vec(params)?;
            self.client
                .call(method, params)
                .await
                .map_err(EngineError::from)
        })
    }

    fn name(&self) -> &'static str {
        "aria2"
    }
}

/// 进程内 native 引擎（engine-net）。
#[cfg(feature = "native-http")]
pub struct NativeClient {
    engine: Arc<engine_net::rpc::methods::Engine>,
}

#[cfg(feature = "native-http")]
impl NativeClient {
    pub fn new(engine: Arc<engine_net::rpc::methods::Engine>) -> Self {
        Self { engine }
    }

    pub fn engine(&self) -> &Arc<engine_net::rpc::methods::Engine> {
        &self.engine
    }
}

#[cfg(feature = "native-http")]
impl EngineClient for NativeClient {
    fn call<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, EngineError>> {
        Box::pin(async move {
            let params = params_to_vec(params)?;
            let engine = Arc::clone(&self.engine);
            // 红线 R9：panic 不得跨 RPC 边界。
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                engine.call(method, &params)
            }));
            match outcome {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(err)) => Err(EngineError::new(err.code, err.message)),
                Err(_) => Err(EngineError::internal("native 引擎 panic（已拦截）")),
            }
        })
    }

    fn name(&self) -> &'static str {
        "native"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn params_normalization_matches_jsonrpc() {
        assert_eq!(params_to_vec(Value::Null).unwrap(), Vec::<Value>::new());
        assert_eq!(params_to_vec(json!([1, 2])).unwrap(), vec![json!(1), json!(2)]);
        let err = params_to_vec(json!({"a": 1})).unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn aria2_server_errors_keep_their_code() {
        let err = EngineError::from(Aria2RpcError::Server {
            code: 1,
            message: "GID 不存在".to_string(),
        });
        assert_eq!(err.code, 1);
        assert_eq!(err.message, "GID 不存在");

        let disconnected = EngineError::from(Aria2RpcError::Disconnected("boom".to_string()));
        assert_eq!(disconnected.code, -32603);
    }

    #[tokio::test]
    async fn aria2_client_rejects_non_array_params() {
        // 端口 1 不可达：本用例只验证参数形状，绝不发起连接。
        let client = Aria2Client::new("ws://127.0.0.1:1/jsonrpc", "secret");
        let err = client.call("aria2.addUri", json!({"broken": true})).await;
        assert_eq!(err.unwrap_err().code, -32602);
        assert_eq!(client.name(), "aria2");
    }
}
