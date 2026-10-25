//! manifest.json 解析、校验与 SHA-256 完整性验证（Q4：M1 只做哈希，不含签名）。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::AppError;

/// 宿主插件 API 版本。manifest.apiVersion 不匹配即拒绝加载。
pub const API_VERSION: &str = "1";

/// 运行时后端标识（增量 25）。
///
/// 默认 `js`（QuickJS）：既有插件零感知，**Wasm 绝不成为默认后端**（增量 25 红线 2）。
pub const ENGINE_JS: &str = "js";
/// Wasmtime 强隔离后端；需对应插件通过 manifest 显式声明，且宿主须带
/// `wasm-runtime` feature 构建。
pub const ENGINE_WASM: &str = "wasm";

const DEFAULT_TIMEOUT_MS: u64 = 2000;
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
    pub id: String,
    pub name: String,
    /// 插件简介（插件管理 UI 展示用）。
    #[serde(default)]
    pub description: String,
    pub version: String,
    /// 宿主插件 API 版本，用于兼容性判定。
    #[serde(default = "default_api_version")]
    pub api_version: String,
    /// 入口脚本（相对插件根目录，禁止绝对路径与 `..` 穿越）。
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
    /// 设置项 schema（M1 只取 key + default，用于 ctx.settings）。
    #[serde(default)]
    pub settings: Vec<SettingSpec>,
    /// 资源上限（宿主另有硬上限兜底）。
    #[serde(default)]
    pub limits: Limits,
    /// 逐文件完整性清单（加载前校验）。
    #[serde(default)]
    pub integrity: Option<Integrity>,
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
    /// 控件类型：`boolean` / `string` / `select`（缺省按 string 处理）。
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

fn default_priority() -> i64 {
    DEFAULT_PRIORITY
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
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
        if self.id.trim().is_empty() {
            return Err(AppError::PluginLoad("manifest.id 不能为空".to_string()));
        }
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
        if self.api_version != API_VERSION {
            return Err(AppError::PluginLoad(format!(
                "apiVersion 不兼容：宿主需要 {API_VERSION}，插件声明 {}",
                self.api_version
            )));
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
                "boolean" | "string" => {}
                "select" if !spec.options.is_empty() => {}
                "select" => {
                    return Err(AppError::PluginLoad(format!(
                        "设置项 {} 的类型为 select，但 options 为空",
                        spec.key
                    )));
                }
                other => {
                    return Err(AppError::PluginLoad(format!(
                        "设置项 {} 的类型非法：`{other}`（仅支持 boolean/string/select）",
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
            id: "com.test.tmp".to_string(),
            name: "Test".to_string(),
            description: String::new(),
            version: "1.0.0".to_string(),
            api_version: API_VERSION.to_string(),
            entry: "main.js".to_string(),
            engine: ENGINE_JS.to_string(),
            priority: 100,
            hooks: Vec::new(),
            permissions: Vec::new(),
            settings: Vec::new(),
            limits: Limits::default(),
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
}
