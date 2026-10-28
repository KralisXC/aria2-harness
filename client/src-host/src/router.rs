//! C-3 Phase 2 §6.3.4–§6.3.6：协议路由 + 6800 对外 JSON-RPC 服务端。
//!
//! 拓扑（Q3 选 B，红线 R2）：
//!
//! ```text
//! UI / 扩展 / browser_api
//!         │  ws://127.0.0.1:6800/jsonrpc（secret 鉴权）
//!         ▼
//!   [service 6800 RpcServer] ── Router ──┬─ Aria2Client → aria2c @127.0.0.1:6801
//!                                        └─ NativeClient → engine-net（同进程）
//! ```
//!
//! 关键约束：
//! - **对外只有 6800**；6801 只绑回环，且 `--engine=aria2` 回滚时根本不启动代理（见 service）。
//! - 同一 GID 只属于一个引擎（R1）：路由表 `gid → engine` 只在 `addUri` 成功时写入。
//! - 路由决策落审计日志（R3）：`gid / url / engine / timestamp`。
//! - 通知双向转发：aria2c 的 `Notification::Aria2` 与 engine-net 的 `TaskEvent`
//!   统一成 `aria2.onDownload*` 帧，广播给每个 WS 客户端。

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};

use crate::engine::{EngineClient, EngineError};

/// `--engine=auto|aria2|native`（与 service 的开关同一份定义，避免两处漂移）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineMode {
    /// 按 URL 协议路由（默认）。
    Auto,
    /// 全部走 aria2c —— 与 v0.9.4 逐字节一致的回滚档（D2）。
    Aria2,
    /// 全部走 engine-net；非 HTTP(S) 协议明确报错，绝不静默降级。
    Native,
}

impl EngineMode {
    /// 解析 `--engine=` / `ARIA2_SERVICE_ENGINE` 的原始取值。
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(str::trim) {
            None | Some("") | Some("auto") => Ok(EngineMode::Auto),
            Some("aria2") => Ok(EngineMode::Aria2),
            Some("native") => Ok(EngineMode::Native),
            Some(other) => Err(format!(
                "--engine 取值非法：{other}（只接受 auto|aria2|native）"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EngineMode::Auto => "auto",
            EngineMode::Aria2 => "aria2",
            EngineMode::Native => "native",
        }
    }
}

/// 引擎归属。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EngineKind {
    Aria2,
    Native,
}

impl EngineKind {
    /// `_engine` 扩展字段 / 审计日志 / DB `engine` 列共用的标识。
    pub fn name(self) -> &'static str {
        match self {
            EngineKind::Aria2 => "aria2",
            EngineKind::Native => "native",
        }
    }
}

// ---------------------------------------------------------------------------
// 协议路由
// ---------------------------------------------------------------------------

/// 单条 URI 的默认归属（`http/https` → native；其余 → aria2）。
///
/// `.torrent` / `.metalink` 走 HTTP 下载但语义是 BT 元数据，统一交给 aria2c。
pub fn uri_target(uri: &str) -> EngineKind {
    let lower = uri.trim().to_ascii_lowercase();
    if (lower.starts_with("http://") || lower.starts_with("https://"))
        && !is_torrent_like(&lower)
    {
        EngineKind::Native
    } else {
        EngineKind::Aria2
    }
}

fn is_torrent_like(uri: &str) -> bool {
    let path = uri.split(['?', '#']).next().unwrap_or_default();
    path.ends_with(".torrent") || path.ends_with(".metalink")
}

/// `aria2.addUri` 的引擎决策。
///
/// - 强制 `aria2` → 全部 aria2c（回滚档）。
/// - 强制 `native` → 只有纯 HTTP(S) 直链放行，其余明确报 `-32602`。
/// - `auto` → 全部 URI 都是纯 HTTP(S) 才走 native；只要有一个 magnet/ftp/sftp/
///   torrent，就整单交给 aria2c（同一任务的多 URI 是互为镜像，不能拆到两个引擎）。
pub fn route_add_uris(uris: &[String], mode: EngineMode) -> Result<EngineKind, EngineError> {
    if uris.is_empty() {
        return Err(EngineError::invalid_params("addUri 至少需要一个 URI"));
    }
    match mode {
        EngineMode::Aria2 => Ok(EngineKind::Aria2),
        EngineMode::Native => {
            if uris.iter().all(|uri| uri_target(uri) == EngineKind::Native) {
                Ok(EngineKind::Native)
            } else {
                Err(EngineError::invalid_params(
                    "engine=native 只支持纯 HTTP(S) 直链；该任务含非 HTTP 协议（magnet/ftp/torrent 等）",
                ))
            }
        }
        EngineMode::Auto => {
            if uris.iter().all(|uri| uri_target(uri) == EngineKind::Native) {
                Ok(EngineKind::Native)
            } else {
                Ok(EngineKind::Aria2)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 通知
// ---------------------------------------------------------------------------

/// 一条待转发的 `aria2.onDownload*` 通知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcNotification {
    pub method: String,
    pub gid: String,
    /// 附加字段（仅 `onDownloadProgress` 使用；数值字段全部是字符串）。
    pub info: Option<Value>,
}

impl RpcNotification {
    pub fn new(method: impl Into<String>, gid: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            gid: gid.into(),
            info: None,
        }
    }

    /// 带进度载荷的通知：`info` 为 `tellStatus` 形状的快照（数值字段全字符串）。
    pub fn with_info(method: impl Into<String>, gid: impl Into<String>, info: Value) -> Self {
        Self {
            method: method.into(),
            gid: gid.into(),
            info: Some(info),
        }
    }

    /// aria2 的通知帧固定为 `params: [{"gid": "..."}]`。
    ///
    /// `onDownloadProgress` 额外携带 `totalLength/completedLength/...`（全部字符串），
    /// 客户端只读 `gid` 也完全兼容（与老版 aria2 的字段风格一致）。
    pub fn to_frame(&self) -> String {
        let mut payload = match self.info.clone() {
            Some(Value::Object(map)) => map,
            _ => Map::new(),
        };
        payload.insert("gid".to_string(), json!(self.gid));
        json!({
            "jsonrpc": "2.0",
            "method": self.method,
            "params": [Value::Object(payload)],
        })
        .to_string()
    }
}

/// aria2c 通知 → 6800 广播。
pub fn spawn_aria2_forwarder(
    client: crate::rpc_client::Client,
    events: broadcast::Sender<RpcNotification>,
) {
    let mut rx = client.subscribe_notifications();
    tokio::spawn(async move {
        while let Ok(notification) = rx.recv().await {
            if let crate::rpc_client::Notification::Aria2 { gid, event } = notification {
                let _ = events.send(RpcNotification::new(event_method(event), gid));
            }
        }
    });
}

fn event_method(event: crate::rpc_client::Event) -> &'static str {
    use crate::rpc_client::Event;
    match event {
        Event::Start => "aria2.onDownloadStart",
        Event::Pause => "aria2.onDownloadPause",
        Event::Stop => "aria2.onDownloadStop",
        Event::Complete => "aria2.onDownloadComplete",
        Event::Error => "aria2.onDownloadError",
        Event::BtComplete => "aria2.onBtDownloadComplete",
    }
}

/// engine-net 任务事件 → 6800 广播（`TaskEventKind::method()` 已给出 aria2 方法名）。
///
/// `onDownloadProgress` 携带 `tellStatus` 形状的快照，序列化由
/// `engine_net::rpc::types::status_to_value` 统一完成（长度/速度/连接数全是字符串）。
#[cfg(feature = "native-http")]
pub fn spawn_native_forwarder(
    engine: Arc<engine_net::rpc::methods::Engine>,
    events: broadcast::Sender<RpcNotification>,
) {
    let mut rx = engine.subscribe_events();
    tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            let notification = match event.progress.as_ref() {
                Some(snapshot) => RpcNotification::with_info(
                    event.kind.method(),
                    event.gid,
                    engine_net::rpc::types::status_to_value(snapshot),
                ),
                None => RpcNotification::new(event.kind.method(), event.gid),
            };
            let _ = events.send(notification);
        }
    });
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// 引擎路由表 + 方法分派。
pub struct Router {
    mode: EngineMode,
    aria2: Arc<dyn EngineClient>,
    native: Option<Arc<dyn EngineClient>>,
    /// GID → 引擎。只在 `addUri` / `addTorrent` 成功时写入（R1）。
    ///
    /// 未命中时按模式回落到 aria2（auto/aria2）：Phase 2 的 native 任务**只能**
    /// 由本 Router 创建，不存在"未命中却是 native"的情况 —— 因此不必依赖
    /// GID 前缀猜测，避免 aria2c 恰好生成 `fffe` 前缀时误路由。
    index: StdMutex<HashMap<String, EngineKind>>,
    events: broadcast::Sender<RpcNotification>,
}

impl Router {
    pub fn new(
        mode: EngineMode,
        aria2: Arc<dyn EngineClient>,
        native: Option<Arc<dyn EngineClient>>,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            mode,
            aria2,
            native,
            index: StdMutex::new(HashMap::new()),
            events,
        })
    }

    pub fn mode(&self) -> EngineMode {
        self.mode
    }

    /// 通知广播源（service 把两个引擎的事件灌进来，RpcServer 分发给各连接）。
    pub fn events(&self) -> broadcast::Sender<RpcNotification> {
        self.events.clone()
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<RpcNotification> {
        self.events.subscribe()
    }

    /// GID 的归属（R1：一个 GID 只属于一个引擎）。
    fn engine_for_gid(&self, gid: &str) -> EngineKind {
        if let Some(kind) = self.index.lock().unwrap().get(gid) {
            return *kind;
        }
        match self.mode {
            EngineMode::Native => EngineKind::Native,
            EngineMode::Auto | EngineMode::Aria2 => EngineKind::Aria2,
        }
    }

    fn client(&self, kind: EngineKind) -> Result<Arc<dyn EngineClient>, EngineError> {
        match kind {
            EngineKind::Aria2 => Ok(Arc::clone(&self.aria2)),
            EngineKind::Native => self.native.as_ref().map(Arc::clone).ok_or_else(|| {
                EngineError::internal(
                    "native 引擎未启用：请以 --features native-http 编译，并确认 --engine 不是 aria2",
                )
            }),
        }
    }

    /// 当前模式下可查询的引擎列表。
    fn query_engines(&self) -> Vec<EngineKind> {
        match self.mode {
            EngineMode::Aria2 => vec![EngineKind::Aria2],
            EngineMode::Native => vec![EngineKind::Native],
            EngineMode::Auto => {
                if self.native.is_some() {
                    vec![EngineKind::Aria2, EngineKind::Native]
                } else {
                    vec![EngineKind::Aria2]
                }
            }
        }
    }

    async fn dispatch(
        &self,
        kind: EngineKind,
        method: &str,
        params: &[Value],
    ) -> Result<Value, EngineError> {
        self.client(kind)?
            .call(method, Value::Array(params.to_vec()))
            .await
    }

    /// 6800 的方法入口。`params` **不含** `token:<secret>`（由 RpcServer 剥离）。
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, EngineError> {
        let args = match params {
            Value::Null => Vec::new(),
            Value::Array(items) => items,
            _ => return Err(EngineError::invalid_params("params 必须是数组")),
        };

        match method {
            "aria2.addUri" => self.add_uri(&args).await,
            "aria2.addTorrent" | "aria2.addMetalink" => {
                self.add_metadata_task(method, &args).await
            }
            "aria2.tellStatus" => self.tell_status(&args).await,
            "aria2.tellActive" | "aria2.tellWaiting" | "aria2.tellStopped" => {
                self.tell_list(method, &args).await
            }
            "aria2.getGlobalStat" => self.global_stat(&args).await,
            "aria2.getVersion" | "aria2.getGlobalOption" | "aria2.changeGlobalOption"
            | "aria2.getSessionInfo" | "aria2.saveSession" => {
                let value = self.dispatch(self.primary(), method, &args).await?;
                Ok(with_engine(value, self.primary()))
            }
            "system.multicall" => {
                // Q3 已签字：system.multicall 名义 Phase 2.5。回滚档必须保持 aria2 原样。
                if self.mode == EngineMode::Aria2 {
                    self.dispatch(EngineKind::Aria2, method, &args).await
                } else {
                    Err(EngineError::new(
                        -32601,
                        "system.multicall 在 auto/native 模式下留待 Phase 2.5",
                    ))
                }
            }
            "aria2.pauseAll" | "aria2.unpauseAll" | "aria2.forcePauseAll" => {
                self.pause_all(method).await
            }
            // 清空已完成必须对**所有**引擎生效：auto 模式下历史任务分散在
            // native 与 aria2c 两张表里，只清一个会让另一个的"幽灵记录"继续
            // 显示在列表里（beta10 实测问题）。并发下发，全部成功才算成功。
            "aria2.purgeDownloadResult" => self.broadcast_all(method, &args).await,
            // 停机类方法一律交给 aria2c（与回滚档行为一致），绝不能落 -32601。
            "aria2.shutdown" | "aria2.forceShutdown" => {
                self.dispatch(EngineKind::Aria2, method, &args).await
            }
            _ => {
                if is_gid_method(method) {
                    self.gid_method(method, &args).await
                } else {
                    Err(EngineError::new(
                        -32601,
                        format!("方法不存在：{method}"),
                    ))
                }
            }
        }
    }

    /// 全局/单例方法的主引擎：auto 下以 aria2c 为准（v0.9.4 的权威来源）。
    fn primary(&self) -> EngineKind {
        match self.mode {
            EngineMode::Native => EngineKind::Native,
            EngineMode::Auto | EngineMode::Aria2 => EngineKind::Aria2,
        }
    }

    async fn add_uri(&self, args: &[Value]) -> Result<Value, EngineError> {
        let uris = uri_list(args)?;
        let kind = route_add_uris(&uris, self.mode)?;
        let value = self.dispatch(kind, "aria2.addUri", args).await?;
        let gid = gid_from_result(&value)?;
        self.record_gid(&gid, kind, &uris[0]);
        Ok(value)
    }

    /// `addTorrent` / `addMetalink` 永远是 BT/元数据任务 → aria2c。
    async fn add_metadata_task(
        &self,
        method: &str,
        args: &[Value],
    ) -> Result<Value, EngineError> {
        if self.mode == EngineMode::Native {
            return Err(EngineError::invalid_params(format!(
                "engine=native 不支持 {method}（BT 任务固定走 aria2c）"
            )));
        }
        let value = self.dispatch(EngineKind::Aria2, method, args).await?;
        let gid = gid_from_result(&value)?;
        let label = if method.ends_with("addTorrent") {
            "torrent"
        } else {
            "metalink"
        };
        self.record_gid(&gid, EngineKind::Aria2, label);
        Ok(value)
    }

    /// 写入 GID 归属 + 审计日志（R1 / R3）。同一 GID 重复出现时以首次为准。
    fn record_gid(&self, gid: &str, kind: EngineKind, source: &str) {
        let mut index = self.index.lock().unwrap();
        index.entry(gid.to_string()).or_insert(kind);
        drop(index);
        println!(
            "[route] gid={gid} url={source} engine={} at={}",
            kind.name(),
            chrono::Local::now().to_rfc3339()
        );
    }

    async fn tell_status(&self, args: &[Value]) -> Result<Value, EngineError> {
        let gid = gid_param(args)?;
        let kind = self.engine_for_gid(&gid);
        let value = self.dispatch(kind, "aria2.tellStatus", args).await?;
        Ok(with_engine(value, kind))
    }

    /// `tellActive` / `tellWaiting` / `tellStopped`：auto 模式下合并两个引擎的结果。
    async fn tell_list(&self, method: &str, args: &[Value]) -> Result<Value, EngineError> {
        let kinds = self.query_engines();
        if kinds.len() == 1 {
            let kind = kinds[0];
            let value = self.dispatch(kind, method, args).await?;
            return Ok(decorate_items(value, kind));
        }

        // auto：先各自取全量，再本地按 aria2 的 offset/num 语义切片。
        let (offset, num) = if method == "aria2.tellActive" {
            (0usize, usize::MAX)
        } else {
            (
                args.first().and_then(Value::as_u64).unwrap_or(0) as usize,
                args.get(1).and_then(Value::as_u64).unwrap_or(1000) as usize,
            )
        };

        let mut merged = Vec::new();
        for kind in kinds {
            let mut page_args = args.to_vec();
            if method != "aria2.tellActive" {
                page_args.clear();
                page_args.push(json!(0));
                page_args.push(json!(100_000));
                if let Some(keys) = args.get(2) {
                    page_args.push(keys.clone());
                }
            }
            let value = self.dispatch(kind, method, &page_args).await?;
            match decorate_items(value, kind) {
                Value::Array(items) => merged.extend(items),
                _ => return Err(EngineError::internal(format!("{method} 返回不是数组"))),
            }
        }

        let items = if method == "aria2.tellActive" {
            merged
        } else {
            merged.into_iter().skip(offset).take(num).collect()
        };
        Ok(Value::Array(items))
    }

    /// `getGlobalStat`：auto 下把两个引擎的计数与速度相加（字段全部字符串）。
    async fn global_stat(&self, args: &[Value]) -> Result<Value, EngineError> {
        let kinds = self.query_engines();
        if kinds.len() == 1 {
            return self.dispatch(kinds[0], "aria2.getGlobalStat", args).await;
        }

        let mut merged: Option<Map<String, Value>> = None;
        let mut last_error = None;
        for kind in kinds {
            match self.dispatch(kind, "aria2.getGlobalStat", args).await {
                Ok(Value::Object(stats)) => {
                    merged = Some(match merged {
                        None => stats,
                        Some(current) => merge_stats(current, stats),
                    })
                }
                Ok(_) => return Err(EngineError::internal("getGlobalStat 返回不是对象")),
                Err(err) => last_error = Some(err),
            }
        }
        match merged {
            Some(stats) => Ok(Value::Object(stats)),
            None => Err(last_error.unwrap_or_else(|| EngineError::internal("没有可用引擎"))),
        }
    }

    async fn gid_method(&self, method: &str, args: &[Value]) -> Result<Value, EngineError> {
        let gid = gid_param(args)?;
        let kind = self.engine_for_gid(&gid);
        self.dispatch(kind, method, args).await
    }

    /// 对全部引擎生效的暂停/恢复。engine-net 没有 `pauseAll`，这里按任务逐个下发。
    async fn pause_all(&self, method: &str) -> Result<Value, EngineError> {
        let mut first_ok: Option<Value> = None;
        let mut last_error: Option<EngineError> = None;
        for kind in self.query_engines() {
            match self.dispatch(kind, method, &[]).await {
                Ok(value) => {
                    if first_ok.is_none() {
                        first_ok = Some(value);
                    }
                }
                // engine-net 未实现 pauseAll：退化成逐 GID 调用。
                Err(err) if err.code == -32601 && kind == EngineKind::Native => {
                    match self.pause_all_native(method).await {
                        Ok(()) => {
                            if first_ok.is_none() {
                                first_ok = Some(json!("OK"));
                            }
                        }
                        Err(err) => last_error = Some(err),
                    }
                }
                Err(err) => last_error = Some(err),
            }
        }
        match first_ok {
            Some(value) => Ok(value),
            None => Err(last_error.unwrap_or_else(|| EngineError::internal("没有可用引擎"))),
        }
    }

    async fn pause_all_native(&self, method: &str) -> Result<(), EngineError> {
        let target: &str = if method == "aria2.unpauseAll" {
            "aria2.tellWaiting"
        } else {
            "aria2.tellActive"
        };
        let list = self
            .dispatch(EngineKind::Native, target, &[json!(0), json!(100_000)])
            .await?;
        let gids: Vec<String> = list
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("gid").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let op = if method == "aria2.unpauseAll" {
            "aria2.unpause"
        } else {
            "aria2.pause"
        };
        for gid in gids {
            let _ = self
                .dispatch(EngineKind::Native, op, &[json!(gid)])
                .await;
        }
        Ok(())
    }

    /// 对所有引擎并发下发同一请求，**全部成功**才算成功。
    ///
    /// 与 `broadcast_first_ok` 的差别：任何一个引擎失败都会把错误回传给调用方，
    /// 绝不因为"另一个引擎清干净了"就谎报成功（beta10 的幽灵残留根因）。
    /// 没有任何引擎时返回 `OK`（无事可做本身就是成功）。
    async fn broadcast_all(&self, method: &str, args: &[Value]) -> Result<Value, EngineError> {
        let requests = self
            .query_engines()
            .into_iter()
            .map(|kind| self.dispatch(kind, method, args));
        let results = futures_util::future::join_all(requests).await;

        let mut first_ok: Option<Value> = None;
        let mut last_error: Option<EngineError> = None;
        for result in results {
            match result {
                Ok(value) => {
                    if first_ok.is_none() {
                        first_ok = Some(value);
                    }
                }
                Err(err) => last_error = Some(err),
            }
        }
        match last_error {
            Some(err) => Err(err),
            None => Ok(first_ok.unwrap_or_else(|| json!("OK"))),
        }
    }
}

fn is_gid_method(method: &str) -> bool {
    matches!(
        method,
        "aria2.pause"
            | "aria2.unpause"
            | "aria2.forcePause"
            | "aria2.remove"
            | "aria2.forceRemove"
            | "aria2.removeDownloadResult"
            | "aria2.tellFiles"
            | "aria2.getFiles"
            | "aria2.getOption"
            | "aria2.changeOption"
            | "aria2.changePosition"
    )
}

fn uri_list(args: &[Value]) -> Result<Vec<String>, EngineError> {
    let uris: Vec<String> = args
        .first()
        .and_then(Value::as_array)
        .ok_or_else(|| EngineError::invalid_params("addUri 第一个参数必须是 URI 数组"))?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    if uris.is_empty() {
        return Err(EngineError::invalid_params("addUri 至少需要一个 URI"));
    }
    Ok(uris)
}

fn gid_param(args: &[Value]) -> Result<String, EngineError> {
    args.first()
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| EngineError::invalid_params("第一个参数必须是 GID 字符串"))
}

fn gid_from_result(value: &Value) -> Result<String, EngineError> {
    value
        .as_str()
        .filter(|gid| !gid.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| EngineError::internal("addUri 返回的 GID 非法"))
}

/// 单条结果补 `_engine` 扩展字段（R3：tellStatus 可查）。
fn with_engine(mut value: Value, kind: EngineKind) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.insert("_engine".to_string(), json!(kind.name()));
    }
    value
}

/// 列表结果逐项补 `_engine`。
fn decorate_items(value: Value, kind: EngineKind) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| with_engine(item, kind))
                .collect(),
        ),
        other => other,
    }
}

/// 两个引擎的 `getGlobalStat` 相加。非数字字段以先到的为准。
fn merge_stats(mut left: Map<String, Value>, right: Map<String, Value>) -> Map<String, Value> {
    for (key, value) in right {
        let merged = match (left.get(&key).and_then(as_u64), as_u64(&value)) {
            (Some(a), Some(b)) => json!((a + b).to_string()),
            _ => value,
        };
        left.insert(key, merged);
    }
    left
}

fn as_u64(value: &Value) -> Option<u64> {
    value.as_str().and_then(|text| text.parse::<u64>().ok())
}

// ---------------------------------------------------------------------------
// 6800 WS JSON-RPC 服务端
// ---------------------------------------------------------------------------

/// 已启动的 6800 服务端句柄：停止时先广播 shutdown，再等 accept 循环退出。
pub struct RpcServerHandle {
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    port: u16,
}

impl RpcServerHandle {
    pub fn port(&self) -> u16 {
        self.port
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

/// 绑定并启动 6800 服务端。绑定失败在返回前就能看到（端口被占用等）。
///
/// `secret` 是**共享可变值**：用户改密钥时由 service 原地写入，本函数不重建监听器、
/// 不断开既有连接（红线：改密钥不得中断当前下载）。
pub async fn start_server(
    router: Arc<Router>,
    port: u16,
    secret: Arc<StdMutex<String>>,
) -> std::io::Result<RpcServerHandle> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let (shutdown, mut shutdown_rx) = watch::channel(false);

    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, peer)) => {
                            let router = Arc::clone(&router);
                            let secret = Arc::clone(&secret);
                            tokio::spawn(async move {
                                if let Err(err) = serve_connection(router, secret, stream).await {
                                    tracing::debug!("[rpc] 客户端 {peer} 断开：{err}");
                                }
                            });
                        }
                        Err(err) => tracing::warn!("[rpc] accept 失败：{err}"),
                    }
                }
            }
        }
        tracing::info!("[rpc] 6800 服务端已停止");
    });

    Ok(RpcServerHandle {
        shutdown,
        task,
        port,
    })
}

async fn serve_connection(
    router: Arc<Router>,
    secret: Arc<StdMutex<String>>,
    stream: tokio::net::TcpStream,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio_tungstenite::tungstenite::Message;

    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut incoming) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<String>(64);

    // 通知泵：把广播帧写进本连接的发送队列。
    let mut events = router.subscribe_events();
    let notify_tx = out_tx.clone();
    let notifier = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(notification) => {
                    if notify_tx.send(notification.to_frame()).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!("[rpc] 通知积压，丢弃 {skipped} 条");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // 写泵：请求回执与通知共用同一条 WS 连接。
    let writer = tokio::spawn(async move {
        while let Some(payload) = out_rx.recv().await {
            if sink.send(Message::text(payload)).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    while let Some(frame) = incoming.next().await {
        let frame = frame?;
        match frame {
            Message::Text(text) => {
                // 每请求读一次当前密钥：热更新后旧连接的下一条请求立即按新密钥鉴权，
                // 连接本身不受影响（不断线、不重启监听器）。
                let current = secret
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                if let Some(response) = handle_request(&router, &current, text.as_str()).await {
                    if out_tx.send(response).await.is_err() {
                        break;
                    }
                }
            }
            Message::Close(_) => break,
            // Ping/Pong 由 tungstenite 自动应答；二进制帧不属于 aria2 协议，忽略。
            _ => {}
        }
    }

    drop(out_tx);
    notifier.abort();
    let _ = writer.await;
    Ok(())
}

/// 单条请求 → 单条响应（无 `id` 的客户端通知回 `None`，不回包）。
async fn handle_request(router: &Arc<Router>, secret: &str, text: &str) -> Option<String> {
    let request: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(err) => {
            return Some(error_frame(
                &Value::Null,
                -32700,
                &format!("JSON 解析失败：{err}"),
            ))
        }
    };

    let id = request.get("id").cloned().unwrap_or(Value::Null);
    // 客户端发来的无 id 帧按通知处理（aria2 同样忽略）。
    if id.is_null() {
        return None;
    }
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return Some(error_frame(&id, -32600, "缺 method 字段"));
    };
    let mut params = match request.get("params") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(_) => return Some(error_frame(&id, -32602, "params 必须是数组")),
    };

    if !authorize(&mut params, secret) {
        return Some(error_frame(
            &id,
            1,
            "Unauthorized：params 首项必须是 token:<secret>",
        ));
    }

    match router.call(method, Value::Array(params)).await {
        Ok(result) => Some(
            json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
        ),
        Err(err) => Some(error_frame(&id, err.code, &err.message)),
    }
}

fn error_frame(id: &Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
    .to_string()
}

/// 校验并剥离 `token:<secret>`（与 aria2 `--rpc-secret` 语义一致）。
fn authorize(params: &mut Vec<Value>, secret: &str) -> bool {
    let Some(Value::String(token)) = params.first() else {
        return false;
    };
    let Some(value) = token.strip_prefix("token:") else {
        return false;
    };
    if value != secret {
        return false;
    }
    params.remove(0);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::BoxFuture;

    // ---------------------------------------------------------------
    // 纯函数：协议路由
    // ---------------------------------------------------------------

    fn uris(list: &[&str]) -> Vec<String> {
        list.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn http_goes_native_and_bt_goes_aria2() {
        assert_eq!(uri_target("http://a.test/x.bin"), EngineKind::Native);
        assert_eq!(uri_target("HTTPS://a.test/x.bin"), EngineKind::Native);
        assert_eq!(uri_target("magnet:?xt=urn:btih:xx"), EngineKind::Aria2);
        assert_eq!(uri_target("ftp://a.test/x.bin"), EngineKind::Aria2);
        assert_eq!(uri_target("sftp://a.test/x.bin"), EngineKind::Aria2);
        // HTTP 承载的 BT 元数据仍属 aria2。
        assert_eq!(uri_target("https://a.test/x.torrent"), EngineKind::Aria2);
        assert_eq!(uri_target("https://a.test/x.metalink?x=1"), EngineKind::Aria2);
    }

    #[test]
    fn auto_routes_by_protocol() {
        assert_eq!(
            route_add_uris(&uris(&["http://a.test/x"]), EngineMode::Auto).unwrap(),
            EngineKind::Native
        );
        assert_eq!(
            route_add_uris(&uris(&["http://a.test/x", "ftp://a.test/x"]), EngineMode::Auto)
                .unwrap(),
            EngineKind::Aria2,
            "同任务多 URI 不能拆到两个引擎"
        );
        assert_eq!(
            route_add_uris(&uris(&["magnet:?xt=urn:btih:xx"]), EngineMode::Auto).unwrap(),
            EngineKind::Aria2
        );
    }

    /// beta12.2 回归：插件分片（HLS 等纯 HTTP(S) 直链）在 auto 模式下必须路由到
    /// native —— 否则 release 漏带 `native-http` 时会报「native 引擎未启用」。
    #[test]
    fn auto_routes_http_segments_to_native() {
        let segments = uris(&[
            "https://cdn.example.com/hls/seg-00001.ts",
            "http://cdn.example.com/hls/seg-00002.ts?token=abc",
            "https://cdn.example.com/hls/index.m3u8",
        ]);
        assert_eq!(
            route_add_uris(&segments, EngineMode::Auto).unwrap(),
            EngineKind::Native,
            "纯 HTTP(S) 分片必须由 auto 路由到 native"
        );
    }

    #[test]
    fn forced_modes_override_protocol() {
        assert_eq!(
            route_add_uris(&uris(&["http://a.test/x"]), EngineMode::Aria2).unwrap(),
            EngineKind::Aria2
        );
        let err =
            route_add_uris(&uris(&["magnet:?xt=urn:btih:xx"]), EngineMode::Native).unwrap_err();
        assert_eq!(err.code, -32602);
        assert!(route_add_uris(&uris(&[]), EngineMode::Auto).is_err());
    }

    #[test]
    fn engine_mode_parse_is_strict() {
        assert_eq!(EngineMode::parse(None).unwrap(), EngineMode::Auto);
        assert_eq!(EngineMode::parse(Some("aria2")).unwrap(), EngineMode::Aria2);
        assert_eq!(EngineMode::parse(Some("native")).unwrap(), EngineMode::Native);
        assert!(EngineMode::parse(Some("Aria2")).is_err());
    }

    // ---------------------------------------------------------------
    // 鉴权
    // ---------------------------------------------------------------

    #[test]
    fn authorize_requires_token_first() {
        let mut params = vec![json!("token:s3"), json!("gid")];
        assert!(authorize(&mut params, "s3"));
        assert_eq!(params, vec![json!("gid")]);

        let mut missing = vec![json!("gid")];
        assert!(!authorize(&mut missing, "s3"));
        let mut wrong = vec![json!("token:x")];
        assert!(!authorize(&mut wrong, "s3"));
    }

    // ---------------------------------------------------------------
    // 统计合并
    // ---------------------------------------------------------------

    #[test]
    fn stats_merge_adds_shared_keys() {
        let left = json!({"downloadSpeed":"10","numActive":"1","numStoppedTotal":"7"});
        let right = json!({"downloadSpeed":"5","numActive":"2","numWaiting":"3"});
        let merged =
            merge_stats(left.as_object().unwrap().clone(), right.as_object().unwrap().clone());
        assert_eq!(merged["downloadSpeed"], "15");
        assert_eq!(merged["numActive"], "3");
        assert_eq!(merged["numStoppedTotal"], "7");
        assert_eq!(merged["numWaiting"], "3");
    }

    // ---------------------------------------------------------------
    // 路由表 + 方法分派（用桩客户端，不联网）
    // ---------------------------------------------------------------

    struct StubClient {
        name: &'static str,
        calls: StdMutex<Vec<(String, Value)>>,
        result: Result<Value, EngineError>,
    }

    impl StubClient {
        fn new(name: &'static str, result: Result<Value, EngineError>) -> Arc<Self> {
            Arc::new(Self {
                name,
                calls: StdMutex::new(Vec::new()),
                result,
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(method, _)| method.clone())
                .collect()
        }
    }

    impl EngineClient for StubClient {
        fn call<'a>(
            &'a self,
            method: &'a str,
            params: Value,
        ) -> BoxFuture<'a, Result<Value, EngineError>> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            let result = self.result.clone();
            Box::pin(async move { result })
        }

        fn name(&self) -> &'static str {
            self.name
        }
    }

    fn engine_error(code: i64) -> EngineError {
        EngineError::new(code, "not found")
    }

    #[tokio::test]
    async fn add_uri_records_owner_and_routes_again_by_gid() {
        let aria2 = StubClient::new("aria2", Ok(json!("2089b05ecca3d829")));
        let native = StubClient::new("native", Ok(json!("fffe000000000001")));
        let router = Router::new(
            EngineMode::Auto,
            Arc::clone(&aria2) as Arc<dyn EngineClient>,
            Some(Arc::clone(&native) as Arc<dyn EngineClient>),
        );

        let gid = router
            .call("aria2.addUri", json!([["http://a.test/x"]])
            )
            .await
            .unwrap();
        assert_eq!(gid, "fffe000000000001");
        assert_eq!(aria2.calls().len(), 0, "HTTP 任务不应打 aria2");

        // 同一 GID 的后续操作必须继续命中 native。
        router
            .call("aria2.tellStatus", json!(["fffe000000000001"]))
            .await
            .unwrap();
        assert_eq!(native.calls(), vec!["aria2.addUri", "aria2.tellStatus"]);
    }

    #[tokio::test]
    async fn unknown_gid_falls_back_to_aria2() {
        let aria2 = StubClient::new("aria2", Ok(json!({"gid":"2089","status":"active"})));
        let native = StubClient::new("native", Err(engine_error(1)));
        let router = Router::new(
            EngineMode::Auto,
            Arc::clone(&aria2) as Arc<dyn EngineClient>,
            Some(Arc::clone(&native) as Arc<dyn EngineClient>),
        );

        let status = router
            .call("aria2.tellStatus", json!(["2089b05ecca3d829"]))
            .await
            .unwrap();
        assert_eq!(status["_engine"], "aria2");
        assert_eq!(aria2.calls(), vec!["aria2.tellStatus"]);
        assert!(native.calls().is_empty());
    }

    #[tokio::test]
    async fn forced_native_rejects_bt_uri() {
        let aria2 = StubClient::new("aria2", Ok(json!("g")));
        let native = StubClient::new("native", Ok(json!("g")));
        let router = Router::new(
            EngineMode::Native,
            Arc::clone(&aria2) as Arc<dyn EngineClient>,
            Some(Arc::clone(&native) as Arc<dyn EngineClient>),
        );
        let err = router
            .call("aria2.addUri", json!([["magnet:?xt=urn:btih:xx"]]))
            .await
            .unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[tokio::test]
    async fn notification_frame_matches_aria2_shape() {
        let frame = RpcNotification::new("aria2.onDownloadComplete", "fffe000000000001");
        let value: Value = serde_json::from_str(&frame.to_frame()).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["method"], "aria2.onDownloadComplete");
        assert_eq!(value["params"][0]["gid"], "fffe000000000001");
    }

    /// 第三方（非 UI）推到 6800 的 `aria2.addUri` 必须**立刻**向所有已连接的 WS
    /// 客户端广播 `aria2.onDownloadStart` —— UI 的下载列表就靠这条通知秒级出卡片。
    ///
    /// 这里同时把 Router / native 引擎 / 转发器 / WS 服务端串成真实链路，
    /// 用裸 WS 客户端扮演"第三方脚本"，避免只测单点函数却漏掉接线。
    #[cfg(feature = "native-http")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn third_party_add_uri_broadcasts_start_to_ws_clients() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        // `start_server` 不回传 0 号端口的真实值，先探测一个空闲端口。
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let resolver = engine_net::dns::resolver::DnsResolver::new(
            &engine_net::dns::resolver::DnsConfig::default(),
        )
        .unwrap();
        let engine = Arc::new(engine_net::rpc::methods::Engine::new(resolver));
        let native = Arc::new(crate::engine::NativeClient::new(Arc::clone(&engine)));
        let router = Router::new(
            EngineMode::Native,
            StubClient::new("aria2", Ok(json!("stub"))) as Arc<dyn EngineClient>,
            Some(native as Arc<dyn EngineClient>),
        );
        spawn_native_forwarder(Arc::clone(&engine), router.events());
        let server = start_server(
            Arc::clone(&router),
            port,
            Arc::new(StdMutex::new("s3".to_string())),
        )
        .await
        .unwrap();

        let (mut ws, _) =
            tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/jsonrpc"))
                .await
                .unwrap();
        let dir = std::env::temp_dir().to_string_lossy().into_owned();
        let request = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "aria2.addUri",
            "params": [
                "token:s3",
                ["http://127.0.0.1:9/beta8.2-push-probe.bin"],
                { "dir": dir }
            ]
        });
        ws.send(Message::text(request.to_string())).await.unwrap();

        let mut seen: Option<String> = None;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            let Ok(Some(Ok(Message::Text(text)))) =
                tokio::time::timeout_at(deadline, ws.next()).await
            else {
                continue;
            };
            let value: Value = serde_json::from_str(&text).unwrap();
            // 先到的可能是 addUri 回执（带 id），只认通知帧。
            if value.get("id").is_some() {
                continue;
            }
            if value.get("method").and_then(Value::as_str) == Some("aria2.onDownloadStart") {
                seen = value["params"][0]["gid"].as_str().map(str::to_string);
                break;
            }
        }

        assert!(
            seen.as_deref().is_some_and(|gid| !gid.is_empty()),
            "第三方 addUri 必须立即广播 aria2.onDownloadStart（带 gid）"
        );
        server.shutdown().await;
    }
}
