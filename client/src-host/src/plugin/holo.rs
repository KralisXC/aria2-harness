//! 全息探针数据包（beta12 · 模块 E）。
//!
//! 扩展在用户**按需触发**（Popup「解析当前页面」或悬浮按钮）时，把页面 URL、
//! 网络层完整请求头与页面上下文打包成 JSON POST 到宿主 `/probe`；本模块负责
//! 解析、清洗与域名匹配，不做任何站点硬编码。
//!
//! 安全边界：
//! - `host` 只从 `url` 解析，绝不信任客户端字段；
//! - 所有文本剔除 `\r` / `\n` / `\0`，防 Header / 日志注入；
//! - 超限字段直接丢弃（不截断半截 JSON）；
//! - 探针只唤醒插件，不创建下载。

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

/// 全息探针触发插件注册的 hook 名。
pub const HOOK_HOLO: &str = "onHolo";

/// HTML 快照上限（256 KB）。
pub const MAX_HTML_BYTES: usize = 256 * 1024;
/// 页面全局状态上限（128 KB）。
pub const MAX_STATE_BYTES: usize = 128 * 1024;
/// 请求头条数上限。
pub const MAX_HEADERS: usize = 64;
/// 请求头总计上限（16 KB）。
pub const MAX_HEADER_BYTES: usize = 16 * 1024;
/// 单个请求头值上限（8 KB）。
pub const MAX_HEADER_VALUE_BYTES: usize = 8 * 1024;
/// Cookie 上限（8 KB）。
pub const MAX_COOKIE_BYTES: usize = 8 * 1024;
/// 标题最大字符数。
const MAX_TITLE_CHARS: usize = 512;

/// 网络线索条数上限（与扩展侧 `NET_HINT_LIMIT` 对齐）。
pub const MAX_NET_HINTS: usize = 48;
/// 单条线索 URL 上限（字节）。
pub const MAX_HINT_URL: usize = 2048;

/// 网络层媒体线索（1.0.0-rc1 · 模块三）。
///
/// 扩展在 MAIN world 劫持 fetch / XHR 采集到的候选媒体地址，随 `/probe` 一并上报。
/// 宿主只做清洗（协议 / 类型 / 长度 / 去重 / 条数），不做任何站点判定。
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkHint {
    pub url: String,
    /// 归一化类型：`m3u8` / `mpd` / `m4s` / `mp4` / `flv` / `ts`。
    pub kind: String,
}

/// 清洗后的全息数据包（序列化给插件）。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HoloPacket {
    pub url: String,
    /// 从 `url` 解析出的主机（小写、无端口）。
    pub host: String,
    pub title: String,
    /// 保留 Cookie 的完整请求头（与下载链路的 `clean_headers` 规则不同）。
    pub headers: BTreeMap<String, String>,
    pub cookie: String,
    pub user_agent: String,
    pub referer: String,
    pub initial_state: Option<String>,
    pub html: Option<String>,
    /// 网络层媒体线索（扩展未上报时为空表）。
    pub network_hints: Vec<NetworkHint>,
    pub received_at: u64,
}

/// 解析扩展提交的 JSON 包。
pub fn from_probe(value: &Value) -> Result<HoloPacket, String> {
    let url = value
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| "url 不能为空".to_string())?;

    let scheme_ok = {
        let lower = url.to_ascii_lowercase();
        lower.starts_with("http://") || lower.starts_with("https://")
    };
    if !scheme_ok {
        return Err("仅支持 http/https URL".to_string());
    }

    let host = crate::url_host(url);
    if host.is_empty() {
        return Err("无法从 url 解析主机名".to_string());
    }

    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(|text| clean_text(text, MAX_TITLE_CHARS))
        .unwrap_or_default();

    let headers = parse_headers(value.get("headers"));

    let cookie = value
        .get("cookie")
        .and_then(Value::as_str)
        .and_then(|text| bounded_text(text, MAX_COOKIE_BYTES))
        .or_else(|| headers.get("cookie").cloned())
        .unwrap_or_default();

    let user_agent = value
        .get("userAgent")
        .or_else(|| value.get("user_agent"))
        .and_then(Value::as_str)
        .and_then(|text| bounded_text(text, MAX_HEADER_VALUE_BYTES))
        .or_else(|| headers.get("user-agent").cloned())
        .unwrap_or_default();

    let referer = value
        .get("referer")
        .and_then(Value::as_str)
        .and_then(|text| bounded_text(text, MAX_HEADER_VALUE_BYTES))
        .or_else(|| headers.get("referer").cloned())
        .unwrap_or_default();

    let initial_state = value
        .get("initialState")
        .or_else(|| value.get("initial_state"))
        .and_then(Value::as_str)
        .and_then(|text| bounded_text(text, MAX_STATE_BYTES));

    let html = value
        .get("html")
        .and_then(Value::as_str)
        .and_then(|text| bounded_text(text, MAX_HTML_BYTES));

    Ok(HoloPacket {
        url: url.to_string(),
        host,
        title,
        headers,
        cookie,
        user_agent,
        referer,
        initial_state,
        html,
        network_hints: parse_network_hints(value.get("networkHints")),
        received_at: now_ms(),
    })
}

/// 解析网络线索：仅 http(s)、类型归一、长度受限、按 URL 去重、最多 [`MAX_NET_HINTS`] 条。
///
/// 扩展实际上报的类型是 `hls` / `dash`（见 `inject-network.js`），这里统一归一为
/// `m3u8` / `mpd`；未知类型与非法条目静默丢弃，绝不影响探针本身。
fn parse_network_hints(raw: Option<&Value>) -> Vec<NetworkHint> {
    let Some(Value::Array(list)) = raw else {
        return Vec::new();
    };
    let mut out: Vec<NetworkHint> = Vec::new();
    for item in list {
        if out.len() >= MAX_NET_HINTS {
            break;
        }
        let Some(object) = item.as_object() else {
            continue;
        };
        let Some(url) = object
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
        else {
            continue;
        };
        let lower = url.to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) {
            continue;
        }
        if url.len() > MAX_HINT_URL {
            continue;
        }
        let kind = object
            .get("kind")
            .and_then(Value::as_str)
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        let kind = match kind.as_str() {
            "m3u8" | "hls" => "m3u8",
            "mpd" | "dash" => "mpd",
            "m4s" => "m4s",
            "mp4" => "mp4",
            "flv" => "flv",
            "ts" => "ts",
            _ => continue,
        };
        if out.iter().any(|hint| hint.url == url) {
            continue;
        }
        out.push(NetworkHint {
            url: url.to_string(),
            kind: kind.to_string(),
        });
    }
    out
}

/// 解析请求头：兼容对象与 `Name: Value` 数组两种形态。
fn parse_headers(raw: Option<&Value>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut total = 0usize;
    let mut push = |name: &str, value: &str| {
        if out.len() >= MAX_HEADERS {
            return;
        }
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty()
            || !name.is_ascii()
            || name.contains(':')
            || name.chars().any(char::is_whitespace)
        {
            return;
        }
        // Cookie / Host 等保留：探针的目的就是还原真实请求上下文。
        let Some(value) = bounded_text(value, MAX_HEADER_VALUE_BYTES) else {
            return;
        };
        if out.contains_key(&name) {
            return;
        }
        let cost = name.len() + value.len();
        if total + cost > MAX_HEADER_BYTES {
            return;
        }
        total += cost;
        out.insert(name, value);
    };

    match raw {
        Some(Value::Object(map)) => {
            for (name, value) in map {
                if let Some(value) = value.as_str() {
                    push(name, value);
                }
            }
        }
        Some(Value::Array(lines)) => {
            for line in lines {
                let Some(line) = line.as_str() else { continue };
                let Some((name, value)) = line.split_once(':') else {
                    continue;
                };
                push(name, value);
            }
        }
        _ => {}
    }
    out
}

/// 清洗并限制长度；空值 / 超限返回 `None`。
fn bounded_text(raw: &str, max_bytes: usize) -> Option<String> {
    let cleaned = raw.replace(['\r', '\n', '\0'], " ");
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || cleaned.len() > max_bytes {
        return None;
    }
    Some(cleaned.to_string())
}

/// 标题清洗：去控制字符并按字符数截断（标题不参与严格上限判定）。
fn clean_text(raw: &str, max_chars: usize) -> String {
    let cleaned = raw.replace(['\r', '\n', '\0'], " ");
    let cleaned = cleaned.trim();
    cleaned.chars().take(max_chars).collect()
}

/// 域名规则匹配（大小写不敏感）。
///
/// - `*.example.com` 命中 `example.com` 与任意子域；
/// - `example.com` 只命中精确主机；
/// - 其它含 `*` 的写法一律不命中（避免把懒激活退化成全量激活）。
pub fn domain_matches(host: &str, pattern: &str) -> bool {
    let host = normalize_host(host);
    let pattern = pattern.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || pattern.is_empty() {
        return false;
    }
    if let Some(base) = pattern.strip_prefix("*.") {
        if base.is_empty() || base.contains('*') {
            return false;
        }
        host == base || host.ends_with(&format!(".{base}"))
    } else if pattern.contains('*') {
        false
    } else {
        host == pattern
    }
}

/// 是否被该插件的域名规则激活；空规则 = 始终激活（兼容既有插件）。
pub fn is_activated(host: &str, patterns: &[String]) -> bool {
    patterns.is_empty() || patterns.iter().any(|pattern| domain_matches(host, pattern))
}

/// 用户自定义域名的设置项 key（manifest.settings_schema / configs/<id>.json）。
pub const SETTING_CUSTOM_DOMAINS: &str = "custom_domains";

/// 自定义域名条数上限（防止误粘贴超长列表拖慢每一次 `/check`）。
pub const MAX_CUSTOM_DOMAINS: usize = 64;

/// 解析用户在「自定义域名白名单」里填写的文本（每行一个）。
///
/// 面向人工输入的宽松语义（比 manifest 校验宽，但绝不产生"匹配一切"的规则）：
/// - 支持 `example.com`、`*.example.com`、`*example.com*`、直接粘贴的 URL；
/// - 归一为裸域后统一按「裸域 + 其子域」匹配（用户列一个域名即覆盖其子域）；
/// - 空行 / `#` 注释行 / 非法项一律跳过，不因一行写错导致整次保存失败。
pub fn parse_custom_domains(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.lines() {
        if out.len() >= MAX_CUSTOM_DOMAINS {
            break;
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        let Some(base) = normalize_custom_domain(line) else {
            continue;
        };
        let pattern = format!("*.{base}");
        if !out.contains(&pattern) {
            out.push(pattern);
        }
    }
    out
}

/// 自定义域名归一化：去协议 / 路径 / 查询 / 端口 / userinfo / 任意位置的 `*`。
fn normalize_custom_domain(line: &str) -> Option<String> {
    let mut host = line.trim().to_ascii_lowercase();
    if let Some((_, rest)) = host.split_once("://") {
        host = rest.to_string();
    }
    let host = host
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .replace('*', "");
    let host = host.trim_matches('.').trim().to_string();
    if host.is_empty() || host.len() > 253 || !host.contains('.') {
        return None;
    }
    if host
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
    {
        return None;
    }
    if host
        .split('.')
        .any(|seg| seg.is_empty() || seg.starts_with('-') || seg.ends_with('-'))
    {
        return None;
    }
    Some(host)
}

/// 合并 manifest 域名与用户自定义域名（并集，manifest 规则在前）。
///
/// 每次匹配都实时调用，因此保存设置后**无需重启**即刻生效。
pub fn merge_domains(manifest: &[String], custom_text: &str) -> Vec<String> {
    let mut merged: Vec<String> = manifest
        .iter()
        .map(|domain| domain.trim().to_string())
        .filter(|domain| !domain.is_empty())
        .collect();
    for pattern in parse_custom_domains(custom_text) {
        if !merged.contains(&pattern) {
            merged.push(pattern);
        }
    }
    merged
}

/// 对一张 mock / 真实路由表求命中插件 id（纯函数，便于单测）。
///
/// 入参为 `(plugin_id, activation_domains)`；返回顺序与入参一致。
pub fn match_plugins<'a>(
    host: &str,
    plugins: impl IntoIterator<Item = (&'a str, &'a [String])>,
) -> Vec<&'a str> {
    plugins
        .into_iter()
        .filter(|(_, patterns)| is_activated(host, patterns))
        .map(|(id, _)| id)
        .collect()
}

/// 仅匹配**站点插件**（`/check` 专用）：刻意排除 `activation_domains` 为空的
/// 始终激活插件，避免扩展在百度等普通网页上注入悬浮按钮。
pub fn match_site_plugins<'a>(
    host: &str,
    plugins: impl IntoIterator<Item = (&'a str, &'a [String])>,
) -> Vec<&'a str> {
    plugins
        .into_iter()
        .filter(|(_, patterns)| !patterns.is_empty() && is_activated(host, patterns))
        .map(|(id, _)| id)
        .collect()
}

/// 主机归一化：去 userinfo / 端口 / 末尾点，统一小写。
fn normalize_host(host: &str) -> String {
    let host = host.trim().trim_end_matches('.');
    let host = host.rsplit('@').next().unwrap_or(host);
    host.split(':').next().unwrap_or(host).to_ascii_lowercase()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        domain_matches, from_probe, is_activated, match_plugins, match_site_plugins,
        MAX_HTML_BYTES, MAX_STATE_BYTES,
    };

    fn mock_probe() -> serde_json::Value {
        json!({
            "url": "https://www.bilibili.com/video/BV1xx411c7mD?p=1",
            "title": "【测试】视频标题",
            "headers": {
                "user-agent": "Mozilla/5.0 Chrome/140",
                "referer": "https://www.bilibili.com/",
                "cookie": "SESSDATA=abc; bili_jct=def",
                "accept-language": "zh-CN,zh;q=0.9"
            },
            "cookie": "SESSDATA=abc; bili_jct=def",
            "userAgent": "Mozilla/5.0 Chrome/140",
            "referer": "https://www.bilibili.com/",
            "initialState": "{\"videoData\":{\"bvid\":\"BV1xx411c7mD\"}}",
            "html": "<!DOCTYPE html><html><body>BV1xx411c7mD</body></html>",
            "source": "browser-extension"
        })
    }

    #[test]
    fn parses_full_mock_packet() {
        let packet = from_probe(&mock_probe()).expect("mock 包应解析成功");
        assert_eq!(packet.host, "www.bilibili.com");
        assert_eq!(packet.title, "【测试】视频标题");
        assert_eq!(packet.cookie, "SESSDATA=abc; bili_jct=def");
        assert_eq!(packet.user_agent, "Mozilla/5.0 Chrome/140");
        assert_eq!(packet.referer, "https://www.bilibili.com/");
        assert_eq!(
            packet.initial_state.as_deref(),
            Some("{\"videoData\":{\"bvid\":\"BV1xx411c7mD\"}}")
        );
        assert!(packet.html.as_deref().unwrap_or_default().contains("BV1xx411c7mD"));
        assert_eq!(packet.headers.get("cookie").map(String::as_str), Some("SESSDATA=abc; bili_jct=def"));
        assert!(packet.received_at > 0);
    }

    #[test]
    fn oversized_state_and_html_are_dropped() {
        let value = json!({
            "url": "https://example.com/a",
            "initialState": "x".repeat(MAX_STATE_BYTES + 1),
            "html": "y".repeat(MAX_HTML_BYTES + 1)
        });
        let packet = from_probe(&value).unwrap();
        assert!(packet.initial_state.is_none(), "超限 state 必须整段丢弃");
        assert!(packet.html.is_none(), "超限 html 必须整段丢弃");
    }

    #[test]
    fn header_injection_is_cleaned() {
        let value = json!({
            "url": "https://example.com/a",
            "headers": {
                "x-evil": "a\r\nSet-Cookie: hacked",
                "referer": "https://example.com/\r\nX: y"
            }
        });
        let packet = from_probe(&value).unwrap();
        assert_eq!(packet.headers.get("x-evil").map(String::as_str), Some("a  Set-Cookie: hacked"));
        assert_eq!(packet.headers.get("referer").map(String::as_str), Some("https://example.com/  X: y"));
    }

    #[test]
    fn cookie_falls_back_to_headers() {
        let value = json!({
            "url": "https://example.com/a",
            "headers": { "cookie": "k=v" }
        });
        let packet = from_probe(&value).unwrap();
        assert_eq!(packet.cookie, "k=v");
    }

    #[test]
    fn non_http_url_is_rejected() {
        for url in ["file:///C:/x", "javascript:alert(1)", "ftp://example.com/a", ""] {
            assert!(from_probe(&json!({ "url": url })).is_err(), "应拒绝：{url}");
        }
    }

    #[test]
    fn wildcard_matches_bare_and_subdomains_only() {
        for host in ["bilibili.com", "www.bilibili.com", "api.live.bilibili.com", "WWW.BILIBILI.COM"] {
            assert!(domain_matches(host, "*.bilibili.com"), "应命中：{host}");
        }
        for host in ["evilbilibili.com", "bilibili.com.evil.com", "bilibili.org"] {
            assert!(!domain_matches(host, "*.bilibili.com"), "不应命中：{host}");
        }
        // 无通配符只匹配精确主机。
        assert!(domain_matches("www.bilibili.com", "www.bilibili.com"));
        assert!(!domain_matches("api.bilibili.com", "www.bilibili.com"));
    }

    #[test]
    fn empty_patterns_mean_always_active() {
        let patterns: Vec<String> = Vec::new();
        assert!(is_activated("baidu.com", &patterns));
    }

    #[test]
    fn mock_route_table_only_hits_matching_plugins() {
        let bilibili: Vec<String> = vec!["*.bilibili.com".to_string()];
        let twitch: Vec<String> = vec!["*.twitch.tv".to_string()];
        let always: Vec<String> = Vec::new();
        let table = vec![
            ("com.harness.m4s-merger", bilibili.as_slice()),
            ("com.harness.live-recorder", twitch.as_slice()),
            ("com.harness.always-on", always.as_slice()),
        ];
        let matched = match_plugins("www.bilibili.com", table);
        assert_eq!(
            matched,
            vec!["com.harness.m4s-merger", "com.harness.always-on"],
            "只应唤醒命中域名与始终激活的插件"
        );

        let table = vec![
            ("com.harness.m4s-merger", bilibili.as_slice()),
            ("com.harness.live-recorder", twitch.as_slice()),
            ("com.harness.always-on", always.as_slice()),
        ];
        assert_eq!(
            match_plugins("www.baidu.com", table),
            vec!["com.harness.always-on"],
            "普通网页不得唤醒任何站点插件"
        );
    }

    /// `/check` 只返回站点插件：百度等普通网页必须为空（悬浮按钮绝不注入）。
    #[test]
    fn site_match_excludes_always_on_plugins() {
        let bilibili: Vec<String> = vec!["*.bilibili.com".to_string()];
        let always: Vec<String> = Vec::new();
        let table = vec![
            ("com.harness.m4s-merger", bilibili.as_slice()),
            ("com.harness.always-on", always.as_slice()),
        ];
        assert!(
            match_site_plugins("www.baidu.com", table).is_empty(),
            "普通网页不得出现站点悬浮按钮"
        );

        let table = vec![
            ("com.harness.m4s-merger", bilibili.as_slice()),
            ("com.harness.always-on", always.as_slice()),
        ];
        assert_eq!(
            match_site_plugins("www.bilibili.com", table),
            vec!["com.harness.m4s-merger"],
            "始终激活插件不应进入 /check 的站点命中列表"
        );
    }

    /// 自定义域名：宽松解析（注释 / 通配符 / URL / 端口）与去重。
    #[test]
    fn custom_domains_are_parsed_leniently() {
        let text = "\
# 注释行
// 另一种注释
*.Playertest.Longtailvideo.com
*foo.example.com*
https://bar.example.com/watch?v=1
 example.com:8443
not-a-domain
*.playertest.longtailvideo.com
";
        assert_eq!(
            super::parse_custom_domains(text),
            vec![
                "*.playertest.longtailvideo.com",
                "*.foo.example.com",
                "*.bar.example.com",
                "*.example.com",
            ],
            "非法行必须跳过、重复项必须去重"
        );
    }

    /// 自定义域名与 manifest 规则取并集，原规则不受影响。
    #[test]
    fn custom_domains_merge_with_manifest_as_union() {
        let manifest: Vec<String> = vec!["*.bilibili.com".to_string()];
        let merged = super::merge_domains(&manifest, "playertest.longtailvideo.com\n*.example.org");
        assert!(merged.contains(&"*.bilibili.com".to_string()));
        assert!(merged.contains(&"*.playertest.longtailvideo.com".to_string()));
        assert!(merged.contains(&"*.example.org".to_string()));

        let table = vec![("com.harness.m4s-merger", merged.as_slice())];
        assert_eq!(
            match_site_plugins("www.playertest.longtailvideo.com", table),
            vec!["com.harness.m4s-merger"],
            "自定义域名命中后必须进入 /check 结果"
        );
    }

    /// 原本没有 activationEvents 的插件，加上自定义域名后只在这些站点出现。
    #[test]
    fn custom_domains_activate_site_less_plugin() {
        let empty: Vec<String> = Vec::new();
        assert!(
            match_site_plugins("www.example.com", vec![("p", empty.as_slice())]).is_empty(),
            "未配置域名时不得注入悬浮按钮"
        );
        let merged = super::merge_domains(&empty, "example.com");
        assert_eq!(
            match_site_plugins("a.b.example.com", vec![("p", merged.as_slice())]),
            vec!["p"],
            "自定义域名应同时覆盖裸域与子域"
        );
    }

    /// 空配置不改变任何既有匹配结果。
    #[test]
    fn empty_custom_domains_change_nothing() {
        let manifest: Vec<String> = vec!["*.bilibili.com".to_string()];
        assert_eq!(super::merge_domains(&manifest, "  \n\n #x"), manifest);
        assert!(super::parse_custom_domains("*").is_empty(), "单独 * 绝不能变成匹配一切");
    }

    /// 1.0.0-rc1 · 模块三：网络线索按扩展真实词表归一（hls → m3u8、dash → mpd）。
    #[test]
    fn network_hints_are_parsed_and_normalized() {
        let value = json!({
            "url": "https://www.playertest.longtailvideo.com/player.html",
            "networkHints": [
                { "url": "https://cdn.example.com/a.m3u8", "kind": "hls" },
                { "url": "https://cdn.example.com/b.mpd", "kind": "dash" },
                { "url": "https://cdn.example.com/c.m4s", "kind": "m4s" },
                { "url": "https://cdn.example.com/d.mp4", "kind": "MP4" }
            ]
        });
        let packet = from_probe(&value).unwrap();
        let kinds: Vec<&str> = packet
            .network_hints
            .iter()
            .map(|hint| hint.kind.as_str())
            .collect();
        assert_eq!(kinds, vec!["m3u8", "mpd", "m4s", "mp4"]);
    }

    /// 非法条目（非 http(s) / 未知类型 / 超长 / 非对象）必须静默丢弃，不影响探针。
    #[test]
    fn illegal_network_hints_are_dropped() {
        let long_url = format!("https://cdn.example.com/{}.m3u8", "x".repeat(super::MAX_HINT_URL));
        let value = json!({
            "url": "https://example.com/watch",
            "networkHints": [
                { "url": "file:///C:/a.m3u8", "kind": "m3u8" },
                { "url": "https://cdn.example.com/a.m3u8", "kind": "exe" },
                { "url": long_url, "kind": "m3u8" },
                { "url": "", "kind": "m3u8" },
                { "kind": "m3u8" },
                "not-an-object",
                { "url": "https://cdn.example.com/ok.m3u8", "kind": "m3u8" }
            ]
        });
        let packet = from_probe(&value).unwrap();
        assert_eq!(packet.network_hints.len(), 1, "只有合法条目应保留");
        assert_eq!(packet.network_hints[0].url, "https://cdn.example.com/ok.m3u8");
    }

    /// 去重 + 上限截断：重复 URL 只留一条，总数不超过 MAX_NET_HINTS。
    #[test]
    fn network_hints_are_deduped_and_capped() {
        let mut list = vec![
            json!({ "url": "https://cdn.example.com/0.mp4", "kind": "mp4" }),
            json!({ "url": "https://cdn.example.com/0.mp4", "kind": "mp4" }),
        ];
        for index in 1..(super::MAX_NET_HINTS + 10) {
            list.push(json!({ "url": format!("https://cdn.example.com/{index}.mp4"), "kind": "mp4" }));
        }
        let value = json!({ "url": "https://example.com/watch", "networkHints": list });
        let packet = from_probe(&value).unwrap();
        assert_eq!(packet.network_hints.len(), super::MAX_NET_HINTS, "超限必须截断");
        let unique: std::collections::HashSet<&str> =
            packet.network_hints.iter().map(|hint| hint.url.as_str()).collect();
        assert_eq!(unique.len(), super::MAX_NET_HINTS, "不得出现重复 URL");
    }

    /// 缺省为空表：老扩展（不报 networkHints）零感知。
    #[test]
    fn missing_network_hints_is_empty() {
        assert!(from_probe(&mock_probe()).unwrap().network_hints.is_empty());
        let value = json!({ "url": "https://example.com/a", "networkHints": "oops" });
        assert!(from_probe(&value).unwrap().network_hints.is_empty());
    }
}
