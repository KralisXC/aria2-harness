//! 插件授权与启用状态持久化（`plugins/registry.json`）。
//!
//! 结构：每个插件一条记录 `{ id, enabled, granted_permissions }`。
//!
//! 语义（M2.5 安全加固）：
//! - `enabled`：用户开关。禁用 = 挂起（线程存活但不再派发事件）。
//! - `granted_permissions`：**已授权**的能力令牌快照。首次加载某插件时自动写入；
//!   此后若 manifest 申请了超出该快照的**新增权限**，插件将被默认禁用并要求用户重新授权
//!   —— 防"静默提权"（插件更新后偷偷拿到新能力）。

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::AppError;

/// registry.json 文件名。
const REGISTRY_FILE: &str = "registry.json";

/// 单个插件的授权与启用记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRecord {
    pub id: String,
    /// 用户开关；缺省视为启用。
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// 已授权能力令牌快照（用于检测 manifest 的新增未授权权限）。
    #[serde(default)]
    pub granted_permissions: Vec<String>,
    /// 用户保存的插件设置（key -> 任意 JSON 值）。
    ///
    /// 与 manifest.settings 的 schema 对齐；未保存过的 key 不在表中，
    /// 加载时由 manifest 默认值补全。老记录缺该字段时 serde 默认空表。
    #[serde(default)]
    pub settings: HashMap<String, Value>,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub plugins: Vec<PluginRecord>,
}

impl Registry {
    /// 从插件目录读取；文件缺失或损坏一律降级为空账本（不阻断宿主启动）。
    ///
    /// 注：旧版结构 `{"disabled":[...]}` 会被 serde 忽略未知字段解析为**空账本**，
    /// 其效果等价于"所有插件按首次加载重新授权并启用"。
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(REGISTRY_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                eprintln!(
                    "[plugin] registry.json 解析失败（{}），本次按空账本处理：{e}",
                    path.display()
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// 查询某插件的记录（克隆返回，便于调用方随后做可变操作）。
    pub fn record(&self, id: &str) -> Option<&PluginRecord> {
        self.plugins.iter().find(|r| r.id == id)
    }

    /// 计算 manifest 申请、但**授权快照里没有**的新增权限。
    ///
    /// 首次加载（无记录）返回空 —— 首次授权由调用方 `insert` 直接完成，不计为"新增"。
    pub fn new_permissions(&self, id: &str, requested: &[String]) -> Vec<String> {
        match self.record(id) {
            None => Vec::new(),
            Some(rec) => requested
                .iter()
                .filter(|p| !rec.granted_permissions.contains(p))
                .cloned()
                .collect(),
        }
    }

    /// 更新某插件的启用态（不存在则新建空授权记录）。
    pub fn set_enabled(&mut self, id: &str, enabled: bool) {
        match self.plugins.iter_mut().find(|r| r.id == id) {
            Some(rec) => rec.enabled = enabled,
            None => self.plugins.push(PluginRecord {
                id: id.to_string(),
                enabled,
                granted_permissions: Vec::new(),
                settings: HashMap::new(),
            }),
        }
    }

    /// 读取某插件已保存的设置（不存在返回空表）。
    pub fn settings_of(&self, id: &str) -> HashMap<String, Value> {
        self.record(id)
            .map(|rec| rec.settings.clone())
            .unwrap_or_default()
    }

    /// 覆盖写入某插件的设置快照（不存在记录时补建，保持启用态）。
    ///
    /// beta11（C4）起运行时设置以 `data/plugins/configs/<id>.json` 为准，
    /// 本方法仅保留给旧 `registry.json` 记录的兼容/迁移路径，暂无调用方。
    #[allow(dead_code)]
    pub fn set_settings(&mut self, id: &str, settings: HashMap<String, Value>) {
        match self.plugins.iter_mut().find(|r| r.id == id) {
            Some(rec) => rec.settings = settings,
            None => self.plugins.push(PluginRecord {
                id: id.to_string(),
                enabled: true,
                granted_permissions: Vec::new(),
                settings,
            }),
        }
    }

    /// 写入/刷新授权快照（用户显式启用即视为"重新授权"）。
    pub fn grant(&mut self, id: &str, permissions: &[String]) {
        match self.plugins.iter_mut().find(|r| r.id == id) {
            Some(rec) => rec.granted_permissions = permissions.to_vec(),
            None => self.plugins.push(PluginRecord {
                id: id.to_string(),
                enabled: true,
                granted_permissions: permissions.to_vec(),
                settings: HashMap::new(),
            }),
        }
    }

    /// 首次加载落库：建立授权快照 + 设定启用态。
    pub fn insert(&mut self, id: &str, enabled: bool, permissions: &[String]) {
        self.grant(id, permissions);
        self.set_enabled(id, enabled);
    }

    /// 删除某插件的整条记录（beta11 · B13：卸载时清理授权账本）。
    pub fn remove(&mut self, id: &str) {
        self.plugins.retain(|record| record.id != id);
    }

    /// 写回磁盘：先写 `.tmp` 再原子替换，避免半写损坏。
    pub fn save(&self, dir: &Path) -> Result<(), AppError> {
        let path = dir.join(REGISTRY_FILE);
        let tmp = dir.join(format!("{REGISTRY_FILE}.tmp"));
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&tmp, text)
            .map_err(|e| AppError::fs(format!("写入 registry 失败（{}）", tmp.display()), e))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| AppError::fs(format!("替换 registry 失败（{}）", path.display()), e))
    }
}
