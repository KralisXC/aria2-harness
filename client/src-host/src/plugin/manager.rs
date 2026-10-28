//! PluginManager：扫描插件目录、加载、维护生命周期、热重载。
//!
//! 每插件 = 一条专属线程 + 一条有界 `sync_channel`；宿主只做 `try_send`。
//!
//! ## 并发模型（增量 21 热重载引入）
//!
//! - `plugins` 与 `subs` 均置于内部可变容器：热重载要在运行期增删 / 替换实例。
//! - **锁序恒定**：`reload_lock` → `plugins` → `registry` / `subs`；严禁反向获取。
//! - 旧线程的 `Shutdown + join` 必须在**不持有任何锁**时执行：join 有界等待 2s，
//!   若在持锁期间等待，会把事件派发路径（读 `subs`）卡死 2s。
//! - **new-before-swap**：新版本（哈希 + 线程 Load）完全就绪后才替换旧实例；
//!   新版本失败时旧实例分毫未动，天然满足"改坏了也能退回上一次可用版本"。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

// `watch` / `unwatch` 由该 trait 提供，必须显式引入作用域。
use notify::Watcher;

use crate::error::AppError;
use crate::host::HostContext;

use super::dispatcher::{self, PluginJob, Subscriber};
use super::holo;
use super::hostapi::{HostBridge, PluginHostContext};
use super::manifest::{ActionSpec, Manifest, RoutingRule, SettingSpec};
use super::plugin_tasks::PluginTask;
use super::registry::Registry;
use super::runtime::{self, PluginRuntime};
use super::task_router::{self, RouteEntry};
use super::QUEUE_CAP;

/// 单插件同步加载的超时上界。
const LOAD_TIMEOUT: Duration = Duration::from_secs(3);
/// 退出时等待插件线程收敛的上界。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(2000);
/// 熔断阈值：连续 N 次 hook 失败即自动禁用（**挂起**语义，绝不 kill 线程）。
const BREAKER_THRESHOLD: usize = 3;
/// 热重载防抖窗口：变更后静默这么长时间才批量重载（合并编辑器保存产生的多个事件）。
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);
/// `onTaskDetected` 的同步回执上界（契约：3 秒内必须返回 claim 决策）。
const DETECT_TIMEOUT: Duration = Duration::from_secs(3);
/// 取消 hook 的预算（清理动作应快速完成）。
const CANCEL_BUDGET_MS: u64 = 5_000;

/// 监听线程的退出标志。退出期由 [`stop_watching`] 置位，监听线程在下一个静默
/// 窗口（≤ `RELOAD_DEBOUNCE`）内自行收敛退出。
static WATCH_STOP: AtomicBool = AtomicBool::new(false);

pub struct PluginManager {
    /// 已加载插件表；热重载需要运行期增删，故置于 `Mutex` 内。
    plugins: StdMutex<Vec<LoadedPlugin>>,
    /// 启用状态的持久化账本（只记录被显式禁用的插件）。
    registry: StdMutex<Registry>,
    /// registry.json 所在目录（即插件根目录）。
    registry_dir: PathBuf,
    /// **订阅者快照的唯一真源**：由 [`Self::refresh_subs`] 重算；
    /// `dispatcher` 每轮循环从该单元读取最新列表（热重载后新实例即时可见）。
    subs: Arc<StdMutex<Vec<Subscriber>>>,
    /// 串行化热重载：避免同一目录的重复事件并发互换造成竞态。
    reload_lock: StdMutex<()>,
    /// 供热重载构建新实例使用（均为廉价 clone 句柄）。
    /// 阶段三：改为宿主抽象（取代原先的 `tauri::AppHandle`）。
    host: Arc<dyn HostContext>,
    bridge: HostBridge,
}

/// 插件管理 UI 的只读视图（序列化给前端）。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    /// 作者（beta11 · A5：卡片与详情展示）。
    pub author: String,
    /// 图标（beta11 · A6：已转成 data URI；读失败 / 超限为 `None`）。
    pub icon: Option<String>,
    /// 入口脚本（相对插件根目录，详情弹窗展示）。
    pub entry: String,
    /// 右键动作声明（beta11 · FF-4：仅已完成任务展示）。
    pub actions: Vec<ActionSpec>,
    /// 运行开关当前值（禁用 = 挂起，线程存活但不再派发事件）。
    pub enabled: bool,
    pub priority: i64,
    pub hooks: Vec<String>,
    pub permissions: Vec<String>,
    /// manifest 申请但**授权快照里没有**的新增权限（非空时禁止直接启用，需前端弹窗确认）。
    pub pending_permissions: Vec<String>,
    /// 是否已被熔断（连续失败自动禁用；用户重新启用即清除标记）。
    pub circuit_broken: bool,
    /// manifest 声明的设置 schema + 当前生效值（无 settings 时为空数组）。
    pub settings: Vec<PluginSettingView>,
}

/// 单个设置项的前端视图（schema + 当前值合并输出，前端无需二次查询）。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSettingView {
    pub key: String,
    pub label: String,
    /// 控件类型：`boolean` / `string` / `select`（序列化为 `type`）。
    #[serde(rename = "type")]
    pub kind: String,
    pub options: Vec<String>,
    pub default: Value,
    pub value: Value,
}

/// 共享组件的前端视图（beta12.2：`data/plugins/.shared/<id>/`）。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    /// 落盘目录（绝对路径，详情展示用）。
    pub dir: String,
    /// 递归统计的文件数（FFmpeg 共享包 = exe + DLL 等）。
    pub file_count: usize,
}

/// 一次成功 claim 的结果（`add_download` 据此创建虚拟任务并投递 `onTaskRun`）。
#[derive(Clone, Debug)]
pub struct TaskClaim {
    pub plugin_id: String,
    /// 插件返回的展示标题；空时由调用方回退到 URL 文件名。
    pub title: String,
}

struct LoadedPlugin {
    id: String,
    name: String,
    version: String,
    description: String,
    /// beta11：manifest 元数据（卡片 / 详情展示）。
    author: String,
    /// 已解析的图标 data URI（加载期算一次，`list()` 不再读盘）。
    icon: Option<String>,
    entry: String,
    actions: Vec<ActionSpec>,
    permissions: Vec<String>,
    /// manifest 声明的设置 schema（前端表单渲染依据）。
    setting_specs: Vec<SettingSpec>,
    /// 运行时设置快照：与 `PluginHostContext.settings` 共享同一 `Arc`，
    /// 保存后立即对 `ctx.settings.get()` 生效，无需重载插件。
    settings: Arc<RwLock<HashMap<String, Value>>>,
    /// 已授权能力快照（与 `PluginHostContext.granted` 共享同一 `Arc`）。
    ///
    /// 与 manifest 的 `permissions`（"申请了什么"）不同，这里记录的是"用户同意了什么"；
    /// `exec:local` 等敏感能力要求两者同时具备。授权 → 原地写穿，无需重载。
    granted: Arc<RwLock<HashSet<String>>>,
    /// 插件所在目录（热重载按目录定位实例：`notify` 给出的是路径而非 id）。
    dir: PathBuf,
    tx: SyncSender<PluginJob>,
    hooks: Arc<HashSet<String>>,
    /// manifest.routing（beta10 任务路由）。
    routing: Vec<RoutingRule>,
    /// 懒激活域名规则（beta12 · 模块 E）；空 = 始终激活。
    activation: Vec<String>,
    /// 当前是否已被全息探针唤醒（仅内存，重启 / 热重载后回到休眠）。
    activated: Arc<AtomicBool>,
    /// 长任务预算（`onTaskRun`）。
    task_timeout_ms: u64,
    /// 运行开关：与 `Subscriber` 共享同一 `Arc`，UI 切换即时生效。
    enabled: Arc<AtomicBool>,
    priority: i64,
    /// 连续失败计数（hook 成功即清零；用于熔断判定）。
    failures: Arc<AtomicUsize>,
    /// 熔断标记（**仅内存**，重启不持久化；用户重新启用即清除）。
    breaker: Arc<AtomicBool>,
    join: StdMutex<Option<JoinHandle<()>>>,
}

impl PluginManager {
    /// 扫描 `plugins/` 并加载全部插件（单个失败只跳过该插件，不影响宿主）。
    pub fn load_all(host: &Arc<dyn HostContext>, bridge: HostBridge) -> Self {
        let dir = super::plugin_root();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("[plugin] 创建插件目录失败（{}）：{e}", dir.display());
        }

        let mut dirs: Vec<PathBuf> = match std::fs::read_dir(&dir) {
            Ok(entries) => entries
                .flatten()
                .map(|entry| entry.path())
                // beta11 · C5：`configs` 是配置目录，点目录是安装 / 备份临时目录，
                // 都不是插件 —— 扫描时直接跳过（否则会被当成"缺 manifest 的插件"）。
                .filter(|path| {
                    path.is_dir()
                        && path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .map(|name| !is_non_plugin_dir_name(name))
                            .unwrap_or(false)
                })
                .collect(),
            Err(e) => {
                eprintln!("[plugin] 读取插件目录失败（{}）：{e}", dir.display());
                Vec::new()
            }
        };
        dirs.sort();

        // 授权账本：加载前先读取，用于恢复启用态 + 检测新增未授权权限。
        let mut registry = Registry::load(&dir);
        let mut changed = false;

        let mut plugins = Vec::new();
        for plugin_dir in dirs {
            match Self::load_one(host, &bridge, &plugin_dir, &mut registry) {
                Ok((plugin, mutated)) => {
                    changed |= mutated;
                    plugins.push(plugin);
                }
                Err(e) => eprintln!("[plugin] 跳过 {}：{e}", plugin_dir.display()),
            }
        }

        // 首次授权 / 新增权限降权都会改动账本，仅在确有变更时落盘。
        if changed {
            if let Err(e) = registry.save(&dir) {
                eprintln!("[plugin] 保存 registry 失败：{e}");
            }
        }

        println!("[plugin] 插件加载完成，共 {} 个", plugins.len());
        let manager = Self {
            plugins: StdMutex::new(plugins),
            registry: StdMutex::new(registry),
            registry_dir: dir.to_path_buf(),
            subs: Arc::new(StdMutex::new(Vec::new())),
            reload_lock: StdMutex::new(()),
            host: host.clone(),
            bridge,
        };
        manager.refresh_subs();
        manager
    }

    /// 重算订阅者快照并写入共享单元（**锁序：plugins → subs**）。
    ///
    /// 只有"插件集合发生变化"（加载 / 热重载 / 关闭）才需要调用；
    /// 单纯启停不需重算——`Subscriber.enabled` 与插件共享同一 `Arc<AtomicBool>`。
    fn refresh_subs(&self) {
        let mut list: Vec<Subscriber> = {
            let plugins = self
                .plugins
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            plugins
                .iter()
                .map(|p| Subscriber {
                    plugin_id: p.id.clone(),
                    priority: p.priority,
                    hooks: p.hooks.clone(),
                    enabled: p.enabled.clone(),
                    activated: p.activated.clone(),
                    tx: p.tx.clone(),
                })
                .collect()
        };
        list.sort_by_key(|s| s.priority);
        *self.subs.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = list;
    }

    /// 共享订阅者单元（交给 `HookDispatcher` 常驻任务，热重载后即时生效）。
    pub fn subs_cell(&self) -> Arc<StdMutex<Vec<Subscriber>>> {
        self.subs.clone()
    }

    /// HookDispatcher 所需的订阅者快照（按 priority 升序）。
    pub fn subscribers(&self) -> Vec<Subscriber> {
        self.subs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 当前启用插件的路由表（按优先级仲裁）。
    fn route_entries(&self) -> Vec<RouteEntry> {
        let plugins = self
            .plugins
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        plugins
            .iter()
            .filter(|plugin| plugin.enabled.load(Ordering::Relaxed))
            .flat_map(|plugin| {
                plugin.routing.iter().map(|rule| RouteEntry {
                    plugin_id: plugin.id.clone(),
                    plugin_priority: plugin.priority,
                    pattern: rule.pattern.clone(),
                    priority: rule.priority,
                    handler: rule.handler.clone(),
                })
            })
            .collect()
    }

    fn plugin_sender(&self, plugin_id: &str) -> Option<(SyncSender<PluginJob>, u64)> {
        let plugins = self
            .plugins
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        plugins
            .iter()
            .find(|plugin| {
                plugin.id == plugin_id
                    && plugin.enabled.load(Ordering::Relaxed)
                    // beta12 · 模块 E：休眠插件不参与认领 / 长任务。
                    && plugin.activated.load(Ordering::Relaxed)
            })
            .map(|plugin| (plugin.tx.clone(), plugin.task_timeout_ms))
    }

    /// 有效域名规则：`manifest.activationEvents.domains` ∪ 用户自定义域名。
    ///
    /// beta12.4：每次匹配都从**运行期设置快照**读取 `custom_domains`，
    /// 因此前端保存后即刻生效（无需重启、无需重载插件）；设置本身与
    /// `data/plugins/configs/<id>.json` 严格一致（见 [`Self::update_settings`]）。
    fn domains_of(plugin: &LoadedPlugin) -> Vec<String> {
        let custom = plugin
            .settings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(holo::SETTING_CUSTOM_DOMAINS)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        holo::merge_domains(&plugin.activation, &custom)
    }

    /// 仅查询某主机被哪些**站点插件**声明（`GET /check`），**不改变激活状态**。
    ///
    /// 与 [`Self::dispatch_holo`] 的差异：这里刻意排除 `activationEvents` 为空的
    /// 始终激活插件，否则扩展会在百度等普通网页上也注入站点悬浮按钮。
    pub fn match_holo(&self, host: &str) -> Vec<String> {
        let plugins = self
            .plugins
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let table: Vec<(String, Vec<String>)> = plugins
            .iter()
            .filter(|plugin| plugin.enabled.load(Ordering::Relaxed))
            .map(|plugin| (plugin.id.clone(), Self::domains_of(plugin)))
            .collect();
        let mut ids: Vec<String> = holo::match_site_plugins(
            host,
            table
                .iter()
                .map(|(id, patterns)| (id.as_str(), patterns.as_slice())),
        )
        .into_iter()
        .map(str::to_string)
        .collect();
        ids.sort();
        ids
    }

    /// 接收全息探针（beta12 · 模块 E）：只唤醒域名命中的插件。
    ///
    /// 未命中 ⇒ 保持休眠；命中 ⇒ 置 `activated=true`，并在插件声明了 `onHolo`
    /// 时投递 `{hook:"onHolo", packet}`。返回命中的插件 id（升序）。
    pub fn dispatch_holo(&self, packet: &holo::HoloPacket) -> Vec<String> {
        let plugins = self
            .plugins
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let table: Vec<(String, Vec<String>)> = plugins
            .iter()
            .filter(|plugin| plugin.enabled.load(Ordering::Relaxed))
            .map(|plugin| (plugin.id.clone(), Self::domains_of(plugin)))
            .collect();
        let mut matched_ids: Vec<String> = holo::match_plugins(
            packet.host.as_str(),
            table
                .iter()
                .map(|(id, patterns)| (id.as_str(), patterns.as_slice())),
        )
        .into_iter()
        .map(str::to_string)
        .collect();
        matched_ids.sort();
        let matched: HashSet<String> = matched_ids.iter().cloned().collect();

        for plugin in plugins.iter() {
            if !matched.contains(plugin.id.as_str()) {
                continue;
            }
            plugin.activated.store(true, Ordering::Relaxed);
            if !plugin.hooks.contains(holo::HOOK_HOLO) {
                continue;
            }
            let payload = json!({ "hook": holo::HOOK_HOLO, "packet": packet });
            match plugin.tx.try_send(PluginJob::Hook {
                hook: holo::HOOK_HOLO.to_string(),
                payload,
            }) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    eprintln!("[plugin:{}] 队列已满，丢弃全息探针", plugin.id);
                    super::audit::log_dropped_event(&plugin.id, holo::HOOK_HOLO);
                    crate::metrics::METRICS
                        .plugin_dropped
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
            }
        }
        matched_ids
    }

    /// 询问路由命中的插件是否接管该 URL（契约：3 秒内返回 `{claim}`）。
    ///
    /// 未命中 / 插件繁忙 / 超时 / 返回 `claim:false` 一律返回 `None`，
    /// 调用方继续走既有的前置拦截链路。
    pub fn claim_task(&self, url: &str, options: &Value) -> Option<TaskClaim> {
        let entries = self.route_entries();
        let route = task_router::match_route(&entries, url)?.clone();
        let (tx, _) = self.plugin_sender(&route.plugin_id)?;

        let payload = json!({
            "hook": route.handler,
            "url": url,
            "options": options,
        });
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        // 非阻塞投递：插件正忙于长任务时直接放行到原有链路，绝不让 IPC 卡满 3 秒。
        if tx
            .try_send(PluginJob::Detect {
                hook: route.handler,
                payload,
                budget_ms: DETECT_TIMEOUT.as_millis() as u64,
                reply: reply_tx,
            })
            .is_err()
        {
            return None;
        }

        match reply_rx.recv_timeout(DETECT_TIMEOUT) {
            Ok(Ok(Some(value))) if value.get("claim").and_then(Value::as_bool) == Some(true) => {
                let title = value
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                Some(TaskClaim {
                    plugin_id: route.plugin_id,
                    title,
                })
            }
            Ok(Ok(_)) => None,
            Ok(Err(e)) => {
                eprintln!(
                    "[plugin:{}] onTaskDetected 失败，回退普通下载链路：{e}",
                    route.plugin_id
                );
                None
            }
            Err(_) => {
                eprintln!(
                    "[plugin:{}] onTaskDetected 超时（{}s），回退普通下载链路",
                    route.plugin_id,
                    DETECT_TIMEOUT.as_secs()
                );
                None
            }
        }
    }

    /// 投递长任务 hook（`onTaskRun`）；预算取 manifest `limits.taskTimeoutMs`。
    pub fn run_plugin_task(
        &self,
        plugin_id: &str,
        hook: &str,
        payload: Value,
    ) -> Result<(), AppError> {
        let (tx, budget_ms) = self.plugin_sender(plugin_id).ok_or_else(|| {
            AppError::InvalidArgs(format!("插件不可用或已禁用：{plugin_id}"))
        })?;
        tx.try_send(PluginJob::TaskHook {
            hook: hook.to_string(),
            payload,
            budget_ms,
        })
        .map_err(|_| AppError::Internal(format!("插件任务队列已满：{plugin_id}")))
    }

    /// 取消虚拟任务：先中断该插件正在等待的批量下载，再投递 `onTaskCancel` 做清理。
    pub fn cancel_plugin_task(&self, task_id: &str) -> Result<PluginTask, AppError> {
        let plugin_id = super::plugin_tasks::plugin_of(task_id)
            .ok_or_else(|| AppError::InvalidArgs(format!("未找到插件任务：{task_id}")))?;
        let task = super::plugin_tasks::get(task_id).ok_or_else(|| {
            AppError::InvalidArgs(format!("未找到插件任务：{task_id}"))
        })?;

        // 中断 `addTasksAndWait`：否则插件线程仍卡在长任务上，取消要等 30 分钟才生效。
        super::tasks::cancel(&plugin_id);
        // beta11 · B4：强杀该插件已 spawn 的 FFmpeg / FFprobe（录制 / 转码）。
        // 临时分片目录的清理仍交给插件的 onTaskCancel（宿主不猜路径）。
        super::exec::kill_plugin(&plugin_id);
        let cancelled = super::plugin_tasks::cancel(task_id).unwrap_or(task);

        if let Some((tx, _)) = self.plugin_sender(&plugin_id) {
            let payload = json!({
                "hook": "onTaskCancel",
                "task": {
                    "id": cancelled.id,
                    "title": cancelled.title,
                    "url": cancelled.url,
                }
            });
            if tx
                .try_send(PluginJob::TaskHook {
                    hook: "onTaskCancel".to_string(),
                    payload,
                    budget_ms: CANCEL_BUDGET_MS,
                })
                .is_err()
            {
                tracing::warn!("取消 hook 未送达（插件队列已满）：plugin={plugin_id}");
            }
        }

        if self.host.frontend_alive() {
            self.host.emit(
                "plugin://task",
                json!({ "pluginId": plugin_id, "task": cancelled }),
            );
        }
        Ok(cancelled)
    }

    /// 加载单个插件；返回 `(插件, 是否改动授权账本)`。
    fn load_one(
        host: &Arc<dyn HostContext>,
        bridge: &HostBridge,
        dir: &Path,
        registry: &mut Registry,
    ) -> Result<(LoadedPlugin, bool), AppError> {
        // L6：先校验完整性，再加载代码
        let manifest = Manifest::from_dir(dir)?;
        manifest.verify_integrity(dir)?;

        // P0 授权快照：对比 manifest.permissions 与已授权快照，阻断静默提权。
        let (enabled, mutated) = Self::decide_enabled(&manifest, registry);

        // 设置快照：manifest 默认值 ← 用户已保存值（只接受 schema 中声明的 key）。
        //
        // beta11 · C3：优先读 `plugins/configs/<id>.json`；旧版本把设置存在
        // registry.json 里，读不到配置文件时回退账本（兼容升级后的存量用户）。
        let stored = Self::read_config(&manifest.id)
            .unwrap_or_else(|| registry.settings_of(&manifest.id));
        let settings = merge_settings(&manifest, &stored);
        // 授权快照：首次加载由 `decide_enabled` 写入全部权限，这里读到的即最终快照。
        let granted: HashSet<String> = registry
            .record(&manifest.id)
            .map(|rec| rec.granted_permissions.iter().cloned().collect())
            .unwrap_or_default();

        let plugin = Self::build_plugin(host, bridge, dir, &manifest, enabled, settings, granted)?;
        println!(
            "[plugin] 已加载 {} v{}（{}，优先级 {}，hooks {} 个）{}",
            manifest.name,
            manifest.version,
            manifest.id,
            manifest.priority,
            manifest.hooks.len(),
            if enabled { "" } else { " — 已禁用（挂起）" }
        );
        Ok((plugin, mutated))
    }

    /// 授权决策（纯账本逻辑，供首次加载与热重载共用）。
    ///
    /// 返回 `(是否启用, 是否改动账本)`；调用方需在持有 registry 锁时调用。
    fn decide_enabled(manifest: &Manifest, registry: &mut Registry) -> (bool, bool) {
        let requested = manifest.permissions.clone();
        match registry.record(&manifest.id).cloned() {
            // 首次加载：自动建立授权快照并启用。
            None => {
                registry.insert(&manifest.id, true, &requested);
                println!(
                    "[plugin][授权] {} 首次加载，已授权权限：[{}]",
                    manifest.id,
                    requested.join("、")
                );
                (true, true)
            }
            Some(rec) => {
                let added = registry.new_permissions(&manifest.id, &requested);
                if added.is_empty() {
                    (rec.enabled, false)
                } else {
                    // 新增未授权权限：默认禁用并要求用户重新授权（保守策略）。
                    eprintln!(
                        "[plugin][安全][严重] {}（{}）申请了【新增未授权权限】：{}；\n    \
                         已默认禁用。请在插件管理中确认后重新启用（启用即视为重新授权）。",
                        manifest.name,
                        manifest.id,
                        added.join("、")
                    );
                    // 保持原授权快照不变（不自动提权），并强制禁用。
                    registry.set_enabled(&manifest.id, false);
                    (false, true)
                }
            }
        }
    }

    /// 构建一个插件实例（专属线程 + 有界队列 + 同步 Load 校验）。
    ///
    /// 无锁、可被首次加载与热重载共用；失败时**自行回收**半成品线程，不留孤儿。
    fn build_plugin(
        host: &Arc<dyn HostContext>,
        bridge: &HostBridge,
        dir: &Path,
        manifest: &Manifest,
        enabled: bool,
        settings: HashMap<String, Value>,
        granted: HashSet<String>,
    ) -> Result<LoadedPlugin, AppError> {
        let entry_path = dir.join(&manifest.entry);
        // 按 engine 分流读取：JS 走 UTF-8 文本，Wasm 走二进制（增量 25）。
        let source = runtime::read_entry(&manifest.engine, &entry_path)?;

        let settings = Arc::new(RwLock::new(settings));
        let granted = Arc::new(RwLock::new(granted));
        let host_ctx = PluginHostContext {
            plugin_id: manifest.id.clone(),
            permissions: Arc::new(manifest.permission_set()),
            granted: granted.clone(),
            settings: settings.clone(),
            host: host.clone(),
            bridge: bridge.clone(),
            limits: manifest.limits.clone(),
            plugin_dir: dir.to_path_buf(),
        };

        // 专属线程 + 有界队列
        let (tx, rx) = mpsc::sync_channel::<PluginJob>(QUEUE_CAP);
        let thread_id = manifest.id.clone();
        let thread_manifest = manifest.clone();
        // 熔断共享状态：`enabled` 决定是否派发；`failures` 连续失败计数；`breaker` 标记运行期
        // 自动禁用（仅内存，重启不持久化）。
        let enabled_flag = Arc::new(AtomicBool::new(enabled));
        let failures = Arc::new(AtomicUsize::new(0));
        let breaker = Arc::new(AtomicBool::new(false));
        // beta12 · 模块 E：无 activationEvents ⇒ 始终保持激活（既有插件零感知）。
        let activation = manifest.activation_events.domains.clone();
        let activated_flag = Arc::new(AtomicBool::new(activation.is_empty()));
        let thread_enabled = enabled_flag.clone();
        let thread_failures = failures.clone();
        let thread_breaker = breaker.clone();
        let join = std::thread::Builder::new()
            .name(format!("plugin-{thread_id}"))
            .spawn(move || {
                plugin_thread(
                    thread_id,
                    thread_manifest,
                    host_ctx,
                    rx,
                    thread_enabled,
                    thread_failures,
                    thread_breaker,
                )
            })
            .map_err(|e| AppError::PluginLoad(format!("启动插件线程失败：{e}")))?;

        // 同步载入：校验脚本可解析 + manifest 声明的 hook 已注册
        let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<(), AppError>>(1);
        if tx
            .send(PluginJob::Load {
                source,
                reply: reply_tx,
            })
            .is_err()
        {
            // 线程已退出（极罕见）：作废队列并回收句柄
            drop(tx);
            wait_thread(join, SHUTDOWN_TIMEOUT);
            return Err(AppError::PluginLoad("插件线程已退出".to_string()));
        }
        match reply_rx.recv_timeout(LOAD_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                abort_thread(tx, join);
                return Err(e);
            }
            Err(_) => {
                abort_thread(tx, join);
                return Err(AppError::PluginLoad("插件加载超时".to_string()));
            }
        }

        Ok(LoadedPlugin {
            id: manifest.id.clone(),
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            description: manifest.description.clone(),
            author: manifest.author.clone(),
            // beta11 · A6：图标在此一次性转 data URI（读失败 / 超 256KB → None）。
            icon: icon_data_uri(dir, manifest.icon.as_deref()),
            entry: manifest.entry.clone(),
            actions: manifest.actions.clone(),
            permissions: manifest.permissions.clone(),
            setting_specs: manifest.settings.clone(),
            settings,
            granted,
            dir: dir.to_path_buf(),
            tx,
            hooks: Arc::new(manifest.hooks.iter().cloned().collect()),
            routing: manifest.routing.clone(),
            activation,
            activated: activated_flag,
            task_timeout_ms: manifest.limits.task_timeout_ms,
            // 启用态取自授权账本（首次加载=启用；新增未授权权限=禁用）。
            enabled: enabled_flag,
            priority: manifest.priority,
            failures,
            breaker,
            join: StdMutex::new(Some(join)),
        })
    }

    /// 插件管理 UI 的只读列表。
    pub fn list(&self) -> Vec<PluginInfo> {
        // 锁序：plugins → registry（读取授权账本以计算"待授权权限"）。
        let plugins = self
            .plugins
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let registry = self
            .registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        plugins
            .iter()
            .map(|p| {
                let mut hooks: Vec<String> = p.hooks.iter().cloned().collect();
                hooks.sort();
                PluginInfo {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    version: p.version.clone(),
                    description: p.description.clone(),
                    author: p.author.clone(),
                    icon: p.icon.clone(),
                    entry: p.entry.clone(),
                    actions: p.actions.clone(),
                    enabled: p.enabled.load(Ordering::Relaxed),
                    priority: p.priority,
                    hooks,
                    permissions: p.permissions.clone(),
                    pending_permissions: registry.new_permissions(&p.id, &p.permissions),
                    circuit_broken: p.breaker.load(Ordering::Relaxed),
                    settings: p
                        .setting_specs
                        .iter()
                        .map(|spec| {
                            let current = p
                                .settings
                                .read()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            PluginSettingView {
                                key: spec.key.clone(),
                                label: if spec.label.trim().is_empty() {
                                    spec.key.clone()
                                } else {
                                    spec.label.clone()
                                },
                                kind: spec.kind.clone(),
                                options: spec.options.clone(),
                                default: spec.default.clone(),
                                value: current
                                    .get(&spec.key)
                                    .cloned()
                                    .unwrap_or_else(|| spec.default.clone()),
                            }
                        })
                        .collect(),
                }
            })
            .collect()
    }

    /// 共享组件列表（beta12.2）：扫描 `.shared/<id>/manifest.json`。
    ///
    /// 解析失败 / `type` 非 `shared` 的目录直接跳过（不报错：点目录是安装 / 备份残留）。
    pub fn list_shared(&self) -> Vec<SharedInfo> {
        let root = super::install::shared_root();
        let mut dirs: Vec<PathBuf> = match std::fs::read_dir(&root) {
            Ok(entries) => entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect(),
            Err(_) => return Vec::new(),
        };
        dirs.sort();

        let mut out = Vec::new();
        for dir in dirs {
            let name = dir.file_name().and_then(|value| value.to_str()).unwrap_or("");
            if name.starts_with('.') || !dir.join("manifest.json").is_file() {
                continue;
            }
            let manifest = match Manifest::from_dir(&dir) {
                Ok(manifest) => manifest,
                Err(_) => continue,
            };
            if manifest.plugin_type != super::manifest::TYPE_SHARED {
                continue;
            }
            out.push(SharedInfo {
                id: manifest.id,
                name: manifest.name,
                version: manifest.version,
                description: manifest.description,
                author: manifest.author,
                dir: dir.to_string_lossy().into_owned(),
                file_count: count_files(&dir),
            });
        }
        out
    }

    /// 卸载共享组件（beta12.2）：删 `.shared/<id>` 目录并通知前端刷新。
    ///
    /// 路径安全：`canonicalize` 后必须仍是 `.shared` 的子目录（防目录穿越 / 符号链接）。
    pub fn uninstall_shared(&self, id: &str) -> Result<(), AppError> {
        if !super::manifest::is_valid_plugin_id(id) {
            return Err(AppError::InvalidArgs(format!("非法共享包 id：{id}")));
        }
        let root = super::install::shared_root();
        let root_real = std::fs::canonicalize(&root).map_err(|e| {
            AppError::fs(format!("共享包目录不存在（{}）", root.display()), e)
        })?;
        let target = root.join(id);
        let target_real = std::fs::canonicalize(&target).map_err(|e| {
            AppError::fs(format!("共享组件不存在（{}）", target.display()), e)
        })?;
        if target_real == root_real || !target_real.starts_with(&root_real) {
            return Err(AppError::InvalidArgs(format!(
                "共享包路径越界，拒绝卸载：{id}"
            )));
        }
        std::fs::remove_dir_all(&target_real)
            .map_err(|e| AppError::fs(format!("删除共享包失败（{}）", target.display()), e))?;

        self.host.emit(
            "plugin://reloaded",
            json!({ "id": id, "status": "uninstalled", "shared": true }),
        );
        println!("[plugin] 已卸载共享组件 {id}");
        Ok(())
    }

    /// 运行时启停：置位共享的 `AtomicBool`（dispatch 阶段即时生效，挂起语义），
    /// 并把新状态与授权快照落盘到 `registry.json`。
    ///
    /// 语义：**启用即视为"重新授权"** —— 把当前 manifest 权限写入授权快照，
    /// 从而清除"新增未授权权限"的降权标记（用户已确认）。
    ///
    /// P0-A 授权可解释性（增量 20）：若该插件存在**新增未授权权限**，直接启用会被拒绝
    /// （返回 `Err`）—— 必须由前端弹窗展示权限后走 [`Self::grant_permissions`]，杜绝静默提权。
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), AppError> {
        // 仅在 plugins 锁内取快照，随后释放；避免持锁做落盘。
        let (flag, breaker, failures, manifest_perms, name, granted) = {
            let plugins = self
                .plugins
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let plugin = plugins
                .iter()
                .find(|p| p.id == id)
                .ok_or_else(|| AppError::InvalidArgs(format!("未找到插件：{id}")))?;
            (
                plugin.enabled.clone(),
                plugin.breaker.clone(),
                plugin.failures.clone(),
                plugin.permissions.clone(),
                plugin.name.clone(),
                plugin.granted.clone(),
            )
        };

        if enabled {
            let pending = {
                let guard = self
                    .registry
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.new_permissions(id, &manifest_perms)
            };
            if !pending.is_empty() {
                return Err(AppError::Internal(format!(
                    "该插件申请了新增权限（{}），需用户确认授权后方可启用",
                    pending.join("、")
                )));
            }
        }

        flag.store(enabled, Ordering::Relaxed);
        // beta10：禁用立即取消该插件正在进行的批量任务；重新启用则清除取消标记。
        if enabled {
            super::tasks::clear_cancel(id);
        } else {
            super::tasks::cancel(id);
            super::plugin_tasks::cancel_all_for_plugin(id);
            // beta11 · B4：禁用即挂起 —— 一并强杀该插件正在跑的 FFmpeg。
            super::exec::kill_plugin(id);
        }
        // 用户任一启停操作都视为"给插件一次重新机会"：清空熔断标记与连续失败计数。
        breaker.store(false, Ordering::Relaxed);
        failures.store(0, Ordering::Relaxed);

        // 持久化：先更新内存账本，再原子写回磁盘；落盘失败不阻断本次切换。
        {
            let mut guard = self
                .registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.set_enabled(id, enabled);
            if enabled {
                guard.grant(id, &manifest_perms);
                // 授权快照同步写穿到运行时：`ctx.exec` 的 `exec:local` 判定立即可见，
                // 不必重载插件（用户确认即生效）。
                *granted.write().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    manifest_perms.iter().cloned().collect();
            }
            if let Err(e) = guard.save(&self.registry_dir) {
                eprintln!("[plugin] 持久化 registry 失败：{e}");
            }
        }

        println!(
            "[plugin] {} 已{}",
            name,
            if enabled { "启用" } else { "禁用（挂起）" }
        );
        Ok(())
    }

    /// 用户在前端权限确认弹窗中**显式确认**后：强制刷新授权快照并启用该插件。
    ///
    /// 与 [`Self::set_enabled`] 的区别：跳过"新增未授权权限"拦截，直接授权并启用
    /// —— 这正是 P0-A 授权可解释性的闭环动作（用户已看清权限清单并同意）。
    pub fn grant_permissions(&self, id: &str) -> Result<(), AppError> {
        let (flag, breaker, failures, manifest_perms, name, granted) = {
            let plugins = self
                .plugins
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let plugin = plugins
                .iter()
                .find(|p| p.id == id)
                .ok_or_else(|| AppError::InvalidArgs(format!("未找到插件：{id}")))?;
            (
                plugin.enabled.clone(),
                plugin.breaker.clone(),
                plugin.failures.clone(),
                plugin.permissions.clone(),
                plugin.name.clone(),
                plugin.granted.clone(),
            )
        };

        flag.store(true, Ordering::Relaxed);
        super::tasks::clear_cancel(id);
        breaker.store(false, Ordering::Relaxed);
        failures.store(0, Ordering::Relaxed);

        {
            let mut guard = self
                .registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.grant(id, &manifest_perms);
            guard.set_enabled(id, true);
            // 与 `set_enabled` 同理：授权快照即时写穿，`exec:local` 无需重载即生效。
            *granted.write().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                manifest_perms.iter().cloned().collect();
            if let Err(e) = guard.save(&self.registry_dir) {
                eprintln!("[plugin] 持久化 registry 失败：{e}");
            }
        }

        println!(
            "[plugin] {} 已确认授权并启用（授权快照已刷新：[{}]）",
            name,
            manifest_perms.join("、")
        );
        Ok(())
    }

    /// 保存插件设置：校验 schema → 写 `plugins/configs/<id>.json` → **热更新运行时快照**。
    ///
    /// 红线：
    /// - 只接受 manifest.settings 中声明过的 key，杜绝插件/前端写入任意垃圾数据；
    /// - 类型与 select 候选值必须匹配，否则拒绝整次保存（不做部分写入）；
    /// - 落盘成功后才更新运行时，保证 `ctx.settings.get()` 与磁盘绝对一致；
    /// - 持锁范围：快照 → 释放；配置文件落盘；最后写 settings 单元。
    pub fn update_settings(
        &self,
        id: &str,
        values: HashMap<String, Value>,
    ) -> Result<(), AppError> {
        let (name, specs, cell, current) = {
            let plugins = self
                .plugins
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let plugin = plugins
                .iter()
                .find(|p| p.id == id)
                .ok_or_else(|| AppError::InvalidArgs(format!("未找到插件：{id}")))?;
            let current = plugin
                .settings
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            (
                plugin.name.clone(),
                plugin.setting_specs.clone(),
                plugin.settings.clone(),
                current,
            )
        };

        for (key, value) in &values {
            let spec = specs.iter().find(|s| &s.key == key).ok_or_else(|| {
                AppError::InvalidArgs(format!("插件 {id} 未声明设置项：{key}"))
            })?;
            match spec.kind.as_str() {
                "boolean" if value.is_boolean() => {}
                // `text` / `textarea` 与 `string` 同一套类型校验（前端只换控件）。
                "string" | "textarea" | "text" if value.is_string() => {}
                // 1.0.0-rc1 · 模块二：`number` 只接受 JSON 数字（字符串形式的数字同样拒绝）。
                "number" if value.is_number() => {}
                "select" => {
                    let text = value.as_str().ok_or_else(|| {
                        AppError::InvalidArgs(format!("设置项 {key} 必须为字符串"))
                    })?;
                    if !spec.options.iter().any(|option| option == text) {
                        return Err(AppError::InvalidArgs(format!(
                            "设置项 {key} 的取值 `{text}` 不在候选中：{}",
                            spec.options.join("、")
                        )));
                    }
                }
                "boolean" => {
                    return Err(AppError::InvalidArgs(format!(
                        "设置项 {key} 必须为布尔值"
                    )));
                }
                other => {
                    return Err(AppError::InvalidArgs(format!(
                        "设置项 {key} 的类型 `{other}` 与提交值不匹配"
                    )));
                }
            }
        }

        // 全量快照：以当前值为底，覆盖本次提交项。未提交项保持原值（前端只提交改动也安全）。
        let mut next = current;
        for (key, value) in values {
            next.insert(key, value);
        }

        {
            // beta11 · C4：设置写 `plugins/configs/<id>.json`（tmp + rename 原子替换），
            // 不再写 registry.settings（该字段仅保留读取兼容）。
            Self::write_config(id, &next)?;
        }

        *cell.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = next;

        println!("[plugin] {} 的设置已更新（{} 项）", name, specs.len());
        Ok(())
    }

    /// 插件配置文件：`data/plugins/configs/<id>.json`（beta11 · C2）。
    fn config_file(id: &str) -> PathBuf {
        crate::paths::plugin_configs_dir().join(format!("{id}.json"))
    }

    /// 读取已保存设置；id 非法 / 文件缺失 / 解析失败一律 `None`（调用方回退 registry）。
    fn read_config(id: &str) -> Option<HashMap<String, Value>> {
        if !super::manifest::is_valid_plugin_id(id) {
            return None;
        }
        let text = std::fs::read_to_string(Self::config_file(id)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// 写入设置：`create_dir_all(configs)` → `<id>.json.tmp` → `rename` 原子替换。
    fn write_config(id: &str, values: &HashMap<String, Value>) -> Result<(), AppError> {
        if !super::manifest::is_valid_plugin_id(id) {
            return Err(AppError::InvalidArgs(format!("非法插件 id：{id}")));
        }
        let dir = crate::paths::plugin_configs_dir();
        std::fs::create_dir_all(&dir).map_err(|e| {
            AppError::fs(format!("创建插件配置目录失败（{}）", dir.display()), e)
        })?;
        let path = Self::config_file(id);
        let tmp = dir.join(format!("{id}.json.tmp"));
        let text = serde_json::to_string_pretty(values)?;
        std::fs::write(&tmp, text).map_err(|e| {
            AppError::fs(format!("写入插件配置失败（{}）", tmp.display()), e)
        })?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| AppError::fs(format!("替换插件配置失败（{}）", path.display()), e))
    }

    /// 删除设置文件（卸载时调用；文件不存在视为成功）。
    fn delete_config(id: &str) {
        if !super::manifest::is_valid_plugin_id(id) {
            return;
        }
        let path = Self::config_file(id);
        if let Err(e) = std::fs::remove_file(&path) {
            if path.exists() {
                tracing::warn!("删除插件配置失败（{}）：{e}", path.display());
            }
        }
    }

    /// 卸载插件（beta11 · B13/B14）：摘实例 → 关闭线程（含强杀）→ 删目录 →
    /// 删配置 → 清授权账本 → 通知前端。
    ///
    /// 顺序红线：先摘实例再 join（join 有界等待，**不持任何锁**），避免卸载过程中
    /// 仍有事件派发进旧插件；删目录失败时直接返回错误（配置与账本保持原样，可重试）。
    pub fn uninstall(&self, id: &str) -> Result<(), AppError> {
        if !super::manifest::is_valid_plugin_id(id) {
            return Err(AppError::InvalidArgs(format!("非法插件 id：{id}")));
        }

        let removed = {
            let mut plugins = self
                .plugins
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            plugins
                .iter()
                .position(|plugin| plugin.id == id)
                .map(|index| plugins.remove(index))
        };
        let dir = match removed {
            Some(plugin) => {
                let dir = plugin.dir.clone();
                // shutdown_one 内含 kill_plugin + 有界 join（此刻不持任何锁）。
                shutdown_one(plugin);
                dir
            }
            // 实例不在内存（加载失败 / 尚未加载）：按 id 推导落盘目录。
            None => super::install::plugin_dir_for(id),
        };
        self.refresh_subs();

        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| {
                AppError::fs(format!("删除插件目录失败（{}）", dir.display()), e)
            })?;
        }

        Self::delete_config(id);
        {
            let mut registry = self
                .registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            registry.remove(id);
            if let Err(e) = registry.save(&self.registry_dir) {
                eprintln!("[plugin] 持久化 registry 失败：{e}");
            }
        }

        // 沿用热重载通道通知前端（status=uninstalled），前端据此重拉列表。
        self.host
            .emit("plugin://reloaded", json!({ "id": id, "status": "uninstalled" }));
        println!("[plugin] 已卸载 {id}");
        Ok(())
    }

    /// 热重载单个插件目录（**new-before-swap**：新版本完全就绪后才替换旧实例）。
    ///
    /// 成功返回新实例的 manifest id。失败返回原因，且**旧实例保持原样可用**。
    ///
    /// 顺序：
    /// 1. 解析 + 完整性校验（哈希不符即失败，旧实例分毫未动）；
    /// 2. 授权决策（新增未授权权限 → 置待授权并**不自动运行**）；
    /// 3. 构建新实例（新线程 + 同步 Load；语法错误在此暴露）；
    /// 4. 原子替换列表项并刷新订阅者快照（锁序：plugins → subs）；
    /// 5. 关闭旧线程（**不持任何锁**，避免把事件派发路径卡住）；
    /// 6. 审计 + 通知前端。
    pub fn reload_dir(&self, dir: &Path) -> Result<String, AppError> {
        // 串行化：避免同一目录的重复事件并发互换。
        let _serial = self
            .reload_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // 1. 解析 + 哈希校验（无锁；失败直接返回 —— 旧版本继续跑）
        let manifest = Manifest::from_dir(dir)?;
        manifest.verify_integrity(dir)?;
        let id = manifest.id.clone();

        // 2. 授权决策（仅持 registry 锁）
        //
        // beta11 · C3：设置优先读 `configs/<id>.json`（热重载不覆盖用户配置），
        // 读不到（旧版本 / 从未保存过）再回退 registry.settings。
        let stored_config = Self::read_config(&manifest.id);
        let (enabled, settings, granted) = {
            let mut registry = self
                .registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let (enabled, mutated) = Self::decide_enabled(&manifest, &mut registry);
            if mutated {
                if let Err(e) = registry.save(&self.registry_dir) {
                    eprintln!("[plugin] 持久化 registry 失败：{e}");
                }
            }
            let stored = stored_config
                .clone()
                .unwrap_or_else(|| registry.settings_of(&manifest.id));
            let settings = merge_settings(&manifest, &stored);
            let granted: HashSet<String> = registry
                .record(&manifest.id)
                .map(|rec| rec.granted_permissions.iter().cloned().collect())
                .unwrap_or_default();
            (enabled, settings, granted)
        };

        // 3. 构建新实例（无锁；内含线程 spawn + 3s Load 超时，失败即回收半成品线程）
        let new_plugin = Self::build_plugin(
            &self.host,
            &self.bridge,
            dir,
            &manifest,
            enabled,
            settings,
            granted,
        )?;

        // 4. 原子替换（锁序：plugins → subs）
        let old = {
            let mut plugins = self
                .plugins
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let replaced = plugins
                .iter()
                .position(|p| p.dir.as_path() == dir || p.id == id)
                .map(|index| plugins.remove(index));
            plugins.push(new_plugin);
            drop(plugins);
            self.refresh_subs();
            replaced
        };

        // 5. 关闭旧线程（此刻不持任何锁）
        let existed = old.is_some();
        if let Some(prev) = old {
            shutdown_one(prev);
        }
        // beta10：热重载后新实例沿用同一 id，必须清掉 shutdown_one 设置的取消标记。
        super::tasks::clear_cancel(&id);

        // 6. 审计 + 通知前端
        let status = if !enabled {
            "disabled"
        } else if existed {
            "reloaded"
        } else {
            "loaded"
        };
        super::audit::log_reload_ok(&id);
        self.host
            .emit("plugin://reloaded", json!({ "id": id.clone(), "status": status }));
        println!("[plugin][热重载] {id} → {status}");
        Ok(id)
    }

    /// 宿主启动完成，向所有插件广播一次 onAppStart。
    pub fn notify_app_start(&self) {
        let subs = self.subscribers();
        dispatcher::dispatch(&subs, "onAppStart", json!({ "hook": "onAppStart" }));
    }

    /// 优雅退出：给插件 onShutdown 机会，再有界等待线程收敛（超时则分离）。
    pub fn shutdown(&self) {
        // 一次性摘空插件表：关闭期间不再向任何插件派发事件。
        let drained: Vec<LoadedPlugin> = {
            let mut plugins = self
                .plugins
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *plugins)
        };
        self.refresh_subs();

        for plugin in drained {
            println!("[plugin] 正在关闭 {}", plugin.name);
            shutdown_one(plugin);
        }
    }
}

/// 插件图标上限（beta11 · A6）：超过即视为无效，避免把大图塞进 IPC。
const ICON_MAX_BYTES: u64 = 256 * 1024;

/// 插件根目录下的"非插件"目录名：`configs`（配置）与点目录（安装 / 备份临时目录）。
fn is_non_plugin_dir_name(name: &str) -> bool {
    name == "configs" || name.starts_with('.')
}

/// 递归统计目录内文件数（beta12.2：共享组件卡片展示 `fileCount`）。
fn count_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += count_files(&path);
        } else if path.is_file() {
            total += 1;
        }
    }
    total
}

/// 图标路径 -> data URI（beta11 · A6）。
///
/// 约束：只认常见位图 / 矢量后缀；文件必须存在、是普通文件且不超过 256KB；
/// 任何读失败 / 越界一律**静默返回 `None`**（前端回退首字母色块）。
fn icon_data_uri(dir: &Path, icon: Option<&str>) -> Option<String> {
    let rel = icon.map(str::trim).filter(|text| !text.is_empty())?;
    let ext = Path::new(rel)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())?;
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "gif" => "image/gif",
        _ => return None,
    };
    let path = dir.join(rel);
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > ICON_MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    if bytes.len() as u64 > ICON_MAX_BYTES {
        return None;
    }
    Some(format!("data:{mime};base64,{}", base64_encode(&bytes)))
}

/// 标准 base64 编码（与 `install::decode_base64` 对称；刻意不引依赖）。
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// 合并设置快照：manifest 默认值 + 用户已保存值（只保留 schema 声明的 key）。
///
/// 老版本插件新增/删除 settings 字段时，旧 registry 中的多余 key 会被自然丢弃，
/// 不会把过期配置带进运行时。
fn merge_settings(manifest: &Manifest, stored: &HashMap<String, Value>) -> HashMap<String, Value> {
    let mut values = manifest.settings_defaults();
    for spec in &manifest.settings {
        if let Some(value) = stored.get(&spec.key) {
            values.insert(spec.key.clone(), value.clone());
        }
    }
    values
}

/// 记录一次 hook 失败并做熔断判定（纯逻辑，便于单测）。
///
/// 返回 `Some(n)` **仅当本次恰好跨过阈值**（`breaker` 由 false → true）；
/// 返回 `None` 表示未达阈值，或已处于熔断态（避免重复触发与重复审计）。
fn note_failure(failures: &AtomicUsize, breaker: &AtomicBool, threshold: usize) -> Option<usize> {
    let n = failures.fetch_add(1, Ordering::Relaxed) + 1;
    if n >= threshold && !breaker.swap(true, Ordering::Relaxed) {
        Some(n)
    } else {
        None
    }
}

/// 有界等待线程收敛；超时则分离（进程退出时由内核回收，绝不无限阻塞调用方）。
fn wait_thread(handle: JoinHandle<()>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if handle.is_finished() {
        // join 失败 = 插件线程 panic 退出（panic 消息已由默认 hook 打到 stderr）。记 error 便于审计。
        if handle.join().is_err() {
            tracing::error!("插件线程 panic 退出（线程已收敛，句柄已回收）");
        }
    } else {
        eprintln!("[plugin] 插件线程退出超时，已分离");
    }
}

/// 加载失败时回收半成品线程：先 Shutdown，再作废队列，最后有界等待。
fn abort_thread(tx: SyncSender<PluginJob>, join: JoinHandle<()>) {
    // Shutdown 若投递失败（队列满 / 线程已退出），仍有下面的 `drop(tx)` 兜底：
    // 线程消费完在飞任务后 `recv()` 报错即自行退出，不会泄漏。
    if tx.try_send(PluginJob::Shutdown).is_err() {
        tracing::debug!("加载失败回收：Shutdown 未送达（队列满/线程已退出），改由 drop 队列兜底");
    }
    // 作废队列：即使 Shutdown 未送达，线程在消费完在飞任务后也会因 recv 失败而退出。
    drop(tx);
    wait_thread(join, SHUTDOWN_TIMEOUT);
}

/// 关闭单个插件实例：先请求 onShutdown，再释放其句柄，最后有界 join。
///
/// **必须在调用方不持有任何锁时调用**（join 最长等 `SHUTDOWN_TIMEOUT`）。
fn shutdown_one(plugin: LoadedPlugin) {
    // beta10：先请求取消该插件正在进行的批量任务（下一次轮询即收敛并清理内部 gid），
    // 再投递 Shutdown —— 否则插件线程阻塞在 `addTasksAndWait` 时收不到关闭信号。
    super::tasks::cancel(&plugin.id);
    super::plugin_tasks::cancel_all_for_plugin(&plugin.id);
    // beta11 · B4：关闭插件同样强杀其子进程（FFmpeg 不该比插件活得更久）。
    super::exec::kill_plugin(&plugin.id);
    // 关键路径：Shutdown 投递失败意味着插件线程不会收到显式关闭信号 → onShutdown 钩子不执行。
    // 记 warn（区别于 abort_thread 的 debug：这里是正常关闭路径，异常必须可见）。
    if plugin.tx.try_send(PluginJob::Shutdown).is_err() {
        tracing::warn!(
            "插件关闭信号未送达（队列满/线程已退出）：plugin={}",
            plugin.id
        );
    }
    let handle = plugin
        .join
        .lock()
        .ok()
        .and_then(|mut guard| guard.take());
    drop(plugin);
    if let Some(handle) = handle {
        wait_thread(handle, SHUTDOWN_TIMEOUT);
    }
}

/// 插件专属线程主体：Runtime/Context 在此线程内创建、使用、析构。
fn plugin_thread(
    id: String,
    manifest: Manifest,
    host: PluginHostContext,
    rx: mpsc::Receiver<PluginJob>,
    enabled: Arc<AtomicBool>,
    failures: Arc<AtomicUsize>,
    breaker: Arc<AtomicBool>,
) {
    // host 会被移入运行时；另行保留一份用于"拦截完成回执"（clone 廉价，内部均为 Arc/AppHandle）。
    let host_reply = host.clone();
    // 后端分流在 `runtime::build` 内完成（按 manifest.engine）：
    //   `js`   → QuickJsRuntime（默认，既有插件零感知）
    //   `wasm` → WasmRuntime（需 `wasm-runtime` feature，否则返回明确错误）
    let mut runtime: Option<Box<dyn PluginRuntime>> = None;
    // 记录初始化失败的**原始原因**，让 `Load` 的回执带上真正可读的报错
    // （典型场景：未带 `wasm-runtime` feature 却加载 wasm 插件）。
    let mut runtime_error: Option<AppError> = None;
    match runtime::build(&manifest, host) {
        Ok(rt) => runtime = Some(rt),
        Err(e) => {
            eprintln!("[plugin:{id}] 初始化插件运行时失败：{e}");
            runtime_error = Some(e);
        }
    }

    while let Ok(job) = rx.recv() {
        match job {
            PluginJob::Load { source, reply } => {
                let result = match runtime.as_mut() {
                    Some(rt) => rt.load_source(&source),
                    None => Err(runtime_error
                        .take()
                        .unwrap_or_else(|| AppError::PluginLoad("插件运行时不可用".to_string()))),
                };
                if let Err(e) = &result {
                    eprintln!("[plugin:{id}] 加载失败：{e}");
                    runtime = None;
                }
                // 加载结果回执：调用方若已因 LOAD_TIMEOUT(3s) 放弃等待，receiver 已 drop → 正常竞态。
                if reply.send(result).is_err() {
                    tracing::warn!("插件加载结果未送达（调用方已超时退出）：plugin={id}");
                }
            }
            PluginJob::Hook { hook, payload } => {
                if let Some(rt) = runtime.as_mut() {
                    match rt.invoke(&hook, &payload) {
                        // 成功即清零"连续失败"计数（连续计次语义）。
                        Ok(()) => failures.store(0, Ordering::Relaxed),
                        Err(e) => {
                            eprintln!("{e}");
                            if let Some(n) = note_failure(&failures, &breaker, BREAKER_THRESHOLD) {
                                // P1-B 熔断：只**挂起**（置 `enabled=false`），绝不 kill 插件线程。
                                enabled.store(false, Ordering::Relaxed);
                                super::audit::log_circuit_breaker(&id, n);
                                host_reply.host.emit(
                                    "plugin://circuit-breaker",
                                    json!({ "pluginId": id, "consecutiveFailures": n }),
                                );
                                eprintln!(
                                    "[plugin:{id}] 连续 {n} 次 hook 异常，已自动禁用（熔断，挂起）"
                                );
                            }
                        }
                    }
                }
                // 前置拦截为同步等待型 hook：无论是否改写 URL，执行完都要回报"完成"，
                // 否则宿主会一直等到超时。
                if hook == dispatcher::HOOK_BEFORE_ADD {
                    host_reply.intercept_complete();
                }
            }
            PluginJob::TaskHook {
                hook,
                payload,
                budget_ms,
            } => {
                if let Some(rt) = runtime.as_mut() {
                    match rt.invoke_with_budget(&hook, &payload, budget_ms) {
                        Ok(_) => failures.store(0, Ordering::Relaxed),
                        Err(e) => {
                            eprintln!("{e}");
                            if let Some(n) = note_failure(&failures, &breaker, BREAKER_THRESHOLD) {
                                enabled.store(false, Ordering::Relaxed);
                                super::audit::log_circuit_breaker(&id, n);
                                host_reply.host.emit(
                                    "plugin://circuit-breaker",
                                    json!({ "pluginId": id, "consecutiveFailures": n }),
                                );
                                eprintln!(
                                    "[plugin:{id}] 连续 {n} 次 hook 异常，已自动禁用（熔断，挂起）"
                                );
                            }
                        }
                    }
                }
            }
            PluginJob::Detect {
                hook,
                payload,
                budget_ms,
                reply,
            } => {
                let result = match runtime.as_mut() {
                    Some(rt) => rt
                        .invoke_with_budget(&hook, &payload, budget_ms)
                        .map_err(|e| e.to_string()),
                    None => Err("插件运行时不可用".to_string()),
                };
                match &result {
                    Ok(_) => failures.store(0, Ordering::Relaxed),
                    Err(e) => {
                        eprintln!("{e}");
                        if let Some(n) = note_failure(&failures, &breaker, BREAKER_THRESHOLD) {
                            enabled.store(false, Ordering::Relaxed);
                            super::audit::log_circuit_breaker(&id, n);
                            host_reply.host.emit(
                                "plugin://circuit-breaker",
                                json!({ "pluginId": id, "consecutiveFailures": n }),
                            );
                        }
                    }
                }
                if reply.send(result).is_err() {
                    tracing::debug!("任务认领回执未送达（宿主已超时）：plugin={id}");
                }
            }
            PluginJob::Shutdown => {
                if let Some(rt) = runtime.as_mut() {
                    rt.shutdown();
                }
                break;
            }
        }
    }

    drop(runtime);
}

/// 通知监听线程停止（幂等）。退出期调用；监听线程在下一个静默窗口内收敛退出。
pub fn stop_watching() {
    WATCH_STOP.store(true, Ordering::Relaxed);
}

/// 监听 `PLUGIN_DIR` 的文件变更并做防抖热重载。
///
/// 线程模型（红线 1 对齐）：
/// - `notify` 的**回调线程**只做一次非阻塞 `send`，绝不做 IO / 重载；
/// - 专属 std 线程做**防抖收敛**（`recv_timeout(300ms)`：静默窗口满才交付一批）；
/// - 真正的重载丢给 `spawn_blocking`（其含旧线程有界 join），不阻塞监听线程与
///   tokio 异步工作线程。
pub fn watch_plugins(host: Arc<dyn HostContext>) {
    let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<notify::Event>>();

    let watcher = match notify::recommended_watcher(move |res| {
        // 回调线程只做一次非阻塞 send（红线：绝不做 IO）；接收端退出后失败属预期。
        if raw_tx.send(res).is_err() {
            tracing::debug!("插件监听原始事件投递失败（监听线程已退出）");
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[plugin][热重载] 创建监听器失败：{e}");
            return;
        }
    };

    let mut watcher = watcher;
    let root = super::plugin_root();
    if let Err(e) = watcher.watch(&root, notify::RecursiveMode::Recursive) {
        eprintln!("[plugin][热重载] 监听 {} 失败：{e}", root.display());
        return;
    }

    let spawned = std::thread::Builder::new()
        .name("plugin-watcher".to_string())
        .spawn(move || {
            // 持有 watcher：线程存活期间监听才有效（drop 即 unwatch）。
            let _keep_alive = watcher;
            let mut pending: HashSet<String> = HashSet::new();
            loop {
                if WATCH_STOP.load(Ordering::Relaxed) {
                    println!("[plugin][热重载] 监听线程已停止");
                    return;
                }
                match raw_rx.recv_timeout(RELOAD_DEBOUNCE) {
                    Ok(Ok(event)) => collect_changed_dirs(&event, &mut pending),
                    Ok(Err(e)) => eprintln!("[plugin][热重载] 监听报错：{e}"),
                    // 静默窗口已满：把这一批变更一次性交付后台重载（防抖收敛点）。
                    Err(mpsc::RecvTimeoutError::Timeout) => flush_batch(&host, &mut pending),
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        });

    match spawned {
        Ok(_) => println!("[plugin][热重载] 已监听 {}", root.display()),
        Err(e) => eprintln!("[plugin][热重载] 启动监听线程失败：{e}"),
    }
}

/// 从一次 `notify` 事件中提取受影响的**插件目录名**（写进 `out` 去重）。
fn collect_changed_dirs(event: &notify::Event, out: &mut HashSet<String>) {
    for path in &event.paths {
        if let Some(name) = plugin_dir_name(path) {
            out.insert(name);
        }
    }
}

/// 把 `PLUGIN_DIR/<name>/...` 还原为 `<name>`。
///
/// 关键过滤：`registry.json` 与 `*.tmp`（热重载自身会原子写 registry.json，
/// 若不过滤将触发"监听 → 重载 → 再写"的自触发循环）；beta11 起 `configs`
/// 与点目录也不是插件（配置写入 / 安装临时目录绝不能触发重载）。
fn plugin_dir_name(path: &Path) -> Option<String> {
    let root = super::plugin_root();
    let rel = path.strip_prefix(&root).ok()?;
    let mut comps = rel.components();
    let first = match comps.next()? {
        std::path::Component::Normal(seg) => seg.to_string_lossy().into_owned(),
        _ => return None,
    };
    if first == "registry.json" || first.ends_with(".tmp") || is_non_plugin_dir_name(&first) {
        return None;
    }
    if comps.next().is_some() {
        // `<name>/...`：插件目录内部的变更，直接命中。
        Some(first)
    } else if path.is_dir() {
        // `<name>` 本身：只接受"新插件目录被创建"。
        Some(first)
    } else {
        None
    }
}

/// 把一批变更目录交付后台阻塞线程池逐个重载（失败只记日志 + 通知前端，**绝不重试**）。
fn flush_batch(host: &Arc<dyn HostContext>, pending: &mut HashSet<String>) {
    if pending.is_empty() {
        return;
    }
    for name in pending.drain() {
        let host = host.clone();
        // 重载含"旧线程有界 join"，属阻塞动作 → 交给阻塞线程池。
        crate::rt().spawn_blocking(move || {
            let dir = super::plugin_root().join(&name);
            // 阶段三：插件管理器改走进程级全局访问器。
            let manager = crate::plugins();
            if let Err(e) = manager.reload_dir(&dir) {
                let msg = e.to_string();
                super::audit::log_reload_failed(&name, &msg);
                host.emit(
                    "plugin://reloaded",
                    json!({ "id": name, "status": "failed", "error": msg }),
                );
                eprintln!("[plugin][热重载] {name} 失败（保留旧版本或标记失败）：{msg}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> (AtomicUsize, AtomicBool) {
        (AtomicUsize::new(0), AtomicBool::new(false))
    }

    #[test]
    fn breaker_trips_on_consecutive_failures() {
        let (failures, breaker) = fresh();
        // 前两次失败：未达阈值，不熔断
        assert_eq!(note_failure(&failures, &breaker, 3), None);
        assert_eq!(note_failure(&failures, &breaker, 3), None);
        assert!(!breaker.load(Ordering::Relaxed), "未达阈值不应熔断");
        // 第三次跨过阈值 → 熔断，并回报连续次数
        assert_eq!(note_failure(&failures, &breaker, 3), Some(3));
        assert!(breaker.load(Ordering::Relaxed), "应标记熔断");
        // 已熔断后再失败不重复触发（避免重复审计 / 重复提示）
        assert_eq!(note_failure(&failures, &breaker, 3), None);
    }

    #[test]
    fn success_resets_consecutive_counter() {
        let (failures, breaker) = fresh();
        assert_eq!(note_failure(&failures, &breaker, 3), None);
        assert_eq!(note_failure(&failures, &breaker, 3), None);
        // 一次成功 → 连续计数清零，重新计次
        failures.store(0, Ordering::Relaxed);
        assert_eq!(note_failure(&failures, &breaker, 3), None);
        assert_eq!(note_failure(&failures, &breaker, 3), None);
        assert_eq!(note_failure(&failures, &breaker, 3), Some(3));
    }

    #[test]
    fn dir_name_ignores_registry_and_temp_sources() {
        let root = crate::plugin::plugin_root();
        // 插件目录内部的文件变更 → 命中目录名
        assert_eq!(
            plugin_dir_name(&root.join("com.example.hello").join("main.js")),
            Some("com.example.hello".to_string())
        );
        assert_eq!(
            plugin_dir_name(&root.join("com.example.hello").join("manifest.json")),
            Some("com.example.hello".to_string())
        );
        // 自触发源：registry.json / *.tmp 一律忽略（否则死循环）
        assert_eq!(plugin_dir_name(&root.join("registry.json")), None);
        assert_eq!(plugin_dir_name(&root.join("registry.json.tmp")), None);
        // beta11 · C5：配置目录与点目录不是插件，绝不能触发重载
        assert_eq!(plugin_dir_name(&root.join("configs")), None);
        assert_eq!(
            plugin_dir_name(&root.join("configs").join("com.example.hello.json")),
            None
        );
        assert_eq!(plugin_dir_name(&root.join(".install-tmp")), None);
    }
}
