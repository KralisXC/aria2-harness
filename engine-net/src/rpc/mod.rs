//! aria2 兼容 JSON-RPC（Phase 1 只做最小集，Q3 已签）。
//!
//! 分发入口必须 `catch_unwind`（红线 R9），错误转结构化 JSON-RPC error。

pub mod methods;
pub mod types;

use std::panic::{AssertUnwindSafe, catch_unwind};

use serde_json::Value;

use crate::rpc::methods::Engine;
use crate::rpc::types::{CODE_INTERNAL_ERROR, CODE_PARSE_ERROR, RpcError, failure};

/// 单行 JSON 请求 → 单行 JSON 响应（不换行）。
pub fn handle_json(engine: &Engine, text: &str) -> String {
    let request: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(err) => {
            return failure(
                &Value::Null,
                &RpcError::new(CODE_PARSE_ERROR, format!("JSON 解析失败：{err}")),
            )
            .to_string();
        }
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    guarded(&id, || engine.handle_value(&request)).to_string()
}

/// 已解析请求的受保护入口（供 bin 的 TCP 服务复用，含 R9 catch_unwind）。
pub fn handle_value_guarded(engine: &Engine, request: &Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    guarded(&id, || engine.handle_value(request))
}

/// R9：RPC handler 内部 panic 不得冒泡到进程。
pub(crate) fn guarded<F>(id: &Value, handler: F) -> Value
where
    F: FnOnce() -> Value,
{
    match catch_unwind(AssertUnwindSafe(handler)) {
        Ok(response) => response,
        Err(_) => failure(
            id,
            &RpcError::new(
                CODE_INTERNAL_ERROR,
                "RPC handler panic（已被 catch_unwind 拦截）",
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn panic_is_converted_to_jsonrpc_error() {
        let id = json!("7");
        let response = guarded(&id, || panic!("故意触发"));
        assert_eq!(response["id"], "7");
        assert_eq!(response["error"]["code"], CODE_INTERNAL_ERROR);
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("catch_unwind")
        );
    }

    #[test]
    fn invalid_json_yields_parse_error() {
        let engine = Engine::new(
            crate::dns::resolver::DnsResolver::new(&Default::default()).expect("resolver"),
        );
        let response: Value = serde_json::from_str(&handle_json(&engine, "not json")).unwrap();
        assert_eq!(response["error"]["code"], CODE_PARSE_ERROR);
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let engine = Engine::new(
            crate::dns::resolver::DnsResolver::new(&Default::default()).expect("resolver"),
        );
        let response: Value = serde_json::from_str(&handle_json(
            &engine,
            r#"{"jsonrpc":"2.0","id":"1","method":"aria2.nope","params":[]}"#,
        ))
        .unwrap();
        assert_eq!(response["error"]["code"], crate::rpc::types::CODE_METHOD_NOT_FOUND);
    }
}
