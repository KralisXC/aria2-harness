//! Wasmtime 强隔离插件后端（增量 25 · P2-D）。
//!
//! 由 `wasm-runtime` feature 门控（见 `mod.rs` 的 `pub mod wasm_runtime`）：
//! 未开启该 feature 时本文件**完全不参与编译**，manifest 声明 `"engine": "wasm"`
//! 会在 `runtime::build_wasm` 处得到明确的拒绝原因。
//!
//! ## 与 guest 的 ABI 约定
//!
//! guest 必须导出：
//! - `memory`         : 线性内存，宿主读写载荷/结果用
//! - `harness_alloc`  : `(i32 size) -> i32 ptr`，宿主写 JSON 前申请（MVS 为 bump 分配器）
//! - `harness_call`   : `(hook_ptr,hook_len,payload_ptr,payload_len) -> i32`，统一入口
//!
//! guest 可导入（由宿主注入）：
//! - `env.log`        : `(level_ptr,level_len,msg_ptr,msg_len)`，走 `hostapi::respond("log")` 统一通路
//! - `env.host_call`  : `(op_ptr,op_len,payload_ptr,payload_len) -> i32 result_ptr`，
//!                      `op` 与 JS 侧 `ctx.*` 同源，复用同一套权限校验 / 审计 / metrics
//!
//! ## 资源限制（增量 34：fuel + 线性内存双闸门）
//!
//! 1. **燃料（fuel）**：`Config::consume_fuel(true)` + 每次 hook 前
//!    `Store::set_fuel(manifest.limits.fuel_units)`。燃料是**指令级定额**，死循环必然
//!    耗尽 → `Trap::OutOfFuel` 陷落被宿主捕获返回错误，**绝不卡死宿主线程**。
//!    epoch 墙钟兜底留给终态（`timeout_ms` 在 Wasm 后端暂不生效）。
//! 2. **线性内存**：`StoreLimitsBuilder::memory_size(manifest.limits.memory_mb)` +
//!    `trap_on_grow_failure(true)`，经 `Store::limiter` 挂到 store 上。此前
//!    `memory_mb` **被完全忽略**，AI 生成的恶意 guest 可用 `memory.grow` 把宿主机吃干；
//!    现在超限的 `memory.grow` 直接 trap，而非静默返回 -1。
//!
//! 此外，**每一次宿主↔guest 的载荷交换都有硬上限**（[`MAX_EXCHANGE_BYTES`]）：
//! guest 侧任何越界 / 负值 / 超长的 ptr+len 组合，宿主一律**升为 trap**（`Err`），
//! 绝不 panic、绝不读越界、绝不为一个荒谬的请求分配内存。

use std::ops::Range;
use std::sync::OnceLock;

use serde_json::{json, Value};
use wasmtime::{
    AsContext, AsContextMut, Caller, Config, Engine, Extern, Func, Instance, Linker, Memory, Module,
    Store, StoreLimits, StoreLimitsBuilder, Trap,
};

use crate::error::AppError;

use super::hostapi::PluginHostContext;
use super::manifest::Manifest;
use super::runtime::PluginRuntime;

/// 单次「宿主 ↔ guest」载荷交换的硬上限（1 MiB）。
///
/// 宿主写进 guest 的只有 hook 名与 POD JSON（实际几 KB）；guest 回给宿主的只有 op 名与
/// 参数 JSON。任何超出此额度的请求都不是正常插件行为，直接 trap 掉，
/// 避免恶意 / 失控 guest 用 `len = i32::MAX` 之类的手法让宿主 OOM。
const MAX_EXCHANGE_BYTES: usize = 1 << 20;

/// Wasm 后端的 store 数据：宿主上下文 + 线性内存限额。
///
/// `StoreLimits` 不实现 `Clone`，把它**封在 Wasm 后端内部**可以：
/// 1. 让 `PluginHostContext`（hostapi / manager 共用的公共类型）保持零改动；
/// 2. 保证限额只作用于本插件的 store，不会串到 QuickJS 后端。
struct HostState {
    host: PluginHostContext,
    limits: StoreLimits,
}

/// guest 必须导出的名字。
const EXPORT_MEMORY: &str = "memory";
const EXPORT_ALLOC: &str = "harness_alloc";
const EXPORT_CALL: &str = "harness_call";
/// 宿主注入的导入名（module `env`）。
const IMPORT_LOG: &str = "log";
const IMPORT_HOST_CALL: &str = "host_call";
/// `env.log` 里固定使用的宿主 op（与 JS 侧 `ctx.log` 同源）。
const OP_LOG: &str = "log";

/// 进程级共享引擎。
///
/// `Engine` 内部有 `unsafe impl Sync`（wasmtime 49 `engine.rs:58`），可安全跨线程复用；
/// 共享它可省掉每个 wasm 插件的 JIT 初始化开销。创建失败时返回错误文案，
/// 由 `OnceLock` 缓存"失败"这一事实（避免反复初始化）。
fn shared_engine() -> Result<&'static Engine, AppError> {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    if let Some(engine) = ENGINE.get() {
        return Ok(engine);
    }
    let mut config = Config::new();
    // MVS 只开燃料：指令级计量 → 死循环必然耗尽。
    config.consume_fuel(true);
    let engine = Engine::new(&config)
        .map_err(|e| AppError::PluginRuntime(format!("创建 Wasm 引擎失败：{e}")))?;
    let _ = ENGINE.set(engine);
    Ok(ENGINE.get().expect("刚写入的引擎必然存在"))
}

/// Wasmtime 后端运行时（每插件一个实例，跑在插件专属线程上）。
pub struct WasmRuntime {
    /// 共享引擎（`'static`，不参与 drop）。
    engine: &'static Engine,
    plugin_id: String,
    /// 导入表（`env.log` / `env.host_call`），热重载后复用同一份。
    linker: Linker<HostState>,
    /// store 的 data = 宿主上下文 + 线性内存限额：`env.*` 里通过 `caller.data()` 取用。
    store: Store<HostState>,
    instance: Option<Instance>,
    memory: Option<Memory>,
    alloc_func: Option<Func>,
    call_func: Option<Func>,
    /// 单次 hook 调用的燃料预算。
    fuel_units: u64,
}

impl WasmRuntime {
    pub fn new(manifest: &Manifest, host: PluginHostContext) -> Result<Self, AppError> {
        let engine = shared_engine()?;
        let mut linker: Linker<HostState> = Linker::new(engine);
        register_imports(&mut linker)?;

        // 增量 34：把 manifest.limits.memoryMB 真正落到 guest 线性内存上。
        // `trap_on_grow_failure(true)` ⇒ 越限的 `memory.grow` 直接 trap，
        // 而不是静默返回 -1 让 guest 在"分配失败"的假象里继续乱跑。
        let memory_bytes = manifest.limits.memory_mb.max(1).saturating_mul(1024 * 1024);
        let limits = StoreLimitsBuilder::new()
            .memory_size(memory_bytes)
            .trap_on_grow_failure(true)
            .build();

        let mut store = Store::new(
            engine,
            HostState {
                host,
                limits,
            },
        );
        store.limiter(|state| &mut state.limits);

        Ok(Self {
            engine,
            plugin_id: manifest.id.clone(),
            linker,
            store,
            instance: None,
            memory: None,
            alloc_func: None,
            call_func: None,
            fuel_units: manifest.limits.fuel_units,
        })
    }

    /// 把一段字节写进 guest 线性内存（末尾补 NUL），返回 `(ptr, len)`。
    ///
    /// 安全边界：
    /// - 长度先做 [`MAX_EXCHANGE_BYTES`] 闸门，再 `try_into` 成 `i32`（防 `len+1` 溢出）；
    /// - `harness_alloc` 返回的指针经 [`guest_range`] 校验，越界立即报错（**不写入**）。
    fn write_guest(&mut self, bytes: &[u8]) -> Result<(i32, i32), AppError> {
        let alloc_func = self.alloc_func.ok_or_else(|| {
            AppError::PluginRuntime(format!("[plugin:{}] 尚未加载入口产物", self.plugin_id))
        })?;
        if bytes.len() > MAX_EXCHANGE_BYTES {
            return Err(AppError::PluginRuntime(format!(
                "[plugin:{}] 待写入 guest 的载荷过大（{} 字节，上限 {MAX_EXCHANGE_BYTES}）",
                self.plugin_id,
                bytes.len()
            )));
        }
        let len: i32 = bytes.len().try_into().map_err(|_| {
            AppError::PluginRuntime(format!(
                "[plugin:{}] 载荷长度 {} 超出 i32 表示范围",
                self.plugin_id,
                bytes.len()
            ))
        })?;
        // `len + 1`（含 NUL）同样不能溢出。
        let request = len.checked_add(1).ok_or_else(|| {
            AppError::PluginRuntime(format!(
                "[plugin:{}] 载荷长度 {len} + 1 溢出 i32",
                self.plugin_id
            ))
        })?;

        let alloc = alloc_func.typed::<i32, i32>(&self.store).map_err(|e| {
            AppError::PluginRuntime(format!(
                "[plugin:{}] `{EXPORT_ALLOC}` 签名不符：{e}",
                self.plugin_id
            ))
        })?;
        let ptr = alloc.call(&mut self.store, request).map_err(|e| {
            AppError::PluginRuntime(format!(
                "[plugin:{}] 调用 `{EXPORT_ALLOC}` 失败：{}",
                self.plugin_id,
                describe_error(&self.plugin_id, EXPORT_ALLOC, &e)
            ))
        })?;

        let memory = self.memory.ok_or_else(|| {
            AppError::PluginRuntime(format!("[plugin:{}] 缺少线性内存", self.plugin_id))
        })?;
        // 一次性算好 [start, end] 与 NUL 位，全部走 checked 运算。
        let data = memory.data_mut(&mut self.store);
        let (start, end) = checked_span(data.len(), ptr, len).ok_or_else(|| {
            AppError::PluginRuntime(format!(
                "[plugin:{}] 分配器返回越界指针 {ptr}（载荷 {len} 字节，内存 {} 字节）",
                self.plugin_id,
                data.len()
            ))
        })?;
        // end 处必须还能落一个 NUL（`checked_span` 已保证 end 本身在界内，这里再要求 end+1 在界内）。
        if end >= data.len() {
            return Err(AppError::PluginRuntime(format!(
                "[plugin:{}] 分配器返回的区间 {start}..{end} 不足以容纳结尾 NUL（内存 {} 字节）",
                self.plugin_id,
                data.len()
            )));
        }
        data[start..end].copy_from_slice(bytes);
        data[end] = 0;
        Ok((ptr, len))
    }
}

impl PluginRuntime for WasmRuntime {
    fn load_source(&mut self, source: &[u8]) -> Result<(), AppError> {
        // `Module::new` 在开启 `wat` feature 时可直接吃 WAT 文本（探针插件即 WAT）。
        let module = Module::new(self.engine, source).map_err(|e| {
            AppError::PluginRuntime(format!("[plugin:{}] 解析 Wasm 模块失败：{e}", self.plugin_id))
        })?;

        // 实例化可能执行 start 段，先灌燃料防被截断。
        self.store
            .set_fuel(self.fuel_units)
            .map_err(|e| AppError::PluginRuntime(format!("[plugin:{}] 设置燃料失败：{e}", self.plugin_id)))?;
        let instance = self
            .linker
            .instantiate(&mut self.store, &module)
            .map_err(|e| {
                AppError::PluginRuntime(format!(
                    "[plugin:{}] 实例化 Wasm 模块失败：{e}",
                    self.plugin_id
                ))
            })?;

        let memory = instance
            .get_export(&mut self.store, EXPORT_MEMORY)
            .and_then(Extern::into_memory)
            .ok_or_else(|| {
                AppError::PluginRuntime(format!(
                    "[plugin:{}] 缺少导出 `{EXPORT_MEMORY}`",
                    self.plugin_id
                ))
            })?;
        let alloc_func = instance
            .get_export(&mut self.store, EXPORT_ALLOC)
            .and_then(Extern::into_func)
            .ok_or_else(|| {
                AppError::PluginRuntime(format!(
                    "[plugin:{}] 缺少导出 `{EXPORT_ALLOC}`",
                    self.plugin_id
                ))
            })?;
        let call_func = instance
            .get_export(&mut self.store, EXPORT_CALL)
            .and_then(Extern::into_func)
            .ok_or_else(|| {
                AppError::PluginRuntime(format!(
                    "[plugin:{}] 缺少导出 `{EXPORT_CALL}`",
                    self.plugin_id
                ))
            })?;
        // 就地校验签名，避免第一次 hook 调用才发现 ABI 不符。
        alloc_func.typed::<i32, i32>(&self.store).map_err(|e| {
            AppError::PluginRuntime(format!(
                "[plugin:{}] `{EXPORT_ALLOC}` 签名不符：{e}",
                self.plugin_id
            ))
        })?;
        call_func
            .typed::<(i32, i32, i32, i32), i32>(&self.store)
            .map_err(|e| {
                AppError::PluginRuntime(format!(
                    "[plugin:{}] `{EXPORT_CALL}` 签名不符：{e}",
                    self.plugin_id
                ))
            })?;

        self.instance = Some(instance);
        self.memory = Some(memory);
        self.alloc_func = Some(alloc_func);
        self.call_func = Some(call_func);
        Ok(())
    }

    fn invoke(&mut self, hook: &str, payload: &Value) -> Result<(), AppError> {
        let call_func = self.call_func.ok_or_else(|| {
            AppError::PluginRuntime(format!(
                "[plugin:{}] 尚未加载入口，无法调用 hook `{hook}`",
                self.plugin_id
            ))
        })?;
        let payload_json = serde_json::to_vec(payload)?;

        let (hook_ptr, hook_len) = self.write_guest(hook.as_bytes())?;
        let (payload_ptr, payload_len) = self.write_guest(&payload_json)?;

        // 每次 hook 重新灌满燃料：预算是**单次调用定额**，不是插件生命周期总量。
        self.store
            .set_fuel(self.fuel_units)
            .map_err(|e| AppError::PluginRuntime(format!("[plugin:{}] 设置燃料失败：{e}", self.plugin_id)))?;

        let call = call_func
            .typed::<(i32, i32, i32, i32), i32>(&self.store)
            .map_err(|e| {
                AppError::PluginRuntime(format!(
                    "[plugin:{}] `{EXPORT_CALL}` 签名不符：{e}",
                    self.plugin_id
                ))
            })?;
        call.call(
            &mut self.store,
            (hook_ptr, hook_len, payload_ptr, payload_len),
        )
        .map_err(|e| AppError::PluginRuntime(describe_error(&self.plugin_id, hook, &e)))?;
        Ok(())
    }

    fn shutdown(&mut self) {
        // 与 QuickJS 后端语义对齐：先派发一次 `onShutdown`，再释放实例。
        if self.call_func.is_some() {
            if let Err(e) = self.invoke("onShutdown", &json!({})) {
                eprintln!("[plugin:{}] onShutdown 执行失败：{e}", self.plugin_id);
            }
        }
        // 用 `take()` 显式取出并释放实例（同时避免 `instance` 被判为 never-read）。
        let _ = self.instance.take();
        self.call_func = None;
        self.alloc_func = None;
        self.memory = None;
    }
}

/// 注册宿主注入的两个导入（`env.log` / `env.host_call`）。
///
/// 两个闭包都返回 `wasmtime::Result`：`Ok` 正常返回，`Err` 在 wasm 层等价于
/// **自定义 trap**（[wasmtime func.rs:613-658]）。因此 guest 传入越界指针 / 超长长度时，
/// 宿主不会 panic，而是让该次 hook 调用以 trap 收场 —— 与燃料耗尽同一处理路径，
/// 熔断器（连续 3 次失败）自动兜底。
fn register_imports(linker: &mut Linker<HostState>) -> Result<(), AppError> {
    linker
        .func_wrap(
            "env",
            IMPORT_LOG,
            |mut caller: Caller<'_, HostState>,
             lp: i32,
             ll: i32,
             mp: i32,
             ml: i32|
             -> wasmtime::Result<()> {
                let level = read_guest(&mut caller, "env.log.level", lp, ll)?;
                let msg = read_guest(&mut caller, "env.log.msg", mp, ml)?;
                // 与 JS 侧 `ctx.log` 完全同源：走 hostapi 的统一通路（权限/审计/metrics）。
                let payload = json!({ "level": level, "msg": msg }).to_string();
                // `respond` 返回 String（log 分支固定返回空串），此处为 Wasm 导入的返回值，
                // 无 Result 可吞 —— 显式丢弃即可，日志副作用已在 hostapi 内完成。
                let _ = caller.data().host.respond(OP_LOG, &payload);
                Ok(())
            },
        )
        .map_err(|e| AppError::PluginRuntime(format!("注册 `env.{IMPORT_LOG}` 失败：{e}")))?;

    linker
        .func_wrap(
            "env",
            IMPORT_HOST_CALL,
            |mut caller: Caller<'_, HostState>,
             op_ptr: i32,
             op_len: i32,
             p_ptr: i32,
             p_len: i32|
             -> wasmtime::Result<i32> {
                let op = read_guest(&mut caller, "env.host_call.op", op_ptr, op_len)?;
                let payload = read_guest(&mut caller, "env.host_call.payload", p_ptr, p_len)?;
                let result = caller.data().host.respond(&op, &payload);
                write_result(&mut caller, &result)
            },
        )
        .map_err(|e| {
            AppError::PluginRuntime(format!("注册 `env.{IMPORT_HOST_CALL}` 失败：{e}"))
        })?;
    Ok(())
}

/// 把 `(ptr, len)` 归一成 `[start, end)`；越界 / 负值 / 溢出**一律 `None`**。
///
/// 这是宿主侧唯一的指针合法性判据：
/// - `i32` → `usize` 用 `try_from`（负值直接失败，杜绝 `ptr as usize` 的巨值陷阱）；
/// - `ptr + len` 用 `checked_add`（杜绝 64 位上的回绕）；
/// - 最后与**当前**线性内存长度比对。
fn guest_range(mem_len: usize, ptr: i32, len: i32) -> Option<Range<usize>> {
    let len = usize::try_from(len).ok()?;
    let start = usize::try_from(ptr).ok()?;
    let end = start.checked_add(len)?;
    if end > mem_len {
        return None;
    }
    Some(start..end)
}

/// `(ptr, len)` → `(start, end)`，并额外要求 `end` 本身是合法索引
/// （供写入结尾 NUL 使用；即 `end < mem_len`）。
fn checked_span(mem_len: usize, ptr: i32, len: i32) -> Option<(usize, usize)> {
    let range = guest_range(mem_len, ptr, len)?;
    if range.end >= mem_len {
        return None;
    }
    Some((range.start, range.end))
}

/// 从 guest 线性内存读一段 UTF-8。
///
/// 与旧实现的关键差异：**越界不再静默降级为空串，而是升为 trap**。
/// 静默降级会让 guest 的越界 bug 变成"宿主悄悄返回空数据"，排查起来毫无线索；
/// 升为 trap 既让恶意调用立刻失败，也让正常插件的 ABI 错误在开发期就暴露。
fn read_guest<T>(
    caller: &mut Caller<'_, T>,
    what: &str,
    ptr: i32,
    len: i32,
) -> wasmtime::Result<String> {
    // `len == 0` 是约定的"无数据"（guest 常以 `(0, 0)` 表示不传载荷）。
    if len == 0 {
        return Ok(String::new());
    }
    if len < 0 || ptr < 0 {
        return Err(wasmtime::format_err!(
            "{what} 指针非法（负值）：ptr={ptr}, len={len}"
        ));
    }
    let len_u = usize::try_from(len)
        .map_err(|_| wasmtime::format_err!("{what} 长度 {len} 无法表示为 usize"))?;
    if len_u > MAX_EXCHANGE_BYTES {
        return Err(wasmtime::format_err!(
            "{what} 请求读取 {len_u} 字节，超过单次交换上限 {MAX_EXCHANGE_BYTES}"
        ));
    }
    let memory = caller
        .get_export(EXPORT_MEMORY)
        .and_then(Extern::into_memory)
        .ok_or_else(|| wasmtime::format_err!("guest 未导出 `{EXPORT_MEMORY}`，无法读取 {what}"))?;
    let data = memory.data(caller.as_context());
    let range = guest_range(data.len(), ptr, len).ok_or_else(|| {
        wasmtime::format_err!(
            "{what} 越界：ptr={ptr}, len={len}, guest 内存仅 {} 字节",
            data.len()
        )
    })?;
    Ok(String::from_utf8_lossy(&data[range]).into_owned())
}

/// 把宿主返回的字符串回写进 guest 内存（借 guest 自己的分配器），返回指针。
///
/// 约定：返回 `0` 表示"无返回值"；否则指针指向一段 **NUL 结尾**的 UTF-8。
///
/// 安全边界（旧实现的三处缺陷已修复）：
/// 1. `text.len() as i32` 的**截断/溢出** ⇒ 改为 `try_from` + `checked_add`；
/// 2. 分配器返回越界指针时**静默跳过写入却仍返回 ptr** ⇒ 改为直接 trap；
/// 3. 回包长度无上限 ⇒ 加 [`MAX_EXCHANGE_BYTES`] 闸门。
fn write_result<T>(caller: &mut Caller<'_, T>, text: &str) -> wasmtime::Result<i32> {
    if text.is_empty() {
        return Ok(0);
    }
    let bytes = text.as_bytes();
    if bytes.len() > MAX_EXCHANGE_BYTES {
        return Err(wasmtime::format_err!(
            "host_call 回包 {} 字节，超过单次交换上限 {MAX_EXCHANGE_BYTES}",
            bytes.len()
        ));
    }
    let len = i32::try_from(bytes.len())
        .map_err(|_| wasmtime::format_err!("回包长度 {} 超出 i32 表示范围", bytes.len()))?;
    let request = len
        .checked_add(1)
        .ok_or_else(|| wasmtime::format_err!("回包长度 {len} + 1 溢出 i32"))?;

    let alloc = caller
        .get_export(EXPORT_ALLOC)
        .and_then(Extern::into_func)
        .ok_or_else(|| wasmtime::format_err!("guest 未导出 `{EXPORT_ALLOC}`，无法回写结果"))?;
    let typed = alloc
        .typed::<i32, i32>(caller.as_context())
        .map_err(|e| wasmtime::format_err!("`{EXPORT_ALLOC}` 签名不符：{e}"))?;
    let ptr = typed
        .call(&mut *caller, request)
        .map_err(|e| wasmtime::format_err!("回写结果时调用 `{EXPORT_ALLOC}` 失败：{e}"))?;

    let memory = caller
        .get_export(EXPORT_MEMORY)
        .and_then(Extern::into_memory)
        .ok_or_else(|| wasmtime::format_err!("guest 未导出 `{EXPORT_MEMORY}`，无法回写结果"))?;
    let data = memory.data_mut(caller.as_context_mut());
    let (start, end) = checked_span(data.len(), ptr, len).ok_or_else(|| {
        wasmtime::format_err!(
            "`{EXPORT_ALLOC}` 返回越界指针 {ptr}（回包 {len} 字节，guest 内存 {} 字节）",
            data.len()
        )
    })?;
    data[start..end].copy_from_slice(bytes);
    data[end] = 0;
    Ok(ptr)
}

/// 把 wasmtime 错误翻译成对运维友好的文案（燃料耗尽单独识别）。
fn describe_error(plugin_id: &str, hook: &str, err: &wasmtime::Error) -> String {
    if let Some(trap) = err.downcast_ref::<Trap>() {
        if *trap == Trap::OutOfFuel {
            return format!(
                "[plugin:{plugin_id}] hook `{hook}` 燃料耗尽（疑似死循环），已由宿主截断，插件线程未受影响"
            );
        }
        return format!("[plugin:{plugin_id}] hook `{hook}` 触发 Wasm 陷落：{trap}");
    }
    format!("[plugin:{plugin_id}] hook `{hook}` 执行失败：{err}")
}

// ---------------------------------------------------------------------------
// 探针插件自检（无需拉起 Tauri App）
//
// `WasmRuntime` 本体需要 `PluginHostContext`（内含 `AppHandle`），无法在单测里构造；
// 但 ABI 形状与 fuel 截断这两件最容易出错的事，可以在这里用**替身宿主函数**独立验证：
// 用的是与生产完全相同的 `shared_engine()`（同一份 fuel 配置）。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::{Arc, Mutex as StdMutex};

    use crate::host::HostContext;
    use crate::plugin::hostapi::{HostBridge, HostCall};
    use wasmtime::TypedFunc;

    // -----------------------------------------------------------------------
    // 测试替身：宿主上下文 + 假回执桥（无需拉起 App）
    // -----------------------------------------------------------------------

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

    /// 构造一个"带假回执"的 `PluginHostContext`。
    ///
    /// 假回执跑在进程级 tokio 运行时上：`getTask` 固定返回一段可断言的 JSON，
    /// 其它 op 一律空回包 —— 与 `hostapi::serve` 的 getTask / getTasks 契约一致。
    fn stub_host_ctx(
        permissions: &[&str],
        settings: &[(&str, Value)],
    ) -> (PluginHostContext, Arc<StubHost>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<HostCall>();
        crate::rt().spawn(async move {
            while let Some(call) = rx.recv().await {
                let reply = match call.op.as_str() {
                    "getTask" => r#"{"gid":"probe-gid","status":"active"}"#.to_string(),
                    _ => String::new(),
                };
                // 插件侧可能已超时放弃等待，send 失败属预期，显式忽略。
                if call.reply.send(reply).is_err() {
                    continue;
                }
            }
        });
        let host = Arc::new(StubHost {
            emitted: StdMutex::new(Vec::new()),
        });
        let ctx = PluginHostContext {
            plugin_id: "com.test.wasm-ctx".to_string(),
            permissions: Arc::new(permissions.iter().map(|s| (*s).to_string()).collect()),
            settings: Arc::new(std::sync::RwLock::new(
                settings
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), v.clone()))
                    .collect(),
            )),
            host: host.clone(),
            bridge: HostBridge::for_test(tx),
            granted: Arc::new(std::sync::RwLock::new(
                permissions.iter().map(|s| (*s).to_string()).collect(),
            )),
            limits: crate::plugin::manifest::Limits::default(),
            plugin_dir: std::path::PathBuf::from("."),
        };
        (ctx, host)
    }

    /// 起一个"生产同款"的 store + linker（真实 `register_imports` + StoreLimits）。
    fn boot(
        memory_mb: usize,
        permissions: &[&str],
        settings: &[(&str, Value)],
    ) -> (Store<HostState>, Linker<HostState>, Arc<StubHost>) {
        let engine = shared_engine().expect("共享引擎应可创建");
        let mut linker: Linker<HostState> = Linker::new(engine);
        register_imports(&mut linker).expect("生产导入应可注册");
        let (ctx, stub) = stub_host_ctx(permissions, settings);
        let limits = StoreLimitsBuilder::new()
            .memory_size(memory_mb.max(1) * 1024 * 1024)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(
            engine,
            HostState {
                host: ctx,
                limits,
            },
        );
        store.limiter(|state| &mut state.limits);
        (store, linker, stub)
    }

    /// 读 guest 内存里 `ptr` 处的 NUL 结尾字符串（`ptr == 0` 视为"无回包"）。
    fn read_guest_cstr(store: &Store<HostState>, memory: &Memory, ptr: i32) -> Option<String> {
        if ptr == 0 {
            return None;
        }
        let start = usize::try_from(ptr).ok()?;
        let data = memory.data(store);
        if start >= data.len() {
            return None;
        }
        let end = start + data[start..].iter().position(|b| *b == 0)?;
        Some(String::from_utf8_lossy(&data[start..end]).into_owned())
    }

    /// 把 wasmtime 错误的整条 context 链拼成可断言 / 可打印的文本。
    ///
    /// 注意：`wasmtime::Error` **不实现** `std::error::Error`（它是 anyhow 风格的自研
    /// 类型），因此不能走 `Error::source()`；改用它自带的 [`wasmtime::Error::chain`]。
    fn error_text(err: &wasmtime::Error) -> String {
        err.chain()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(" <- ")
    }

    /// 逐项校验 com.test.wasm 的导出/导入形状，并确认死循环 hook 被燃料截断。
    #[test]
    fn probe_plugin_matches_host_abi_and_is_fuel_truncated() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/com.test.wasm/plugin.wat");
        // 探针插件是开发期资产：缺失时直接跳过，避免把目录布局写死进构建流程。
        let Ok(source) = std::fs::read(&path) else {
            eprintln!("跳过：未找到探针插件 {}", path.display());
            return;
        };

        let engine = shared_engine().expect("共享引擎应可创建");
        let module = Module::new(engine, &source).expect("探针 plugin.wat 必须能被 wasmtime 解析");

        // 替身宿主函数：只校验**形状**（参数/返回值类型）与生产一致。
        let mut linker: Linker<()> = Linker::new(engine);
        linker
            .func_wrap("env", IMPORT_LOG, |_: i32, _: i32, _: i32, _: i32| {})
            .expect("env.log 形状应可注册");
        linker
            .func_wrap("env", IMPORT_HOST_CALL, |_: i32, _: i32, _: i32, _: i32| -> i32 { 0 })
            .expect("env.host_call 形状应可注册");

        let mut store = Store::new(engine, ());
        // 关键：开了 `consume_fuel` 之后 Store 初始燃料为 0，**实例化本身也会烧燃料**
        // （数据段初始化是生成代码）。必须先把燃料灌进去，否则一定 trap。
        // 生产路径 `load_source` 同样先 `set_fuel` 再 `instantiate`。
        store.set_fuel(100_000).expect("fuel 应可用");
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("探针插件必须能实例化（导入/导出齐全）");

        let memory = instance
            .get_export(&mut store, EXPORT_MEMORY)
            .and_then(Extern::into_memory)
            .expect("必须导出 memory");
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, EXPORT_ALLOC)
            .expect("必须导出 harness_alloc(i32)->i32");
        let call = instance
            .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, EXPORT_CALL)
            .expect("必须导出 harness_call(i32,i32,i32,i32)->i32");

        // 把 hook 名写进 guest 内存（借 guest 自己的分配器）。
        let hook = b"onDownloadError";
        let ptr = alloc.call(&mut store, hook.len() as i32 + 1).expect("分配应成功");
        let start = ptr as usize;
        memory.data_mut(&mut store)[start..start + hook.len()].copy_from_slice(hook);

        // 灌满燃料后调用：探针该 hook 是死循环，必须被截断而不是挂死。
        store.set_fuel(100_000).expect("fuel 应可用");
        let err = call
            .call(&mut store, (ptr, hook.len() as i32, 0, 0))
            .expect_err("onDownloadError 是死循环，必须被燃料截断");
        assert_eq!(
            err.downcast_ref::<Trap>().copied(),
            Some(Trap::OutOfFuel),
            "燃料耗尽必须映射为 Trap::OutOfFuel，实际：{err}"
        );
    }

    // -----------------------------------------------------------------------
    // (A) 指针判据纯函数：越界 / 负值 / 溢出的语义边界
    // -----------------------------------------------------------------------

    #[test]
    fn guest_range_rejects_negative_and_out_of_bounds() {
        let mem = 64 * 1024;
        // 合法：正好贴住内存尾部（读场景允许 end == mem_len）。
        assert_eq!(guest_range(mem, 0, 8), Some(0..8));
        assert_eq!(guest_range(mem, mem as i32 - 8, 8), Some(mem - 8..mem));
        assert_eq!(guest_range(mem, 0, 0), Some(0..0));
        // 负指针 / 负长度：`try_from` 直接失败，绝不 `as usize` 变成天文数字。
        assert_eq!(guest_range(mem, -1, 8), None);
        assert_eq!(guest_range(mem, 0, -1), None);
        assert_eq!(guest_range(mem, i32::MIN, i32::MIN), None);
        // 越界与溢出。
        assert_eq!(guest_range(mem, mem as i32, 1), None);
        assert_eq!(guest_range(mem, i32::MAX, 1), None);
        assert_eq!(guest_range(mem, 0, i32::MAX), None);
    }

    #[test]
    fn checked_span_reserves_room_for_trailing_nul() {
        let mem = 100;
        // 结尾 NUL 也需要一个字节：end == mem_len 时容不下。
        assert_eq!(checked_span(mem, 0, 100), None);
        assert_eq!(checked_span(mem, 0, 99), Some((0, 99)));
        assert_eq!(checked_span(mem, 98, 1), Some((98, 99)));
        // 空串也要有 NUL 的位置：末尾留一个字节即可（99..99 + NUL@99）。
        assert_eq!(checked_span(mem, 99, 0), Some((99, 99)));
        assert_eq!(checked_span(mem, 100, 0), None);
    }

    // -----------------------------------------------------------------------
    // (B) 完整 ctx 通路：settings.get / notify / getTask 三条全打通
    // -----------------------------------------------------------------------

    /// 打开 `com.test.wasm-ctx` 探针：走**生产同一条** `Manifest::from_dir` →
    /// `verify_integrity` → `WasmRuntime::new` → `load_source` 通道。
    fn open_ctx_probe(
        permissions: &[&str],
        settings: &[(&str, Value)],
    ) -> (WasmRuntime, Arc<StubHost>) {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/com.test.wasm-ctx");
        let path = dir.join("plugin.wat");
        let source = std::fs::read(&path).unwrap_or_else(|e| {
            panic!("读取探针失败 {}：{e}", path.display());
        });

        // 顺带校验随仓库分发的 manifest 是自洽的（手算 SHA-256 很容易写错）。
        let manifest = Manifest::from_dir(&dir).expect("探针 manifest 必须可解析");
        manifest
            .verify_integrity(&dir)
            .expect("探针 manifest 的 SHA-256 必须与 plugin.wat 一致");

        let (ctx, stub) = stub_host_ctx(permissions, settings);
        let mut rt = WasmRuntime::new(&manifest, ctx).expect("WasmRuntime::new 应成功");
        rt.load_source(&source).expect("探针插件必须能加载");
        (rt, stub)
    }

    /// 读 guest 导出的 i32 全局。
    fn global_i32(rt: &mut WasmRuntime, name: &str) -> i32 {
        let instance = rt.instance.expect("实例必须已加载");
        let global = instance
            .get_global(&mut rt.store, name)
            .unwrap_or_else(|| panic!("必须导出 global `{name}`"));
        global.get(&mut rt.store).i32().unwrap_or_else(|| {
            panic!("`{name}` 应为 i32");
        })
    }

    /// 读"全局指针 → guest 内存 NUL 结尾串"这一探针观测出口。
    fn global_cstr(rt: &mut WasmRuntime, name: &str) -> Option<String> {
        let ptr = global_i32(rt, name);
        let memory = rt.memory.expect("实例必须导出 memory");
        read_guest_cstr(&rt.store, &memory, ptr)
    }

    #[test]
    fn wasm_ctx_probe_reaches_settings_notify_and_get_task() {
        let (mut rt, stub) = open_ctx_probe(
            &["notify", "downloads:read"],
            &[("greeting", Value::String("wasm-ctx-ok".to_string()))],
        );

        // --- 单通路 1：settings.get（回包 = 该 key 的 JSON 值）---
        rt.invoke("probeSettings", &json!({}))
            .expect("probeSettings 必须成功");
        assert_eq!(
            global_cstr(&mut rt, "last_settings_ptr").as_deref(),
            Some(r#""wasm-ctx-ok""#),
            "ctx.settings.get 必须把该 key 的 JSON 值回写到 guest 内存"
        );

        // --- 单通路 2：getTask（需 downloads:read；回包经 HostBridge 转发）---
        rt.invoke("probeGetTask", &json!({}))
            .expect("probeGetTask 必须成功");
        assert_eq!(
            global_cstr(&mut rt, "last_result_ptr").as_deref(),
            Some(r#"{"gid":"probe-gid","status":"active"}"#),
            "ctx.getTask 必须把宿主回执逐字节回写到 guest 内存"
        );

        // --- 单通路 3：notify（需 notify 权限；副作用 = 一条 plugin://notify 事件）---
        rt.invoke("probeNotify", &json!({}))
            .expect("probeNotify 必须成功");

        // --- 真实场景：onAppStart（log -> notify -> getTask -> settings.get 串行）---
        rt.invoke("onAppStart", &json!({}))
            .expect("onAppStart 必须成功");
        assert_eq!(
            global_cstr(&mut rt, "last_result_ptr").as_deref(),
            Some(r#"{"gid":"probe-gid","status":"active"}"#),
            "onAppStart 串行链路里 getTask 也应拿到回执"
        );
        assert_eq!(
            global_cstr(&mut rt, "last_settings_ptr").as_deref(),
            Some(r#""wasm-ctx-ok""#),
            "onAppStart 串行链路里 settings.get 也应拿到回包"
        );

        // notify 被调用两次（probeNotify + onAppStart），两次都必须带正确载荷。
        let emitted = stub.emitted.lock().unwrap();
        let notifies: Vec<&Value> = emitted
            .iter()
            .filter(|(event, _)| event == "plugin://notify")
            .map(|(_, payload)| payload)
            .collect();
        assert_eq!(notifies.len(), 2, "notify 应被派发两次：{emitted:?}");
        for payload in notifies {
            assert_eq!(payload["pluginId"], "com.test.wasm-ctx");
            assert_eq!(payload["title"], "Wasm");
            assert_eq!(payload["body"], "ctx online");
        }
    }

    /// 能力闸门对 Wasm 后端同样生效：未声明 `notify` 权限时事件一条都不该发。
    #[test]
    fn wasm_ctx_probe_without_permission_stays_silent() {
        let (mut rt, stub) = open_ctx_probe(&[], &[]);
        rt.invoke("probeNotify", &json!({}))
            .expect("越权调用本身不应 trap（与 JS 侧一致：静默拒绝 + 审计）");
        assert!(
            stub.emitted.lock().unwrap().is_empty(),
            "未授权插件不得向前端派发任何事件"
        );
    }

    /// 探针里的死循环 hook 必须被燃料截断（生产路径：`WasmRuntime::invoke`）。
    #[test]
    fn wasm_ctx_probe_spin_is_fuel_truncated() {
        let (mut rt, _stub) = open_ctx_probe(&[], &[]);
        let err = rt
            .invoke("probeSpin", &json!({}))
            .expect_err("probeSpin 是死循环，必须被燃料截断");
        assert!(
            err.to_string().contains("燃料耗尽"),
            "错误应点明燃料耗尽，实际：{err}"
        );
    }

    // -----------------------------------------------------------------------
    // (C) 恶意 guest 的边界防御：宿主只 trap，绝不 panic / 越界访问
    // -----------------------------------------------------------------------

    /// 越界读：内存在 65536 字节，`ptr=65530 + len=100` 必然越界。
    const MALICIOUS_OOB_READ: &str = r#"
        (module
          (import "env" "host_call" (func $hc (param i32 i32 i32 i32) (result i32)))
          (memory (export "memory") 1)
          (func (export "harness_call") (param i32 i32 i32 i32) (result i32)
            (drop (call $hc (i32.const 65530) (i32.const 100) (i32.const 0) (i32.const 0)))
            (i32.const 0)))
    "#;

    /// 负长度 / 负指针（`as usize` 旧写法会变成天文数字）。
    const MALICIOUS_NEG_LEN: &str = r#"
        (module
          (import "env" "host_call" (func $hc (param i32 i32 i32 i32) (result i32)))
          (memory (export "memory") 1)
          (func (export "harness_call") (param i32 i32 i32 i32) (result i32)
            (drop (call $hc (i32.const 0) (i32.const -1) (i32.const 0) (i32.const 0)))
            (i32.const 0)))
    "#;

    const MALICIOUS_NEG_PTR: &str = r#"
        (module
          (import "env" "host_call" (func $hc (param i32 i32 i32 i32) (result i32)))
          (memory (export "memory") 1)
          (func (export "harness_call") (param i32 i32 i32 i32) (result i32)
            (drop (call $hc (i32.const -1) (i32.const 8) (i32.const 0) (i32.const 0)))
            (i32.const 0)))
    "#;

    /// 荒谬长度：`i32::MAX`，远超 `MAX_EXCHANGE_BYTES`。
    const MALICIOUS_HUGE_LEN: &str = r#"
        (module
          (import "env" "host_call" (func $hc (param i32 i32 i32 i32) (result i32)))
          (memory (export "memory") 1)
          (func (export "harness_call") (param i32 i32 i32 i32) (result i32)
            (drop (call $hc (i32.const 0) (i32.const 2147483647) (i32.const 0) (i32.const 0)))
            (i32.const 0)))
    "#;

    /// 越界回写：分配器被恶意改成"永远返回内存尾部附近"，而回包非空
    /// → 旧实现在此处会静默跳过写入却仍返回指针；新实现必须 trap。
    const MALICIOUS_OOB_WRITE: &str = r#"
        (module
          (import "env" "host_call" (func $hc (param i32 i32 i32 i32) (result i32)))
          (memory (export "memory") 1)
          (data (i32.const 1024) "settings.get")
          (data (i32.const 1040) "{\"key\":\"greeting\"}")
          (func (export "harness_alloc") (param i32) (result i32) (i32.const 65530))
          (func (export "harness_call") (param i32 i32 i32 i32) (result i32)
            (drop (call $hc (i32.const 1024) (i32.const 12) (i32.const 1040) (i32.const 18)))
            (i32.const 0)))
    "#;

    /// 内存炸弹：`memory.grow` 1000 页（64 MB），远超 1 MB 限额。
    const MALICIOUS_MEMORY_BOMB: &str = r#"
        (module
          (memory (export "memory") 1)
          (func (export "harness_call") (param i32 i32 i32 i32) (result i32)
            (drop (memory.grow (i32.const 1000)))
            (i32.const 0)))
    "#;

    /// 实例化即爆：模块声明 1000 页初始内存，超出限额应在 `instantiate` 就被拒。
    const MALICIOUS_INITIAL_MEMORY_BOMB: &str = r#"
        (module (memory (export "memory") 1000))
    "#;

    /// 断言：该恶意模块的 `harness_call` 必须返回 `Err`（wasm trap），并且
    /// 错误链里能看见预期的防御文案。
    #[track_caller]
    fn assert_trapped(source: &str, needle: &str) {
        let engine = shared_engine().expect("共享引擎应可创建");
        let module = Module::new(engine, source).expect("恶意模块本身应能编译");
        let (mut store, linker, _stub) = boot(
            1,
            &["notify", "downloads:read"],
            &[("greeting", Value::String("wasm-ctx-ok".to_string()))],
        );
        store.set_fuel(200_000).expect("fuel 应可用");
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("恶意模块应能实例化（恶意点在调用期）");
        let call = instance
            .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, EXPORT_CALL)
            .expect("必须导出 harness_call");

        store.set_fuel(200_000).expect("fuel 应可用");
        let err = call
            .call(&mut store, (0, 0, 0, 0))
            .expect_err("恶意调用必须被宿主 trap，绝不能成功");
        let text = error_text(&err);
        assert!(
            text.contains(needle),
            "trap 文案应含 `{needle}`，实际错误链：{text}"
        );
    }

    #[test]
    fn malicious_oob_read_is_trapped() {
        assert_trapped(MALICIOUS_OOB_READ, "越界");
    }

    #[test]
    fn malicious_negative_length_is_trapped() {
        assert_trapped(MALICIOUS_NEG_LEN, "非法（负值）");
    }

    #[test]
    fn malicious_negative_pointer_is_trapped() {
        assert_trapped(MALICIOUS_NEG_PTR, "非法（负值）");
    }

    #[test]
    fn malicious_huge_length_is_trapped_before_allocating() {
        assert_trapped(MALICIOUS_HUGE_LEN, "超过单次交换上限");
    }

    #[test]
    fn malicious_oob_write_back_is_trapped() {
        assert_trapped(MALICIOUS_OOB_WRITE, "返回越界指针");
    }

    #[test]
    fn malicious_memory_grow_is_trapped_by_store_limits() {
        let engine = shared_engine().expect("共享引擎应可创建");
        let module = Module::new(engine, MALICIOUS_MEMORY_BOMB).expect("模块应能编译");
        let (mut store, linker, _stub) = boot(1, &[], &[]);
        store.set_fuel(200_000).expect("fuel 应可用");
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("模块应能实例化");
        let call = instance
            .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, EXPORT_CALL)
            .expect("必须导出 harness_call");

        store.set_fuel(200_000).expect("fuel 应可用");
        let err = call
            .call(&mut store, (0, 0, 0, 0))
            .expect_err("越限的 memory.grow 必须 trap，而不是静默返回 -1");
        let text = error_text(&err);
        assert!(
            text.contains("forcing trap when growing memory"),
            "应由 StoreLimits::memory_growing 触发 trap，实际错误链：{text}"
        );
    }

    #[test]
    fn oversize_initial_memory_fails_instantiation() {
        let engine = shared_engine().expect("共享引擎应可创建");
        let module = Module::new(engine, MALICIOUS_INITIAL_MEMORY_BOMB).expect("模块应能编译");
        let (mut store, linker, _stub) = boot(1, &[], &[]);
        store.set_fuel(200_000).expect("fuel 应可用");
        let err = linker
            .instantiate(&mut store, &module)
            .expect_err("1000 页初始内存超出 1 MB 限额，实例化必须失败");
        eprintln!("初始内存超限的实例化错误：{}", error_text(&err));
    }
}
