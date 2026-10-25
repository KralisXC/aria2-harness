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
    }
  };
  var ctx = {
    log: function (level, msg) {
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
    // 前置拦截：改写即将添加的任务 URL（需 downloads:intercept 权限）。
    resolveUrl: function (url) {
      return hostResult("resolveUrl", { url: String(url) });
    },
    settings: settings,
    fs: fs
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
        fn(ctx, event);
        return "{\"called\":true,\"ok\":true}";
      } catch (e) {
        var msg = "";
        try { msg = String((e && e.stack) || e); } catch (x) { msg = "unknown"; }
        return JSON.stringify({ called: true, ok: false, error: msg });
      }
    }
  };
  globalThis.harness = harness;
  globalThis.ctx = ctx;
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

        self.context.with(move |ctx| -> Result<(), AppError> {
            // 1) 注入 __harness_host（闭包无生命周期参数，规避 IntoJsFunc 的标注难题）
            let func = Function::new(ctx.clone(), move |op: String, payload: String| -> String {
                host.respond(&op, &payload)
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
        let payload_json = serde_json::to_string(payload).unwrap_or_else(|_| "null".to_string());
        let hook_name = hook.to_string();
        let plugin_id = self.plugin_id.clone();
        let deadline = self.deadline.clone();
        let timeout = self.timeout_ms;

        self.context.with(move |ctx| -> Result<(), AppError> {
            deadline.store(now_ms() + timeout, Ordering::Relaxed);
            let called = (|| -> Result<String, rquickjs::Error> {
                let harness: Object = ctx.globals().get("harness")?;
                let call: Function = harness.get("_call")?;
                call.call((hook_name.clone(), payload_json.clone()))
            })();
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
            if let Ok(value) = serde_json::from_str::<Json>(&raw) {
                if value.get("ok").and_then(|b| b.as_bool()) == Some(false) {
                    let msg = value
                        .get("error")
                        .and_then(|s| s.as_str())
                        .unwrap_or("未知异常");
                    return Err(AppError::PluginRuntime(format!(
                        "[plugin:{plugin_id}] hook `{hook_name}` 抛出异常：{msg}"
                    )));
                }
            }
            Ok(())
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
