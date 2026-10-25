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
use super::hostapi::{HostBridge, PluginHostContext};
use super::manifest::{Manifest, SettingSpec};
use super::registry::Registry;
use super::runtime::{self, PluginRuntime};
use super::QUEUE_CAP;

/// 单插件同步加载的超时上界。
const LOAD_TIMEOUT: Duration = Duration::from_secs(3);
/// 退出时等待插件线程收敛的上界。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(2000);
/// 熔断阈值：连续 N 次 hook 失败即自动禁用（**挂起**语义，绝不 kill 线程）。
const BREAKER_THRESHOLD: usize = 3;
/// 热重载防抖窗口：变更后静默这么长时间才批量重载（合并编辑器保存产生的多个事件）。
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);

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

struct LoadedPlugin {
    id: String,
    name: String,
    version: String,
    description: String,
    permissions: Vec<String>,
    /// manifest 声明的设置 schema（前端表单渲染依据）。
    setting_specs: Vec<SettingSpec>,
    /// 运行时设置快照：与 `PluginHostContext.settings` 共享同一 `Arc`，
    /// 保存后立即对 `ctx.settings.get()` 生效，无需重载插件。
    settings: Arc<RwLock<HashMap<String, Value>>>,
    /// 插件所在目录（热重载按目录定位实例：`notify` 给出的是路径而非 id）。
    dir: PathBuf,
    tx: SyncSender<PluginJob>,
    hooks: Arc<HashSet<String>>,
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
                .filter(|path| path.is_dir())
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
        let settings = merge_settings(&manifest, &registry.settings_of(&manifest.id));

        let plugin = Self::build_plugin(host, bridge, dir, &manifest, enabled, settings)?;
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
    ) -> Result<LoadedPlugin, AppError> {
        let entry_path = dir.join(&manifest.entry);
        // 按 engine 分流读取：JS 走 UTF-8 文本，Wasm 走二进制（增量 25）。
        let source = runtime::read_entry(&manifest.engine, &entry_path)?;

        let settings = Arc::new(RwLock::new(settings));
        let host_ctx = PluginHostContext {
            plugin_id: manifest.id.clone(),
            permissions: Arc::new(manifest.permission_set()),
            settings: settings.clone(),
            host: host.clone(),
            bridge: bridge.clone(),
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
            permissions: manifest.permissions.clone(),
            setting_specs: manifest.settings.clone(),
            settings,
            dir: dir.to_path_buf(),
            tx,
            hooks: Arc::new(manifest.hooks.iter().cloned().collect()),
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
        let (flag, breaker, failures, manifest_perms, name) = {
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
        let (flag, breaker, failures, manifest_perms, name) = {
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
            )
        };

        flag.store(true, Ordering::Relaxed);
        breaker.store(false, Ordering::Relaxed);
        failures.store(0, Ordering::Relaxed);

        {
            let mut guard = self
                .registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.grant(id, &manifest_perms);
            guard.set_enabled(id, true);
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

    /// 保存插件设置：校验 schema → 写 registry.json → **热更新运行时快照**。
    ///
    /// 红线：
    /// - 只接受 manifest.settings 中声明过的 key，杜绝插件/前端写入任意垃圾数据；
    /// - 类型与 select 候选值必须匹配，否则拒绝整次保存（不做部分写入）；
    /// - 落盘成功后才更新运行时，保证 `ctx.settings.get()` 与磁盘绝对一致；
    /// - 持锁范围：快照 → 释放；registry 落盘；最后写 settings 单元。
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
                "string" if value.is_string() => {}
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
            let mut registry = self
                .registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            registry.set_settings(id, next.clone());
            registry
                .save(&self.registry_dir)
                .map_err(|e| AppError::Internal(format!("保存插件设置失败：{e}")))?;
        }

        *cell.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = next;

        println!("[plugin] {} 的设置已更新（{} 项）", name, specs.len());
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
        let (enabled, settings) = {
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
            let settings = merge_settings(&manifest, &registry.settings_of(&manifest.id));
            (enabled, settings)
        };

        // 3. 构建新实例（无锁；内含线程 spawn + 3s Load 超时，失败即回收半成品线程）
        let new_plugin =
            Self::build_plugin(&self.host, &self.bridge, dir, &manifest, enabled, settings)?;

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
/// 若不过滤将触发"监听 → 重载 → 再写"的自触发循环）。
fn plugin_dir_name(path: &Path) -> Option<String> {
    let root = super::plugin_root();
    let rel = path.strip_prefix(&root).ok()?;
    let mut comps = rel.components();
    let first = match comps.next()? {
        std::path::Component::Normal(seg) => seg.to_string_lossy().into_owned(),
        _ => return None,
    };
    if first == "registry.json" || first.ends_with(".tmp") || first.starts_with('.') {
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
    }
}
