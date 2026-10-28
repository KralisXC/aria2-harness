//! 自研 aria2 JSON-RPC 2.0 客户端（WebSocket）。
//!
//! 取代第三方 `aria2-ws`：后者把"请求"与"订阅"拆成两条独立通道，由同一个
//! `select!` 消费 —— 一旦响应先于订阅登记被处理，回执就被静默丢弃，调用方的
//! `oneshot` 永久悬挂（E2E 中表现为整条串行 IPC 通道静默失联）。
//!
//! 本实现从结构上消灭该竞态：
//!
//! 1. **先登记、后发送**：`call` 在把请求帧投入写队列**之前**就把 `id → Sender`
//!    写进 `pending`，因此任何回执到达时都必然能查到等待者；
//! 2. **超时必清理**：`tokio::time::timeout` 到期先摘除 `id` 再返回错误，避免
//!    `pending` 泄漏、也避免迟到的回执被误认；
//! 3. **断链必唤醒**：连接断开时遍历 `pending` 向所有等待者回 `Disconnected`
//!    并清表，绝不让任何调用方无限期等待；
//! 4. **读写在同一个任务里 `select!`**：没有跨通道的排队顺序问题。
//!
//! 通知（`aria2.onDownloadStart` 等）走 `broadcast`，与请求回执完全分离。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

/// 单次 RPC 调用的默认等待上界。
///
/// 3~5s 是"本地回环 + 引擎单线程处理"的合理上限：正常调用在毫秒级完成，
/// 超过这个时间说明回执已丢或引擎卡死，继续等只会拖垮调用方。
pub const RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// 建立 WebSocket 连接的上界（回环地址，正常 <10ms）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// 重连退避：起点、封顶（1s → 2s → 4s … → 30s）。
const RECONNECT_START: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// 待发请求队列容量。有界：队列满说明在途请求异常堆积，宁可显式报错也不无限吃内存。
const OUT_QUEUE_CAP: usize = 256;

/// beta9.1：心跳发送间隔（WebSocket Ping）。
const HEARTBEAT_TICK: Duration = Duration::from_secs(5);
/// beta9.1：连续无任何入站帧的上界。超过即判定连接假死并强制断开重连。
///
/// 背景：aria2-service.exe 被强杀后，Windows 有时不会立刻给对端送 RST，
/// 只靠读错误会让 UI 长期停在「已连接」；必须由心跳兜底。
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);

/// 通知广播容量。订阅者落后时会被丢弃并报 `Lagged`，由订阅方决定如何记账。
const NOTIFY_CAP: usize = 256;

/// aria2 任务事件类型（与 `aria2.onDownload*` 通知一一对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Start,
    Pause,
    Stop,
    Complete,
    Error,
    BtComplete,
}

/// 客户端对外产出的消息。
#[derive(Debug, Clone)]
pub enum Notification {
    /// 引擎任务事件。
    Aria2 { gid: String, event: Event },
    /// WebSocket 已连上（含重连成功）。
    WebSocketConnected,
    /// WebSocket 已断开（随后会自动重连）。
    WebsocketClosed,
}

/// RPC 层错误。刻意区分"没连上 / 超时 / 服务端拒绝 / 协议异常"，
/// 便于上层决定是重试、提示用户还是记日志。
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// 未连接 / 连接已断开（含等待中对方断开）。
    #[error("aria2 RPC 未连接：{0}")]
    Disconnected(String),
    /// 等待回执超时（`pending` 已清理）。
    #[error("aria2 RPC 超时（{}s）：{method}", .timeout.as_secs())]
    Timeout { method: String, timeout: Duration },
    /// 服务端返回 JSON-RPC error 对象（aria2 的业务错误，如 GID 不存在）。
    #[error("aria2 RPC 错误[{code}]：{message}")]
    Server { code: i64, message: String },
    /// 帧不是合法 JSON / 结构不符合 JSON-RPC。
    #[error("aria2 RPC 协议异常：{0}")]
    Protocol(String),
    /// 写队列已满（在途请求异常堆积）。
    #[error("aria2 RPC 发送队列已满")]
    Busy,
}

type Pending = HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>;

struct Inner {
    url: String,
    /// `rpc-secret`：所有请求自动前置 `token:<secret>` 参数。
    ///
    /// 必须**可运行时更新**：引擎崩溃后被服务重新拉起时密钥会轮换，
    /// 若沿用旧值，每次调用都会被判 `Unauthorized`（E2E 实测）。
    secret: std::sync::RwLock<String>,
    next_id: AtomicU64,
    pending: StdMutex<Pending>,
    /// 待发帧队列（有界）。只有连接存活时才会被消费。
    out_tx: mpsc::Sender<String>,
    notify_tx: broadcast::Sender<Notification>,
    connected: AtomicBool,
    /// 已请求停止重连（进程退出流程中置位）。
    closing: AtomicBool,
    /// 重连唤醒：窗口重新获得焦点时打断退避，立即重试（见 [`Client::reconnect_now`]）。
    wake: tokio::sync::Notify,
    /// beta9.1：主动踢掉当前连接（健康检查失败时），让守护任务立即重连。
    kick: tokio::sync::Notify,
}

/// 共享句柄（`Arc` 内部，clone 廉价）。
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl Client {
    /// 创建客户端并**立即**启动连接守护任务。
    ///
    /// 返回时连接可能尚未建立（守护任务在后台握手）：调用方通过
    /// [`Client::is_connected`] 或 [`Notification::WebSocketConnected`] 判断就绪。
    /// 首连失败不会向上抛错 —— 守护任务按指数退避持续重试（1s→30s）。
    pub fn start(url: impl Into<String>, secret: impl Into<String>) -> Self {
        let (out_tx, out_rx) = mpsc::channel::<String>(OUT_QUEUE_CAP);
        let (notify_tx, _) = broadcast::channel::<Notification>(NOTIFY_CAP);
        let client = Self {
            inner: Arc::new(Inner {
                url: url.into(),
                secret: std::sync::RwLock::new(secret.into()),
                next_id: AtomicU64::new(1),
                pending: StdMutex::new(HashMap::new()),
                out_tx,
                notify_tx,
                connected: AtomicBool::new(false),
                closing: AtomicBool::new(false),
                wake: tokio::sync::Notify::new(),
                kick: tokio::sync::Notify::new(),
            }),
        };
        let inner = client.inner.clone();
        tokio::spawn(async move { supervise(inner, out_rx).await });
        client
    }

    /// 当前是否处于已连接状态。
    pub fn is_connected(&self) -> bool {
        self.inner.connected.load(Ordering::Acquire)
    }

    /// 订阅通知（可多订阅者）。
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<Notification> {
        self.inner.notify_tx.subscribe()
    }

    /// 停止自动重连（退出流程用）：当前连接继续可用，断开后守护任务退出。
    pub fn stop_reconnecting(&self) {
        self.inner.closing.store(true, Ordering::Release);
    }

    /// 立即重连：打断当前退避等待并把退避复位到最短间隔。
    ///
    /// 由「窗口重新获得焦点」触发 —— 后台期间引擎可能已恢复，没必要再等最长 30s。
    pub fn reconnect_now(&self) {
        self.inner.wake.notify_one();
    }

    /// beta9.1：强制踢掉当前会话（健康检查失败 / 引擎无响应时）。
    ///
    /// 守护任务收到通知后跳出读循环，走标准的「标记断开 → 通知前端 → 退避重连」路径。
    pub fn force_disconnect(&self) {
        self.inner.kick.notify_waiters();
    }

    /// 更新 `rpc-secret`（引擎重启后由核心服务重新下发）。
    pub fn update_secret(&self, secret: impl Into<String>) {
        let mut guard = self
            .inner
            .secret
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = secret.into();
    }

    /// 发起一次 JSON-RPC 调用，返回原始 `result`。
    ///
    /// 关键顺序（见模块注释）：**先登记 `pending`，再入写队列**。
    pub async fn call(&self, method: &str, params: Vec<Value>) -> Result<Value, RpcError> {
        self.call_with_timeout(method, params, RPC_TIMEOUT).await
    }

    async fn call_with_timeout(
        &self,
        method: &str,
        params: Vec<Value>,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        if !self.is_connected() {
            return Err(RpcError::Disconnected("尚未建立连接".to_string()));
        }

        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self
                .inner
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.insert(id, tx);
        }

        // token 必须作为第一个位置参数：aria2 对所有 RPC 方法统一这样鉴权。
        let secret = self
            .inner
            .secret
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut full_params = Vec::with_capacity(params.len() + 1);
        full_params.push(json!(format!("token:{secret}")));
        full_params.extend(params);

        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": full_params,
        })
        .to_string();

        if let Err(err) = self.inner.out_tx.try_send(frame) {
            self.forget(id);
            return Err(match err {
                mpsc::error::TrySendError::Full(_) => RpcError::Busy,
                mpsc::error::TrySendError::Closed(_) => {
                    RpcError::Disconnected("发送队列已关闭".to_string())
                }
            });
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            // 发送端被 `fail_all_pending` 丢弃 —— 断链时统一唤醒。
            Ok(Err(_)) => Err(RpcError::Disconnected("连接已断开，请求未完成".to_string())),
            Err(_) => {
                // 超时必须摘除，否则迟到的回执会被当作有效结果、且 map 会持续增长。
                self.forget(id);
                Err(RpcError::Timeout {
                    method: method.to_string(),
                    timeout,
                })
            }
        }
    }

    /// 发起调用并按 `T` 反序列化 `result`。
    pub async fn call_typed<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Vec<Value>,
    ) -> Result<T, RpcError> {
        let value = self.call(method, params).await?;
        serde_json::from_value(value).map_err(|e| {
            RpcError::Protocol(format!("{method} 返回值不符合预期：{e}"))
        })
    }

    fn forget(&self, id: u64) {
        let mut guard = self
            .inner
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.remove(&id);
    }

    // ------------------------------------------------------------------
    // aria2 语义化封装（参数顺序严格对齐 aria2 RPC 文档）
    // ------------------------------------------------------------------

    async fn call_empty(&self, method: &str, params: Vec<Value>) -> Result<(), RpcError> {
        self.call(method, params).await.map(|_| ())
    }

    /// `aria2.tellStatus(gid, keys?)`
    pub async fn tell_status(
        &self,
        gid: &str,
        keys: Option<Vec<String>>,
    ) -> Result<Map<String, Value>, RpcError> {
        let mut params = vec![json!(gid)];
        if let Some(keys) = keys {
            params.push(json!(keys));
        }
        self.call_typed("aria2.tellStatus", params).await
    }

    /// `aria2.tellActive(keys?)`
    pub async fn tell_active(
        &self,
        keys: Option<Vec<String>>,
    ) -> Result<Vec<Map<String, Value>>, RpcError> {
        let params = keys.map(|k| vec![json!(k)]).unwrap_or_default();
        self.call_typed("aria2.tellActive", params).await
    }

    /// `aria2.tellWaiting(offset, num, keys?)`
    pub async fn tell_waiting(
        &self,
        offset: i64,
        num: i64,
        keys: Option<Vec<String>>,
    ) -> Result<Vec<Map<String, Value>>, RpcError> {
        let mut params = vec![json!(offset), json!(num)];
        if let Some(keys) = keys {
            params.push(json!(keys));
        }
        self.call_typed("aria2.tellWaiting", params).await
    }

    /// `aria2.tellStopped(offset, num, keys?)`
    pub async fn tell_stopped(
        &self,
        offset: i64,
        num: i64,
        keys: Option<Vec<String>>,
    ) -> Result<Vec<Map<String, Value>>, RpcError> {
        let mut params = vec![json!(offset), json!(num)];
        if let Some(keys) = keys {
            params.push(json!(keys));
        }
        self.call_typed("aria2.tellStopped", params).await
    }

    /// `aria2.addUri(uris, options?)` → 新任务 gid。
    pub async fn add_uri(
        &self,
        uris: Vec<String>,
        options: Map<String, Value>,
    ) -> Result<String, RpcError> {
        let mut params = vec![json!(uris)];
        if !options.is_empty() {
            params.push(Value::Object(options));
        }
        self.call_typed("aria2.addUri", params).await
    }

    /// `aria2.forcePause(gid)`
    pub async fn force_pause(&self, gid: &str) -> Result<(), RpcError> {
        self.call_empty("aria2.forcePause", vec![json!(gid)]).await
    }

    /// `aria2.forcePauseAll()` —— 立即中断所有 ACTIVE 任务并断开底层连接。
    /// 系统休眠前用它代替"遍历连接逐个 retire"，语义等价且风险更低。
    pub async fn force_pause_all(&self) -> Result<(), RpcError> {
        self.call_empty("aria2.forcePauseAll", Vec::new()).await
    }

    /// `aria2.unpauseAll()` —— 恢复全部处于暂停队列的任务（唤醒后调用）。
    pub async fn unpause_all(&self) -> Result<(), RpcError> {
        self.call_empty("aria2.unpauseAll", Vec::new()).await
    }

    /// `aria2.unpause(gid)`
    pub async fn unpause(&self, gid: &str) -> Result<(), RpcError> {
        self.call_empty("aria2.unpause", vec![json!(gid)]).await
    }

    /// `aria2.forceRemove(gid)`
    pub async fn force_remove(&self, gid: &str) -> Result<(), RpcError> {
        self.call_empty("aria2.forceRemove", vec![json!(gid)]).await
    }

    /// `aria2.removeDownloadResult(gid)`
    pub async fn remove_download_result(&self, gid: &str) -> Result<(), RpcError> {
        self.call_empty("aria2.removeDownloadResult", vec![json!(gid)])
            .await
    }

    /// `aria2.changeGlobalOption(options)` —— 成功返回字符串 `"OK"`，此处只关心是否出错。
    pub async fn change_global_option(&self, options: Map<String, Value>) -> Result<(), RpcError> {
        self.call_empty("aria2.changeGlobalOption", vec![Value::Object(options)])
            .await
    }

    /// `aria2.saveSession()`
    pub async fn save_session(&self) -> Result<(), RpcError> {
        self.call_empty("aria2.saveSession", Vec::new()).await
    }

    /// `aria2.shutdown()`
    pub async fn shutdown(&self) -> Result<(), RpcError> {
        self.call_empty("aria2.shutdown", Vec::new()).await
    }
}

// ---------------------------------------------------------------------------
// 连接守护：握手 / 重连 / 读写循环
// ---------------------------------------------------------------------------

/// 守护任务：维持连接，断线后按指数退避重连（1s → 30s，带抖动）。
async fn supervise(inner: Arc<Inner>, mut out_rx: mpsc::Receiver<String>) {
    let mut backoff = RECONNECT_START;

    loop {
        if inner.closing.load(Ordering::Acquire) {
            return;
        }

        let handshake = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async(inner.url.as_str()),
        )
        .await;

        let ws = match handshake {
            Ok(Ok((ws, _resp))) => ws,
            Ok(Err(err)) => {
                tracing::warn!("aria2 RPC 连接失败：{err}");
                if !sleep_backoff(&inner, &mut backoff).await {
                    return;
                }
                continue;
            }
            Err(_) => {
                tracing::warn!(
                    "aria2 RPC 连接超时（{}s）",
                    CONNECT_TIMEOUT.as_secs()
                );
                if !sleep_backoff(&inner, &mut backoff).await {
                    return;
                }
                continue;
            }
        };

        // 连上即复位退避：引擎短暂抖动恢复后应立刻回到最短重连间隔。
        backoff = RECONNECT_START;
        inner.connected.store(true, Ordering::Release);
        let _ = inner.notify_tx.send(Notification::WebSocketConnected);
        tracing::info!("aria2 RPC 已连接（{}）", inner.url);

        session(&inner, ws, &mut out_rx).await;

        // 会话结束：先标记断开，再唤醒所有等待者，最后清空残留待发帧。
        inner.connected.store(false, Ordering::Release);
        fail_all_pending(&inner, "连接已断开");
        while out_rx.try_recv().is_ok() {}
        let _ = inner.notify_tx.send(Notification::WebsocketClosed);
        tracing::warn!("aria2 RPC 连接已断开，准备重连");

        if !sleep_backoff(&inner, &mut backoff).await {
            return;
        }
    }
}

/// 退避等待；返回 `false` 表示收到退出信号、守护任务应当结束。
async fn sleep_backoff(inner: &Inner, backoff: &mut Duration) -> bool {
    let wait = *backoff;
    let base = wait.as_millis() as u64;
    // 抖动上限取 1/4：多实例同时重连时不至于形成"惊群"。
    let jitter = Duration::from_millis(jitter_seed() % (base / 4).max(1));
    let deadline = tokio::time::Instant::now() + wait + jitter;

    // 分片睡眠：退出信号 / 焦点唤醒都可以尽快打断退避，而不是傻等满 30s。
    loop {
        if inner.closing.load(Ordering::Acquire) {
            return false;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        let sleep = tokio::time::sleep(std::cmp::min(
            deadline - now,
            Duration::from_millis(250),
        ));
        tokio::select! {
            _ = sleep => {}
            _ = inner.wake.notified() => {
                if inner.closing.load(Ordering::Acquire) {
                    return false;
                }
                // 用户回到窗口：立刻重试，并把退避复位到最短间隔。
                *backoff = RECONNECT_START;
                return true;
            }
        }
    }

    *backoff = std::cmp::min(wait * 2, RECONNECT_MAX);
    true
}

/// 单次连接的读写循环（读帧、派发回执/通知，同时消费写队列）。
async fn session(
    inner: &Arc<Inner>,
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    out_rx: &mut mpsc::Receiver<String>,
) {
    let (mut sink, mut stream) = ws.split();

    // beta9.1：心跳。任何入站帧（回执 / 通知 / Ping / Pong）都算"活着"；
    // 超过 HEARTBEAT_TIMEOUT 完全没有入站帧即判定假死，主动断开让守护任务重连。
    let mut heartbeat = tokio::time::interval(HEARTBEAT_TICK);
    heartbeat.tick().await; // 消费首次立即到期的 tick，避免刚连上就误判。
    let mut last_inbound = tokio::time::Instant::now();

    loop {
        tokio::select! {
            // `biased`：优先读，保证关闭帧/回执不会被写队列长期饿死。
            biased;

            // 健康检查失败 / 引擎无响应：上层可直接踢掉会话。
            _ = inner.kick.notified() => {
                tracing::warn!("aria2 RPC 收到强制断开指令，结束当前会话");
                break;
            }

            incoming = stream.next() => {
                match incoming {
                    Some(Ok(message)) => {
                        last_inbound = tokio::time::Instant::now();
                        match message {
                            Message::Text(text) => handle_text(inner, text.as_str()),
                            Message::Binary(bytes) => {
                                // aria2 只发文本帧；二进制帧按 UTF-8 容错处理，不静默丢弃。
                                match std::str::from_utf8(&bytes) {
                                    Ok(text) => handle_text(inner, text),
                                    Err(_) => tracing::warn!("aria2 RPC 收到非法二进制帧（非 UTF-8）"),
                                }
                            }
                            Message::Ping(payload) => {
                                if let Err(err) = sink.send(Message::Pong(payload)).await {
                                    tracing::warn!("aria2 RPC 回 Pong 失败：{err}");
                                    break;
                                }
                            }
                            Message::Close(frame) => {
                                tracing::info!("aria2 RPC 收到关闭帧：{frame:?}");
                                break;
                            }
                            // Pong / Frame 等无需处理（Pong 已在上面刷新 last_inbound）。
                            _ => {}
                        }
                    }
                    Some(Err(err)) => {
                        tracing::warn!("aria2 RPC 读取出错：{err}");
                        break;
                    }
                    None => break,
                }
            }

            _ = heartbeat.tick() => {
                if last_inbound.elapsed() >= HEARTBEAT_TIMEOUT {
                    tracing::warn!(
                        "aria2 RPC 心跳超时（{}s 无入站帧），强制断开",
                        HEARTBEAT_TIMEOUT.as_secs()
                    );
                    break;
                }
                // 主动 Ping：对端若已消失但 TCP 未报错，Pong 缺失会让上面的超时兜底生效。
                if let Err(err) = sink.send(Message::Ping(Vec::new().into())).await {
                    tracing::warn!("aria2 RPC 心跳发送失败：{err}");
                    break;
                }
            }

            outgoing = out_rx.recv() => {
                let Some(frame) = outgoing else { break };
                if let Err(err) = sink.send(Message::text(frame)).await {
                    tracing::warn!("aria2 RPC 发送失败：{err}");
                    break;
                }
            }
        }
    }
}

/// 处理一个文本帧：先按 `id` 找等待者（响应），否则按 `method` 当通知广播。
fn handle_text(inner: &Inner, text: &str) {
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!("aria2 RPC 帧不是合法 JSON：{err}");
            return;
        }
    };

    if let Some(id) = response_id(&value) {
        let waiter = {
            let mut guard = inner
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.remove(&id)
        };
        let Some(waiter) = waiter else {
            // 超时后迟到的回执：正常现象，丢弃即可（`pending` 已在超时时清理）。
            tracing::debug!("aria2 RPC 收到无人等待的回执 id={id}");
            return;
        };
        let result = match value.get("error") {
            Some(error) if !error.is_null() => Err(RpcError::Server {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误")
                    .to_string(),
            }),
            _ => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
        };
        // 接收方可能已因超时放弃：send 失败是预期内的。
        let _ = waiter.send(result);
        return;
    }

    let Some(method) = value.get("method").and_then(Value::as_str) else {
        tracing::debug!("aria2 RPC 收到无法识别的帧，已忽略");
        return;
    };
    let Some(event) = event_for_method(method) else {
        tracing::debug!("aria2 RPC 收到未知通知 {method}，已忽略");
        return;
    };
    // aria2 的通知固定为 `params: [{"gid": "..."}]`。
    let gid = value
        .get("params")
        .and_then(|p| p.get(0))
        .and_then(|p| p.get("gid"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let _ = inner.notify_tx.send(Notification::Aria2 { gid, event });
}

/// 取出响应里的 `id`。aria2 通常原样回显数字 id，但兼容字符串形式。
fn response_id(value: &Value) -> Option<u64> {
    let id = value.get("id")?;
    if let Some(n) = id.as_u64() {
        return Some(n);
    }
    id.as_str().and_then(|s| s.parse::<u64>().ok())
}

fn event_for_method(method: &str) -> Option<Event> {
    match method {
        "aria2.onDownloadStart" => Some(Event::Start),
        "aria2.onDownloadPause" => Some(Event::Pause),
        "aria2.onDownloadStop" => Some(Event::Stop),
        "aria2.onDownloadComplete" => Some(Event::Complete),
        "aria2.onDownloadError" => Some(Event::Error),
        "aria2.onBtDownloadComplete" => Some(Event::BtComplete),
        _ => None,
    }
}

/// 断链时唤醒所有等待者并清表 —— 任何调用方都不会无限期悬挂。
fn fail_all_pending(inner: &Inner, reason: &str) {
    let waiters: Vec<(u64, oneshot::Sender<Result<Value, RpcError>>)> = {
        let mut guard = inner
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.drain().collect()
    };
    let count = waiters.len();
    for (_, waiter) in waiters {
        let _ = waiter.send(Err(RpcError::Disconnected(reason.to_string())));
    }
    if count > 0 {
        tracing::warn!("aria2 RPC 断链：已唤醒 {count} 个在途调用");
    }
}

/// 便宜的抖动种子（不引入 `rand`）：取系统时间亚秒纳秒位，仅用于打散重连时刻。
fn jitter_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 帧解析：`id` 既支持数字也支持字符串（aria2 不同版本行为不完全一致）。
    #[test]
    fn parses_numeric_and_string_ids() {
        assert_eq!(response_id(&json!({ "id": 7 })), Some(7));
        assert_eq!(response_id(&json!({ "id": "7" })), Some(7));
        assert_eq!(response_id(&json!({ "method": "aria2.onDownloadStart" })), None);
    }

    /// 通知方法名 → 事件枚举全覆盖。
    #[test]
    fn maps_all_download_notifications() {
        assert_eq!(event_for_method("aria2.onDownloadStart"), Some(Event::Start));
        assert_eq!(event_for_method("aria2.onDownloadComplete"), Some(Event::Complete));
        assert_eq!(event_for_method("aria2.onDownloadError"), Some(Event::Error));
        assert_eq!(event_for_method("aria2.onBtDownloadComplete"), Some(Event::BtComplete));
        assert_eq!(event_for_method("aria2.onDownloadPause"), Some(Event::Pause));
        assert_eq!(event_for_method("aria2.onDownloadStop"), Some(Event::Stop));
        assert_eq!(event_for_method("system.multicall"), None);
    }

    /// 断链唤醒：所有等待者都收到 `Disconnected`，且 map 被清空（幂等）。
    #[test]
    fn fail_all_pending_wakes_every_waiter() {
        let (out_tx, _out_rx) = mpsc::channel::<String>(1);
        let (notify_tx, _) = broadcast::channel::<Notification>(1);
        let inner = Inner {
            url: "ws://127.0.0.1:6800/jsonrpc".to_string(),
            secret: std::sync::RwLock::new("s".to_string()),
            next_id: AtomicU64::new(1),
            pending: StdMutex::new(HashMap::new()),
            out_tx,
            notify_tx,
            connected: AtomicBool::new(true),
            closing: AtomicBool::new(false),
            wake: tokio::sync::Notify::new(),
            kick: tokio::sync::Notify::new(),
        };

        let (tx1, mut rx1) = oneshot::channel();
        let (tx2, mut rx2) = oneshot::channel();
        {
            let mut guard = inner.pending.lock().unwrap();
            guard.insert(1, tx1);
            guard.insert(2, tx2);
        }

        fail_all_pending(&inner, "连接已断开");
        assert!(inner.pending.lock().unwrap().is_empty(), "映射表必须清空");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            for rx in [&mut rx1, &mut rx2] {
                match rx.await {
                    Ok(Err(RpcError::Disconnected(_))) => {}
                    other => panic!("期望 Disconnected，实际 {other:?}"),
                }
            }
        });

        // 二次调用为空操作（幂等）。
        fail_all_pending(&inner, "连接已断开");
    }

    /// 未连接时调用必须**立刻**返回 Disconnected，绝不进入等待。
    #[test]
    fn call_without_connection_fails_fast() {
        let (out_tx, _out_rx) = mpsc::channel::<String>(1);
        let (notify_tx, _) = broadcast::channel::<Notification>(1);
        let client = Client {
            inner: Arc::new(Inner {
                url: "ws://127.0.0.1:6800/jsonrpc".to_string(),
                secret: std::sync::RwLock::new("s".to_string()),
                next_id: AtomicU64::new(1),
                pending: StdMutex::new(HashMap::new()),
                out_tx,
                notify_tx,
                connected: AtomicBool::new(false),
                closing: AtomicBool::new(false),
                wake: tokio::sync::Notify::new(),
                kick: tokio::sync::Notify::new(),
            }),
        };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(async { client.call("aria2.getVersion", Vec::new()).await })
            .unwrap_err();
        assert!(matches!(err, RpcError::Disconnected(_)));
        assert!(client.inner.pending.lock().unwrap().is_empty());
    }
}
