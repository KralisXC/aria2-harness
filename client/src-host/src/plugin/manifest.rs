//! manifest.json 解析、校验与 SHA-256 完整性验证（Q4：M1 只做哈希，不含签名）。
//!
//! ## 懒加载红线（beta11 · A3）
//!
//! 启动/加载阶段**只读** `manifest.json` 与 `entry` 入口脚本：
//! `bin/`、`models/` 等大文件一律不在此处预读，只有在 `ctx.exec` 或插件显式
//! `ctx.fs.readText` 时才按路径读取。新增字段校验请勿引入全量目录扫描。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::AppError;

/// 宿主插件 API 版本（1.0 契约）。manifest.apiVersion 不匹配即拒绝加载。
pub const API_VERSION: &str = "1";
/// beta10 新契约：新增 `net:http` / `exec:ffmpeg` / `downloads:write` 权限、
/// `routing` 路由声明与长任务预算。1.0 插件继续按旧能力加载。
pub const API_VERSION_1_1: &str = "1.1";

/// 运行时后端标识（增量 25）。
///
/// 默认 `js`（QuickJS）：既有插件零感知，**Wasm 绝不成为默认后端**（增量 25 红线 2）。
pub const ENGINE_JS: &str = "js";
/// Wasmtime 强隔离后端；需对应插件通过 manifest 显式声明，且宿主须带
/// `wasm-runtime` feature 构建。
pub const ENGINE_WASM: &str = "wasm";

/// 包类型：普通插件（manifest 缺省值，旧包零感知）。
pub const TYPE_PLUGIN: &str = "plugin";
/// 包类型：共享依赖包（beta12.2：`data/plugins/.shared/<共享id>/`）。
///
/// 共享包只提供二进制 / 资源，不含入口脚本，因此不要求 `main.js`、不生成 integrity。
pub const TYPE_SHARED: &str = "shared";

const DEFAULT_TIMEOUT_MS: u64 = 2000;
/// 长任务（`onTaskDetected` / `onTaskRun`）的默认预算：30 分钟。
const DEFAULT_TASK_TIMEOUT_MS: u64 = 1_800_000;
/// 单次 `ctx.httpGet` 的响应体上限：8MB（HLS 清单 / 分片索引足够）。
const DEFAULT_MAX_HTTP_BYTES: usize = 8 * 1024 * 1024;
/// 单次 `ctx.addTasksAndWait` 的任务条数上限。
const DEFAULT_MAX_TASKS_PER_CALL: usize = 500;
const DEFAULT_MEMORY_MB: usize = 64;
const DEFAULT_PRIORITY: i64 = 100;
/// Wasm 后端的默认燃料预算（MVS 定额）：足够跑完正常 hook，又能快速截断死循环。
const DEFAULT_FUEL_UNITS: u64 = 100_000;

/// manifest.json 的数据模型。
///
/// `rename_all = "camelCase"` 让 `api_version` 对应 `apiVersion`；未在本阶段消费的
/// 字段（author/icon 等）刻意不建模，由 serde 默认忽略。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// 包类型（beta12.2）：`"plugin"`（默认）或 `"shared"`（共享依赖包）。
    ///
    /// `#[serde(default)]` ⇒ 旧 manifest 未声明时落到 `plugin`，行为零变化。
    #[serde(rename = "type", default = "default_plugin_type")]
    pub plugin_type: String,
    pub id: String,
    pub name: String,
    /// 插件简介（插件管理 UI 展示用）。
    #[serde(default)]
    pub description: String,
    /// 作者（beta11：插件管理中心卡片展示）。
    #[serde(default)]
    pub author: String,
    /// 图标（相对插件根目录的图片路径；beta11：宿主转为 data URI 输出）。
    #[serde(default)]
    pub icon: Option<String>,
    pub version: String,
    /// 宿主插件 API 版本，用于兼容性判定。
    #[serde(default = "default_api_version")]
    pub api_version: String,
    /// 入口脚本（相对插件根目录，禁止绝对路径与 `..` 穿越）。
    ///
    /// beta12.2：`shared` 包不含入口脚本，因此缺省为空串（由 `validate` 按类型判定）。
    #[serde(default)]
    pub entry: String,
    /// 运行时后端：`"js"`（QuickJS，默认）或 `"wasm"`（Wasmtime 强隔离）。
    ///
    /// `#[serde(default)]` ⇒ 既有插件（未声明该字段）全部落到 `js`，行为不变。
    #[serde(default = "default_engine")]
    pub engine: String,
    /// 越小越先收到事件（同 hook 多插件时）。
    #[serde(default = "default_priority")]
    pub priority: i64,
    /// 静态订阅的 Hook 名单（权威订阅列表）。
    #[serde(default)]
    pub hooks: Vec<String>,
    /// 能力令牌：默认拒绝，逐项显式申请。
    #[serde(default)]
    pub permissions: Vec<String>,
    /// 设置项 schema（key + default + 控件类型，用于 ctx.settings 与前端表单）。
    ///
    /// beta11：兼容生态里常见的 `settings_schema` 写法（与 `settings` 等价）。
    #[serde(default, alias = "settings_schema")]
    pub settings: Vec<SettingSpec>,
    /// 右键动作（beta11 / ff.md）：已完成任务上动态展示的插件动作。
    #[serde(default)]
    pub actions: Vec<ActionSpec>,
    /// 资源上限（宿主另有硬上限兜底）。
    #[serde(default)]
    pub limits: Limits,
    /// 任务路由声明（beta10）：命中 pattern 的 URL 交给 `handler` 认领。
    #[serde(default)]
    pub routing: Vec<RoutingRule>,
    /// 懒激活声明（beta12 · 模块 E）：全息探针命中这些域名时才唤醒插件；
    /// 缺省 / 空数组表示始终激活（既有插件零感知）。
    #[serde(default)]
    pub activation_events: ActivationEvents,
    /// 逐文件完整性清单（加载前校验）。
    #[serde(default)]
    pub integrity: Option<Integrity>,
}

/// 懒激活声明（beta12 · 模块 E）。
///
/// `domains` 支持 `*.example.com`（命中裸域与子域）与精确主机；大小写不敏感。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivationEvents {
    #[serde(default)]
    pub domains: Vec<String>,
}

/// 一条任务路由规则（beta10 · 模块 B）。
///
/// `pattern` 为 glob（大小写不敏感），如 `*.m3u8`；`priority` 越小越先匹配；
/// `handler` 是插件内注册的 hook 名（默认 `onTaskDetected`）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingRule {
    pub pattern: String,
    #[serde(default = "default_priority")]
    pub priority: i64,
    #[serde(default = "default_routing_handler")]
    pub handler: String,
}

fn default_routing_handler() -> String {
    "onTaskDetected".to_string()
}

/// 一条右键插件动作（ff.md · 模块五.3）。
///
/// `id` 是插件在 `onAction` hook 里自行分流的稳定标识；`label` 是右键菜单文案。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionSpec {
    pub id: String,
    pub label: String,
}

/// 设置项 schema（插件设置 UI 的数据契约）。
///
/// `type` 是 Rust 保留字，故字段名为 `kind`，序列化仍保持 manifest 里的 `"type"`。
/// 老插件未声明 settings 时 `Vec` 为空，前端不显示“设置”按钮。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingSpec {
    pub key: String,
    /// 前端表单标签（缺省时前端回退显示 key）。
    #[serde(default)]
    pub label: String,
    /// 控件类型：`boolean` / `string` / `text` / `textarea` / `number` / `select`（缺省按 string 处理）。
    #[serde(rename = "type", default = "default_setting_type")]
    pub kind: String,
    /// `select` 的候选值（字符串数组）。
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub default: Value,
}

fn default_setting_type() -> String {
    "string".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// 长任务预算（仅 `onTaskDetected` / `onTaskRun` / 插件任务生命周期）。
    #[serde(default = "default_task_timeout_ms")]
    pub task_timeout_ms: u64,
    /// 单次 `ctx.httpGet` 的响应体上限（宿主另有 8MB 硬上限）。
    #[serde(default = "default_max_http_bytes")]
    pub max_http_bytes: usize,
    /// 单次 `ctx.addTasksAndWait` 的任务条数上限（宿主另有 500 硬上限）。
    #[serde(default = "default_max_tasks_per_call")]
    pub max_tasks_per_call: usize,
    #[serde(default = "default_memory_mb")]
    pub memory_mb: usize,
    /// Wasm 后端的**燃料预算**（指令级计量，增量 25）。
    ///
    /// 注意语义差异：`timeout_ms` 是墙钟上界（QuickJS 走 deadline + interrupt），
    /// 而 fuel 是**定额指令预算**，二者没有固定换算。MVS 阶段只做 fuel 定额，
    /// epoch 墙钟兜底留给终态。
    #[serde(default = "default_fuel_units")]
    pub fuel_units: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_TIMEOUT_MS,
            task_timeout_ms: DEFAULT_TASK_TIMEOUT_MS,
            max_http_bytes: DEFAULT_MAX_HTTP_BYTES,
            max_tasks_per_call: DEFAULT_MAX_TASKS_PER_CALL,
            memory_mb: DEFAULT_MEMORY_MB,
            fuel_units: DEFAULT_FUEL_UNITS,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Integrity {
    #[serde(default = "default_algorithm")]
    pub algorithm: String,
    /// 相对插件根的路径 -> 期望的十六进制哈希。
    #[serde(default)]
    pub files: HashMap<String, String>,
}

fn default_api_version() -> String {
    API_VERSION.to_string()
}

fn default_engine() -> String {
    ENGINE_JS.to_string()
}

fn default_plugin_type() -> String {
    TYPE_PLUGIN.to_string()
}

fn default_priority() -> i64 {
    DEFAULT_PRIORITY
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

fn default_task_timeout_ms() -> u64 {
    DEFAULT_TASK_TIMEOUT_MS
}

fn default_max_http_bytes() -> usize {
    DEFAULT_MAX_HTTP_BYTES
}

fn default_max_tasks_per_call() -> usize {
    DEFAULT_MAX_TASKS_PER_CALL
}

fn default_memory_mb() -> usize {
    DEFAULT_MEMORY_MB
}

fn default_fuel_units() -> u64 {
    DEFAULT_FUEL_UNITS
}

fn default_algorithm() -> String {
    "sha256".to_string()
}

/// 反向域名插件 id 校验（beta11 · A1）。
///
/// 规则：
/// - 长度 3–128；
/// - 仅 `[a-z0-9.-]`（禁止大写 / 下划线 / 空格 / 路径分隔符）；
/// - 至少两段（必须包含 `.`）；
/// - 每段非空，且不以 `-` 开头或结尾；
/// - 整体不以 `.` / `-` 开头或结尾。
pub fn is_valid_plugin_id(id: &str) -> bool {
    if id.len() < 3 || id.len() > 128 {
        return false;
    }
    let bytes = id.as_bytes();
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'.' || *b == b'-')
    {
        return false;
    }
    if id.starts_with('.') || id.starts_with('-') || id.ends_with('.') || id.ends_with('-') {
        return false;
    }
    if !id.contains('.') {
        return false;
    }
    id.split('.')
        .all(|seg| !seg.is_empty() && !seg.starts_with('-') && !seg.ends_with('-'))
}

impl Manifest {
    /// 从插件目录读取并校验 manifest.json。
    pub fn from_dir(dir: &Path) -> Result<Self, AppError> {
        let path = dir.join("manifest.json");
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| AppError::fs(format!("读取 manifest 失败（{}）", path.display()), e))?;
        let manifest: Manifest = serde_json::from_str(&raw).map_err(|e| {
            AppError::PluginLoad(format!("解析 manifest 失败（{}）：{e}", path.display()))
        })?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<(), AppError> {
        // beta12.2 · 模块二：先判包类型 —— 未知 type 必须 fail fast（不允许拼写错误
        // 静默退化成 plugin，进而把一个共享包当插件去加载 main.js）。
        let is_shared = match self.plugin_type.as_str() {
            TYPE_PLUGIN => false,
            TYPE_SHARED => true,
            other => {
                return Err(AppError::PluginLoad(format!(
                    "manifest.type 非法：`{other}`（仅支持 `{TYPE_PLUGIN}` / `{TYPE_SHARED}`）"
                )));
            }
        };
        if self.id.trim().is_empty() {
            return Err(AppError::PluginLoad("manifest.id 不能为空".to_string()));
        }
        // beta11 · A1：反向域名是生态唯一标识，非法 id 直接拒绝加载。
        if !is_valid_plugin_id(&self.id) {
            return Err(AppError::PluginLoad(format!(
                "manifest.id 非反向域名格式（示例 com.harness.hls-downloader）：{}",
                self.id
            )));
        }
        // beta11 · A2：icon 与 entry 同一套相对路径约束（禁止逃出插件目录）。
        if let Some(icon) = self.icon.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let icon = icon.replace('\\', "/");
            if icon.starts_with('/') || icon.contains("..") || icon.contains(':') {
                return Err(AppError::PluginLoad(format!(
                    "manifest.icon 非法（禁止绝对路径与 .. 穿越）：{}",
                    self.icon.as_deref().unwrap_or_default()
                )));
            }
        }
        if is_shared {
            // 共享包只要求元数据完整：`entry` 可空、不要求 main.js、不生成 integrity。
            if self.name.trim().is_empty() {
                return Err(AppError::PluginLoad(
                    "manifest.name 不能为空（共享包）".to_string(),
                ));
            }
            if self.version.trim().is_empty() {
                return Err(AppError::PluginLoad(
                    "manifest.version 不能为空（共享包）".to_string(),
                ));
            }
        } else {
            if self.entry.trim().is_empty() {
                return Err(AppError::PluginLoad(
                    "manifest.entry 不能为空".to_string(),
                ));
            }
            // 入口不得逃出插件目录（防路径穿越）
            let entry = self.entry.replace('\\', "/");
            if entry.starts_with('/') || entry.contains("..") || entry.contains(':') {
                return Err(AppError::PluginLoad(format!(
                    "manifest.entry 非法（禁止绝对路径与 .. 穿越）：{}",
                    self.entry
                )));
            }
        }
        if self.api_version != API_VERSION && self.api_version != API_VERSION_1_1 {
            return Err(AppError::PluginLoad(format!(
                "apiVersion 不兼容：宿主需要 {API_VERSION} 或 {API_VERSION_1_1}，插件声明 {}",
                self.api_version
            )));
        }
        // beta10：路由声明必须可执行 —— 空 pattern / 空 handler 会让匹配静默失效。
        for rule in &self.routing {
            if rule.pattern.trim().is_empty() {
                return Err(AppError::PluginLoad(
                    "manifest.routing[].pattern 不能为空".to_string(),
                ));
            }
            if rule.handler.trim().is_empty() {
                return Err(AppError::PluginLoad(
                    "manifest.routing[].handler 不能为空".to_string(),
                ));
            }
        }
        // beta12 · 模块 E：懒激活域名必须可匹配 —— 空规则 / 带协议 / 路径 / 端口
        // 或单独 `*` 都会让"按需唤醒"退化成全量或彻底失效。
        for domain in &self.activation_events.domains {
            let domain = domain.trim();
            if domain.is_empty()
                || domain == "*"
                || domain.contains("://")
                || domain.contains('/')
                || domain.contains(':')
                || domain.chars().any(char::is_whitespace)
            {
                return Err(AppError::PluginLoad(format!(
                    "manifest.activationEvents.domains 非法（仅支持域名 / *.域名）：{domain}"
                )));
            }
        }
        // beta11 / ff.md：右键动作的 id / label 必须可展示、可分流。
        let mut action_ids = HashSet::new();
        for action in &self.actions {
            if action.id.trim().is_empty() || action.label.trim().is_empty() {
                return Err(AppError::PluginLoad(
                    "manifest.actions[].id / label 不能为空".to_string(),
                ));
            }
            if !action_ids.insert(action.id.as_str()) {
                return Err(AppError::PluginLoad(format!(
                    "manifest.actions 存在重复 id：{}",
                    action.id
                )));
            }
        }
        // 后端标识必须在编译期就拒绝未知值，避免运行到构造运行时才炸。
        match self.engine.as_str() {
            ENGINE_JS | ENGINE_WASM => {}
            other => {
                return Err(AppError::PluginLoad(format!(
                    "manifest.engine 非法：`{other}`（仅支持 `{ENGINE_JS}` / `{ENGINE_WASM}`）"
                )));
            }
        }
        // 设置 schema 的 fail-fast 校验：非法控件类型 / 空 select 候选会直接让
        // 前端表单渲染不出来，因此在加载期就拒绝，避免运行期出现“空白设置项”。
        let mut keys = HashSet::new();
        for spec in &self.settings {
            if spec.key.trim().is_empty() {
                return Err(AppError::PluginLoad(
                    "manifest.settings[].key 不能为空".to_string(),
                ));
            }
            if !keys.insert(spec.key.as_str()) {
                return Err(AppError::PluginLoad(format!(
                    "manifest.settings 存在重复 key：{}",
                    spec.key
                )));
            }
            match spec.kind.as_str() {
                // 1.0.0-rc1 · 模块二：`text` 是 textarea 的别名，`number` 走数字输入框。
                "boolean" | "string" | "textarea" | "text" | "number" => {}
                "select" if !spec.options.is_empty() => {}
                "select" => {
                    return Err(AppError::PluginLoad(format!(
                        "设置项 {} 的类型为 select，但 options 为空",
                        spec.key
                    )));
                }
                other => {
                    return Err(AppError::PluginLoad(format!(
                        "设置项 {} 的类型非法：`{other}`（仅支持 boolean/string/text/textarea/number/select）",
                        spec.key
                    )));
                }
            }
        }
        Ok(())
    }

    /// 能力令牌集合（默认拒绝模型的输入）。
    pub fn permission_set(&self) -> HashSet<String> {
        self.permissions.iter().cloned().collect()
    }

    /// 设置项默认值（M1 ctx.settings 只读来源）。
    pub fn settings_defaults(&self) -> HashMap<String, Value> {
        self.settings
            .iter()
            .map(|s| (s.key.clone(), s.default.clone()))
            .collect()
    }

    /// 逐文件 SHA-256 校验（Q4：M1 只做完整性，不引入签名）。
    pub fn verify_integrity(&self, dir: &Path) -> Result<(), AppError> {
        let integrity = self.integrity.as_ref().ok_or_else(|| {
            AppError::PluginLoad("缺少 integrity 清单（M1 要求逐文件 SHA-256）".to_string())
        })?;

        if !integrity.algorithm.eq_ignore_ascii_case("sha256") {
            return Err(AppError::PluginLoad(format!(
                "不支持的完整性算法：{}",
                integrity.algorithm
            )));
        }
        if integrity.files.is_empty() {
            return Err(AppError::PluginLoad(
                "integrity.files 为空，拒绝加载".to_string(),
            ));
        }

        for (rel, expected) in &integrity.files {
            let rel_norm = rel.replace('\\', "/");
            if rel_norm.starts_with('/') || rel_norm.contains("..") || rel_norm.contains(':') {
                return Err(AppError::PluginLoad(format!("integrity 路径非法：{rel}")));
            }
            let file = dir.join(rel);
            let bytes = std::fs::read(&file).map_err(|e| {
                AppError::fs(format!("读取待校验文件失败（{}）", file.display()), e)
            })?;

            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            let actual = hex_lower(&hasher.finalize());

            if !actual.eq_ignore_ascii_case(expected.trim()) {
                return Err(AppError::PluginLoad(format!(
                    "完整性校验失败：{rel}\n    期望 {}\n    实际 {actual}",
                    expected.trim()
                )));
            }
        }
        Ok(())
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// P0-B′：verify_integrity（SHA-256 完整性闸门）单元测试
//
// 测试卫生：全部使用 tempfile::tempdir()，测试结束自动清理，绝不触碰真实下载目录。
// 本模块**只补测试**，不改动 verify_integrity 的实现。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;

    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::{hex_lower, API_VERSION, ENGINE_JS, ENGINE_WASM, Integrity, Limits, Manifest};

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex_lower(&hasher.finalize())
    }

    /// 按给定完整性清单构造一个最小可用的 Manifest。
    fn manifest_with(files: Option<HashMap<String, String>>) -> Manifest {
        Manifest {
            plugin_type: super::TYPE_PLUGIN.to_string(),
            id: "com.test.tmp".to_string(),
            name: "Test".to_string(),
            description: String::new(),
            author: String::new(),
            icon: None,
            version: "1.0.0".to_string(),
            api_version: API_VERSION.to_string(),
            entry: "main.js".to_string(),
            engine: ENGINE_JS.to_string(),
            priority: 100,
            hooks: Vec::new(),
            permissions: Vec::new(),
            settings: Vec::new(),
            actions: Vec::new(),
            limits: Limits::default(),
            routing: Vec::new(),
            activation_events: super::ActivationEvents::default(),
            integrity: files.map(|files| Integrity {
                algorithm: "sha256".to_string(),
                files,
            }),
        }
    }

    #[test]
    fn missing_integrity_is_rejected() {
        let dir = tempdir().unwrap();
        assert!(
            manifest_with(None).verify_integrity(dir.path()).is_err(),
            "缺少 integrity 清单必须拒绝加载"
        );
    }

    #[test]
    fn empty_files_list_is_rejected() {
        let dir = tempdir().unwrap();
        let err = manifest_with(Some(HashMap::new()))
            .verify_integrity(dir.path())
            .unwrap_err();
        assert!(
            err.to_string().contains("integrity.files"),
            "错误信息应点明空清单：{err}"
        );
    }

    #[test]
    fn single_file_hash_matches() {
        let dir = tempdir().unwrap();
        let bytes = b"console.log('hi');";
        fs::write(dir.path().join("main.js"), bytes).unwrap();

        let mut files = HashMap::new();
        files.insert("main.js".to_string(), sha256_hex(bytes));
        assert!(manifest_with(Some(files)).verify_integrity(dir.path()).is_ok());
    }

    #[test]
    fn single_file_hash_mismatch_is_rejected_with_filename() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("main.js"), b"real-content").unwrap();

        let mut files = HashMap::new();
        files.insert("main.js".to_string(), sha256_hex(b"tampered-content"));
        let err = manifest_with(Some(files))
            .verify_integrity(dir.path())
            .unwrap_err();
        assert!(
            err.to_string().contains("main.js"),
            "错误信息应含不匹配的文件名：{err}"
        );
    }

    #[test]
    fn multi_file_all_match() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("lib")).unwrap();
        let entry = b"entry-script";
        let util = b"util-script";
        fs::write(dir.path().join("main.js"), entry).unwrap();
        fs::write(dir.path().join("lib").join("util.js"), util).unwrap();

        let mut files = HashMap::new();
        files.insert("main.js".to_string(), sha256_hex(entry));
        files.insert("lib/util.js".to_string(), sha256_hex(util));
        assert!(manifest_with(Some(files)).verify_integrity(dir.path()).is_ok());
    }

    #[test]
    fn multi_file_one_mismatch_is_rejected_with_filename() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("lib")).unwrap();
        let entry = b"entry-script";
        let util = b"util-script";
        fs::write(dir.path().join("main.js"), entry).unwrap();
        fs::write(dir.path().join("lib").join("util.js"), util).unwrap();

        let mut files = HashMap::new();
        files.insert("main.js".to_string(), sha256_hex(entry));
        // 故意写错 util.js 的期望值
        files.insert("lib/util.js".to_string(), sha256_hex(b"something-else"));
        let err = manifest_with(Some(files))
            .verify_integrity(dir.path())
            .unwrap_err();
        assert!(
            err.to_string().contains("lib/util.js"),
            "错误信息应含出错的文件名：{err}"
        );
    }

    #[test]
    fn listed_file_missing_on_disk_is_rejected() {
        let dir = tempdir().unwrap();
        let mut files = HashMap::new();
        files.insert("ghost.js".to_string(), sha256_hex(b"x"));
        let err = manifest_with(Some(files))
            .verify_integrity(dir.path())
            .unwrap_err();
        assert!(
            err.to_string().contains("ghost.js"),
            "错误信息应含缺失的文件名：{err}"
        );
    }

    /// 未声明 engine 的旧 manifest 必须默认 `js`（既有插件零感知）。
    #[test]
    fn engine_defaults_to_js_when_absent() {
        let json = r#"{
            "id": "com.test.legacy",
            "name": "Legacy",
            "version": "1.0.0",
            "entry": "main.js"
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.engine, ENGINE_JS, "缺省 engine 必须是 js");
        assert_eq!(m.api_version, API_VERSION, "apiVersion 也应走默认值");
        m.validate().unwrap();
    }

    /// 显式声明 `wasm` 必须被正确读出并通过校验。
    #[test]
    fn engine_wasm_is_parsed() {
        let json = r#"{
            "id": "com.test.wasm",
            "name": "Wasm",
            "version": "1.0.0",
            "entry": "plugin.wat",
            "engine": "wasm"
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.engine, ENGINE_WASM);
        m.validate().unwrap();
    }

    /// beta10：apiVersion 1.1 必须被接受，且 routing / limits 能正确解析。
    #[test]
    fn api_version_1_1_with_routing_is_accepted() {
        let json = r#"{
            "id": "com.test.hls",
            "name": "HLS",
            "version": "1.0.0",
            "entry": "main.js",
            "apiVersion": "1.1",
            "permissions": ["net:http", "downloads:write", "exec:ffmpeg"],
            "limits": { "taskTimeoutMs": 600000, "maxHttpBytes": 1048576, "maxTasksPerCall": 32 },
            "routing": [{ "pattern": "*.m3u8", "priority": 10, "handler": "onTaskDetected" }]
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        m.validate().unwrap();
        assert_eq!(m.limits.task_timeout_ms, 600_000);
        assert_eq!(m.limits.max_http_bytes, 1_048_576);
        assert_eq!(m.limits.max_tasks_per_call, 32);
        assert_eq!(m.routing.len(), 1);
        assert_eq!(m.routing[0].pattern, "*.m3u8");
        assert_eq!(m.routing[0].handler, "onTaskDetected");
    }

    /// 空 pattern 的路由规则必须拒绝（否则匹配静默失效）。
    #[test]
    fn empty_routing_pattern_is_rejected() {
        let json = r#"{
            "id": "com.test.bad-route",
            "name": "Bad",
            "version": "1.0.0",
            "entry": "main.js",
            "apiVersion": "1.1",
            "routing": [{ "pattern": "  " }]
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("routing"), "应点明 routing：{err}");
    }

    /// 非 1 / 1.1 的 apiVersion 仍然拒绝。
    #[test]
    fn unknown_api_version_is_rejected() {
        let json = r#"{
            "id": "com.test.future",
            "name": "Future",
            "version": "1.0.0",
            "entry": "main.js",
            "apiVersion": "2.0"
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert!(m.validate().is_err());
    }

    /// 未知 engine 必须在 validate 阶段就被拒绝（fail fast，而非拖到构造运行时）。
    #[test]
    fn unknown_engine_is_rejected() {
        let json = r#"{
            "id": "com.test.bad",
            "name": "Bad",
            "version": "1.0.0",
            "entry": "main.js",
            "engine": "lua"
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        let err = m.validate().unwrap_err();
        assert!(
            err.to_string().contains("engine"),
            "错误信息应点明 engine：{err}"
        );
    }

    /// beta11 · A1：反向域名 id 的边界用例。
    #[test]
    fn reverse_domain_id_validation() {
        for valid in [
            "com.harness.hls-downloader",
            "io.github.kralisxc.youtube-downloader",
            "com.test.tmp",
            "a.b",
        ] {
            assert!(super::is_valid_plugin_id(valid), "应通过：{valid}");
        }
        for invalid in [
            "HLS-Downloader",
            "com_harness_hls",
            "com..x",
            "com.harness.",
            "hls",
            "-com.x",
            "com.x-",
            "com.harness.hls downloader",
            "com/harness",
        ] {
            assert!(!super::is_valid_plugin_id(invalid), "应拒绝：{invalid}");
        }
    }

    /// beta11 · A1：加载期非法 id 必须拒绝。
    #[test]
    fn invalid_id_is_rejected_on_load() {
        let json = r#"{
            "id": "HLS-Downloader",
            "name": "Bad",
            "version": "1.0.0",
            "entry": "main.js"
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("反向域名"), "{err}");
    }

    /// beta11 · C1：`settings_schema` 别名与 textarea 必须被接受。
    #[test]
    fn settings_schema_alias_and_textarea_are_accepted() {
        let json = r#"{
            "id": "com.test.settings",
            "name": "Settings",
            "version": "1.0.0",
            "entry": "main.js",
            "author": "Tester",
            "icon": "icon.png",
            "settings_schema": [
                { "key": "cookie", "label": "登录 Cookie", "type": "textarea", "default": "" },
                { "key": "quality", "label": "最高画质", "type": "select", "options": ["1080P", "4K"], "default": "1080P" }
            ],
            "actions": [{ "id": "to-mp4", "label": "转成 MP4" }]
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        m.validate().unwrap();
        assert_eq!(m.author, "Tester");
        assert_eq!(m.icon.as_deref(), Some("icon.png"));
        assert_eq!(m.settings.len(), 2);
        assert_eq!(m.settings[0].kind, "textarea");
        assert_eq!(m.actions.len(), 1);
    }

    /// beta12 · 模块 E：懒激活域名可正常解析并通过校验。
    #[test]
    fn activation_events_domains_are_parsed() {
        let json = r#"{
            "id": "com.test.lazy",
            "name": "Lazy",
            "version": "1.0.0",
            "entry": "main.js",
            "activationEvents": { "domains": ["*.bilibili.com", "www.twitch.tv"] }
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        m.validate().unwrap();
        assert_eq!(m.activation_events.domains.len(), 2);
        assert_eq!(m.activation_events.domains[0], "*.bilibili.com");
    }

    /// beta12 · 模块 E：不可匹配的域名规则必须在加载期拒绝。
    #[test]
    fn invalid_activation_domains_are_rejected() {
        for domain in ["", "  ", "*", "https://bilibili.com", "bilibili.com/x", "bilibili.com:443", "a b.com"] {
            let json = format!(
                r#"{{"id":"com.test.bad-domain","name":"Bad","version":"1.0.0","entry":"main.js","activationEvents":{{"domains":["{domain}"]}}}}"#
            );
            let m: Manifest = serde_json::from_str(&json).unwrap();
            let err = m.validate().unwrap_err();
            assert!(
                err.to_string().contains("activationEvents"),
                "应点明 activationEvents（domain=`{domain}`）：{err}"
            );
        }
    }

    /// beta12.2：未声明 type 的旧 manifest 必须默认 plugin（行为零变化）。
    #[test]
    fn type_defaults_to_plugin_when_absent() {
        let json = r#"{
            "id": "com.test.legacy-type",
            "name": "Legacy",
            "version": "1.0.0",
            "entry": "main.js"
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.plugin_type, super::TYPE_PLUGIN);
        m.validate().unwrap();
    }

    /// beta12.2：shared 包无需 entry / main.js，只要求 id / name / version / type。
    #[test]
    fn shared_manifest_allows_missing_entry() {
        let json = r#"{
            "id": "com.harness.shared-ffmpeg",
            "name": "共享 FFmpeg",
            "version": "7.1.0",
            "type": "shared"
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.plugin_type, super::TYPE_SHARED);
        assert!(m.entry.is_empty(), "shared 包 entry 可为空");
        m.validate().unwrap();
    }

    /// beta12.2：shared 包的 name / version 仍必须非空。
    #[test]
    fn shared_manifest_requires_name_and_version() {
        let json = r#"{"id":"com.harness.shared-x","name":" ","version":"1.0.0","type":"shared"}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert!(m.validate().is_err(), "空 name 必须拒绝");

        let json = r#"{"id":"com.harness.shared-x","name":"X","version":"","type":"shared"}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert!(m.validate().is_err(), "空 version 必须拒绝");
    }

    /// beta12.2：未知 type 必须在加载期拒绝（不允许静默退化成 plugin）。
    #[test]
    fn unknown_type_is_rejected() {
        let json = r#"{"id":"com.test.bad-type","name":"Bad","version":"1.0.0","entry":"main.js","type":"library"}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("manifest.type"), "应点明 type：{err}");
    }
}
