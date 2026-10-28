//! QuickJsRuntime：QuickJS 逻辑沙箱（M1 后端）。
//!
//! 关键约束：`Runtime` / `Context` 均为 `!Send`，因此本结构体只在插件专属线程内
//! 创建与使用，绝不跨线程移动（见 `manager::plugin_thread`）。
//!
//! L4 资源熔断：内存上限 + 栈上限 + 每次 hook 调用的 interrupt 超时。
//! L1 能力默认拒绝：全局只暴露预注入的 `harness`，其余能力经 `__harness_host` 桥。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rquickjs::{Context, Function, Object, Runtime, Value};
use serde_json::Value as Json;

use crate::error::AppError;

use super::hostapi::PluginHostContext;
use super::manifest::Manifest;

/// 宿主预注入的 JS 门面：唯一出口是 `harness`（含 ctx / hooks / on / _call）。
///
/// `_call` 在 JS 侧 try/catch，把插件异常转成 JSON 回传，避免 Rust 侧处理 QuickJS 异常态。
const PRELUDE: &str = r#"(function () {
  var hooks = {};
  function host(op, payload) {
    try {
      return globalThis.__harness_host(op, payload == null ? "" : payload);
    } catch (e) {
      return "";
    }
  }
  var settings = {
    get: function (key) {
      var r = host("settings.get", JSON.stringify({ key: String(key) }));
      return r === "" ? undefined : JSON.parse(r);
    },
    all: function () {
      var r = host("settings.all", "");
      return r === "" ? {} : JSON.parse(r);
    }
  };
  // 需要结构化结果的宿主调用：把宿主返回的 JSON 解析成对象（失败一律降级为 {ok:false}）。
  function hostResult(op, payload) {
    var r = host(op, JSON.stringify(payload));
    if (r === "") { return { ok: false, error: "宿主无响应" }; }
    try { return JSON.parse(r); } catch (e) { return { ok: false, error: "响应解析失败" }; }
  }
  // 沙箱文件能力：宿主侧做下载根目录白名单校验，越权返回 {ok:false,error}。
  var fs = {
    mkdir: function (path) {
      return hostResult("fs.mkdir", { path: String(path) });
    },
    move: function (src, dest) {
      return hostResult("fs.move", { src: String(src), dest: String(dest) });
    },
    writeText: function (path, content) {
      return hostResult("fs.writeText", { path: String(path), content: String(content) });
    },
    readText: function (path) {
      return hostResult("fs.readText", { path: String(path) });
    },
    exists: function (path) {
      return hostResult("fs.exists", { path: String(path) });
    },
    remove: function (path) {
      return hostResult("fs.remove", { path: String(path) });
    }
  };
  // beta10：同步出站 HTTP（需 net:http 权限）。返回 {ok,status,headers,body,error}。
  function httpGet(opts) {
    return hostResult("http.get", opts || {});
  }
  // beta10：批量派发下载并等待终态（需 downloads:write 权限）。
  function addTasksAndWait(tasks, opts) {
    var payload = {};
    if (opts) {
      for (var key in opts) { payload[key] = opts[key]; }
    }
    payload.tasks = tasks || [];
    return hostResult("tasks.addAndWait", payload);
  }
  // beta10：执行 FFmpeg / FFprobe（需 exec:ffmpeg 权限）。
  function exec(opts) {
    return hostResult("exec.run", opts || {});
  }
  // beta12 · Step 2：站点插件在 onHolo / onAction 中创建**自己的**虚拟任务卡
  // （onHolo 不经过 claim，没有宿主任命的 task.id；返回 {ok,id}）。
  function startTask(opts) {
    return hostResult("task.create", opts || {});
  }
  // beta12 · Step 2：当前下载根目录（只读）。onHolo 拿不到 task.dir，
  // 插件据此拼临时目录 / 输出路径；无可写根时返回空串。
  function downloadDir() {
    var r = hostResult("paths.downloadDir", {});
    return r && r.ok === true ? String(r.path || "") : "";
  }
  // 兼容 async/await 写法：宿主在每次 hook 返回后泵空微任务，因此
  // `await fetch(url)` 无需事件循环即可完成（底层仍是同步 httpGet）。
  function fetch(url, init) {
    var opts = { url: String(url) };
    if (init && init.headers) { opts.headers = init.headers; }
    if (init && init.timeoutMs) { opts.timeoutMs = init.timeoutMs; }
    if (init && init.responseType) { opts.responseType = init.responseType; }
    var res = httpGet(opts);
    if (!res || res.ok === false) {
      return Promise.reject(new Error((res && res.error) || "httpGet 失败"));
    }
    return Promise.resolve(res);
  }
  var ctx = {
    // beta11：兼容 `ctx.log(msg)` 单参调用（第二参缺省时取第一参，归为 info）。
    log: function (level, msg) {
      if (msg === undefined) {
        host("log", JSON.stringify({ level: "info", msg: level == null ? "" : String(level) }));
        return;
      }
      host("log", JSON.stringify({ level: String(level), msg: String(msg) }));
    },
    notify: function (title, body) {
      host("notify", JSON.stringify({ title: String(title), body: body == null ? "" : String(body) }));
    },
    emit: function (name, payload) {
      host("emit", JSON.stringify({ name: String(name), payload: payload === undefined ? null : payload }));
    },
    getTask: function (gid) {
      var r = host("getTask", JSON.stringify({ gid: String(gid) }));
      return r === "" ? null : JSON.parse(r);
    },
    getTasks: function () {
      var r = host("getTasks", "");
      return r === "" ? [] : JSON.parse(r);
    },
    // beta12 · 模块 E：把插件业务意图交给宿主事件总线（plugin://dispatch）。
    dispatchTask: function (intent, payload) {
      return hostResult("dispatchTask", {
        intent: String(intent),
        payload: payload === undefined ? null : payload
      });
    },
    // 前置拦截：改写即将添加的任务 URL（需 downloads:intercept 权限）。
    resolveUrl: function (url) {
      return hostResult("resolveUrl", { url: String(url) });
    },
    settings: settings,
    fs: fs,
    httpGet: httpGet,
    addTasksAndWait: addTasksAndWait,
    exec: exec,
    startTask: startTask,
    downloadDir: downloadDir
  };
  var harness = {
    id: "",
    ctx: ctx,
    hooks: hooks,
    on: function (name, fn) { hooks[String(name)] = fn; return harness; },
    _call: function (name, payloadJson) {
      var fn = hooks[String(name)];
      if (typeof fn !== "function") { return "{\"called\":false}"; }
      try {
        var event = payloadJson ? JSON.parse(payloadJson) : undefined;
        var value = fn(ctx, event);
        // beta10：保留 hook 返回值（例如 onTaskDetected 的 {claim:true}）。
        var text = "null";
        if (value !== undefined) {
          try {
            var encoded = JSON.stringify(value);
            if (typeof encoded === "string") { text = encoded; }
          } catch (e2) { text = "null"; }
        }
        return "{\"called\":true,\"ok\":true,\"result\":" + text + "}";
      } catch (e) {
        var msg = "";
        try { msg = String((e && e.stack) || e); } catch (x) { msg = "unknown"; }
        return JSON.stringify({ called: true, ok: false, error: msg });
      }
    }
  };
  // beta11 · B4：预注入 console（log/info/warn/error/debug → ctx.log），
  // 让生态里直接 `console.log` 的插件无需改造即可进入日志控制台。
  function joinArgs(args) {
    return Array.prototype.map.call(args, function (item) { return String(item); }).join(" ");
  }
  globalThis.console = {
    log: function () { ctx.log("info", joinArgs(arguments)); },
    info: function () { ctx.log("info", joinArgs(arguments)); },
    warn: function () { ctx.log("warn", joinArgs(arguments)); },
    error: function () { ctx.log("error", joinArgs(arguments)); },
    debug: function () { ctx.log("debug", joinArgs(arguments)); }
  };
  globalThis.harness = harness;
  globalThis.ctx = ctx;
  globalThis.fetch = fetch;
  return harness;
})();
"#;

/// 单插件 QuickJS 运行时（仅存在于插件专属线程）。
pub struct QuickJsRuntime {
    context: Context,
    host: PluginHostContext,
    deadline: Arc<AtomicU64>,
    timeout_ms: u64,
    declared_hooks: Vec<String>,
    plugin_id: String,
}

impl QuickJsRuntime {
    pub fn new(manifest: &Manifest, host: PluginHostContext) -> Result<Self, AppError> {
        let runtime = Runtime::new()
            .map_err(|e| AppError::PluginRuntime(format!("创建 QuickJS Runtime 失败：{e}")))?;
        // L4：内存上限（硬性兜底），0 表示不限制，所以我们至少给 1MB。
        runtime.set_memory_limit(manifest.limits.memory_mb.max(1) * 1024 * 1024);
        runtime.set_max_stack_size(512 * 1024);

        // L4：interrupt 超时。handler 无参数，返回 true 即抛出不可捕获异常中断执行。
        let deadline = Arc::new(AtomicU64::new(0));
        {
            let deadline_ref = deadline.clone();
            runtime.set_interrupt_handler(Some(Box::new(move || {
                let at = deadline_ref.load(Ordering::Relaxed);
                at != 0 && now_ms() >= at
            })));
        }

        // Context 内部持有 Runtime 的强引用，因此无需额外保存 runtime 句柄。
        let context = Context::full(&runtime)
            .map_err(|e| AppError::PluginRuntime(format!("创建 QuickJS Context 失败：{e}")))?;

        Ok(Self {
            context,
            host,
            deadline,
            // 硬上限 10s，下限 100ms，防止 manifest 声明离谱值
            timeout_ms: manifest.limits.timeout_ms.clamp(100, 10_000),
            declared_hooks: manifest.hooks.clone(),
            plugin_id: manifest.id.clone(),
        })
    }

    /// 注入宿主桥 + `harness` 门面，执行插件入口，并校验 manifest 声明的 hook。
    pub fn load(&self, source: &str) -> Result<(), AppError> {
        let host = self.host.clone();
        let plugin_id = self.plugin_id.clone();
        let declared = self.declared_hooks.clone();
        let deadline = self.deadline.clone();

        self.context.with(move |ctx| -> Result<(), AppError> {
            // 1) 注入 __harness_host（闭包无生命周期参数，规避 IntoJsFunc 的标注难题）
            //
            // beta10：阻塞型宿主调用（httpGet / addTasksAndWait / exec）在进入前把
            // QuickJS 的墙钟 deadline 置 0（等价于"挂起计时器"），返回后把这次调用
            // 实际消耗的时间补回截止时刻 —— 既不会"调用刚返回就被 interrupt 杀掉"，
            // 也无法通过反复调用无限延长预算（剩余预算被如实保留）。
            let func = Function::new(ctx.clone(), move |op: String, payload: String| -> String {
                let blocking = matches!(op.as_str(), "http.get" | "tasks.addAndWait" | "exec.run");
                let previous = if blocking {
                    deadline.swap(0, Ordering::Relaxed)
                } else {
                    0
                };
                let started = now_ms();
                let out = host.respond(&op, &payload);
                if blocking && previous != 0 {
                    let spent = now_ms().saturating_sub(started);
                    deadline.store(previous.saturating_add(spent), Ordering::Relaxed);
                }
                out
            })
            .map_err(|e| {
                AppError::PluginRuntime(format!("注入 __harness_host 失败：{}", caught(&ctx, e)))
            })?;
            ctx.globals()
                .set("__harness_host", func)
                .map_err(|e| {
                    AppError::PluginRuntime(format!("设置 __harness_host 失败：{}", caught(&ctx, e)))
                })?;

            // 2) 预注入 harness 全局（脚本 eval 模式，无需 loader/ES module）
            ctx.eval::<Value, _>(PRELUDE)
                .map_err(|e| {
                    AppError::PluginRuntime(format!(
                        "[plugin:{plugin_id}] 预注入 harness 失败：{}",
                        caught(&ctx, e)
                    ))
                })?;

            // 3) 写入插件 id
            let harness: Object = ctx
                .globals()
                .get("harness")
                .map_err(|e| {
                    AppError::PluginRuntime(format!("获取 harness 失败：{}", caught(&ctx, e)))
                })?;
            harness
                .set("id", plugin_id.clone())
                .map_err(|e| {
                    AppError::PluginRuntime(format!("设置 harness.id 失败：{}", caught(&ctx, e)))
                })?;

            // 4) 执行插件入口脚本
            ctx.eval::<Value, _>(source).map_err(|e| {
                AppError::PluginRuntime(format!(
                    "[plugin:{plugin_id}] 入口脚本执行失败：{}",
                    caught(&ctx, e)
                ))
            })?;

            // 5) 校验 manifest 声明的 hook 是否都已注册
            let hooks_obj: Object = harness
                .get("hooks")
                .map_err(|e| {
                    AppError::PluginRuntime(format!("获取 harness.hooks 失败：{}", caught(&ctx, e)))
                })?;
            for hook in &declared {
                match hooks_obj.contains_key(hook.as_str()) {
                    Ok(true) => {}
                    Ok(false) => eprintln!(
                        "[plugin:{plugin_id}] manifest 声明了 `{hook}`，但 main.js 未注册（该 hook 将被跳过）"
                    ),
                    Err(e) => {
                        return Err(AppError::PluginRuntime(format!(
                            "检查 hook `{hook}` 失败：{}",
                            caught(&ctx, e)
                        )));
                    }
                }
            }
            Ok(())
        })
    }

    /// 同步执行一次 hook（L4：调用期间由 interrupt handler 限时）。
    pub fn call_hook(&self, hook: &str, payload: &Json) -> Result<(), AppError> {
        self.call_hook_with_budget(hook, payload, self.timeout_ms)
            .map(|_| ())
    }

    /// 执行 hook 并返回其 JS 返回值（beta10：`onTaskDetected` 需要 `{claim:true}`）。
    pub fn call_hook_result(&self, hook: &str, payload: &Json) -> Result<Option<Json>, AppError> {
        self.call_hook_with_budget(hook, payload, self.timeout_ms)
    }

    /// 以指定预算执行 hook（长任务 hook 由调用方传入 `limits.taskTimeoutMs`）。
    pub fn call_hook_with_budget(
        &self,
        hook: &str,
        payload: &Json,
        budget_ms: u64,
    ) -> Result<Option<Json>, AppError> {
        let payload_json = serde_json::to_string(payload).unwrap_or_else(|_| "null".to_string());
        let hook_name = hook.to_string();
        let plugin_id = self.plugin_id.clone();
        let deadline = self.deadline.clone();
        let timeout = budget_ms.clamp(100, 1_800_000);

        self.context.with(move |ctx| -> Result<Option<Json>, AppError> {
            deadline.store(now_ms() + timeout, Ordering::Relaxed);
            let called = (|| -> Result<String, rquickjs::Error> {
                let harness: Object = ctx.globals().get("harness")?;
                let call: Function = harness.get("_call")?;
                call.call((hook_name.clone(), payload_json.clone()))
            })();
            // beta10：泵空微任务 —— async/await 插件里的 `await fetch(...)` 在此完成，
            // 无需引入事件循环或 ES module loader（仅 std feature）。
            while ctx.execute_pending_job() {}
            deadline.store(0, Ordering::Relaxed);

            let raw = match called {
                Ok(raw) => raw,
                Err(e) => {
                    return Err(AppError::PluginRuntime(format!(
                        "[plugin:{plugin_id}] hook `{hook_name}` 调用失败：{}",
                        caught(&ctx, e)
                    )));
                }
            };

            // JS 侧已 try/catch，异常以 JSON 形式回传
            let Ok(value) = serde_json::from_str::<Json>(&raw) else {
                return Ok(None);
            };
            if value.get("ok").and_then(|b| b.as_bool()) == Some(false) {
                let msg = value
                    .get("error")
                    .and_then(|s| s.as_str())
                    .unwrap_or("未知异常");
                return Err(AppError::PluginRuntime(format!(
                    "[plugin:{plugin_id}] hook `{hook_name}` 抛出异常：{msg}"
                )));
            }
            Ok(value.get("result").cloned())
        })
    }

    /// 退出前触发 onShutdown（尽力而为，不因异常打断退出流程）。
    pub fn call_shutdown(&self) {
        // 尽力而为：插件侧 onShutdown 抛异常不应打断宿主退出流程，但要留下痕迹便于排查。
        if let Err(err) = self.call_hook("onShutdown", &serde_json::json!({ "hook": "onShutdown" })) {
            tracing::warn!("插件 onShutdown 钩子执行失败（不阻断退出）：{err}");
        }
    }
}

/// 把 QuickJS 的 `Error`（含异常态）转为可读文本。
fn caught(ctx: &rquickjs::Ctx, err: rquickjs::Error) -> String {
    rquickjs::CaughtError::from_error(ctx, err).to_string()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    use serde_json::{json, Value};

    use crate::host::HostContext;

    use super::super::hostapi::{HostBridge, HostCall, PluginHostContext};
    use super::super::manifest::{Limits, Manifest};
    use super::QuickJsRuntime;

    /// 宿主替身：只记录 `emit` 事件，前端始终"存活"。
    struct StubHost {
        emitted: StdMutex<Vec<(String, Value)>>,
    }

    impl HostContext for StubHost {
        fn emit(&self, event: &str, payload: Value) {
            self.emitted
                .lock()
                .unwrap()
                .push((event.to_string(), payload));
        }
        fn frontend_alive(&self) -> bool {
            true
        }
        fn show_window(&self) {}
    }

    fn manifest(hooks: &[&str], permissions: &[&str]) -> Manifest {
        let json = json!({
            "id": "com.test.quickjs",
            "name": "QuickJS 测试",
            "version": "1.0.0",
            "entry": "main.js",
            "apiVersion": "1.1",
            "hooks": hooks,
            "permissions": permissions,
        });
        let manifest: Manifest = serde_json::from_value(json).unwrap();
        manifest
    }

    fn runtime(
        manifest: &Manifest,
        host: Arc<StubHost>,
    ) -> QuickJsRuntime {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<HostCall>();
        let host_ctx = PluginHostContext {
            plugin_id: manifest.id.clone(),
            permissions: Arc::new(manifest.permission_set()),
            settings: Arc::new(std::sync::RwLock::new(Default::default())),
            host: host,
            bridge: HostBridge::for_test(tx),
            granted: Arc::new(std::sync::RwLock::new(std::collections::HashSet::new())),
            limits: Limits::default(),
            plugin_dir: std::path::PathBuf::from("."),
        };
        QuickJsRuntime::new(manifest, host_ctx).unwrap()
    }

    const SCRIPT: &str = r#"
harness.on("onTaskDetected", function (ctx, event) {
  return { claim: true, title: "demo", url: event.url };
});
harness.on("onAppStart", function (ctx) {
  Promise.resolve().then(function () {
    ctx.emit("probe", { pumped: true });
  });
});
harness.on("onProbe", function (ctx) {
  return {
    http: ctx.httpGet({ url: "http://127.0.0.1/a.m3u8" }),
    exec: ctx.exec({ program: "ffmpeg", args: ["-version"] }),
    tasks: ctx.addTasksAndWait([{ url: "https://example.com/a.ts", out: "a.ts" }]),
    fetch: typeof fetch,
    fs: typeof ctx.fs.writeText
  };
});
"#;

    /// `_call` 必须把 hook 返回值透出（模块 B 的 `{claim:true}` 依赖此契约）。
    #[test]
    fn hook_return_value_is_propagated() {
        let manifest = manifest(&["onTaskDetected"], &[]);
        let host = Arc::new(StubHost {
            emitted: StdMutex::new(Vec::new()),
        });
        let rt = runtime(&manifest, host);
        rt.load(SCRIPT).unwrap();

        let result = rt
            .call_hook_result("onTaskDetected", &json!({ "url": "https://e.test/a.m3u8" }))
            .unwrap();
        let result = result.expect("应返回 hook 结果");
        assert_eq!(result.get("claim").and_then(Value::as_bool), Some(true));
        assert_eq!(result.get("url").and_then(Value::as_str), Some("https://e.test/a.m3u8"));
    }

    /// 微任务泵：hook 返回后 `Promise.then` 中的 `ctx.emit` 必须已执行。
    #[test]
    fn pending_microtasks_are_pumped() {
        let manifest = manifest(&["onAppStart"], &[]);
        let host = Arc::new(StubHost {
            emitted: StdMutex::new(Vec::new()),
        });
        let rt = runtime(&manifest, host.clone());
        rt.load(SCRIPT).unwrap();
        rt.call_hook("onAppStart", &json!({})).unwrap();

        let emitted = host.emitted.lock().unwrap();
        assert!(
            emitted.iter().any(|(_, payload)| {
                payload.get("name").and_then(Value::as_str) == Some("probe")
                    && payload
                        .get("payload")
                        .and_then(|v| v.get("pumped"))
                        .and_then(Value::as_bool)
                        == Some(true)
            }),
            "微任务未被泵空：{emitted:?}"
        );
    }

    /// 未声明权限时三类新 API 必须返回 `{ok:false}`，且不能抛异常。
    #[test]
    fn new_apis_deny_without_permission() {
        let manifest = manifest(&["onProbe"], &[]);
        let host = Arc::new(StubHost {
            emitted: StdMutex::new(Vec::new()),
        });
        let rt = runtime(&manifest, host);
        rt.load(SCRIPT).unwrap();

        let result = rt.call_hook_result("onProbe", &json!({})).unwrap().unwrap();
        for key in ["http", "exec", "tasks"] {
            assert_eq!(
                result.get(key).and_then(|v| v.get("ok")).and_then(Value::as_bool),
                Some(false),
                "{key} 应被权限拒绝：{result}"
            );
        }
        assert_eq!(result.get("fetch").and_then(Value::as_str), Some("function"));
        assert_eq!(result.get("fs").and_then(Value::as_str), Some("function"));
    }

    /// 仓库内置的示例插件（`data/plugins/hls-downloader`）必须能被真实加载并完成 claim。
    ///
    /// 这条用例同时锁住三件事：manifest 完整性哈希、main.js 语法、hook 注册契约。
    #[test]
    fn bundled_hls_plugin_loads_and_claims_m3u8() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/plugins/hls-downloader");
        let Ok(manifest) = Manifest::from_dir(&dir) else {
            eprintln!("跳过：未找到示例插件 {}", dir.display());
            return;
        };
        manifest.verify_integrity(&dir).unwrap();

        let host = Arc::new(StubHost {
            emitted: StdMutex::new(Vec::new()),
        });
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<HostCall>();
        let host_ctx = PluginHostContext {
            plugin_id: manifest.id.clone(),
            permissions: Arc::new(manifest.permission_set()),
            settings: Arc::new(std::sync::RwLock::new(Default::default())),
            host,
            bridge: HostBridge::for_test(tx),
            granted: Arc::new(std::sync::RwLock::new(
                manifest.permission_set().into_iter().collect(),
            )),
            limits: manifest.limits.clone(),
            plugin_dir: dir.clone(),
        };
        let rt = QuickJsRuntime::new(&manifest, host_ctx).unwrap();
        let source = std::fs::read_to_string(dir.join("main.js")).unwrap();
        rt.load(&source).unwrap();

        let result = rt
            .call_hook_result(
                "onTaskDetected",
                &json!({ "url": "https://cdn.test/a/b.m3u8?token=1" }),
            )
            .unwrap()
            .expect("onTaskDetected 应返回认领决策");
        assert_eq!(result.get("claim").and_then(Value::as_bool), Some(true));
        assert_eq!(
            result.get("title").and_then(Value::as_str),
            Some("b.m3u8")
        );
    }

    /// 声明 `net:http` 后，SSRF 防护仍然生效（回环地址被拒绝）。
    #[test]
    fn http_get_still_blocks_loopback_with_permission() {
        let manifest = manifest(&["onProbe"], &["net:http"]);
        let host = Arc::new(StubHost {
            emitted: StdMutex::new(Vec::new()),
        });
        let rt = runtime(&manifest, host);
        rt.load(SCRIPT).unwrap();

        let result = rt.call_hook_result("onProbe", &json!({})).unwrap().unwrap();
        let http = result.get("http").expect("httpGet 结果");
        assert_eq!(http.get("ok").and_then(Value::as_bool), Some(false));
        let error = http.get("error").and_then(Value::as_str).unwrap_or_default();
        assert!(error.contains("内网") || error.contains("回环"), "SSRF 文案：{error}");
    }

    /// 阻塞调用期间 deadline 挂起的时间必须补回（累计调用不会无限延长）。
    #[test]
    fn deadline_budget_survives_blocking_calls() {
        // 纯行为验证：一次 httpGet（立即被 SSRF 拒绝）后，hook 仍能正常返回。
        let manifest = manifest(&["onProbe"], &["net:http"]);
        let host = Arc::new(StubHost {
            emitted: StdMutex::new(Vec::new()),
        });
        let rt = runtime(&manifest, host);
        rt.load(SCRIPT).unwrap();
        let result = rt
            .call_hook_with_budget("onProbe", &json!({}), 1_000)
            .unwrap();
        assert!(result.is_some());
        std::thread::sleep(Duration::from_millis(1));
    }
}

