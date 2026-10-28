//! 文件名提取 / 清洗 / Windows 路径长度容错（beta8.3 模块一、三）。
//!
//! 单一裁决点：[`resolve_out_name`]。RPC 建任务时只能看到原始 URL，
//! 因此先用它生成临时名；任务启动后拿到响应头与最终 URL，再调用一次做改名。
//!
//! 红线：URL / 响应头里的名字一律先 [`sanitize_name`]，绝不允许 `/`、`\`、`*`
//! 等字符进入落盘路径；纯哈希名落到 `Download_<时间戳>`。

use std::path::{Path, PathBuf};

/// Windows 单段文件名长度上限（NTFS 255 个 UTF-16 码元）。
const MAX_COMPONENT: usize = 255;
/// Windows 传统路径总长上限（含结尾 NUL）。
const MAX_PATH: usize = 260;
/// 视为「扩展名」的最长后缀，避免把 `v1.2.3.4.5` 之类长串当后缀保留。
const MAX_EXT: usize = 16;

/// Windows 文件名清洗：非法字符 / 控制字符 → `_`，去掉首尾空白与尾部点。
///
/// 尾部点同时防住 `.` / `..` 这类路径穿越名（清洗后为空 → 调用方走兜底名）。
pub fn sanitize_name(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            ch if ch.is_control() => '_',
            ch => ch,
        })
        .collect();
    replaced.trim().trim_end_matches(['.', ' ']).to_string()
}

/// `%XX` → 字节，再按 UTF-8 解码（`+` 在 path 段不是空格，保持原样）。
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 不透明名：无扩展名 + 长度 ≥ 16 + 全是 `[0-9a-zA-Z_-]`；空名同样视为不透明。
pub fn is_opaque_name(name: &str) -> bool {
    if name.is_empty() {
        return true;
    }
    if has_extension(name) {
        return false;
    }
    name.chars().count() >= 16
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// 终极兜底名。
pub fn fallback_timestamp_name() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    format!("Download_{millis}")
}

/// `Content-Disposition` 里的文件名：优先 RFC 5987 `filename*`，其次 `filename`。
pub fn filename_from_content_disposition(raw: &str) -> Option<String> {
    let extended = cd_param(raw, "filename*").map(|value| {
        // `UTF-8''%E4%B8%AD.zip` → 取第二个 `''` 之后的部分。
        let encoded = value
            .rsplit_once("''")
            .map(|(_, rest)| rest)
            .unwrap_or(value.as_str());
        percent_decode(encoded.trim_matches('"'))
    });
    if let Some(name) = extended.map(|value| sanitize_name(&value)) {
        if !name.is_empty() {
            return Some(name);
        }
    }
    cd_param(raw, "filename")
        .map(|value| sanitize_name(&value))
        .filter(|name| !name.is_empty())
}

/// 取一个 Content-Disposition 参数（忽略大小写；值去掉成对引号）。
fn cd_param(raw: &str, key: &str) -> Option<String> {
    for part in raw.split(';').skip(1) {
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case(key) {
            continue;
        }
        let value = value.trim().trim_matches('"').trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// URL 的 Path 末段（百分号解码 + 清洗）；空 / `.` / `..` / 不透明哈希 → `None`。
pub fn name_from_url_path(uri: &str) -> Option<String> {
    let path = match uri.parse::<http::Uri>() {
        Ok(parsed) => parsed.path().to_string(),
        Err(_) => uri
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .to_string(),
    };
    let raw = path.rsplit('/').next().unwrap_or_default();
    let name = sanitize_name(&percent_decode(raw));
    if name.is_empty() || is_opaque_name(&name) {
        None
    } else {
        Some(name)
    }
}

/// 最终 URL Query 里的文件名线索：`fin` / `filename` / `fname` / `name`。
///
/// Query 与 Path 不同：`+` 表示空格，必须先还原再百分号解码。
pub fn query_filename_hint(uri: &str) -> Option<String> {
    let query = match uri.parse::<http::Uri>() {
        Ok(parsed) => parsed.query().map(str::to_owned),
        Err(_) => uri
            .split_once('?')
            .map(|(_, rest)| rest.split('#').next().unwrap_or_default().to_string()),
    }?;

    let mut pairs: Vec<(String, String)> = Vec::new();
    for item in query.split('&') {
        let Some((key, value)) = item.split_once('=') else {
            continue;
        };
        pairs.push((key.to_ascii_lowercase(), value.to_string()));
    }

    for key in ["fin", "filename", "fname", "name"] {
        if let Some((_, value)) = pairs.iter().find(|(name, _)| name == key) {
            let decoded = percent_decode(&value.replace('+', " "));
            let name = sanitize_name(&decoded);
            if !name.is_empty() && !is_opaque_name(&name) {
                return Some(name);
            }
        }
    }
    None
}

/// 文件名裁决（命中即停止）：
///
/// 1. 显式 `out`；
/// 2. `Content-Disposition` 的 filename；
/// 3. 重定向前的原始 URL Path 末段（仅接受带扩展名 / 非 ASCII 的「像文件名」段）；
/// 4. 最终 URL 的 Query 线索（`fin` / `filename` / `fname` / `name`）；
/// 5. 最终 URL 的 Path 末段；
/// 6. 以上全落空或只剩纯哈希 → `Download_<时间戳>`。
///
/// 第 3 步的保守判断是必要的：形如 `https://pan.baidu.com/s/1abc` 的分享页
/// 末段只是短 ID，若被当成文件名会抢在 `fin=真实文件名.zip` 之前。
pub fn resolve_out_name(
    explicit_out: Option<&str>,
    content_disposition: Option<&str>,
    original_url: &str,
    final_url: &str,
) -> String {
    if let Some(explicit) = explicit_out {
        let name = sanitize_name(explicit);
        if !name.is_empty() {
            return name;
        }
    }
    if let Some(raw) = content_disposition {
        if let Some(name) = filename_from_content_disposition(raw) {
            return name;
        }
    }
    if let Some(name) = name_from_url_path(original_url) {
        if looks_like_file_name(&name) {
            return name;
        }
    }
    if let Some(name) = query_filename_hint(final_url) {
        return name;
    }
    if let Some(name) = name_from_url_path(final_url) {
        return name;
    }
    fallback_timestamp_name()
}

/// 「像文件名」：带合法扩展名，或含非 ASCII 字符（中文名常见且不会与 ID 混淆）。
fn looks_like_file_name(name: &str) -> bool {
    has_extension(name) || name.chars().any(|ch| !ch.is_ascii())
}

/// 是否有 `.<ext>` 形式的扩展名（点不在开头，后缀 1..=16 位字母数字）。
fn has_extension(name: &str) -> bool {
    let Some(index) = name.rfind('.') else {
        return false;
    };
    if index == 0 || index + 1 >= name.len() {
        return false;
    }
    let ext = &name[index + 1..];
    let count = ext.chars().count();
    count >= 1 && count <= MAX_EXT && ext.chars().all(|ch| ch.is_ascii_alphanumeric())
}

/// 拼接 `dir + name`，并按 Windows 传统路径上限（260）截断文件名主体。
///
/// 只截断文件名，绝不改写用户选择的目录；目录本身过长时原样返回并告警。
pub fn fit_windows_path(dir: &Path, name: &str) -> PathBuf {
    let sanitized = sanitize_name(name);
    let name = if sanitized.is_empty() {
        fallback_timestamp_name()
    } else {
        sanitized
    };

    let dir_text = dir.to_string_lossy();
    let dir_chars = dir_text.chars().count();
    let separator = usize::from(
        dir_chars > 0 && !dir_text.ends_with('\\') && !dir_text.ends_with('/'),
    );
    let dir_len = dir_chars + separator;

    let (stem, extension) = split_extension(&name);
    let ext_len = extension.chars().count();
    let stem_len = stem.chars().count();

    // 目录自身就吃掉几乎全部预算：不改写目录，原样交给调用方去报错。
    if dir_len + ext_len + 1 > MAX_PATH {
        tracing::warn!(
            dir = %dir.display(),
            "下载目录过长，无法在 260 字符内放下任何文件名，保持原路径"
        );
        return dir.join(name);
    }

    let budget = MAX_PATH - dir_len - ext_len;
    let limit = stem_len.min(budget).min(MAX_COMPONENT - ext_len.min(MAX_COMPONENT));
    if limit == stem_len {
        return dir.join(name);
    }

    let mut fitted: String = stem.chars().take(limit.max(1)).collect();
    fitted.push_str(extension);
    dir.join(fitted)
}

/// 拆分主体与扩展名；返回的扩展名含点，无扩展名时为空串。
///
/// 多点后缀（`.tar.gz`）整体保留，只要总长不超过 [`MAX_EXT`]；
/// 否则截断时会把 `.tar` 切掉，落盘名变成只有 `.gz`。
fn split_extension(name: &str) -> (&str, &str) {
    let Some(mut start) = name.rfind('.') else {
        return (name, "");
    };
    if start == 0 || !is_extension_segment(&name[start + 1..]) {
        return (name, "");
    }
    while let Some(prev) = name[..start].rfind('.') {
        if prev == 0
            || !is_extension_segment(&name[prev + 1..start])
            || name[prev..].chars().count() > MAX_EXT
        {
            break;
        }
        start = prev;
    }
    (&name[..start], &name[start..])
}

/// 扩展名的一段：1..=[`MAX_EXT`] 位 ASCII 字母数字。
fn is_extension_segment(segment: &str) -> bool {
    let count = segment.chars().count();
    count >= 1 && count <= MAX_EXT && segment.chars().all(|ch| ch.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_percent_encoded_names() {
        assert_eq!(
            name_from_url_path("http://a.test/%E4%B8%AD%E6%96%87.zip").as_deref(),
            Some("中文.zip")
        );
        assert_eq!(name_from_url_path("http://a.test/a%20b.zip").as_deref(), Some("a b.zip"));
        // 非法百分号序列原样保留。
        assert_eq!(name_from_url_path("http://a.test/100%zz.zip").as_deref(), Some("100%zz.zip"));
    }

    #[test]
    fn sanitizes_windows_illegal_chars() {
        assert_eq!(
            name_from_url_path("http://a.test/a%3Ab%2Fc.zip").as_deref(),
            Some("a_b_c.zip")
        );
        // `.` / `..` 清洗后为空 → 不产生名字。
        assert_eq!(name_from_url_path("http://a.test/.."), None);
    }

    #[test]
    fn opaque_names_are_rejected() {
        assert_eq!(
            name_from_url_path(
                "https://github.com/x/y/releases/download/v1/abc123def4567890abcdef"
            ),
            None
        );
        assert_eq!(name_from_url_path("http://a.test/word").as_deref(), Some("word"));
        assert_eq!(name_from_url_path("http://a.test/LICENSE").as_deref(), Some("LICENSE"));
    }

    #[test]
    fn parses_content_disposition() {
        assert_eq!(
            filename_from_content_disposition(r#"attachment; filename="game.zip""#).as_deref(),
            Some("game.zip")
        );
        assert_eq!(
            filename_from_content_disposition(
                "attachment; filename*=UTF-8''%E4%B8%AD%E6%96%87.zip"
            )
            .as_deref(),
            Some("中文.zip")
        );
        // filename* 优先于 filename。
        assert_eq!(
            filename_from_content_disposition(
                "attachment; filename=\"fallback.zip\"; filename*=UTF-8''real.zip"
            )
            .as_deref(),
            Some("real.zip")
        );
        assert_eq!(filename_from_content_disposition("attachment"), None);
    }

    #[test]
    fn reads_query_hints() {
        assert_eq!(
            query_filename_hint("https://pan.baidu.com/s/1abc?fin=%E5%AE%89%E8%A3%85.zip")
                .as_deref(),
            Some("安装.zip")
        );
        assert_eq!(
            query_filename_hint("http://a.test/dl?filename=a+b.zip").as_deref(),
            Some("a b.zip")
        );
        // 纯哈希线索不采用。
        assert_eq!(
            query_filename_hint("http://a.test/dl?fin=abc123def4567890abcdef"),
            None
        );
    }

    #[test]
    fn resolves_in_documented_priority_order() {
        let original = "https://pan.baidu.com/s/1abc";
        let final_url = "https://cdn.test/dl?fin=%E6%95%B0%E6%8D%AE.zip";
        // 1. 显式 out 最高。
        assert_eq!(
            resolve_out_name(Some("手动.exe"), Some("attachment; filename=\"x.zip\""), original, final_url),
            "手动.exe"
        );
        // 2. Content-Disposition。
        assert_eq!(
            resolve_out_name(None, Some("attachment; filename=\"头部.bin\""), original, final_url),
            "头部.bin"
        );
        // 3. 原始 URL 有扩展名时优先于最终 URL 的 query。
        assert_eq!(
            resolve_out_name(None, None, "https://a.test/game.part1.rar", final_url),
            "game.part1.rar"
        );
        // 分享页短 ID 不算文件名 → 落到 query 线索。
        assert_eq!(resolve_out_name(None, None, original, final_url), "数据.zip");
        // 4. query 优先于最终 path。
        assert_eq!(
            resolve_out_name(None, None, "https://a.test/share/1abc", "https://a.test/dl/file.bin?fname=真名.zip"),
            "真名.zip"
        );
        // 5. 最终 URL 的 path。
        assert_eq!(
            resolve_out_name(None, None, "https://a.test/share/1abc", "https://a.test/dl/file.bin"),
            "file.bin"
        );
        // 6. 兜底。
        assert!(resolve_out_name(None, None, "https://a.test/", "https://a.test/").starts_with("Download_"));
    }

    #[test]
    fn fits_long_paths_keeping_extension() {
        let dir = Path::new("C:\\Users\\Public\\Downloads");
        let short = fit_windows_path(dir, "game.zip");
        assert_eq!(short, dir.join("game.zip"));

        let long = "长".repeat(300) + ".zip";
        let fitted = fit_windows_path(dir, &long);
        let name = fitted.file_name().unwrap().to_string_lossy();
        assert!(name.ends_with(".zip"));
        assert!(fitted.to_string_lossy().chars().count() <= MAX_PATH);
        assert!(name.chars().count() > 4);
    }

    #[test]
    fn fits_long_paths_without_extension() {
        let dir = Path::new("C:\\Downloads");
        let fitted = fit_windows_path(dir, &"a".repeat(400));
        let name = fitted.file_name().unwrap().to_string_lossy();
        assert_eq!(name.chars().count(), MAX_PATH - "C:\\Downloads\\".chars().count());
    }

    #[test]
    fn directory_too_long_is_left_untouched() {
        let long_dir = format!("C:\\{}\\", "d".repeat(280));
        let dir = Path::new(&long_dir);
        let fitted = fit_windows_path(dir, "file.zip");
        assert_eq!(fitted, dir.join("file.zip"));
    }

    #[test]
    fn path_length_boundaries_are_exact() {
        // 目录 "C:\Downloads" 拼接后含分隔符共 13 个字符。
        let dir = Path::new("C:\\Downloads");
        let dir_len = "C:\\Downloads\\".chars().count();
        // 恰好 MAX_PATH 字符：不截断。
        let exact = format!("{}.zip", "a".repeat(MAX_PATH - dir_len - 4));
        let fitted = fit_windows_path(dir, &exact);
        assert_eq!(
            fitted.file_name().unwrap().to_string_lossy(),
            exact.as_str()
        );
        assert_eq!(fitted.to_string_lossy().chars().count(), MAX_PATH);
        // MAX_PATH - 1 字符：不截断。
        let short_name = format!("{}.zip", "a".repeat(MAX_PATH - dir_len - 4 - 1));
        assert_eq!(
            fit_windows_path(dir, &short_name)
                .file_name()
                .unwrap()
                .to_string_lossy(),
            short_name.as_str()
        );
        // MAX_PATH + 1 / 300 字符：截到 MAX_PATH 且保留后缀。
        for body_len in [MAX_PATH - dir_len - 4 + 1, 300usize] {
            let name = format!("{}.zip", "a".repeat(body_len));
            let fitted = fit_windows_path(dir, &name);
            let text = fitted.to_string_lossy();
            assert_eq!(text.chars().count(), MAX_PATH, "总长必须正好压到上限");
            assert!(text.ends_with(".zip"));
            assert!(fitted.file_name().unwrap().to_string_lossy().chars().count() <= MAX_COMPONENT);
        }
    }

    #[test]
    fn multi_dot_suffix_is_preserved_whole() {
        let dir = Path::new("C:\\Downloads");
        let long = format!("{}.tar.gz", "a".repeat(300));
        let fitted = fit_windows_path(dir, &long);
        let name = fitted.file_name().unwrap().to_string_lossy();
        assert!(name.ends_with(".tar.gz"), "多点后缀必须整体保留：{name}");
        assert!(name.chars().count() <= MAX_COMPONENT);
        assert_eq!(fitted.to_string_lossy().chars().count(), MAX_PATH);
    }

    #[test]
    fn count_is_by_chars_not_bytes_for_cjk() {
        let dir = Path::new("C:\\Downloads");
        let long = format!("{}.zip", "中".repeat(300));
        let fitted = fit_windows_path(dir, &long);
        let name = fitted.file_name().unwrap().to_string_lossy();
        assert!(name.ends_with(".zip"));
        // 按字符计数：中文 3 字节/字，按字节截断会严重不足或越界。
        assert_eq!(fitted.to_string_lossy().chars().count(), MAX_PATH);
        assert!(name.chars().count() <= MAX_COMPONENT);
    }

    #[test]
    fn illegal_chars_are_scrubbed_in_fitted_path() {
        let dir = Path::new("C:\\Downloads");
        let fitted = fit_windows_path(dir, "a<b>c:d\"e/f\\g|h?i*j.zip");
        let name = fitted.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!name.contains(['<', '>', ':', '"', '/', '\\', '|', '?', '*']), "{name}");
        assert!(name.ends_with(".zip"));
    }
}
