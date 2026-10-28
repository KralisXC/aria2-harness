//! 方法表：addUri / tellStatus / pause / unpause / remove / getVersion /
//! getGlobalOption / changeGlobalOption / system.multicall。
//!
//! 只做 §5.4.5 的最小集；`system.multicall` 内部逐条调用，禁止递归嵌套。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::dns::resolver::DnsResolver;
use crate::download::SlicingOptions;
use crate::http::client::HttpOptions;
use crate::http::proxy::{NoProxyRule, ProxySettings, ProxySlot};
use crate::progress::control::DEFAULT_PIECE_LENGTH;
use crate::rpc::types::{
    CODE_INVALID_PARAMS, RpcError, failure, status_to_value, success,
};
use crate::task::registry::{Registry, TaskSpec, TaskStatus};

/// aria2 协议版本（客户端按这个值判断能力）。
pub const ARIA2_PROTOCOL_VERSION: &str = "1.37.0";

/// 引擎默认下载目录：当前用户的 `%USERPROFILE%\Downloads`。
///
/// - LocalSystem（服务态）没有可用的用户配置：`systemprofile\Downloads` 用户既看不见
///   也没权限，退到 `%SystemDrive%\Users\Public\Downloads`；真正路径由宿主按任务下发。
/// - 解析失败时才退回 cwd，保证至少是个绝对路径。
fn default_download_dir() -> std::path::PathBuf {
    let is_system = std::env::var("USERNAME")
        .map(|name| name.eq_ignore_ascii_case("SYSTEM"))
        .unwrap_or(false);
    if is_system {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
        return std::path::PathBuf::from(format!(r"{drive}\Users\Public\Downloads"));
    }
    std::env::var("USERPROFILE")
        .ok()
        .map(std::path::PathBuf::from)
        .map(|profile| profile.join("Downloads"))
        .filter(|path| {
            path.is_absolute()
                && !path
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains("systemprofile")
        })
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

pub struct Engine {
    registry: Arc<Registry>,
    global_options: Mutex<HashMap<String, String>>,
}

impl Engine {
    pub fn new(resolver: DnsResolver) -> Self {
        let engine = Self {
            registry: Arc::new(Registry::new(resolver)),
            global_options: Mutex::new(default_global_options()),
        };
        // 默认下载目录必须是**当前用户的绝对路径**。旧实现取 `current_dir()`：
        // 服务以 LocalSystem 运行时 cwd 是 `C:\Windows\System32`，一旦调用方没显式
        // 下发 `dir`，文件就会落到 `systemprofile\Downloads`。
        engine.set_global_option("dir", &default_download_dir().to_string_lossy());
        engine
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// 设置一条全局选项（bin 用 `--dir` 覆盖默认工作目录）。
    pub fn set_global_option(&self, name: &str, value: &str) {
        self.global_options
            .lock()
            .unwrap()
            .insert(name.to_string(), value.to_string());
    }

    /// 进程内调用入口（Phase 2：service 在 6800 上复用同一套方法表）。
    ///
    /// `params` **不含** `token:<secret>` —— 鉴权由 service 侧统一完成。
    pub fn call(&self, method: &str, params: &[Value]) -> Result<Value, RpcError> {
        self.dispatch(method, params)
    }

    /// 任务事件订阅（service 转发为 `aria2.onDownload*`）。
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<crate::task::event::TaskEvent> {
        self.registry.subscribe_events()
    }

    /// 无 catch_unwind 的入口（供 `rpc::handle_json` 包裹）。
    pub fn handle_value(&self, request: &Value) -> Value {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let Some(method) = request.get("method").and_then(Value::as_str) else {
            return failure(
                &id,
                &RpcError::new(super::types::CODE_INVALID_REQUEST, "缺 method 字段"),
            );
        };
        let empty = Vec::new();
        let params = match request.get("params") {
            None | Some(Value::Null) => &empty,
            Some(Value::Array(items)) => items,
            Some(_) => {
                return failure(
                    &id,
                    &RpcError::new(CODE_INVALID_PARAMS, "params 必须是数组"),
                );
            }
        };

        match self.dispatch(method, params) {
            Ok(result) => success(&id, result),
            Err(err) => failure(&id, &err),
        }
    }

    fn dispatch(&self, method: &str, params: &[Value]) -> Result<Value, RpcError> {
        match method {
            "aria2.addUri" => self.add_uri(params),
            "aria2.tellStatus" => self.tell_status(params),
            "aria2.tellActive" => self.tell_active(params),
            "aria2.tellWaiting" => self.tell_waiting(params),
            "aria2.tellStopped" => self.tell_stopped(params),
            "aria2.getGlobalStat" => Ok(self.get_global_stat()),
            "aria2.pause" => self.with_gid(params, |gid| self.registry.pause(gid)),
            "aria2.unpause" => self.with_gid(params, |gid| self.registry.unpause(gid)),
            "aria2.remove" => self.with_gid(params, |gid| self.registry.remove(gid)),
            // 前端 / 宿主实际下发的是 aria2c 的别名（`RpcClient::force_pause` 等）：
            // - forcePause：立即断连暂停；对 paused 幂等（aria2 语义，UI 连点不报错）。
            // - forceRemove：终止 + 标记 removed + 清控制文件（结果记录保留，aria2 语义）。
            // - removeDownloadResult：清结果记录（从任务表摘掉，UI 删除后列表不再显示）。
            "aria2.forcePause" => self.force_pause(params),
            "aria2.forceRemove" => self.with_gid(params, |gid| self.registry.remove(gid)),
            // removeDownloadResult 是「清结果记录」：必须把任务从表里摘掉，
            // 否则 UI 删除后 tellStopped 仍会枚举到已删除的任务。
            "aria2.removeDownloadResult" => {
                self.with_gid(params, |gid| self.registry.remove_result(gid))
            }
            // 批量 / 结果清理 / 文件与选项查询：UI 与扩展都可能下发，
            // 一律给出 aria2 兼容语义，绝不能落到 -32601。
            "aria2.pauseAll" | "aria2.forcePauseAll" => {
                self.registry.pause_all();
                Ok(json!("OK"))
            }
            "aria2.unpauseAll" => {
                self.registry.unpause_all();
                Ok(json!("OK"))
            }
            "aria2.purgeDownloadResult" => {
                self.registry.purge_results();
                Ok(json!("OK"))
            }
            "aria2.getFiles" | "aria2.tellFiles" => self.get_files(params),
            "aria2.getOption" => self.get_option(params),
            // `changeOption` 只影响运行中任务的局部参数；当前任务参数在 addUri 时固化，
            // 这里按 aria2 语义返回 "OK"（不报错即视为生效）。
            "aria2.changeOption" => {
                let gid = self.gid_param(params)?;
                if self.registry.snapshot(&gid).is_none() {
                    return Err(RpcError::from(
                        crate::task::registry::TaskError::GidNotFound(gid),
                    ));
                }
                Ok(json!("OK"))
            }
            // `changePosition`：内存 registry 的展示顺序固定按 GID 升序，
            // 这里校验 GID 后原样回显目标位置（aria2 返回新位置整数）。
            "aria2.changePosition" => {
                let gid = self.gid_param(params)?;
                if self.registry.snapshot(&gid).is_none() {
                    return Err(RpcError::from(
                        crate::task::registry::TaskError::GidNotFound(gid),
                    ));
                }
                let pos = params
                    .get(1)
                    .and_then(Value::as_i64)
                    .ok_or_else(|| RpcError::invalid_params("第二个参数必须是整数位置"))?;
                Ok(json!(pos))
            }
            "aria2.getVersion" => Ok(self.get_version()),
            "aria2.getGlobalOption" => Ok(self.get_global_option()),
            "aria2.changeGlobalOption" => self.change_global_option(params),
            // 会话 / 停机类方法：engine-net 没有独立的进程会话，空实现即成功。
            "aria2.saveSession" | "aria2.shutdown" | "aria2.forceShutdown" => Ok(json!("OK")),
            "aria2.getSessionInfo" => Ok(json!({
                "sessionId": format!("{:08x}{:08x}", std::process::id(), self.registry.count()),
            })),
            "system.multicall" => self.multicall(params),
            other => Err(RpcError::method_not_found(other)),
        }
    }

    fn add_uri(&self, params: &[Value]) -> Result<Value, RpcError> {
        let uris: Vec<String> = params
            .first()
            .and_then(Value::as_array)
            .ok_or_else(|| RpcError::invalid_params("addUri 第一个参数必须是 URI 数组"))?
            .iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect();
        if uris.is_empty() {
            return Err(RpcError::invalid_params("addUri 至少需要一个 URI"));
        }
        let options = match params.get(1) {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(_) => return Err(RpcError::invalid_params("addUri 第二个参数必须是对象")),
        };

        let spec = self.build_spec(uris, &options)?;
        let gid = self.registry.add_uri(spec)?;
        Ok(json!(gid))
    }

    fn tell_status(&self, params: &[Value]) -> Result<Value, RpcError> {
        let gid = self.gid_param(params)?;
        let snapshot = self.snapshot_or_err(&gid)?;
        let mut value = status_to_value(&snapshot);
        if let Some(Value::Array(keys)) = params.get(1) {
            value = filter_keys(value, keys);
        }
        Ok(value)
    }

    /// `aria2.getFiles` / 旧别名 `aria2.tellFiles`：与 `tellStatus.files` 同源。
    fn get_files(&self, params: &[Value]) -> Result<Value, RpcError> {
        let gid = self.gid_param(params)?;
        let snapshot = self.snapshot_or_err(&gid)?;
        Ok(status_to_value(&snapshot)["files"].clone())
    }

    /// `aria2.getOption`：返回该任务生效的选项（数值一律字符串）。
    fn get_option(&self, params: &[Value]) -> Result<Value, RpcError> {
        let gid = self.gid_param(params)?;
        let snapshot = self.snapshot_or_err(&gid)?;
        let globals = self.global_options.lock().unwrap().clone();
        let dir = snapshot
            .dest
            .parent()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let out = snapshot
            .dest
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let option = |name: &str, fallback: &str| {
            globals
                .get(name)
                .cloned()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| fallback.to_string())
        };
        Ok(json!({
            "gid": gid,
            "dir": dir,
            "out": out,
            "split": option("split", "5"),
            "max-connection-per-server": option("max-connection-per-server", "1"),
            "min-split-size": option("min-split-size", "20971520"),
            "piece-length": snapshot.piece_length.to_string(),
            // aria2 的 getOption 里 continue 是布尔语义的字符串。
            "continue": "true",
            "sqlite-progress": option("sqlite-progress", "true"),
        }))
    }

    fn snapshot_or_err(&self, gid: &str) -> Result<crate::task::registry::TaskSnapshot, RpcError> {
        self.registry.snapshot(gid).ok_or_else(|| {
            RpcError::from(crate::task::registry::TaskError::GidNotFound(
                gid.to_string(),
            ))
        })
    }

    /// `aria2.tellActive(keys?)`。
    fn tell_active(&self, params: &[Value]) -> Result<Value, RpcError> {
        let keys = params.first().and_then(Value::as_array);
        let items = self
            .sorted_snapshots()
            .into_iter()
            .filter(|snapshot| snapshot.status == TaskStatus::Active)
            .map(|snapshot| apply_keys(status_to_value(&snapshot), keys))
            .collect();
        Ok(Value::Array(items))
    }

    /// `aria2.tellWaiting(offset, num, keys?)` —— waiting + paused 同属等待队列。
    fn tell_waiting(&self, params: &[Value]) -> Result<Value, RpcError> {
        let keys = params.get(2).and_then(Value::as_array);
        let matches = |status: TaskStatus| {
            matches!(status, TaskStatus::Pending | TaskStatus::Paused)
        };
        Ok(Value::Array(self.page(params, matches, keys)))
    }

    /// `aria2.tellStopped(offset, num, keys?)` —— complete / error / removed。
    fn tell_stopped(&self, params: &[Value]) -> Result<Value, RpcError> {
        let keys = params.get(2).and_then(Value::as_array);
        let matches = |status: TaskStatus| {
            matches!(
                status,
                TaskStatus::Complete | TaskStatus::Error | TaskStatus::Removed
            )
        };
        Ok(Value::Array(self.page(params, matches, keys)))
    }

    /// 按 offset/num 截取与 aria2 `tellWaiting` / `tellStopped` 同口径的子集。
    fn page(
        &self,
        params: &[Value],
        matches: impl Fn(TaskStatus) -> bool,
        keys: Option<&Vec<Value>>,
    ) -> Vec<Value> {
        let offset = params.first().and_then(Value::as_u64).unwrap_or(0) as usize;
        let num = params.get(1).and_then(Value::as_u64).unwrap_or(1000) as usize;
        self.sorted_snapshots()
            .into_iter()
            .filter(|snapshot| matches(snapshot.status))
            .skip(offset)
            .take(num)
            .map(|snapshot| apply_keys(status_to_value(&snapshot), keys))
            .collect()
    }

    /// 稳定顺序（GID 升序）：方便 A/B 比对与分页复现。
    fn sorted_snapshots(&self) -> Vec<crate::task::registry::TaskSnapshot> {
        let mut snapshots = self.registry.list();
        snapshots.sort_by(|left, right| left.gid.cmp(&right.gid));
        snapshots
    }

    fn get_global_stat(&self) -> Value {
        let snapshots = self.sorted_snapshots();
        let active = snapshots
            .iter()
            .filter(|snapshot| snapshot.status == TaskStatus::Active)
            .count();
        let waiting = snapshots
            .iter()
            .filter(|snapshot| matches!(snapshot.status, TaskStatus::Pending | TaskStatus::Paused))
            .count();
        let stopped = snapshots
            .iter()
            .filter(|snapshot| {
                matches!(
                    snapshot.status,
                    TaskStatus::Complete | TaskStatus::Error | TaskStatus::Removed
                )
            })
            .count();
        let download_speed: u64 = snapshots
            .iter()
            .filter(|snapshot| snapshot.status == TaskStatus::Active)
            .map(|snapshot| snapshot.download_speed)
            .sum();
        // 字段与 `RpcMethodImpl.cc:1386` 的 Dict 逐项对齐，数值一律字符串。
        json!({
            "downloadSpeed": download_speed.to_string(),
            "uploadSpeed": "0",
            "numActive": active.to_string(),
            "numWaiting": waiting.to_string(),
            "numStopped": stopped.to_string(),
            "numStoppedTotal": stopped.to_string(),
        })
    }

    fn get_version(&self) -> Value {
        json!({
            "version": ARIA2_PROTOCOL_VERSION,
            "enabledFeatures": ["Async DNS", "HTTPS", "native-http", "Range"],
            "_engine": crate::ENGINE_NAME,
            "_engineVersion": crate::version(),
        })
    }

    fn get_global_option(&self) -> Value {
        let options = self.global_options.lock().unwrap();
        Value::Object(
            options
                .iter()
                .map(|(key, value)| (key.clone(), json!(value)))
                .collect(),
        )
    }

    fn change_global_option(&self, params: &[Value]) -> Result<Value, RpcError> {
        let Some(Value::Object(updates)) = params.first() else {
            return Err(RpcError::invalid_params(
                "changeGlobalOption 参数必须是对象",
            ));
        };
        {
            let mut options = self.global_options.lock().unwrap();
            for (key, value) in updates {
                if let Some(text) = value_to_string(value) {
                    options.insert(key.clone(), text);
                }
            }
        }
        Ok(json!("OK"))
    }

    fn multicall(&self, params: &[Value]) -> Result<Value, RpcError> {
        let calls = params
            .first()
            .and_then(Value::as_array)
            .ok_or_else(|| RpcError::invalid_params("system.multicall 参数必须是数组"))?;

        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let Some(object) = call.as_object() else {
                results.push(json!({
                    "code": CODE_INVALID_PARAMS,
                    "message": "multicall 条目必须是对象",
                }));
                continue;
            };
            let method = object
                .get("methodName")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if method == "system.multicall" {
                results.push(json!({
                    "code": CODE_INVALID_PARAMS,
                    "message": "不允许嵌套 system.multicall",
                }));
                continue;
            }
            let empty = Vec::new();
            let call_params = match object.get("params") {
                None => &empty,
                Some(Value::Array(items)) => items,
                Some(_) => {
                    results.push(json!({
                        "code": CODE_INVALID_PARAMS,
                        "message": format!("{method} 的 params 必须是数组"),
                    }));
                    continue;
                }
            };

            match self.dispatch(method, call_params) {
                // aria2 约定：成功时把结果包一层数组。
                Ok(result) => results.push(json!([result])),
                Err(err) => results.push(json!({
                    "code": err.code,
                    "message": err.message,
                })),
            }
        }
        Ok(Value::Array(results))
    }

    fn with_gid<F>(&self, params: &[Value], action: F) -> Result<Value, RpcError>
    where
        F: FnOnce(&str) -> Result<(), crate::task::registry::TaskError>,
    {
        let gid = self.gid_param(params)?;
        action(&gid)?;
        Ok(json!(gid))
    }

    /// `aria2.forcePause`：active / waiting 立即暂停；已暂停时直接成功。
    ///
    /// UI 的暂停按钮对 paused 任务也会下发（例如恢复后事件尚未回流时重复点击），
    /// 若在这里报"状态不能暂停"，用户看到的是操作失败但任务其实已停。
    fn force_pause(&self, params: &[Value]) -> Result<Value, RpcError> {
        let gid = self.gid_param(params)?;
        if let Some(snapshot) = self.registry.snapshot(&gid) {
            if snapshot.status == TaskStatus::Paused {
                return Ok(json!(gid));
            }
        }
        self.registry.pause(&gid)?;
        Ok(json!(gid))
    }

    fn gid_param(&self, params: &[Value]) -> Result<String, RpcError> {
        params
            .first()
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| RpcError::invalid_params("第一个参数必须是 GID 字符串"))
    }

    fn build_spec(
        &self,
        uris: Vec<String>,
        options: &Map<String, Value>,
    ) -> Result<TaskSpec, RpcError> {
        let globals = self.global_options.lock().unwrap().clone();

        let dir = local(options, "dir")
            .filter(|value| !value.is_empty())
            .or_else(|| globals.get("dir").cloned())
            .unwrap_or_else(|| ".".to_string());
        // 目录必须是绝对路径：引擎可能以 LocalSystem 运行，`.` 会解析到
        // `C:\Windows\System32`（进而落到 systemprofile\Downloads）。调用方
        // （service / UI）有责任先解析出用户真实下载目录再下发。
        if !PathBuf::from(&dir).is_absolute() {
            return Err(RpcError::invalid_params(format!(
                "dir 必须是绝对路径（收到 `{dir}`）"
            )));
        }
        // 显式 out 优先级最高（用户意图），但必须清洗 Windows 非法字符：
        // 用户可能把 `/`、`:` 或 `?` 粘贴进文件名，不能带进 dest。
        let explicit_out = local(options, "out")
            .filter(|value| !value.is_empty())
            .map(|value| crate::task::naming::sanitize_name(&value))
            .filter(|value| !value.is_empty());
        let out = explicit_out.clone().unwrap_or_else(|| {
            crate::task::naming::resolve_out_name(None, None, &uris[0], &uris[0])
        });
        // 260 字符上限：保留后缀截断主体，避免 tokio::fs 直接 os error 3。
        let dest = crate::task::naming::fit_windows_path(Path::new(&dir), &out);

        let mut http = HttpOptions::default();
        if let Some(user_agent) = local(options, "user-agent")
            .or_else(|| globals.get("user-agent").cloned())
            .filter(|value| !value.is_empty())
        {
            http.user_agent = user_agent;
        }
        http.referer = local(options, "referer")
            .or_else(|| globals.get("referer").cloned())
            .filter(|value| !value.is_empty());
        if let Some(flag) = local(options, "check-certificate")
            .or_else(|| globals.get("check-certificate").cloned())
        {
            http.check_certificate = flag != "false";
        }
        if let Some(flag) = local(options, "enable-http2")
            .or_else(|| globals.get("enable-http2").cloned())
        {
            http.enable_http2 = flag != "false";
        }
        // TLS 客户端证书（aria2 --certificate / --private-key，PEM）。
        http.client_certificate = local(options, "certificate")
            .or_else(|| globals.get("certificate").cloned())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        http.private_key = local(options, "private-key")
            .or_else(|| globals.get("private-key").cloned())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        if let Some(value) = local(options, "max-redirect")
            .or_else(|| globals.get("max-redirect").cloned())
            .and_then(|value| value.parse::<usize>().ok())
        {
            http.max_redirect = value;
        }
        if let Some(secs) = local(options, "connect-timeout")
            .or_else(|| globals.get("connect-timeout").cloned())
            .and_then(|value| value.parse::<u64>().ok())
        {
            http.connect_timeout = Duration::from_secs(secs);
        }
        if let Some(secs) = local(options, "timeout")
            .or_else(|| globals.get("timeout").cloned())
            .and_then(|value| value.parse::<u64>().ok())
        {
            http.read_timeout = Some(Duration::from_secs(secs));
        }
        if let Some(tries) = local(options, "max-tries")
            .or_else(|| globals.get("max-tries").cloned())
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
        {
            http.max_tries = tries;
        }
        if let Some(secs) = local(options, "retry-wait")
            .or_else(|| globals.get("retry-wait").cloned())
            .and_then(|value| value.parse::<u64>().ok())
        {
            http.retry_wait = Duration::from_secs(secs);
        }
        // 自定义请求头：任务级优先，其次全局（设置页 `header`）。
        // 全局值可能是多行字符串，`parse_headers` 会逐行拆分。
        let header_source = options
            .get("header")
            .cloned()
            .or_else(|| globals.get("header").cloned().map(Value::String));
        http.headers = parse_headers(header_source.as_ref())?;

        // Phase 3：代理（--all-proxy / --http-proxy / --https-proxy / --no-proxy）。
        http.proxy = parse_proxy_settings(options, &globals)?;
        // aria2 `--proxy-method=get|tunnel`（默认 get；https 恒走隧道）。
        http.proxy_method_tunnel = option_value(options, &globals, "proxy-method")
            .map(|value| value.eq_ignore_ascii_case("tunnel"))
            .unwrap_or(false);

        // Phase 3：并发上限。split 默认 5（-1 = 无限），-x 默认 1（1–128）。
        http.split = parse_split(option_value(options, &globals, "split").as_deref())?;
        http.max_connection_per_server = parse_max_connection(
            option_value(options, &globals, "max-connection-per-server").as_deref(),
        )?;

        // aria2 `OptionHandlerFactory.cc:992`：piece-length 为 UnitNumberOptionHandler，
        // 默认 1M，合法区间 [1MiB, 1GiB]。越界必须报错，不能静默接受（Phase 2 偏差 1）。
        let raw_piece_length = local(options, "piece-length")
            .or_else(|| globals.get("piece-length").cloned())
            .unwrap_or_else(|| DEFAULT_PIECE_LENGTH.to_string());
        let piece_length = parse_size(&raw_piece_length).ok_or_else(|| {
            RpcError::new(1, format!("Bad number {raw_piece_length}"))
        })?;
        if !(MIN_PIECE_LENGTH..=MAX_PIECE_LENGTH).contains(&piece_length) {
            return Err(RpcError::new(
                1,
                format!(
                    "piece-length must be between {MIN_PIECE_LENGTH} and {MAX_PIECE_LENGTH}."
                ),
            ));
        }

        // Phase 4：动态切割参数（任务级优先，其次全局；非法值必须报错）。
        let slicing = parse_slicing(options, &globals)?;

        // `--sqlite-progress`：产品默认 true（Aria2-Harness 实验特性）。
        let sqlite_progress = local(options, "sqlite-progress")
            .or_else(|| globals.get("sqlite-progress").cloned())
            .map(|value| value != "false")
            .unwrap_or(true);

        Ok(TaskSpec {
            uris,
            dest,
            explicit_out: explicit_out.is_some(),
            http,
            piece_length,
            slicing,
            sqlite_progress,
        })
    }
}

/// aria2 `OptionHandlerFactory.cc:992` 的 piece-length 区间。
const MIN_PIECE_LENGTH: u32 = 1024 * 1024;
const MAX_PIECE_LENGTH: u32 = 1024 * 1024 * 1024;

/// 解析 aria2 的单位数字（`UnitNumberOptionHandler`）：`1048576` / `1M` / `1K` / `1G`。
///
/// 只接受非负整数与 K/M/G 后缀（大小写不敏感），不引入浮点。
fn parse_size(text: &str) -> Option<u32> {
    let text = text.trim();
    let (digits, multiplier) = match text.chars().last()?.to_ascii_uppercase() {
        'K' => (&text[..text.len() - 1], 1024u64),
        'M' => (&text[..text.len() - 1], 1024 * 1024),
        'G' => (&text[..text.len() - 1], 1024 * 1024 * 1024),
        _ => (text, 1),
    };
    let base: u64 = digits.trim().parse().ok()?;
    let value = base.checked_mul(multiplier)?;
    u32::try_from(value).ok()
}

fn default_global_options() -> HashMap<String, String> {
    [
        ("dir", "."),
        ("split", "5"),
        ("max-connection-per-server", "1"),
        ("check-certificate", "true"),
        ("enable-http2", "true"),
        ("user-agent", "aria2/1.37.0"),
        ("referer", ""),
        ("max-redirect", "20"),
        ("piece-length", "1048576"),
        // Phase 4：动态切割（spec §6）。
        ("min-split-size", "20971520"),
        ("enable-dynamic-slicing", "true"),
        ("dynamic-slicing-stall", "12"),
        ("dynamic-slicing-max-preempts", "2"),
        ("dynamic-slicing-victim-cooldown", "5"),
        ("max-overall-download-limit", "0"),
        ("max-download-limit", "0"),
        ("max-tries", "5"),
        ("retry-wait", "0"),
        ("timeout", "60"),
        // aria2 `OptionHandlerFactory.cc:921`：connect-timeout 默认 60（区间 1–600）。
        ("connect-timeout", "60"),
        // aria2 `OptionHandlerFactory.cc:176`：continue 默认 false（续传只由控制文件驱动）。
        ("continue", "false"),
        ("async-dns", "true"),
        ("auto-save-interval", "60"),
        // 产品默认 `--sqlite-progress=true`（Aria2-Harness 实验特性）。
        ("sqlite-progress", "true"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value.to_string()))
    .collect()
}

fn local(options: &Map<String, Value>, name: &str) -> Option<String> {
    options.get(name).and_then(value_to_string)
}

/// 任务级选项优先，其次全局选项（与 `build_spec` 的既有取值顺序一致）。
fn option_value(
    options: &Map<String, Value>,
    globals: &HashMap<String, String>,
    name: &str,
) -> Option<String> {
    local(options, name).or_else(|| globals.get(name).cloned())
}

/// `--split`：默认 5；`-1` 表示无限（None）；0 与其它负数非法。
fn parse_split(raw: Option<&str>) -> Result<Option<usize>, RpcError> {
    let Some(raw) = raw else { return Ok(Some(5)) };
    let value: i64 = raw
        .trim()
        .parse()
        .map_err(|_| RpcError::new(1, format!("Bad number {raw}")))?;
    if value == -1 {
        return Ok(None);
    }
    if value >= 1 {
        return Ok(Some(value as usize));
    }
    Err(RpcError::new(1, "split must be >= 1 or -1."))
}

/// `-x/--max-connection-per-server`：1–128，默认 1（`OptionHandlerFactory.cc:456`）。
fn parse_max_connection(raw: Option<&str>) -> Result<usize, RpcError> {
    let raw = raw.unwrap_or("1");
    let value: i64 = raw
        .trim()
        .parse()
        .map_err(|_| RpcError::new(1, format!("Bad number {raw}")))?;
    if (1..=128).contains(&value) {
        Ok(value as usize)
    } else {
        Err(RpcError::new(
            1,
            "max-connection-per-server must be between 1 and 128.",
        ))
    }
}

/// Phase 4 动态切割参数（spec §6）：任务级优先，其次全局。
///
/// 非法值一律报错（`Bad number`），不做静默夹取，避免用户以为参数生效。
fn parse_slicing(
    options: &Map<String, Value>,
    globals: &HashMap<String, String>,
) -> Result<SlicingOptions, RpcError> {
    let mut slicing = SlicingOptions::default();

    if let Some(raw) = option_value(options, globals, "min-split-size") {
        let value = parse_size_u64(&raw)
            .ok_or_else(|| RpcError::new(1, format!("Bad number {raw}")))?;
        if !(MIN_PIECE_LENGTH as u64..=MAX_PIECE_LENGTH as u64).contains(&value) {
            return Err(RpcError::new(
                1,
                format!("min-split-size must be between {MIN_PIECE_LENGTH} and {MAX_PIECE_LENGTH}."),
            ));
        }
        slicing.min_split_size = value;
    }

    if let Some(raw) = option_value(options, globals, "enable-dynamic-slicing") {
        if raw != "true" && raw != "false" {
            return Err(RpcError::new(
                1,
                format!("enable-dynamic-slicing must be true or false, got {raw}"),
            ));
        }
        slicing.enable_dynamic_slicing = raw == "true";
    }

    if let Some(raw) = option_value(options, globals, "dynamic-slicing-stall") {
        slicing.stall = Duration::from_secs(parse_range_secs(&raw, 1, 600, "dynamic-slicing-stall")?);
    }

    if let Some(raw) = option_value(options, globals, "dynamic-slicing-max-preempts") {
        let value = parse_range_secs(&raw, 0, 100, "dynamic-slicing-max-preempts")?;
        slicing.max_preempts = u8::try_from(value).expect("区间已限制在 0–100");
    }

    if let Some(raw) = option_value(options, globals, "dynamic-slicing-victim-cooldown") {
        slicing.victim_cooldown = Duration::from_secs(parse_range_secs(
            &raw,
            0,
            600,
            "dynamic-slicing-victim-cooldown",
        )?);
    }

    if let Some(raw) = option_value(options, globals, "max-overall-download-limit") {
        slicing.max_overall_download_limit =
            parse_size_u64(&raw).ok_or_else(|| RpcError::new(1, format!("Bad number {raw}")))?;
    }
    if let Some(raw) = option_value(options, globals, "max-download-limit") {
        slicing.max_download_limit =
            parse_size_u64(&raw).ok_or_else(|| RpcError::new(1, format!("Bad number {raw}")))?;
    }

    Ok(slicing)
}

/// 解析闭区间整数秒（用于 stall/cooldown/max-preempts）。
fn parse_range_secs(raw: &str, min: u64, max: u64, name: &str) -> Result<u64, RpcError> {
    let value: u64 = raw
        .trim()
        .parse()
        .map_err(|_| RpcError::new(1, format!("Bad number {raw}")))?;
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(RpcError::new(
            1,
            format!("{name} must be between {min} and {max}."),
        ))
    }
}

/// `parse_size` 的 u64 版本（限速可能超过 4GiB）。
fn parse_size_u64(text: &str) -> Option<u64> {
    let text = text.trim();
    let (digits, multiplier) = match text.chars().last()?.to_ascii_uppercase() {
        'K' => (&text[..text.len() - 1], 1024u64),
        'M' => (&text[..text.len() - 1], 1024 * 1024),
        'G' => (&text[..text.len() - 1], 1024 * 1024 * 1024),
        _ => (text, 1),
    };
    let base: u64 = digits.trim().parse().ok()?;
    base.checked_mul(multiplier)
}

/// 代理选项解析：具体协议优先于 `--all-proxy`，空串等价未设置
/// （`AbstractCommand.cc:660-713`）。
fn parse_proxy_settings(
    options: &Map<String, Value>,
    globals: &HashMap<String, String>,
) -> Result<ProxySettings, RpcError> {
    let slot = |name: &str| -> Result<ProxySlot, RpcError> {
        match option_value(options, globals, name) {
            Some(raw) => ProxySlot::from_raw(&raw)
                .map_err(|err| RpcError::new(1, err.to_string())),
            None => Ok(ProxySlot::Unset),
        }
    };
    let credential = |name: &str| -> Option<String> {
        option_value(options, globals, name).filter(|value| !value.is_empty())
    };
    let with_credentials = |slot: ProxySlot, prefix: &str| -> ProxySlot {
        let user = credential(&format!("{prefix}-user"));
        let pass = credential(&format!("{prefix}-passwd"));
        match slot {
            ProxySlot::Set(config) => ProxySlot::Set(config.with_credentials(user, pass)),
            ProxySlot::Unset => ProxySlot::Unset,
        }
    };

    let no_proxy = option_value(options, globals, "no-proxy")
        .map(|raw| {
            raw.split(',')
                .filter_map(NoProxyRule::parse)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(ProxySettings {
        all: with_credentials(slot("all-proxy")?, "all-proxy"),
        http: with_credentials(slot("http-proxy")?, "http-proxy"),
        https: with_credentials(slot("https-proxy")?, "https-proxy"),
        no_proxy,
    })
}

/// aria2 `keys` 过滤：只保留请求的字段；未请求的字段一律丢弃。
fn filter_keys(value: Value, keys: &[Value]) -> Value {
    let mut filtered = Map::new();
    for key in keys.iter().filter_map(Value::as_str) {
        if let Some(field) = value.get(key) {
            filtered.insert(key.to_string(), field.clone());
        }
    }
    Value::Object(filtered)
}

fn apply_keys(value: Value, keys: Option<&Vec<Value>>) -> Value {
    match keys {
        Some(keys) => filter_keys(value, keys),
        None => value,
    }
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// 解析 `header` 选项：字符串或字符串数组，每个元素都按行拆分（`\r\n` / `\n`）。
///
/// 之所以在 addUri 阶段就校验：非法头名 / 头值若留到真正发请求时才报错，用户看到的
/// 是"任务已添加但下载失败"，定位成本高得多。
fn parse_headers(value: Option<&Value>) -> Result<Vec<(String, String)>, RpcError> {
    let mut headers = Vec::new();
    match value {
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(text) = value_to_string(item) {
                    push_header_lines(&mut headers, &text)?;
                }
            }
        }
        Some(other) => {
            if let Some(text) = value_to_string(other) {
                push_header_lines(&mut headers, &text)?;
            }
        }
        None => {}
    }
    Ok(headers)
}

fn push_header_lines(headers: &mut Vec<(String, String)>, text: &str) -> Result<(), RpcError> {
    for line in text.split(['\r', '\n']) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, header_value)) = line.split_once(':') else {
            return Err(RpcError::invalid_params(format!(
                "请求头格式非法（缺少冒号）：`{line}`，每行应为 `Key: Value`"
            )));
        };
        let name = name.trim();
        let header_value = header_value.trim();
        if name.is_empty() {
            return Err(RpcError::invalid_params(format!(
                "请求头名不能为空：`{line}`"
            )));
        }
        if http::header::HeaderName::from_bytes(name.as_bytes()).is_err() {
            return Err(RpcError::invalid_params(format!(
                "非法请求头名 `{name}`"
            )));
        }
        if http::header::HeaderValue::from_str(header_value).is_err() {
            return Err(RpcError::invalid_params(format!(
                "非法请求头值（`{name}`）：含不可用于 HTTP 的字符"
            )));
        }
        headers.push((name.to_string(), header_value.to_string()));
    }
    Ok(())
}

/// `http://a.test/dir/file.bin?x=1` → `file.bin`。
///
/// beta8.3 起统一的命名裁决在 [`crate::task::naming::resolve_out_name`]：
/// 建任务时只掌握原始 URL，因此这里等价于「原始 = 最终」的临时名；
/// 任务启动后 registry 会用响应头 / 最终 URL 再裁决一次。
#[cfg(test)]
fn out_name(uri: &str) -> String {
    crate::task::naming::resolve_out_name(None, None, uri, uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_out_name_from_uri() {
        assert_eq!(out_name("http://a.test/x/file.bin?token=1"), "file.bin");
        // 无文件名 → 时间戳兜底名。
        assert!(out_name("http://a.test/").starts_with("Download_"));
        assert!(out_name("http://a.test").starts_with("Download_"));
    }

    /// 中文按百分号编码进 URL 时必须还原成可读文件名。
    #[test]
    fn decodes_percent_encoded_names() {
        assert_eq!(out_name("http://a.test/%E4%B8%AD%E6%96%87.zip"), "中文.zip");
        assert_eq!(out_name("http://a.test/a%20b.zip"), "a b.zip");
        // 非法百分号序列原样保留，不能被吞掉。
        assert_eq!(out_name("http://a.test/100%zz.zip"), "100%zz.zip");
    }

    /// Windows 非法字符必须清洗，不能把路径分隔符带进文件名。
    #[test]
    fn sanitizes_windows_illegal_chars() {
        assert_eq!(out_name("http://a.test/a%3Ab%2Fc.zip"), "a_b_c.zip");
        assert_eq!(out_name("http://a.test/a%2A%3F.zip"), "a__.zip");
        // `.` / `..` 等路径穿越名清洗后为空，走兜底名。
        assert!(out_name("http://a.test/..").starts_with("Download_"));
    }

    /// GitHub Release 直链的哈希尾段 → 时间戳兜底名；短名保持原样。
    #[test]
    fn opaque_names_fall_back_to_timestamp() {
        assert!(out_name(
            "https://github.com/x/y/releases/download/v1/abc123def4567890abcdef"
        )
        .starts_with("Download_"));
        assert_eq!(out_name("http://a.test/word"), "word");
        assert_eq!(out_name("http://a.test/LICENSE"), "LICENSE");
    }

    #[test]
    fn headers_accept_string_and_array() {
        assert_eq!(
            parse_headers(Some(&json!("X-A: 1"))).unwrap(),
            vec![("X-A".to_string(), "1".to_string())]
        );
        assert_eq!(
            parse_headers(Some(&json!(["X-A: 1", "X-B: 2"]))).unwrap(),
            vec![
                ("X-A".to_string(), "1".to_string()),
                ("X-B".to_string(), "2".to_string())
            ]
        );
    }

    /// 设置页的多行文本与浏览器扩展的数组，都必须拆成逐条 header。
    #[test]
    fn headers_split_multiline_text() {
        assert_eq!(
            parse_headers(Some(&json!("X-A: 1\r\nX-B: 2\n\n  X-C : 3  "))).unwrap(),
            vec![
                ("X-A".to_string(), "1".to_string()),
                ("X-B".to_string(), "2".to_string()),
                ("X-C".to_string(), "3".to_string())
            ]
        );
        assert_eq!(
            parse_headers(Some(&json!(["X-A: 1\nX-B: 2"]))).unwrap(),
            vec![
                ("X-A".to_string(), "1".to_string()),
                ("X-B".to_string(), "2".to_string())
            ]
        );
    }

    /// 非法行必须在 addUri 阶段被拒绝，而不是等到发请求才失败。
    #[test]
    fn headers_reject_invalid_lines() {
        assert!(parse_headers(Some(&json!("NoColonHere"))).is_err());
        assert!(parse_headers(Some(&json!(": empty-name"))).is_err());
        assert!(parse_headers(Some(&json!("Bad Name: 1"))).is_err());
        assert!(parse_headers(Some(&json!("X-A: a\nb"))).is_err());
    }

    /// 多 URI：镜像数组原样保留，且共用同一份 Header 与落盘路径。
    #[test]
    fn multi_uri_shares_headers_and_dest() {
        let engine = empty_engine();
        let dir = std::env::temp_dir();
        let mut options = Map::new();
        options.insert("dir".to_string(), json!(dir.to_string_lossy()));
        options.insert("header".to_string(), json!(["Cookie: a=1"]));
        let spec = engine
            .build_spec(
                vec![
                    "https://a.test/game.zip".to_string(),
                    "https://b.test/game.zip".to_string(),
                ],
                &options,
            )
            .expect("spec");
        assert_eq!(spec.uris.len(), 2);
        assert_eq!(spec.dest, dir.join("game.zip"));
        assert_eq!(
            spec.http.headers,
            vec![("Cookie".to_string(), "a=1".to_string())]
        );
    }

    fn empty_engine() -> Engine {
        let resolver = crate::dns::resolver::DnsResolver::new(
            &crate::dns::resolver::DnsConfig::default(),
        )
        .expect("DNS 解析器初始化不应联网");
        Engine::new(resolver)
    }

    #[test]
    fn keys_filter_drops_unrequested_fields() {
        let value = json!({"gid":"fffe000000000001","status":"active","totalLength":"9"});
        let filtered = filter_keys(value, &[json!("gid"), json!("_engine")]);
        assert_eq!(filtered, json!({"gid":"fffe000000000001"}));
    }

    /// v1.0-beta3：前端 / 宿主可能下发的全部方法与别名都必须有实现，
    /// 绝不允许 UI 因 `-32601` 直接瘫痪。
    #[test]
    fn frontend_method_table_is_complete() {
        let engine = empty_engine();
        for method in [
            "aria2.purgeDownloadResult",
            "aria2.pauseAll",
            "aria2.forcePauseAll",
            "aria2.unpauseAll",
            "aria2.saveSession",
            "aria2.shutdown",
            "aria2.forceShutdown",
            "aria2.getSessionInfo",
        ] {
            let value = engine
                .call(method, &[])
                .unwrap_or_else(|err| panic!("{method} 必须可用，实际 {err:?}"));
            assert!(!value.is_null(), "{method} 结果不应为空");
        }

        // 带 GID 的方法：GID 不存在时返回业务错误码 1（而不是 -32601）。
        for method in [
            "aria2.getFiles",
            "aria2.tellFiles",
            "aria2.getOption",
            "aria2.changeOption",
        ] {
            let err = engine
                .call(method, &[json!("fffe000000000001")])
                .expect_err(method);
            assert_eq!(err.code, 1, "{method} 必须是业务错误");
        }
        let err = engine
            .call("aria2.changePosition", &[json!("fffe000000000001"), json!(0)])
            .expect_err("changePosition");
        assert_eq!(err.code, 1);
    }

    /// `getGlobalStat` 字段必须与 aria2 同形状（全部字符串）。
    #[test]
    fn global_stat_is_aria2_shaped() {
        let engine = empty_engine();
        let value = engine.call("aria2.getGlobalStat", &[]).unwrap();
        for key in [
            "downloadSpeed",
            "uploadSpeed",
            "numActive",
            "numWaiting",
            "numStopped",
            "numStoppedTotal",
        ] {
            assert!(value[key].is_string(), "{key} 必须是字符串");
        }
        assert_eq!(value["numActive"], "0");
    }

    /// 空引擎下三个列表方法都必须返回空数组，而不是报错。
    #[test]
    fn list_methods_return_empty_arrays() {
        let engine = empty_engine();
        for method in ["aria2.tellActive", "aria2.tellWaiting", "aria2.tellStopped"] {
            let value = engine.call(method, &[]).unwrap();
            assert_eq!(value, json!([]), "{method}");
        }
    }

    /// Phase 4：7 个动态切割选项必须有默认值且能通过 `getGlobalOption` 回读。
    #[test]
    fn dynamic_slicing_defaults_are_readable() {
        let engine = empty_engine();
        let value = engine.call("aria2.getGlobalOption", &[]).unwrap();
        assert_eq!(value["min-split-size"], "20971520");
        assert_eq!(value["enable-dynamic-slicing"], "true");
        assert_eq!(value["dynamic-slicing-stall"], "12");
        assert_eq!(value["dynamic-slicing-max-preempts"], "2");
        assert_eq!(value["dynamic-slicing-victim-cooldown"], "5");
        assert_eq!(value["max-overall-download-limit"], "0");
        assert_eq!(value["max-download-limit"], "0");
    }

    #[test]
    fn slicing_options_accept_units_and_ranges() {
        let mut globals = default_global_options();
        globals.insert("max-download-limit".into(), "3G".into());
        let options = Map::new();
        let slicing = parse_slicing(&options, &globals).expect("全局选项合法");
        assert_eq!(slicing.min_split_size, 20 * 1024 * 1024);
        assert_eq!(slicing.max_download_limit, 3 * 1024 * 1024 * 1024);
        assert!(slicing.enable_dynamic_slicing);

        // 任务级覆盖全局。
        let mut task = Map::new();
        task.insert("min-split-size".into(), json!("4M"));
        task.insert("enable-dynamic-slicing".into(), json!("false"));
        task.insert("dynamic-slicing-stall".into(), json!("30"));
        task.insert("dynamic-slicing-max-preempts".into(), json!("0"));
        task.insert("dynamic-slicing-victim-cooldown".into(), json!("0"));
        let slicing = parse_slicing(&task, &globals).expect("任务级选项合法");
        assert_eq!(slicing.min_split_size, 4 * 1024 * 1024);
        assert!(!slicing.enable_dynamic_slicing);
        assert_eq!(slicing.stall, Duration::from_secs(30));
        assert_eq!(slicing.max_preempts, 0);
        assert!(slicing.victim_cooldown.is_zero());
        assert_eq!(slicing.max_download_limit, 3 * 1024 * 1024 * 1024);
    }

    /// 非法值必须报错（不能静默夹取，否则用户以为参数生效）。
    #[test]
    fn slicing_options_reject_invalid_values() {
        let globals = default_global_options();
        for (name, value) in [
            ("min-split-size", "1K"),
            ("min-split-size", "abc"),
            ("enable-dynamic-slicing", "yes"),
            ("dynamic-slicing-stall", "0"),
            ("dynamic-slicing-stall", "601"),
            ("dynamic-slicing-max-preempts", "101"),
            ("dynamic-slicing-victim-cooldown", "601"),
            ("max-overall-download-limit", "1T"),
            ("max-download-limit", "-1"),
        ] {
            let mut options = Map::new();
            options.insert(name.to_string(), json!(value));
            assert!(
                parse_slicing(&options, &globals).is_err(),
                "{name}={value} 应被拒绝"
            );
        }
    }
}
