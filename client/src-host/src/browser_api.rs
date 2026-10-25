//! 浏览器扩展接管的本地 HTTP API（骨架）。
//!
//! 设计目标与红线：
//! - **只绑 `127.0.0.1`**（默认 17800，可用 `ARIA2_HARNESS_HTTP_PORT` 覆盖），
//!   绝不监听 `0.0.0.0`，避免把下载器暴露到局域网；
//! - 端口被占用时自动顺延（17800 → 17809），并在日志里明确告警实际端口，
//!   绝不静默失败；
//! - 以独立 tokio 任务运行，任何请求都不会阻塞 UI / IPC 主链路；
//! - 允许浏览器扩展跨域访问（CORS `*`），但 **`POST /add` 必须携带
//!   `X-Harness-Token`**（缺失 / 不匹配一律 403）：防止任意网页向本机下载器
//!   投递任务。Token 每次启动随机生成（UUID v4），可用 `ARIA2_HARNESS_HTTP_TOKEN`
//!   固定；前端设置页可查看并复制；
//! - `/status` 与 `/ping` 保持开放，供扩展探测端口与版本；
//! - `GET /auto-token` **只允许浏览器扩展（或本机进程）读取 Token**：校验请求头
//!   `Origin`，`chrome-extension://...` 与**空 Origin**（MV3 Service Worker 实测
//!   不带 Origin）放行，网页（`http(s)://`）与 `null` 一律 403。浏览器不允许网页
//!   省略或伪造 Origin，因此恶意页面拿不到 Token，也就无法投毒；
//!   扩展无需用户手动复制粘贴，装完即用；
//! - `/add` **不在请求里等待用户确认**：数据落入进程级暂存后立即回
//!   `{"status":"ok","action":"pending"}`，再经 `app://browser-intercept` 唤醒
//!   前端。扩展的 fetch 超时只有几秒，而用户可能挂机几十分钟才点确认 ——
//!   只有彻底解耦"网络请求"与"UI 状态"，两端才不会互相拖死。
//!
//! 刻意不引入 axum / tiny_http：本模块只需要四个路由，手写 HTTP/1.1 解析
//! 可做到零新增依赖、零体积增量，也避免 gnullvm 工具链多一个链接风险点。

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{LazyLock, Mutex as StdMutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

/// 默认端口（浏览器扩展的约定端口）。
///
/// 原定 16800，但那是 Motrix 默认的 aria2 RPC 端口，两者同机运行时必然
/// `Address already in use`（实机已验证）。改为 17800 避开常见下载器占用。
const DEFAULT_PORT: u16 = 17800;
/// 端口被占用时的顺延尝试次数：17800 起最多试到 17809。
///
/// 端口冲突不该让接管能力静默失效 —— 扩展按固定端口探测，宿主必须尽力
/// 给出一个可用端口并在日志里说清楚实际落在哪。
const PORT_FALLBACK_SPAN: u16 = 10;
const PORT_ENV: &str = "ARIA2_HARNESS_HTTP_PORT";
const TOKEN_ENV: &str = "ARIA2_HARNESS_HTTP_TOKEN";
/// 与本软件握手时回传的引擎版本标识。
const ENGINE_VERSION: &str = "1.37.0";
/// 请求头上限（防止畸形请求耗尽内存）。
const MAX_HEAD: usize = 16 * 1024;
/// 请求体上限（浏览器扩展只传 URL / 头信息，1MiB 足够）。
const MAX_BODY: usize = 1024 * 1024;
/// 单次读写的墙钟上界。
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// 本机 API Token：启动时生成一次，进程内全局共享（前端设置页经 IPC 读取）。
static API_TOKEN: OnceLock<String> = OnceLock::new();
/// 实际监听端口；0 表示尚未完成绑定（UI 可稍后重试）。
static API_PORT: AtomicU16 = AtomicU16::new(0);

/// 前端接管的订阅事件名（与 `App.vue` 的 `listen` 一致）。
const INTERCEPT_EVENT: &str = "app://browser-intercept";

/// 浏览器扩展投递、等待用户确认的任务快照。
///
/// 字段是**富文本原文**（Cookie / UA 可能很长），由前端在确认时原样拼进
/// aria2 单任务 options —— 宿主这里只负责暂存与清洗 CR/LF。
#[derive(Debug, Clone, serde::Serialize)]
pub struct InterceptedTask {
    pub url: String,
    pub referer: String,
    pub cookie: String,
    pub user_agent: String,
    pub dir: String,
    /// 接收时刻（Unix 毫秒）：仅供 UI / 排查展示，**不做任何过期判定**。
    pub received_at: u64,
}

/// 待确认的浏览器任务（进程级单槽）。
///
/// 单槽而非队列：同一时刻用户只会在 UI 里确认一单，后到的请求直接覆盖前一单
/// （最新一次浏览器点击才是用户真正想要的）；不清空则一直保留，挂机多久都在。
static PENDING: LazyLock<StdMutex<Option<InterceptedTask>>> =
    LazyLock::new(|| StdMutex::new(None));

/// 读取暂存任务（序列化给前端；无暂存时返回 `None`）。
pub(crate) fn pending() -> Option<Value> {
    let guard = PENDING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .as_ref()
        .and_then(|task| serde_json::to_value(task).ok())
}

/// 清空暂存（用户取消 / 确认成功 / 任务被删除）。
pub(crate) fn clear_pending() {
    let mut guard = PENDING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = None;
}

/// 当前 Unix 毫秒时间戳（取不到时回退 0，不影响接管）。
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// 已生成的 API Token（`start` 未调用时返回空串）。
pub(crate) fn token() -> &'static str {
    API_TOKEN.get().map(String::as_str).unwrap_or_default()
}

/// 当前实际监听端口；尚未绑定成功时为 0。
pub(crate) fn port() -> u16 {
    API_PORT.load(Ordering::Relaxed)
}

/// 启动后台 HTTP 服务（幂等由 `bootstrap` 保证只调用一次）。
pub fn start() {
    let port = match std::env::var(PORT_ENV) {
        Ok(raw) => match raw.trim().parse::<u16>() {
            Ok(value) if value > 0 => value,
            _ => {
                tracing::warn!("{PORT_ENV}=`{raw}` 非法，回退默认端口 {DEFAULT_PORT}");
                DEFAULT_PORT
            }
        },
        Err(_) => DEFAULT_PORT,
    };
    // Token 采用"环境变量优先、否则每次启动随机生成"：随机值即商业级默认，
    // 恶意网页无法猜到；需要脚本长期对接时再用环境变量固定。
    let token = std::env::var(TOKEN_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let _ = API_TOKEN.set(token);

    crate::rt().spawn(async move {
        // 关键安全约束：只绑定回环地址（顺延也只在本机回环上顺延）。
        let (listener, actual_port) = match bind_with_fallback(port).await {
            Ok(bound) => bound,
            Err(e) => {
                // 全部顺延端口都不可用：记录明确原因，不阻断宿主启动。
                tracing::error!(
                    "本地接管 API 绑定失败（127.0.0.1:{port} 起尝试 {PORT_FALLBACK_SPAN} 个端口）：{e}"
                );
                return;
            }
        };
        let addr = SocketAddr::from(([127, 0, 0, 1], actual_port));
        // 绑定成功后才对外暴露端口：UI 读到 0 即表示"尚未就绪"。
        API_PORT.store(actual_port, Ordering::Relaxed);
        if actual_port != port {
            // 必须显式告警：浏览器扩展默认探测 17800，用户需要知道实际端口已顺延。
            tracing::warn!(
                "本地接管 API 端口 {port} 已被占用，已自动顺延到 {actual_port}；\
                 浏览器扩展需要把目标端口改为 {actual_port}（或释放 {port} 后重启）"
            );
        }
        tracing::info!(
            "本地接管 API 已启动：http://{addr}（POST /add 必须携带 X-Harness-Token，\
             Token 见设置 → 网络）"
        );

        loop {
            match listener.accept().await {
                Ok((stream, _peer)) => {
                    tokio::spawn(async move {
                        if let Err(e) = handle(stream).await {
                            tracing::debug!("本地接管 API 连接处理失败：{e}");
                        }
                    });
                }
                Err(e) => {
                    // accept 失败通常是瞬时资源问题：短暂退避，避免忙等打满 CPU。
                    tracing::warn!("本地接管 API accept 失败：{e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    });
}

/// 从 `base` 起依次尝试绑定回环端口，返回首个成功的监听器与实际端口。
///
/// 只对 `AddrInUse` 顺延；其它错误（权限不足、地址族不可用等）**立即上抛** ——
/// 盲目顺延只会把真正的配置问题掩盖成"端口怎么飘了"。
async fn bind_with_fallback(base: u16) -> io::Result<(TcpListener, u16)> {
    let mut last_err = None;
    for offset in 0..PORT_FALLBACK_SPAN {
        let Some(port) = base.checked_add(offset) else {
            break;
        };
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        match TcpListener::bind(addr).await {
            Ok(listener) => return Ok((listener, port)),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                tracing::warn!("本地接管 API 端口 {port} 已被占用，顺延尝试下一个");
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::AddrNotAvailable, "端口顺延区间已耗尽")
    }))
}

/// 一次已解析的 HTTP 请求。
struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

/// 解析失败时携带响应状态码的可读错误。
struct HttpError {
    status: u16,
    message: &'static str,
}

impl HttpError {
    fn new(status: u16, message: &'static str) -> Self {
        Self { status, message }
    }
}

/// 单连接处理：解析 → 鉴权 → 路由 → 应答（`Connection: close`，不维持长连接）。
async fn handle(mut stream: TcpStream) -> io::Result<()> {
    let request = match timeout(IO_TIMEOUT, read_request(&mut stream)).await {
        Ok(Ok(request)) => request,
        Ok(Err(err)) => {
            write_json(
                &mut stream,
                err.status,
                &json!({ "status": "error", "message": err.message }),
            )
            .await?;
            return Ok(());
        }
        Err(_) => {
            write_json(
                &mut stream,
                408,
                &json!({ "status": "error", "message": "请求读取超时" }),
            )
            .await?;
            return Ok(());
        }
    };

    // CORS 预检：浏览器扩展的 OPTIONS 请求直接放行。
    if request.method == "OPTIONS" {
        return write_json(&mut stream, 204, &Value::Null).await;
    }

    // 鉴权闸门：只对写入型路由 `/add` 强制 Token，探测型 `/status`、`/ping`
    // 保持开放。缺失或不匹配一律 403（不是 401：本机 API 不提供认证挑战）。
    if request.method == "POST" && request.path == "/add" {
        let provided = request
            .headers
            .get("x-harness-token")
            .map(String::as_str)
            .unwrap_or_default();
        if provided != token() {
            tracing::warn!("本地接管 API 拒绝 POST /add：X-Harness-Token 缺失或不匹配");
            return write_json(
                &mut stream,
                403,
                &json!({ "status": "error", "message": "缺少或错误的 X-Harness-Token" }),
            )
            .await;
        }
    }

    let (status, body) = route(request).await;
    write_json(&mut stream, status, &body).await
}

/// 路由表：`GET /status`、`GET /auto-token`、`POST /ping`、`POST /add`。
async fn route(request: Request) -> (u16, Value) {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/status") => (200, json!({ "status": "ok", "version": ENGINE_VERSION })),
        ("GET", "/auto-token") => auto_token(&request),
        ("POST", "/ping") => (200, json!({ "status": "ok", "pong": true })),
        ("POST", "/add") => add_download(request).await,
        _ => (
            404,
            json!({ "status": "error", "message": "未知路由（仅支持 GET /status、GET /auto-token、POST /ping、POST /add）" }),
        ),
    }
}

/// 浏览器扩展来源判定：Chrome / Edge 扩展页面的 Origin 形如
/// `chrome-extension://<extension-id>`。
///
/// **为什么用 Origin 而不是自定义头**：浏览器禁止网页脚本伪造 Origin（由网络栈
/// 写入），而 `X-*` 自定义头任何页面都能加。命令行工具虽然能伪造 Origin，但本机
/// 任意进程本来就能直接读日志/内存，Token 防的是**网页投毒**这一实际威胁。
fn is_extension_origin(origin: &str) -> bool {
    origin.starts_with("chrome-extension://")
}

/// 是否允许读取 Token 的调用方。
///
/// 放行两类：
/// 1. `Origin: chrome-extension://<id>` —— 扩展显式带 Origin 的 CORS 请求；
/// 2. **完全没有 Origin 头** —— MV3 Service Worker 在扩展已声明
///    `host_permissions` 时由网络栈直发，实测不带 Origin（曾因此每 30s 被 403，
///    角标恒 ERR，而浏览器地址栏访问却正常）。
///
/// 网页脚本无法伪造"空 Origin"：跨域请求的 Origin 由网络栈强制写入，只能省略
/// 不了；而本机任意进程本来就可信（能直接读日志 / 内存），Token 防的是网页投毒。
///
/// 其余一律拒绝：`http(s)://` 网页、`null`（沙箱 / 本地文件）等。
fn is_trusted_token_client(origin: &str) -> bool {
    origin.is_empty() || is_extension_origin(origin)
}

/// `GET /auto-token`：把当前进程的随机 Token 交给**浏览器扩展**。
///
/// 校验不通过一律 403，且不区分具体原因，避免给探测者提供额外信息。
/// 成功时回传 `{status:"ok", token:"..."}`。
fn auto_token(request: &Request) -> (u16, Value) {
    let origin = request
        .headers
        .get("origin")
        .map(String::as_str)
        .unwrap_or_default();
    if !is_trusted_token_client(origin) {
        tracing::warn!("本地接管 API 拒绝 GET /auto-token：Origin=`{origin}` 不是浏览器扩展");
        return (
            403,
            json!({ "status": "error", "message": "仅允许浏览器扩展读取 Token" }),
        );
    }
    (200, json!({ "status": "ok", "token": token() }))
}

/// 浏览器扩展提交的任务（字段刻意宽松：扩展可以只传 url）。
#[derive(Debug, Deserialize)]
struct AddPayload {
    url: String,
    #[serde(default)]
    referer: Option<String>,
    #[serde(default)]
    cookie: Option<String>,
    #[serde(default, rename = "user_agent", alias = "userAgent")]
    user_agent: Option<String>,
    #[serde(default)]
    dir: Option<String>,
}

/// `/add`：**只暂存，不入队**。
///
/// 流程（每一步都必须是"立即返回"）：解析 → 写入 `PENDING` → 唤回主窗口 →
/// 派发 `app://browser-intercept` → 回 200 `action: "pending"`。
/// 真正的 `add_download` 由用户在弹窗里点「开始下载」后经 IPC 触发 ——
/// 扩展的 fetch 等不了人，用户的思考时间也不该被网络超时绑架。
async fn add_download(request: Request) -> (u16, Value) {
    let payload: AddPayload = match serde_json::from_slice(&request.body) {
        Ok(payload) => payload,
        Err(e) => {
            return (
                400,
                json!({ "status": "error", "message": format!("JSON 解析失败：{e}") }),
            );
        }
    };

    let url = payload.url.trim().to_string();
    if url.is_empty() {
        return (400, json!({ "status": "error", "message": "url 不能为空" }));
    }

    let task = InterceptedTask {
        url,
        referer: sanitize_header(payload.referer.as_deref()).unwrap_or_default(),
        cookie: sanitize_header(payload.cookie.as_deref()).unwrap_or_default(),
        user_agent: sanitize_header(payload.user_agent.as_deref()).unwrap_or_default(),
        dir: payload
            .dir
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_default(),
        received_at: now_ms(),
    };

    // 先落暂存：即便下面的窗口唤回 / 事件派发全部失败，数据也不会丢 ——
    // 前端下次挂载时会用 `get_pending_intercept` 补回。
    {
        let mut guard = PENDING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(task.clone());
    }

    // 唤回主窗口（最小化则还原，已销毁则重建；重建后事件会丢，靠上面说的查询兜底）。
    if let Some(host) = crate::host::host() {
        host.show_window();
    }
    if let Ok(payload) = serde_json::to_value(&task) {
        crate::emit(INTERCEPT_EVENT, payload);
    }
    tracing::info!(
        "浏览器接管：已暂存 {}（等待用户在 UI 确认）",
        crate::url_host(&task.url)
    );

    // 秒回扩展：数据已收到，让它立刻 cancel 浏览器原生下载。
    (200, json!({ "status": "ok", "action": "pending" }))
}

/// 清洗可注入 aria2 options 的头部值：去掉 CR/LF，防止请求头注入。
fn sanitize_header(raw: Option<&str>) -> Option<String> {
    raw.map(|value| value.replace(['\r', '\n'], " ").trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 读取并解析 HTTP/1.1 请求（头 + Content-Length 指定的 body）。
async fn read_request(stream: &mut TcpStream) -> Result<Request, HttpError> {
    let mut buffer = Vec::with_capacity(1024);
    let head_end = loop {
        if let Some(position) = find_subslice(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > MAX_HEAD {
            return Err(HttpError::new(431, "请求头过大"));
        }
        let mut chunk = [0u8; 1024];
        let read = timeout(IO_TIMEOUT, stream.read(&mut chunk))
            .await
            .map_err(|_| HttpError::new(408, "请求读取超时"))?
            .map_err(|_| HttpError::new(400, "读取请求失败"))?;
        if read == 0 {
            return Err(HttpError::new(400, "连接在请求完成前关闭"));
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]);
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| HttpError::new(400, "缺少请求行"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| HttpError::new(400, "缺少请求方法"))?
        .to_ascii_uppercase();
    let target = parts
        .next()
        .ok_or_else(|| HttpError::new(400, "缺少请求路径"))?;
    let path = target.split('?').next().unwrap_or(target).to_string();

    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY {
        return Err(HttpError::new(413, "请求体过大"));
    }

    let mut body = buffer[head_end..].to_vec();
    while body.len() < content_length {
        let mut chunk = [0u8; 4096];
        let read = timeout(IO_TIMEOUT, stream.read(&mut chunk))
            .await
            .map_err(|_| HttpError::new(408, "请求体读取超时"))?
            .map_err(|_| HttpError::new(400, "读取请求体失败"))?;
        if read == 0 {
            return Err(HttpError::new(400, "请求体不完整"));
        }
        body.extend_from_slice(&chunk[..read]);
        if body.len() > MAX_BODY {
            return Err(HttpError::new(413, "请求体过大"));
        }
    }
    body.truncate(content_length);

    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

/// 朴素子串查找（仅用于查找头部结束标记，缓冲区很小）。
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 写 JSON 响应：固定 `Connection: close` + CORS 头。
///
/// CORS 头说明（浏览器扩展 fetch 的成败全看这里）：
/// - `Allow-Origin: *`：扩展来源 `chrome-extension://<id>` 必须被放行；
/// - `Allow-Headers: *`：`POST /add` 带 `Content-Type` 与 `X-Harness-Token`，
///   预检必须覆盖任意自定义头，否则 fetch 直接抛 TypeError；
/// - `Allow-Private-Network`：Chrome 142+ 对回环/私网请求的预检要求，缺失会被
///   静默拦截（控制台只留下一句 CORS 错误）。
///
/// `204` 按规范**不得携带响应体**：OPTIONS 预检只发头，`Content-Length: 0`。
async fn write_json(stream: &mut TcpStream, status: u16, body: &Value) -> io::Result<()> {
    let text = if status == 204 {
        String::new()
    } else {
        body.to_string()
    };
    let head = format!(
        "HTTP/1.1 {status} {}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: *\r\n\
         Access-Control-Allow-Private-Network: true\r\n\
         Access-Control-Max-Age: 86400\r\n\
         Connection: close\r\n\r\n",
        reason(status),
        text.len()
    );
    stream.write_all(head.as_bytes()).await?;
    if !text.is_empty() {
        stream.write_all(text.as_bytes()).await?;
    }
    stream.flush().await
}

/// 常用状态码短语（只需覆盖本模块会返回的状态）。
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        502 => "Bad Gateway",
        _ => "OK",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        auto_token, find_subslice, is_extension_origin, is_trusted_token_client, reason,
        sanitize_header, Request,
    };

    #[test]
    fn auto_token_allows_extension_and_empty_origin() {
        fn request_with(origin: Option<&str>) -> Request {
            let mut headers = HashMap::new();
            if let Some(value) = origin {
                headers.insert("origin".to_string(), value.to_string());
            }
            Request {
                method: "GET".to_string(),
                path: "/auto-token".to_string(),
                headers,
                body: Vec::new(),
            }
        }

        // MV3 Service Worker 实测不带 Origin（见模块注释），必须放行；
        // 浏览器也不允许网页省略/伪造 Origin，因此放行空 Origin 不会扩大攻击面。
        let (status, body) = auto_token(&request_with(None));
        assert_eq!(status, 200);
        assert_eq!(body["status"], "ok");
        // 扩展来源放行。
        let (status, body) = auto_token(&request_with(Some("chrome-extension://abcdef")));
        assert_eq!(status, 200);
        assert_eq!(body["status"], "ok");
        // 普通网页与沙箱 null Origin 一律 403。
        assert_eq!(auto_token(&request_with(Some("https://evil.example"))).0, 403);
        assert_eq!(auto_token(&request_with(Some("null"))).0, 403);
    }

    #[test]
    fn only_extension_origins_can_read_token() {
        assert!(is_extension_origin(
            "chrome-extension://abcdefghijklmnopabcdefghijklmnop"
        ));
        // 普通网页 / 本机页面 / 空 Origin 不是扩展来源。
        assert!(!is_extension_origin("https://evil.example"));
        assert!(!is_extension_origin("http://127.0.0.1:17800"));
        assert!(!is_extension_origin(""));
        // 少一个斜杠、或其它扩展体系都不能冒充 Chrome 扩展。
        assert!(!is_extension_origin("chrome-extension:/abc"));
        assert!(!is_extension_origin("moz-extension://abc"));
        // 信任判定 = 扩展来源 ∪ 空 Origin（MV3 Service Worker），网页/null 拒绝。
        assert!(is_trusted_token_client(""));
        assert!(is_trusted_token_client("chrome-extension://abcdef"));
        assert!(!is_trusted_token_client("https://evil.example"));
        assert!(!is_trusted_token_client("null"));
    }

    #[test]
    fn finds_header_terminator() {
        // 返回终止符**起始下标**：请求行 20 字节 + CRLF = 22；"Host: x" 7 字节（x 在第 28 位），
        // 因此紧随其后的 `\r\n\r\n` 从 29 开始。
        assert_eq!(
            find_subslice(b"GET /status HTTP/1.1\r\nHost: x\r\n\r\nbody", b"\r\n\r\n"),
            Some(29)
        );
        assert_eq!(find_subslice(b"no terminator", b"\r\n\r\n"), None);
    }

    #[test]
    fn sanitizes_header_injection() {
        assert_eq!(sanitize_header(Some("a\r\nb")), Some("a  b".to_string()));
        assert_eq!(sanitize_header(Some("   ")), None);
        assert_eq!(sanitize_header(None), None);
    }

    #[test]
    fn reason_covers_used_status_codes() {
        assert_eq!(reason(200), "OK");
        assert_eq!(reason(403), "Forbidden");
        assert_eq!(reason(401), "Unauthorized");
    }
}
