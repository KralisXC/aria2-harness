//! 增量 51：并发自动降级（Anti-403 重试矩阵）。
//!
//! 服务器因并发过高拒绝请求（403 / 429 / 连接重置等）时，不是直接判失败，
//! 而是把该任务的 `split` 与 `max-connection-per-server` 降一档后重新拉起：
//! `128 → 64 → 32 → 16 → 8 → 4 → 2 → 1`。降到 1 仍失败才彻底放弃。
//!
//! ## 为什么不是 "forcePause → changeOption → unpause"
//!
//! 这三步只对 **仍在 RequestGroupMan 里的任务**有效。aria2 在发出
//! `onDownloadError` 通知之前，已经把失败的 RequestGroup 移入 DownloadResult，
//! 此时 `findGroup(gid)` 返回 null：
//! - `unpause` 要求 `STATE_WAITING && isPauseRequested()`（RpcMethodImpl.cc:511）；
//! - `changeOption` 直接抛 `Cannot change option for GID#...`（RpcMethodImpl.cc:1139）。
//! 因此对已失败任务调用它们必然报错，也永远不会真正重试。
//!
//! 可行且对用户无感的做法：**复用同一个 GID 重新 addUri**
//! 1. `removeDownloadResult(gid)` 释放该 GID 与失败结果（GroupId 随之释放）；
//! 2. `addUri` 时带上 `gid=<原 GID>`（aria2 的 PREF_GID 支持指定 GID）、
//!    `continue=true` 续传、原 `dir`/`out`/Referer 等必要 options，并把并发降档。
//! 任务在 UI 上仍是同一张卡（GID 不变），进度从 `.aria2` 控制文件续上。
//!
//! 降级历史只按 GID 记录，不写全局配置：新任务永远从用户设置值重新开始。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::rpc_client::Client;

/// 两次降级之间的最小间隔：防止"刚降一档又被立刻打到失败"造成 RPC 风暴。
const DEBOUNCE: Duration = Duration::from_secs(5);
/// 记录表防御性上限：标签丢失无妨，内存不能无限增长。
const MAX_ENTRIES: usize = 4096;
/// 单次 RPC 查询 / 重试的超时上界（引擎挂死时任务不能悬着）。
const RPC_TIMEOUT: Duration = Duration::from_secs(5);
/// 「文件被占用」的最多自动重试次数（errorCode=17）。
const FILE_IO_MAX_ATTEMPTS: u8 = 3;
/// 每次重试前的退避基数：500ms、1000ms、1500ms。
const FILE_IO_BACKOFF: Duration = Duration::from_millis(500);
/// `removeDownloadResult` 之后额外留给 Windows 回收句柄的时间。
const FILE_IO_RELEASE: Duration = Duration::from_millis(200);

/// 单个 GID 的重试上下文。
struct Entry {
    /// 原始下载地址（`files[0].uri` 取不到时的兜底）。
    url: String,
    /// 下载目录（重新 addUri 时必须显式带上，否则会落到默认目录）。
    dir: String,
    /// 原文件名（`out` 选项）；缺省时让 aria2 从 URL 推导。
    out: Option<String>,
    /// 除并发 / GID / 目录外的全部原始 options（Referer、HTTP1 兼容等）。
    extra: Map<String, Value>,
    /// 当前生效的并发档位（split 与单服连接数同档）。
    current: u32,
    /// 是否已经发生过降级（前端据此显示黄色标签）。
    degraded: bool,
    /// 上次降级时刻，用于 5s 防抖。
    last_degrade: Option<Instant>,
    /// 同一错误事件可能重复到达，重试流程进行中时不再叠加。
    in_flight: bool,
    /// 「文件被占用」（errorCode=17）已重试次数：与并发降档相互独立。
    file_io_attempts: u8,
}

static ENTRIES: LazyLock<Mutex<HashMap<String, Entry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 解析 options 里的并发值（aria2 选项值可能是字符串，插件可能传数字）。
fn concurrency_of(value: Option<&Value>) -> u32 {
    value
        .and_then(|raw| raw.as_str().and_then(|text| text.parse::<u32>().ok()))
        .or_else(|| value.and_then(Value::as_u64).map(|number| number as u32))
        .unwrap_or(0)
}

/// 登记一个刚添加的 HTTP 任务（在 `addUri` 成功后调用）。
///
/// `current` 取 `split` 与 `max-connection-per-server` 的较小值 —— 那才是
/// 实际会打向单台服务器的连接数（多镜像场景下 split 可能更大）。
pub fn register(gid: &str, url: &str, dir: &str, options: &Map<String, Value>) {
    if gid.is_empty() {
        return;
    }
    let split = concurrency_of(options.get("split"));
    let connections = concurrency_of(options.get("max-connection-per-server"));
    let current = match (split, connections) {
        (0, 0) => return, // 没有并发选项（BT 等）不参与降级
        (0, value) => value,
        (value, 0) => value,
        (left, right) => left.min(right),
    };
    let current = current.max(1);

    let mut extra = Map::new();
    for (key, value) in options {
        // 并发与 GID 在重试时重新生成；dir 单独存（重试时显式回填）。
        if matches!(key.as_str(), "split" | "max-connection-per-server" | "gid" | "dir") {
            continue;
        }
        extra.insert(key.clone(), value.clone());
    }
    let out = options
        .get("out")
        .and_then(Value::as_str)
        .map(str::to_string);

    let entry = Entry {
        url: url.to_string(),
        dir: dir.to_string(),
        out,
        extra,
        current,
        degraded: false,
        last_degrade: None,
        in_flight: false,
        file_io_attempts: 0,
    };

    if let Ok(mut map) = ENTRIES.lock() {
        if map.len() >= MAX_ENTRIES {
            map.clear();
        }
        map.insert(gid.to_string(), entry);
    }
}

/// 任务被显式删除后丢弃记录，避免 GID 复用 / 内存滞留。
pub fn forget(gid: &str) {
    if let Ok(mut map) = ENTRIES.lock() {
        map.remove(gid);
    }
}

/// 前端标注用：`(是否降级过, 当前并发)`。
pub fn status(gid: &str) -> Option<(bool, u32)> {
    ENTRIES
        .lock()
        .ok()
        .and_then(|map| map.get(gid).map(|entry| (entry.degraded, entry.current)))
}

/// 该 GID 是否还有降级空间（没有记录的 BT / 单线程任务返回 false）。
pub fn is_managed(gid: &str) -> bool {
    ENTRIES
        .lock()
        .ok()
        .and_then(|map| map.get(gid).map(|entry| entry.current > 1))
        .unwrap_or(false)
}

/// 该 GID 是否由本模块托管（含已降到 1 连接的任务）。
///
/// 「文件被占用」与并发无关，单线程任务同样需要自动续传重试，因此错误入口
/// 用这个更宽的口径判断，而不是 [`is_managed`]。
pub fn is_registered(gid: &str) -> bool {
    ENTRIES
        .lock()
        .ok()
        .map(|map| map.contains_key(gid))
        .unwrap_or(false)
}

/// 用户每次主动「继续」时重置文件占用重试预算：新一轮暂停/继续应重新拥有
/// 最多 3 次自动恢复机会，而不是被上一轮的失败计数拖累。
pub fn reset_file_io_attempts(gid: &str) {
    if let Ok(mut map) = ENTRIES.lock() {
        if let Some(entry) = map.get_mut(gid) {
            entry.file_io_attempts = 0;
        }
    }
}

/// 是否是「文件被占用 / 无法打开已有文件」类错误。
///
/// 典型场景：暂停后立刻继续、或杀毒软件正在扫描 `.aria2` 控制文件与部分文件，
/// aria2 重新打开句柄失败并抛 `errorCode=17`。这类错误等半秒就好，与并发无关。
fn is_file_busy(code: i64, message: &str) -> bool {
    if code == 17 {
        return true;
    }
    let lower = message.to_ascii_lowercase();
    message.contains("无法打开已有文件")
        || lower.contains("could not open existing file")
        || lower.contains("cannot open existing file")
        || lower.contains("being used by another process")
}

/// 错误是否属于"并发过高 / 服务器拒绝 / 网络抖动"这一类（值得降档重试）。
///
/// 覆盖网络与服务器拒绝类；磁盘不足、文件已存在、种子解析失败等
/// 降并发毫无意义的错误一律不重试。
///
/// Phase 5 补入两个项目扩展码（与 `engine-net/src/error.rs` 的 `to_aria2_code`
/// 对齐，否则 native 引擎的这两类故障永远无法进入降级阶梯）：
/// - `33`：HTTP_AUTH_REQUIRED（含转发代理 407）；
/// - `35`：CONNECTION_RESET（PROJECT 内部码，见 `error_code.h`）。
fn is_retryable(code: i64, message: &str) -> bool {
    if matches!(code, 1 | 2 | 6 | 19 | 21 | 22 | 23 | 29 | 33 | 35) {
        return true;
    }
    let lower = message.to_ascii_lowercase();
    ["403", "429", "forbidden", "too many", "reset", "refused", "econnreset"]
        .iter()
        .any(|keyword| lower.contains(keyword))
}

/// 错误通知入口：fire-and-forget，绝不在通知循环里 `.await`。
///
/// 返回前即完成"要不要处理"的同步判断；真正的 RPC 全部在独立任务里执行。
pub fn on_error(client: Client, gid: String, hidden: bool) {
    if !is_registered(&gid) {
        return;
    }
    crate::rt().spawn(async move {
        if !retry_once(&client, &gid).await {
            // 降级未执行（非网络类错误 / RPC 失败 / 已降到 1）：维持原有终态逻辑。
            crate::record_terminal(client.clone(), gid.clone(), "error");
            if hidden {
                crate::notify_terminal(client.clone(), gid, true);
            }
        }
    });
}

/// 执行一次降档重试；成功重新拉起返回 `true`。
async fn retry_once(client: &Client, gid: &str) -> bool {
    // 1. 读错误详情，确认失败原因与原始 URI。
    let keys: Option<Vec<String>> = Some(
        ["status", "errorCode", "errorMessage", "files", "dir", "connections"]
            .iter()
            .map(|key| key.to_string())
            .collect(),
    );
    let detail = match tokio::time::timeout(RPC_TIMEOUT, client.tell_status(gid, keys)).await {
        Ok(Ok(detail)) => detail,
        Ok(Err(err)) => {
            tracing::warn!(gid = %gid, "自动降级前读取任务详情失败：{err}");
            return false;
        }
        Err(_) => {
            tracing::warn!(gid = %gid, "自动降级前读取任务详情超时");
            return false;
        }
    };
    if detail.get("status").and_then(Value::as_str) != Some("error") {
        return false;
    }
    let code = detail
        .get("errorCode")
        .and_then(Value::as_str)
        .and_then(|raw| raw.parse::<i64>().ok())
        .or_else(|| detail.get("errorCode").and_then(Value::as_i64))
        .unwrap_or(1);
    let message = detail
        .get("errorMessage")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // 「文件被占用」不是并发问题：不降档，只做 500ms 起步的退避重试。
    let file_io = is_file_busy(code, message);
    if !file_io && !is_retryable(code, message) {
        return false;
    }

    // 失败文件路径：写进日志，便于区分到底是谁锁了文件（杀软 / 上层工具）。
    let locked_path = detail
        .get("files")
        .and_then(Value::as_array)
        .and_then(|files| files.first())
        .and_then(|file| file.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("");

    // 2. 快照重试上下文并占位（同 GID 的重复错误事件不叠加）。
    let (next, wait, attempt, fallback_url, dir, out, mut extra) = {
        let mut map = match ENTRIES.lock() {
            Ok(map) => map,
            Err(_) => return false,
        };
        let Some(entry) = map.get_mut(gid) else {
            return false;
        };
        // 同一错误事件可能重复到达：已有重试在跑时直接视为"已处理"，
        // 绝不能让调用方再落一条 error 终态。
        if entry.in_flight {
            return true;
        }
        let next;
        let wait;
        let attempt;
        if file_io {
            // 与并发降档互不影响：即使已降到 1 连接也照样重试。
            if entry.file_io_attempts >= FILE_IO_MAX_ATTEMPTS {
                tracing::warn!(
                    gid = %gid,
                    code,
                    path = %locked_path,
                    attempts = u32::from(entry.file_io_attempts),
                    "[file-busy] 连续重试 {FILE_IO_MAX_ATTEMPTS} 次仍无法打开文件，转为失败态"
                );
                return false;
            }
            entry.file_io_attempts += 1;
            attempt = entry.file_io_attempts;
            next = entry.current;
            wait = Some(FILE_IO_BACKOFF * u32::from(attempt));
        } else {
            if entry.current <= 1 {
                return false;
            }
            attempt = 0;
            next = (entry.current / 2).max(1);
            wait = entry
                .last_degrade
                .map(|last| DEBOUNCE.saturating_sub(last.elapsed()))
                .filter(|remaining| !remaining.is_zero());
        }
        entry.in_flight = true;
        (
            next,
            wait,
            attempt,
            entry.url.clone(),
            entry.dir.clone(),
            entry.out.clone(),
            entry.extra.clone(),
        )
    };

    // 3. 等待窗口：降级走 5s 防抖；文件占用走 500 / 1000 / 1500ms 退避。
    if let Some(remaining) = wait {
        if file_io {
            tracing::warn!(
                gid = %gid,
                code,
                attempt = u32::from(attempt),
                path = %locked_path,
                delay_ms = remaining.as_millis() as u64,
                "[file-busy] 检测到文件被占用（errorCode={code}），退避后自动续传重试"
            );
        }
        tokio::time::sleep(remaining).await;
    }

    // 4. 释放旧 GID：先尽量暂停（已停止任务必然失败，忽略即可），
    //    再移除失败结果 —— 不释放的话 addUri 会报 "GID is not unique"。
    let _ = client.force_pause(gid).await;
    if let Err(err) = client.remove_download_result(gid).await {
        tracing::warn!(gid = %gid, "移除失败结果失败（继续尝试重试）：{err}");
    }
    // 文件占用：再给 Windows 一点时间完成句柄回收（关文件 / 杀软扫描结束）。
    if file_io {
        tokio::time::sleep(FILE_IO_RELEASE).await;
    }

    // 5. 原 URI 优先从任务详情取（重定向后的最终地址更准确），其次用登记值。
    let first_file = detail
        .get("files")
        .and_then(Value::as_array)
        .and_then(|files| files.first());
    let url = first_file
        .and_then(|file| file.get("uris"))
        .and_then(Value::as_array)
        .and_then(|uris| uris.first())
        .and_then(|uri| uri.get("uri"))
        .and_then(Value::as_str)
        .unwrap_or(&fallback_url)
        .to_string();
    if url.is_empty() {
        release_in_flight(gid);
        return false;
    }

    // 目标目录 / 文件名以任务详情为准：重定向或 Content-Disposition 会让实际文件名
    // 与 URL 推导值不同，不带 `out` 会另存成一个新文件、旧进度白费。
    let actual_dir = detail
        .get("dir")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or(dir);
    let out = out.or_else(|| {
        first_file
            .and_then(|file| file.get("path"))
            .and_then(Value::as_str)
            .and_then(|path| path.rsplit(['\\', '/']).find(|segment| !segment.is_empty()))
            .map(str::to_string)
    });

    // 6. 以同一 GID 重新拉起：并发降档 + 续传，其余 options（Referer / HTTP1）原样带回。
    extra.insert("dir".to_string(), json!(actual_dir.clone()));
    if let Some(out) = out {
        extra.insert("out".to_string(), json!(out));
    }
    extra.insert("split".to_string(), json!(next.to_string()));
    extra.insert("max-connection-per-server".to_string(), json!(next.to_string()));
    extra.insert("continue".to_string(), json!("true"));
    extra.insert("gid".to_string(), json!(gid));

    match tokio::time::timeout(RPC_TIMEOUT, client.add_uri(vec![url.clone()], extra)).await {
        Ok(Ok(_)) => {
            if let Ok(mut map) = ENTRIES.lock() {
                if let Some(entry) = map.get_mut(gid) {
                    entry.current = next;
                    // 文件占用重试不改变并发档位，也不打「已降级」标签。
                    if !file_io {
                        entry.degraded = true;
                        entry.last_degrade = Some(Instant::now());
                    }
                    entry.in_flight = false;
                }
            }
            if file_io {
                tracing::info!(
                    gid = %gid,
                    attempt = u32::from(attempt),
                    path = %locked_path,
                    "文件句柄已释放，第 {attempt} 次自动续传重试已重新拉起"
                );
            } else {
                tracing::info!(
                    gid = %gid,
                    "服务器拒绝高并发，已自动降级至 {next} 连接并重新拉起（续传）"
                );
            }
            // 让前端立刻刷新这张卡（get_task 会带回 isDegraded / currentConcurrency）。
            crate::emit(
                crate::EVENT_NAME,
                json!({ "type": "aria2", "event": "degraded", "gid": gid }),
            );
            // 历史库回到 waiting，后续终态事件会再次纠正。
            crate::record_added(client.clone(), gid.to_string(), url, actual_dir);
            true
        }
        Ok(Err(err)) => {
            release_in_flight(gid);
            tracing::warn!(gid = %gid, "降级后重新添加失败：{err}");
            false
        }
        Err(_) => {
            release_in_flight(gid);
            tracing::warn!(gid = %gid, "降级后重新添加超时");
            false
        }
    }
}

/// 任何提前返回都必须解占位，否则该 GID 再也无法降级。
fn release_in_flight(gid: &str) {
    if let Ok(mut map) = ENTRIES.lock() {
        if let Some(entry) = map.get_mut(gid) {
            entry.in_flight = false;
        }
    }
}

#[cfg(test)]
mod tests {
    //! Phase 5：验证 engine-net 的故障码能被本模块捕获并走完 128→…→1。
    //!
    //! 用一个假 WebSocket JSON-RPC 引擎回放 `tellStatus` 错误，
    //! 直接调用私有的 `retry_once`，从而绕开真实 aria2c / native 引擎。

    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::JoinHandle;
    use tokio_tungstenite::tungstenite::Message;

    use super::*;
    use crate::rpc_client::Client;

    struct FakeEngine {
        addr: std::net::SocketAddr,
        adds: Arc<StdMutex<Vec<Map<String, Value>>>>,
        handle: JoinHandle<()>,
    }

    impl Drop for FakeEngine {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    impl FakeEngine {
        async fn start(code: i64) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let adds = Arc::new(StdMutex::new(Vec::new()));
            let captured = Arc::clone(&adds);
            let handle = tokio::spawn(async move {
                loop {
                    let Ok((socket, _)) = listener.accept().await else {
                        break;
                    };
                    let captured = Arc::clone(&captured);
                    tokio::spawn(async move {
                        serve(socket, code, captured).await;
                    });
                }
            });
            Self { addr, adds, handle }
        }

        async fn connect(&self) -> Client {
            let client = Client::start(format!("ws://{}/", self.addr), "secret");
            for _ in 0..200 {
                if client.is_connected() {
                    return client;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("假引擎未在 2s 内完成 WS 握手");
        }

        fn adds(&self) -> Vec<Map<String, Value>> {
            self.adds.lock().unwrap().clone()
        }
    }

    async fn serve(socket: TcpStream, code: i64, adds: Arc<StdMutex<Vec<Map<String, Value>>>>) {
        let Ok(mut ws) = tokio_tungstenite::accept_async(socket).await else {
            return;
        };
        while let Some(Ok(message)) = ws.next().await {
            let text = match message {
                Message::Text(text) => text.to_string(),
                Message::Binary(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Message::Close(_) => return,
                _ => continue,
            };
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let id = frame.get("id").cloned().unwrap_or(Value::Null);
            let method = frame
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let params = frame
                .get("params")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            let result = match method {
                "aria2.tellStatus" => json!({
                    "gid": "fffe000000000001",
                    "status": "error",
                    "errorCode": code.to_string(),
                    "errorMessage": "injected fault",
                    "dir": "C:\\tmp",
                    "files": [{
                        "path": "C:\\tmp\\file.bin",
                        "uris": [{"uri": "http://127.0.0.1/file.bin", "status": "used"}],
                    }],
                }),
                "aria2.addUri" => {
                    // params[0]=token, params[1]=uris, params[2]=options
                    if let Some(options) = params.get(2).and_then(Value::as_object) {
                        adds.lock().unwrap().push(options.clone());
                    }
                    params
                        .get(2)
                        .and_then(Value::as_object)
                        .and_then(|options| options.get("gid"))
                        .cloned()
                        .unwrap_or(Value::String("fffe000000000001".to_string()))
                }
                "aria2.forcePause" | "aria2.removeDownloadResult" => json!("OK"),
                _ => json!("OK"),
            };

            let response = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            let _ = ws.send(Message::Text(response.to_string().into())).await;
        }
    }

    fn options_128() -> Map<String, Value> {
        let mut options = Map::new();
        options.insert("split".to_string(), json!("128"));
        options.insert("max-connection-per-server".to_string(), json!("128"));
        options.insert("referer".to_string(), json!("http://example.invalid/"));
        options
    }

    fn clear_debounce(gid: &str) {
        if let Ok(mut map) = super::ENTRIES.lock() {
            if let Some(entry) = map.get_mut(gid) {
                entry.last_degrade = None;
            }
        }
    }

    fn current_of(gid: &str) -> Option<u32> {
        super::ENTRIES
            .lock()
            .ok()
            .and_then(|map| map.get(gid).map(|entry| entry.current))
    }

    /// 六类故障码（1/6/22/29/33/35）都必须走完 128→64→…→1，第 8 次停止。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn degradation_ladder_halves_from_128_to_1() {
        for code in [1, 6, 22, 29, 33, 35] {
            let engine = FakeEngine::start(code).await;
            let client = engine.connect().await;
            let gid = format!("fffe00000000{code:04x}");
            super::register(
                &gid,
                "http://127.0.0.1/file.bin",
                "C:\\tmp",
                &options_128(),
            );
            assert_eq!(current_of(&gid), Some(128), "code={code} 初始档位");

            let mut expected = Vec::new();
            let mut current = 128u32;
            for _ in 0..7 {
                current /= 2;
                expected.push(current.to_string());
                clear_debounce(&gid);
                assert!(
                    retry_once(&client, &gid).await,
                    "code={code} 第 {} 次降级必须成功",
                    8 - current.leading_zeros()
                );
            }

            let adds = engine.adds();
            assert_eq!(adds.len(), 7, "code={code} 必须恰好重新拉起 7 次");
            for (index, options) in adds.iter().enumerate() {
                assert_eq!(
                    options.get("split").and_then(Value::as_str),
                    Some(expected[index].as_str()),
                    "code={code} 第 {index} 步 split 不符"
                );
                assert_eq!(
                    options
                        .get("max-connection-per-server")
                        .and_then(Value::as_str),
                    Some(expected[index].as_str()),
                    "code={code} 第 {index} 步连接数不符"
                );
                assert_eq!(
                    options.get("continue").and_then(Value::as_str),
                    Some("true"),
                    "code={code} 必须续传"
                );
                assert_eq!(
                    options.get("gid").and_then(Value::as_str),
                    Some(gid.as_str()),
                    "code={code} 必须复用同一 GID"
                );
                assert_eq!(
                    options.get("referer").and_then(Value::as_str),
                    Some("http://example.invalid/"),
                    "code={code} 必须保留原 options"
                );
            }

            assert_eq!(expected, ["64", "32", "16", "8", "4", "2", "1"]);
            assert_eq!(current_of(&gid), Some(1), "code={code} 最终档位");
            clear_debounce(&gid);
            assert!(
                !retry_once(&client, &gid).await,
                "code={code} 已到 1 连接，必须停止降级"
            );
            assert_eq!(engine.adds().len(), 7, "code={code} 停止后不得再拉起");
            super::forget(&gid);
            client.stop_reconnecting();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn degradation_ignores_non_retryable_code() {
        let engine = FakeEngine::start(34).await;
        let client = engine.connect().await;
        let gid = "fffe0000000000e1";
        super::register(
            gid,
            "http://127.0.0.1/file.bin",
            "C:\\tmp",
            &options_128(),
        );
        assert!(!retry_once(&client, gid).await, "磁盘满不应降级");
        assert_eq!(current_of(gid), Some(128), "档位不得变化");
        assert!(engine.adds().is_empty(), "不得重新拉起");
        super::forget(gid);
        client.stop_reconnecting();
    }

    /// 入口 `on_error` 必须真正触发一次降档（fire-and-forget 路径）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_error_is_fire_and_forget() {
        let engine = FakeEngine::start(33).await;
        let client = engine.connect().await;
        let gid = "fffe0000000000e2";
        super::register(
            gid,
            "http://127.0.0.1/file.bin",
            "C:\\tmp",
            &options_128(),
        );
        super::on_error(client.clone(), gid.to_string(), false);
        let mut lowered = false;
        for _ in 0..200 {
            if current_of(gid).is_some_and(|current| current < 128) {
                lowered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(lowered, "on_error 未在 2s 内触发降档");
        super::forget(gid);
        client.stop_reconnecting();
    }
}
