//! UI 侧命名管道客户端（增量 23）。
//!
//! 模型与 Service 侧严格对齐：**一次连接 = 一个请求 + 一个响应**。
//! 好处是无状态、无锁、无"长连接断了要重连"的状态机 —— 对本地 IPC 而言，
//! 连接开销可以忽略。代价是每次调用都要新开一个管道句柄。
//!
//! 为什么不用长连接 + `Mutex`：UI 侧的调用点分散在多个 Tauri 异步命令里
//! （且 `RunEvent::Exit` 还要 `block_on` 同步调用），共享一条连接就需要在
//! 同步/异步两条路径上同时加锁，极易引入死锁。短连接把这个复杂度直接消掉。

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::ClientOptions;

use crate::error::AppError;
use crate::service_protocol::{
    decode_response, encode_request, pipe_name, ServiceRequest, ServiceResponse,
};

/// 连接重试次数：Service 在"处理完上一个连接"与"创建下一个监听实例"之间存在
/// 极短的窗口，此时 `open` 会返回 `ERROR_FILE_NOT_FOUND`。几次微退避即可跨过。
const CONNECT_ATTEMPTS: usize = 3;
/// 连接重试间隔。
const CONNECT_BACKOFF: Duration = Duration::from_millis(60);
/// 等待响应的上界（超过则视为服务异常，避免命令悬挂）。
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);

/// 向核心服务发送一个指令并等待响应。
///
/// 错误语义：
/// - 连接不上（服务未启动）→ `Err(AppError::Network("连接核心服务失败…"))`，**不含操作指引**
///   —— 指引由调用方（`bootstrap_service`）统一追加，避免多处拼接出重复的提示语；
/// - 服务可达但指令失败 → `Ok(ServiceResponse { ok: false, error: Some(..) })`；
///   调用方需自行判断 `ok`。
pub async fn request(req: &ServiceRequest) -> Result<ServiceResponse, AppError> {
    let pipe = pipe_name();
    // 先编码：编码失败属于编程错误，无需重试。
    let payload = encode_request(req)?;

    let mut last_err = String::new();
    for attempt in 1..=CONNECT_ATTEMPTS {
        match ClientOptions::new().open(pipe.as_str()) {
            Ok(client) => return round_trip(client, &payload).await,
            Err(err) => {
                last_err = format!("连接核心服务失败（{pipe}）：{err}");
                eprintln!("[service] 第 {attempt}/{CONNECT_ATTEMPTS} 次连接失败：{err}");
                if attempt < CONNECT_ATTEMPTS {
                    tokio::time::sleep(CONNECT_BACKOFF).await;
                }
            }
        }
    }

    Err(AppError::Network(last_err))
}

/// 单次往返：写入一行请求 → 读取一行响应。
async fn round_trip(
    client: tokio::net::windows::named_pipe::NamedPipeClient,
    payload: &str,
) -> Result<ServiceResponse, AppError> {
    let mut conn = BufReader::new(client);

    // `BufReader` 的 `AsyncWrite` 直接透传给内层管道（无独立写缓冲），
    // 因此 `write_all` + `flush` 就是真正的"已发出"。
    conn.write_all(payload.as_bytes())
        .await
        .map_err(|e| AppError::Network(format!("写入服务请求失败：{e}")))?;
    conn.flush()
        .await
        .map_err(|e| AppError::Network(format!("刷新服务请求失败：{e}")))?;

    let mut line = String::new();
    let read = tokio::time::timeout(RESPONSE_TIMEOUT, conn.read_line(&mut line))
        .await
        .map_err(|_| AppError::Network(format!("等待核心服务响应超时（{}s）", RESPONSE_TIMEOUT.as_secs())))?
        .map_err(|e| AppError::Network(format!("读取服务响应失败：{e}")))?;
    if read == 0 {
        return Err(AppError::Network("核心服务未返回任何数据（连接被关闭）".to_string()));
    }

    decode_response(&line)
}
