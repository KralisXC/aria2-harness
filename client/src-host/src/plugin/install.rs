//! 插件安装（beta10.1，beta12.1 批量化）：拖入 `.zip` 或文件夹后的合法性校验 + 落盘 + 预加载回滚。
//!
//! ## 安装契约（顺序即校验顺序）
//!
//! 1. **安全校验**：zip 解压 / 文件夹写入时逐条校验相对路径 —— 拒绝绝对路径、
//!    盘符、`..` 穿越（Zip Slip），拒绝符号链接；限制条目数与解压总量（zip bomb）。
//! 2. **结构校验**：有效根目录必须同时存在 `manifest.json` 与 `main.js`
//!    （zip 若只有一个包装目录，会自动下钻一层）。
//! 3. **批量安装**（beta12.1）：压缩包内**每个包含 `manifest.json` 的子目录**
//!    都视为一个独立插件，逐个安装；单个失败不影响其余插件。
//! 4. **字段校验**：manifest 必须能解析，且 `id` / `version` / `main`（兼容既有
//!    的 `entry`）非空。
//! 5. **预加载校验**：目录移入 `data/plugins/<id>` 后交给 `PluginManager::reload_dir`
//!    在 QuickJS 沙箱里试运行；抛异常则删除新目录并恢复被替换的旧版本。
//!
//! 全部通过才返回成功 —— 前端只在这之后提示「安装成功」。

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::AppError;

use super::manager::PluginManager;
use super::manifest::{Manifest, TYPE_PLUGIN, TYPE_SHARED};

/// 单文件解压上限（ffmpeg 等二进制也够用）。
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// 单个插件解压后的总量上限（防 zip bomb）。
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
/// 条目数上限（防海量小文件拖垮安装）。
const MAX_ENTRIES: usize = 4000;
/// 自动生成完整性清单时纳入哈希的脚本后缀。
///
/// 懒加载红线（beta11 · A3）：只哈希脚本扩展名，**大文件（`bin/`、`models/` 等）
/// 不纳入哈希**，避免安装期做全量 IO 与内存拷贝。
const HASH_EXTENSIONS: [&str; 5] = ["js", "mjs", "cjs", "wat", "wasm"];

/// 前端上传的单文件（文件夹安装通道）：相对路径 + base64 内容。
#[derive(Debug, Clone, Deserialize)]
pub struct InstallFile {
    pub path: String,
    pub data: String,
}

/// 安装结果（前端用于提示与刷新插件列表）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallOutcome {
    pub id: String,
    pub name: String,
    pub version: String,
    pub dir: String,
    /// beta12.2：安装类型 —— `"plugin"`（`plugins/<id>`）或 `"shared"`（`.shared/<id>`）。
    /// 前端据此分组提示（普通插件 / 共享组件）。
    pub kind: String,
    /// 是否覆盖安装了同 id 的旧版本。
    pub replaced: bool,
}

/// 单个插件安装失败项（批量安装时其余插件继续）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallFailure {
    /// 失败目录名（manifest 无法解析时仍能定位是哪个包）。
    pub plugin: String,
    pub error: String,
}

/// 批量安装结果：一次可装入压缩包内的多个插件。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallBatchOutcome {
    pub installed: Vec<InstallOutcome>,
    pub failed: Vec<InstallFailure>,
}

/// 从 zip 字节流安装（支持包内多插件）。
///
/// `allow_exec_local`：用户在红色安全确认弹窗中点“信任并安装”后由前端置 `true`；
/// 为 `false` 且包内存在申请 `exec:local` 的插件时返回
/// [`AppError::ConsentRequired`]，此时**尚未移动任何目录**（staging 随后清理），
/// 因此"拒绝 = 中止 + 零残留"，无需额外回滚。
pub fn install_zip(
    manager: &PluginManager,
    bytes: &[u8],
    allow_exec_local: bool,
) -> Result<InstallBatchOutcome, AppError> {
    let staging = create_staging()?;
    let result = (|| {
        extract_zip(bytes, &staging)?;
        require_exec_local_consent(&staging, allow_exec_local)?;
        finalize_all(manager, &staging)
    })();
    remove_dir_quietly(&staging);
    result
}

/// 从「文件夹拖入」的逐文件内容安装（同样支持多插件目录）。
pub fn install_files(
    manager: &PluginManager,
    files: Vec<InstallFile>,
    allow_exec_local: bool,
) -> Result<InstallBatchOutcome, AppError> {
    let staging = create_staging()?;
    let result = (|| {
        write_files(files, &staging)?;
        require_exec_local_consent(&staging, allow_exec_local)?;
        finalize_all(manager, &staging)
    })();
    remove_dir_quietly(&staging);
    result
}

// ---------------------------------------------------------------------------
// 安装前安全确认（1.0.0-rc1 · 模块一 · exec:local）
// ---------------------------------------------------------------------------

/// 安装前预检：包内若有插件申请 `exec:local` 且用户未确认，返回 `ConsentRequired`。
///
/// 只读 staging 里的原始 `manifest.json`（此时尚未校验 / 改写），
/// 因此损坏的 manifest 由后续 `validate_root` 报告，这里静默跳过。
fn require_exec_local_consent(staging: &Path, allow_exec_local: bool) -> Result<(), AppError> {
    if allow_exec_local {
        return Ok(());
    }
    let roots = resolve_roots(staging)?;
    let names = detect_exec_local(&roots);
    if names.is_empty() {
        return Ok(());
    }
    Err(AppError::ConsentRequired(format!(
        "（{}）",
        names.join("、")
    )))
}

/// 扫描各插件根目录，收集申请了 `exec:local` 的插件名（无命中返回空表）。
pub fn detect_exec_local(roots: &[PathBuf]) -> Vec<String> {
    let mut names = Vec::new();
    for root in roots {
        let path = root.join("manifest.json");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let has_exec_local = value
            .get("permissions")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .any(|permission| permission.trim() == "exec:local")
            })
            .unwrap_or(false);
        if !has_exec_local {
            continue;
        }
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .or_else(|| value.get("id").and_then(Value::as_str))
            .unwrap_or("未知插件")
            .to_string();
        names.push(name);
    }
    names
}

// ---------------------------------------------------------------------------
// 解压 / 写入（安全校验核心）
// ---------------------------------------------------------------------------

fn extract_zip(bytes: &[u8], staging: &Path) -> Result<(), AppError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| AppError::PluginLoad(format!("无法读取 zip（文件已损坏？）：{e}")))?;

    if archive.len() > MAX_ENTRIES {
        return Err(AppError::PluginLoad(format!(
            "zip 条目过多（{} > {MAX_ENTRIES}），拒绝安装",
            archive.len()
        )));
    }

    let mut total: u64 = 0;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| AppError::PluginLoad(format!("读取 zip 条目 {index} 失败：{e}")))?;

        // `enclosed_name` 已做路径穿越判定；为满足「显式拒绝 ..」的契约，
        // 这里再按原始名字复核一次（含反斜杠与冒号）。
        let raw_name = entry
            .name()
            .map_err(|e| AppError::PluginLoad(format!("读取 zip 条目名失败：{e}")))?
            .into_owned();
        if is_macos_junk(&raw_name) {
            continue;
        }
        if !entry.is_dir() && entry.enclosed_name().is_none() {
            return Err(AppError::PluginLoad(format!(
                "zip 中存在非法路径（疑似路径穿越，已拒绝）：{raw_name}"
            )));
        }

        // 符号链接一律拒绝：解压后可能指向插件目录之外。
        if let Some(mode) = entry.unix_mode() {
            if mode & 0o170000 == 0o120000 {
                return Err(AppError::PluginLoad(format!(
                    "zip 中包含符号链接，拒绝安装：{raw_name}"
                )));
            }
        }

        let rel = safe_rel_path(&raw_name)?;
        let dest = staging.join(&rel);

        if entry.is_dir() {
            std::fs::create_dir_all(&dest).map_err(|e| {
                AppError::fs(format!("创建目录失败（{}）", dest.display()), e)
            })?;
            continue;
        }

        if entry.size() > MAX_FILE_BYTES {
            return Err(AppError::PluginLoad(format!(
                "zip 内文件过大（{} > 64MB）：{raw_name}",
                entry.size()
            )));
        }
        total = total.saturating_add(entry.size());
        if total > MAX_TOTAL_BYTES {
            return Err(AppError::PluginLoad(format!(
                "zip 解压后总量超过 256MB，拒绝安装（疑似 zip bomb）：{raw_name}"
            )));
        }

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                AppError::fs(format!("创建目录失败（{}）", parent.display()), e)
            })?;
        }

        // 再按实际读到的字节复核一次上限：`entry.size()` 来自头部，可被伪造。
        let mut buffer = Vec::new();
        let mut limited = (&mut entry).take(MAX_FILE_BYTES + 1);
        limited.read_to_end(&mut buffer).map_err(|e| {
            AppError::fs(format!("解压文件失败（{raw_name}）"), e)
        })?;
        if buffer.len() as u64 > MAX_FILE_BYTES {
            return Err(AppError::PluginLoad(format!(
                "zip 内文件解压后超过 64MB，拒绝安装：{raw_name}"
            )));
        }
        std::fs::write(&dest, &buffer)
            .map_err(|e| AppError::fs(format!("写入文件失败（{}）", dest.display()), e))?;
    }
    Ok(())
}

fn write_files(files: Vec<InstallFile>, staging: &Path) -> Result<(), AppError> {
    if files.is_empty() {
        return Err(AppError::InvalidArgs("安装失败：没有收到任何文件".to_string()));
    }
    if files.len() > MAX_ENTRIES {
        return Err(AppError::PluginLoad(format!(
            "文件数量过多（{} > {MAX_ENTRIES}），拒绝安装",
            files.len()
        )));
    }
    let mut total: u64 = 0;
    for file in files {
        if is_macos_junk(&file.path) {
            continue;
        }
        let rel = safe_rel_path(&file.path)?;
        let bytes = decode_base64(&file.data)?;
        total = total.saturating_add(bytes.len() as u64);
        if bytes.len() as u64 > MAX_FILE_BYTES || total > MAX_TOTAL_BYTES {
            return Err(AppError::PluginLoad(format!(
                "安装内容过大，拒绝安装（单文件上限 64MB / 总量上限 256MB）：{}",
                file.path
            )));
        }
        let dest = staging.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                AppError::fs(format!("创建目录失败（{}）", parent.display()), e)
            })?;
        }
        std::fs::write(&dest, &bytes)
            .map_err(|e| AppError::fs(format!("写入文件失败（{}）", dest.display()), e))?;
    }
    Ok(())
}

/// 相对路径安全校验：拒绝绝对路径 / 盘符 / `..` / 空路径。
///
/// 同时把 `\` 统一成 `/`（Windows 打包工具常写反斜杠），避免绕过判定。
fn safe_rel_path(raw: &str) -> Result<PathBuf, AppError> {
    let normalized = raw.replace('\\', "/");
    let trimmed = normalized.trim_matches('/');
    if normalized.starts_with('/') || normalized.contains(':') {
        return Err(AppError::PluginLoad(format!(
            "安装包中存在绝对路径，拒绝安装：{raw}"
        )));
    }
    if trimmed.is_empty() {
        return Err(AppError::PluginLoad("安装包中存在空路径，拒绝安装".to_string()));
    }
    let mut rel = PathBuf::new();
    for component in trimmed.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return Err(AppError::PluginLoad(format!(
                "安装包中存在路径穿越（..），已拒绝：{raw}"
            )));
        }
        rel.push(component);
    }
    if rel.as_os_str().is_empty() {
        return Err(AppError::PluginLoad("安装包中存在空路径，拒绝安装".to_string()));
    }
    Ok(rel)
}

/// macOS 打包残留：与插件无关，直接忽略。
fn is_macos_junk(name: &str) -> bool {
    let normalized = name.replace('\\', "/");
    normalized.starts_with("__MACOSX/") || normalized.ends_with("/.DS_Store")
}

// ---------------------------------------------------------------------------
// 结构与字段校验
// ---------------------------------------------------------------------------

/// 找到所有真正的插件根目录（beta12.1：支持批量安装）。
///
/// - `staging` 自身有 manifest → 单插件（zip 根目录直接包含 manifest）。
/// - 否则递归下钻：**每个包含 manifest.json 的目录都视为一个独立插件**，
///   命中后不再继续向下（避免把插件内部资源目录误判成第二个插件）。
fn resolve_roots(staging: &Path) -> Result<Vec<PathBuf>, AppError> {
    if staging.join("manifest.json").is_file() {
        return Ok(vec![staging.to_path_buf()]);
    }
    let mut roots = Vec::new();
    collect_roots(staging, &mut roots)?;
    if roots.is_empty() {
        return Err(AppError::PluginLoad(
            "结构校验失败：根目录缺少 manifest.json".to_string(),
        ));
    }
    roots.sort();
    Ok(roots)
}

fn collect_roots(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), AppError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| AppError::fs(format!("读取临时目录失败（{}）", dir.display()), e))?;
    let mut children: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    children.sort();
    for child in children {
        // 点目录是安装 / 备份临时目录，__MACOSX 是打包残留，都不当插件。
        let name = child
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if name.starts_with('.') || name == "__MACOSX" {
            continue;
        }
        if child.join("manifest.json").is_file() {
            out.push(child);
        } else {
            collect_roots(&child, out)?;
        }
    }
    Ok(())
}

/// 结构 + 字段校验，并在需要时补齐 `entry` 与完整性清单。
///
/// beta12.2 · 模块二：先读 manifest 的 `type` —— `shared` 只做字段 / 路径安全校验
/// （不要求 `main.js`、不生成 integrity）；其余沿用普通插件契约。
fn validate_root(root: &Path) -> Result<(), AppError> {
    let manifest_path = root.join("manifest.json");
    if !manifest_path.is_file() {
        return Err(AppError::PluginLoad(
            "结构校验失败：根目录缺少 manifest.json".to_string(),
        ));
    }

    let raw = std::fs::read_to_string(&manifest_path)
        .map_err(|e| AppError::fs(format!("读取 manifest 失败（{}）", manifest_path.display()), e))?;
    let mut value: Value = serde_json::from_str(&raw)
        .map_err(|e| AppError::PluginLoad(format!("manifest.json 不是合法 JSON：{e}")))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| AppError::PluginLoad("manifest.json 必须是 JSON 对象".to_string()))?;

    let is_shared = type_is_shared(object)?;
    let id = require_string(object.get("id"), "id")?;

    // beta11 · A1：id 必须满足反向域名规范，并被用作落盘目录名。
    if !super::manifest::is_valid_plugin_id(&id) {
        return Err(AppError::PluginLoad(format!(
            "字段校验失败：manifest.id 必须是反向域名（仅小写字母/数字/-/.）：{id}"
        )));
    }

    if is_shared {
        // 共享包只要求 id / name / version / type：无入口脚本、无完整性清单。
        require_string(object.get("name"), "name")?;
        let _version = require_string(object.get("version"), "version")?;
        // id 直接作为 `.shared/<id>` 目录名：再复核一次路径字符（防目录穿越）。
        if !is_safe_dir_name(&id) {
            return Err(AppError::PluginLoad(format!(
                "字段校验失败：共享包 id 含非法路径字符：{id}"
            )));
        }
        Manifest::from_dir(root)?;
        return Ok(());
    }

    // 用户契约：根目录必须有 main.js（插件入口的约定文件名）。
    if !root.join("main.js").is_file() {
        return Err(AppError::PluginLoad(
            "结构校验失败：根目录缺少 main.js".to_string(),
        ));
    }

    let _version = require_string(object.get("version"), "version")?;

    // 字段契约：`main` 与既有 `entry` 至少存在一个；只写了 `main` 时补成 `entry`，
    // 后续 Manifest / 加载器沿用同一套 `entry` 语义，不引入第二条入口路径。
    let has_entry = object
        .get("entry")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty());
    let main = object.get("main").and_then(Value::as_str).map(str::to_string);
    if !has_entry {
        let main = main
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| {
                AppError::PluginLoad("字段校验失败：manifest 缺少 main 字段".to_string())
            })?;
        object.insert("entry".to_string(), Value::String(main));
    }

    // 缺 integrity 时按脚本文件自动生成：既有的加载器把完整性当硬闸门，
    // 安装期是唯一的信任边界，在这里一次性算好哈希即可。
    ensure_integrity(root, object)?;

    let text = serde_json::to_string_pretty(&value)?;
    std::fs::write(&manifest_path, text)
        .map_err(|e| AppError::fs(format!("写回 manifest 失败（{}）", manifest_path.display()), e))?;

    // 交给既有解析器做完整校验（apiVersion / engine / routing / settings…）。
    Manifest::from_dir(root)?;
    Ok(())
}

fn require_string(value: Option<&Value>, field: &str) -> Result<String, AppError> {
    let text = value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| {
            AppError::PluginLoad(format!("字段校验失败：manifest 缺少 {field} 字段"))
        })?;
    Ok(text.to_string())
}

/// 判断 manifest 声明的包类型（beta12.2 安装分流）。
///
/// 缺省 = `plugin`（旧包零感知）；未知值在安装期就拒绝，避免把共享包当插件安装。
fn type_is_shared(object: &serde_json::Map<String, Value>) -> Result<bool, AppError> {
    match object.get("type").and_then(Value::as_str).map(str::trim) {
        Some(TYPE_SHARED) => Ok(true),
        Some(TYPE_PLUGIN) | None => Ok(false),
        Some(other) => Err(AppError::PluginLoad(format!(
            "字段校验失败：manifest.type 非法（仅支持 {TYPE_PLUGIN} / {TYPE_SHARED}）：{other}"
        ))),
    }
}

/// 目录名安全校验（beta12.2）：共享包 id 直接拼进 `.shared/<id>`，
/// 因此不得含 `..`、`/`、`\`、`:` 等任何路径字符。
fn is_safe_dir_name(name: &str) -> bool {
    !name.contains("..")
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains(':')
        && safe_rel_path(name).is_ok()
}

/// 若 manifest 没有 integrity.files，则对入口与脚本文件生成 SHA-256 清单。
fn ensure_integrity(root: &Path, object: &mut serde_json::Map<String, Value>) -> Result<(), AppError> {
    let existing = object
        .get("integrity")
        .and_then(Value::as_object)
        .and_then(|integrity| integrity.get("files"))
        .and_then(Value::as_object)
        .map(|files| !files.is_empty())
        .unwrap_or(false);
    if existing {
        return Ok(());
    }

    let mut files = serde_json::Map::new();
    collect_hashes(root, root, &mut files)?;
    if files.is_empty() {
        return Err(AppError::PluginLoad(
            "无法生成完整性清单：插件内没有可校验的脚本文件".to_string(),
        ));
    }
    object.insert(
        "integrity".to_string(),
        json!({ "algorithm": "sha256", "files": Value::Object(files) }),
    );
    Ok(())
}

/// 递归收集脚本文件的 SHA-256（懒加载红线 · beta11 · A3）。
///
/// 只遍历并读取 `HASH_EXTENSIONS` 命中的脚本文件；`bin/`、`models/` 等大文件
/// 直接跳过，避免安装期全量 IO。不要为了"更完整"而扩大哈希范围。
fn collect_hashes(
    root: &Path,
    dir: &Path,
    out: &mut serde_json::Map<String, Value>,
) -> Result<(), AppError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| AppError::fs(format!("读取插件目录失败（{}）", dir.display()), e))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_hashes(root, &path, out)?;
            continue;
        }
        let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
            continue;
        };
        if !HASH_EXTENSIONS.iter().any(|known| known.eq_ignore_ascii_case(ext)) {
            continue;
        }
        let Some(rel) = path.strip_prefix(root).ok() else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let bytes = std::fs::read(&path)
            .map_err(|e| AppError::fs(format!("读取文件失败（{}）", path.display()), e))?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let mut hex = String::with_capacity(64);
        for byte in hasher.finalize() {
            hex.push_str(&format!("{byte:02x}"));
        }
        out.insert(rel, Value::String(hex));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 落盘 + 预加载 + 回滚
// ---------------------------------------------------------------------------

/// 批量落盘：逐个安装，单个失败不影响其余插件。
///
/// beta12.2：同一批次可同时含 shared 包与普通插件 —— 先按 manifest 的 `type`
/// 分流，shared 落 `.shared/<id>`（不预加载），其余沿用插件流程。
fn finalize_all(manager: &PluginManager, staging: &Path) -> Result<InstallBatchOutcome, AppError> {
    let roots = resolve_roots(staging)?;
    let mut installed = Vec::new();
    let mut failed = Vec::new();
    for root in roots {
        let label = root
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("plugin")
            .to_string();
        let outcome = match root_is_shared(&root) {
            Ok(true) => finalize_shared(&root),
            Ok(false) => finalize_plugin(manager, &root),
            Err(error) => Err(error),
        };
        match outcome {
            Ok(outcome) => installed.push(outcome),
            Err(error) => failed.push(InstallFailure {
                plugin: label,
                error: error.to_string(),
            }),
        }
    }
    if installed.is_empty() {
        let detail = failed
            .iter()
            .map(|item| format!("{}：{}", item.plugin, item.error))
            .collect::<Vec<_>>()
            .join("；");
        return Err(AppError::PluginLoad(format!("安装失败：{detail}")));
    }
    Ok(InstallBatchOutcome { installed, failed })
}

/// 读取插件根 manifest 的 `type`（仅用于分流，完整校验仍在 `validate_root`）。
fn root_is_shared(root: &Path) -> Result<bool, AppError> {
    let path = root.join("manifest.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| AppError::fs(format!("读取 manifest 失败（{}）", path.display()), e))?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|e| AppError::PluginLoad(format!("manifest.json 不是合法 JSON：{e}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| AppError::PluginLoad("manifest.json 必须是 JSON 对象".to_string()))?;
    type_is_shared(object)
}

/// 安装单个插件目录：结构校验 → 备份旧版本 → 落盘 → 预加载 → 失败回滚。
fn finalize_plugin(manager: &PluginManager, root: &Path) -> Result<InstallOutcome, AppError> {
    validate_root(root)?;
    let manifest = Manifest::from_dir(root)?;
    let id = manifest.id.clone();
    let name = manifest.name.clone();
    let version = manifest.version.clone();

    let plugins_root = super::plugin_root();
    std::fs::create_dir_all(&plugins_root).map_err(|e| {
        AppError::fs(format!("创建插件目录失败（{}）", plugins_root.display()), e)
    })?;
    let target = plugin_dir_for(&id);

    // 覆盖安装：先把旧目录挪到隐藏备份，预加载失败时再原样恢复。
    let backup = if target.exists() {
        let backup = plugins_root.join(format!(".backup-{id}-{}", now_ms()));
        std::fs::rename(&target, &backup).map_err(|e| {
            AppError::fs(format!("备份旧版本失败（{}）", target.display()), e)
        })?;
        Some(backup)
    } else {
        None
    };
    let replaced = backup.is_some();

    std::fs::rename(root, &target)
        .map_err(|e| AppError::fs(format!("移入插件目录失败（{}）", target.display()), e))?;

    // 预加载：在 QuickJS 沙箱里真实跑一次 Load；失败即回滚。
    match manager.reload_dir(&target) {
        Ok(_) => {
            if let Some(backup) = backup {
                remove_dir_quietly(&backup);
            }
            Ok(InstallOutcome {
                id,
                name,
                version,
                dir: target.to_string_lossy().into_owned(),
                kind: TYPE_PLUGIN.to_string(),
                replaced,
            })
        }
        Err(err) => {
            remove_dir_quietly(&target);
            if let Some(backup) = backup {
                if std::fs::rename(&backup, &target).is_err() {
                    tracing::error!(
                        "插件安装回滚失败：无法恢复旧目录（{}）",
                        backup.display()
                    );
                } else {
                    // 旧版本目录回来了，把运行实例重新挂上（失败也不影响回滚结果）。
                    let _ = manager.reload_dir(&target);
                }
            }
            Err(AppError::PluginLoad(format!(
                "预加载校验失败，已回滚安装：{}",
                err
            )))
        }
    }
}

/// 插件落盘目录：**永远以 manifest.id 命名**（beta11 · A1），与用户上传的文件夹名无关。
pub(crate) fn plugin_dir_for(id: &str) -> PathBuf {
    super::plugin_root().join(id)
}

/// 共享包根目录：`data/plugins/.shared/`（beta12.2：废弃旧的扁平 `.shared/bin`）。
pub(crate) fn shared_root() -> PathBuf {
    super::plugin_root().join(".shared")
}

/// 安装单个共享包：校验 → 备份旧版本 → 移入 `.shared/<id>` → 删备份。
///
/// 共享包不含可执行脚本，**不做 QuickJS 预加载**（失败也无脚本可回滚）。
fn finalize_shared(root: &Path) -> Result<InstallOutcome, AppError> {
    validate_root(root)?;
    let manifest = Manifest::from_dir(root)?;
    if manifest.plugin_type != TYPE_SHARED {
        return Err(AppError::PluginLoad(
            "manifest.type 不是 shared，拒绝按共享包安装".to_string(),
        ));
    }
    let id = manifest.id.clone();
    let name = manifest.name.clone();
    let version = manifest.version.clone();

    let shared_root = shared_root();
    std::fs::create_dir_all(&shared_root).map_err(|e| {
        AppError::fs(format!("创建共享包目录失败（{}）", shared_root.display()), e)
    })?;
    let target = shared_root.join(&id);

    // 覆盖安装：旧目录先挪到隐藏备份，移入失败时原样恢复。
    let backup = if target.exists() {
        let backup = shared_root.join(format!(".backup-shared-{id}-{}", now_ms()));
        std::fs::rename(&target, &backup).map_err(|e| {
            AppError::fs(format!("备份旧共享包失败（{}）", target.display()), e)
        })?;
        Some(backup)
    } else {
        None
    };
    let replaced = backup.is_some();

    if let Err(e) = std::fs::rename(root, &target) {
        if let Some(backup) = &backup {
            if std::fs::rename(backup, &target).is_err() {
                tracing::error!(
                    "共享包安装回滚失败：无法恢复旧目录（{}）",
                    backup.display()
                );
            }
        }
        return Err(AppError::fs(format!(
            "移入共享包目录失败（{}）",
            target.display()
        ), e));
    }
    if let Some(backup) = backup {
        remove_dir_quietly(&backup);
    }
    Ok(InstallOutcome {
        id,
        name,
        version,
        dir: target.to_string_lossy().into_owned(),
        kind: TYPE_SHARED.to_string(),
        replaced,
    })
}

fn create_staging() -> Result<PathBuf, AppError> {
    let root = super::plugin_root();
    std::fs::create_dir_all(&root)
        .map_err(|e| AppError::fs(format!("创建插件目录失败（{}）", root.display()), e))?;
    // 以 `.` 开头：热重载监听会忽略点目录，安装过程不会触发反复重载。
    let staging = root.join(format!(".install-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&staging)
        .map_err(|e| AppError::fs(format!("创建临时安装目录失败（{}）", staging.display()), e))?;
    Ok(staging)
}

fn remove_dir_quietly(path: &Path) {
    if let Err(err) = std::fs::remove_dir_all(path) {
        if path.exists() {
            tracing::warn!("清理临时目录失败（{}）：{err}", path.display());
        }
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 标准 base64 解码（前端 `btoa` 产物；忽略换行与空白）。
///
/// 刻意不引入 `base64` 依赖：整个 crate 只有这一处解码需求。
pub fn decode_base64(input: &str) -> Result<Vec<u8>, AppError> {
    let mut out: Vec<u8> = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\r' | b'\n' | b' ' | b'\t' => continue,
            other => {
                return Err(AppError::InvalidArgs(format!(
                    "base64 内容非法（字符 {other}）"
                )));
            }
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_known_vectors() {
        assert_eq!(decode_base64("").unwrap(), b"");
        assert_eq!(decode_base64("Zg==").unwrap(), b"f");
        assert_eq!(decode_base64("Zm9v").unwrap(), b"foo");
        assert_eq!(decode_base64("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(decode_base64("Zm9v\nYmFy").unwrap(), b"foobar", "换行必须被忽略");
    }

    #[test]
    fn base64_rejects_illegal_character() {
        assert!(decode_base64("Zm9v$").is_err());
    }

    #[test]
    fn safe_rel_path_rejects_traversal_and_absolute() {
        assert!(safe_rel_path("../evil.js").is_err());
        assert!(safe_rel_path("a/../../evil.js").is_err());
        assert!(safe_rel_path("..\\evil.js").is_err(), "反斜杠同样视为分隔符");
        assert!(safe_rel_path("C:\\evil.js").is_err());
        assert!(safe_rel_path("/etc/passwd").is_err());
        assert!(safe_rel_path("").is_err());
    }

    #[test]
    fn safe_rel_path_accepts_normal_files() {
        assert_eq!(safe_rel_path("main.js").unwrap(), PathBuf::from("main.js"));
        assert_eq!(
            safe_rel_path("bin\\ffmpeg.exe").unwrap(),
            PathBuf::from("bin").join("ffmpeg.exe")
        );
        assert_eq!(safe_rel_path("./a/b.js").unwrap(), PathBuf::from("a").join("b.js"));
    }

    #[test]
    fn macos_junk_is_ignored() {
        assert!(is_macos_junk("__MACOSX/foo"));
        assert!(is_macos_junk("pkg/.DS_Store"));
        assert!(!is_macos_junk("main.js"));
    }

    #[test]
    fn resolve_roots_descends_into_single_wrapper_dir() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("my-plugin");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("manifest.json"), "{}").unwrap();
        assert_eq!(resolve_roots(dir.path()).unwrap(), vec![inner]);
    }

    /// beta11 · A1：安装期非法 id（非反向域名）必须被拒绝。
    #[test]
    fn non_reverse_domain_id_is_rejected_on_install() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.js"), "console.log('hi');").unwrap();
        std::fs::write(
            dir.path().join("manifest.json"),
            r#"{ "id": "HLS-Downloader", "name": "Bad", "version": "1.0.0", "main": "main.js" }"#,
        )
        .unwrap();
        let err = validate_root(dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("反向域名"),
            "错误信息应点明反向域名：{err}"
        );
    }

    /// beta11 · A1：落盘目录永远以 manifest.id 命名。
    #[test]
    fn plugin_dir_for_uses_id() {
        let dir = plugin_dir_for("com.harness.hls-downloader");
        assert!(dir.ends_with("com.harness.hls-downloader"), "{}", dir.display());
    }

    /// beta12.1：包内多个插件目录（含嵌套）都要被识别为独立根。
    #[test]
    fn resolve_roots_finds_all_plugins_recursively() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a", "b"] {
            let inner = dir.path().join(name);
            std::fs::create_dir_all(&inner).unwrap();
            std::fs::write(inner.join("manifest.json"), "{}").unwrap();
        }
        let nested = dir.path().join("group").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("manifest.json"), "{}").unwrap();
        let mut found = resolve_roots(dir.path()).unwrap();
        found.sort();
        assert_eq!(found.len(), 3, "应识别 a / b / group/c 三个插件：{found:?}");
        assert!(found.iter().any(|path| path.ends_with("group/c")));
    }

    /// beta12.2：shared 包不要求 main.js，也不生成 integrity。
    #[test]
    fn shared_root_passes_without_main_js_or_integrity() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("manifest.json"),
            r#"{ "id": "com.harness.shared-ffmpeg", "name": "共享 FFmpeg", "version": "7.1.0", "type": "shared" }"#,
        )
        .unwrap();
        assert!(validate_root(dir.path()).is_ok());
        assert!(!dir.path().join("main.js").exists(), "shared 包不应被补 main.js");

        let manifest = Manifest::from_dir(dir.path()).unwrap();
        assert_eq!(manifest.plugin_type, TYPE_SHARED);
        assert!(manifest.integrity.is_none(), "共享包不得生成 integrity");
    }

    /// beta12.2：普通插件仍必须提供 main.js（行为零变化）。
    #[test]
    fn plugin_root_still_requires_main_js() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("manifest.json"),
            r#"{ "id": "com.test.nomain", "name": "NoMain", "version": "1.0.0", "entry": "main.js" }"#,
        )
        .unwrap();
        let err = validate_root(dir.path()).unwrap_err();
        assert!(err.to_string().contains("main.js"), "{err}");
    }

    /// beta12.2：未知 type 必须在安装期直接拒绝（不允许静默当插件装）。
    #[test]
    fn unknown_manifest_type_is_rejected_on_install() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.js"), "console.log('hi');").unwrap();
        std::fs::write(
            dir.path().join("manifest.json"),
            r#"{ "id": "com.test.lib", "name": "Lib", "version": "1.0.0", "entry": "main.js", "type": "library" }"#,
        )
        .unwrap();
        let err = validate_root(dir.path()).unwrap_err();
        assert!(err.to_string().contains("type"), "{err}");
    }

    /// beta12.2：共享包目录名不得含任何路径字符。
    #[test]
    fn shared_dir_name_rejects_path_characters() {
        assert!(is_safe_dir_name("com.harness.shared-ffmpeg"));
        for bad in ["..", "a/../b", "a\\b", "C:evil", "x/../y"] {
            assert!(!is_safe_dir_name(bad), "应拒绝：{bad}");
        }
    }

    /// 1.0.0-rc1 · 模块一：申请 `exec:local` 的插件必须在安装前被识别出来。
    #[test]
    fn detects_plugins_requesting_exec_local() {
        let dir = tempfile::tempdir().unwrap();
        let with_exec = dir.path().join("with-exec");
        let without = dir.path().join("without-exec");
        std::fs::create_dir_all(&with_exec).unwrap();
        std::fs::create_dir_all(&without).unwrap();
        std::fs::write(
            with_exec.join("manifest.json"),
            r#"{ "id": "com.test.local", "name": "自带工具", "version": "1.0.0", "permissions": ["fs:read", "exec:local"] }"#,
        )
        .unwrap();
        std::fs::write(
            without.join("manifest.json"),
            r#"{ "id": "com.test.plain", "name": "普通", "version": "1.0.0", "permissions": ["fs:read"] }"#,
        )
        .unwrap();

        let roots = resolve_roots(dir.path()).unwrap();
        assert_eq!(detect_exec_local(&roots), vec!["自带工具".to_string()]);

        // 未确认 → 结构化拒绝（前端据 code 分流弹红色确认）。
        let err = require_exec_local_consent(dir.path(), false).unwrap_err();
        assert_eq!(err.code(), "PLUGIN_CONSENT_REQUIRED");
        assert!(err.to_string().contains("信任并安装"), "{err}");
        assert!(err.to_string().contains("自带工具"), "文案应点名插件：{err}");

        // 用户已确认 → 放行。
        assert!(require_exec_local_consent(dir.path(), true).is_ok());
    }

    /// 不申请 `exec:local` 的包无需确认（老插件零感知）。
    #[test]
    fn plain_package_needs_no_exec_consent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("manifest.json"),
            r#"{ "id": "com.test.plain", "name": "普通", "version": "1.0.0" }"#,
        )
        .unwrap();
        assert!(require_exec_local_consent(dir.path(), false).is_ok());
        assert!(detect_exec_local(&resolve_roots(dir.path()).unwrap()).is_empty());
    }
}
