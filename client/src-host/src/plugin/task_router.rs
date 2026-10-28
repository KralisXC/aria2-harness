//! 任务路由（beta10 · 模块 B）：把 URL 按 manifest `routing` 规则交给插件认领。
//!
//! 与 `interceptor` 的职责边界：
//! - TaskRouter 决定"这单是否由插件接管"（可接管整个下载生命周期）；
//! - Interceptor 决定"URL 是否被改写后继续交给引擎"。
//!
//! 匹配语义：glob（`*` 任意长度、`?` 单字符），大小写不敏感。
//! 候选依次为完整 URL、去 query/fragment 的 URL、URL path ——
//! 这样 `*.m3u8` 既能命中 `https://a/x.m3u8`，也能命中带鉴权参数的同一地址。

use std::cmp::Ordering;

/// 一条参与仲裁的路由规则（由 `PluginManager` 从已启用插件聚合）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteEntry {
    pub plugin_id: String,
    /// 插件级优先级（manifest.priority），规则同优先级时作为次序。
    pub plugin_priority: i64,
    pub pattern: String,
    /// 规则级优先级（routing[].priority），数值越小越优先。
    pub priority: i64,
    pub handler: String,
}

/// 从候选规则中挑出首匹配：`(rule.priority, plugin.priority, plugin.id)` 升序。
pub fn match_route<'a>(entries: &'a [RouteEntry], url: &str) -> Option<&'a RouteEntry> {
    entries
        .iter()
        .filter(|entry| matches_url(&entry.pattern, url))
        .min_by(|a, b| {
            (a.priority, a.plugin_priority, a.plugin_id.as_str())
                .cmp(&(b.priority, b.plugin_priority, b.plugin_id.as_str()))
                .then(Ordering::Equal)
        })
}

fn matches_url(pattern: &str, url: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    let candidates = candidates(url);
    candidates
        .iter()
        .any(|candidate| glob_match(pattern, candidate))
}

/// 供 glob 匹配的候选串：完整 URL → 去 query/fragment → path → 末段。
fn candidates(url: &str) -> Vec<&str> {
    let trimmed = url.trim();
    let mut out = vec![trimmed];
    let without_query = trimmed
        .split(['?', '#'])
        .next()
        .unwrap_or(trimmed);
    if without_query != trimmed {
        out.push(without_query);
    }
    if let Some(path) = without_query
        .split_once("://")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split_once('/').map(|(_, path)| path))
    {
        out.push(path);
        if let Some(last) = path.rsplit('/').next() {
            out.push(last);
        }
    }
    out
}

/// 经典双指针 glob 匹配（`*` 回溯），大小写不敏感。
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let t: Vec<char> = text.to_ascii_lowercase().chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, pattern: &str, priority: i64, plugin_priority: i64) -> RouteEntry {
        RouteEntry {
            plugin_id: id.to_string(),
            plugin_priority,
            pattern: pattern.to_string(),
            priority,
            handler: "onTaskDetected".to_string(),
        }
    }

    #[test]
    fn plain_suffix_and_wildcards_match() {
        assert!(glob_match("*.m3u8", "https://cdn.test/a/b.m3u8"));
        assert!(glob_match("*playlist*", "https://cdn.test/playlist.m3u8"));
        assert!(glob_match("https://cdn.test/?.m3u8", "https://cdn.test/a.m3u8"));
        assert!(!glob_match("*.mp4", "https://cdn.test/a.m3u8"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(glob_match("*.M3U8", "https://cdn.test/A.M3U8"));
    }

    #[test]
    fn query_string_is_ignored_for_pattern_matching() {
        let rules = vec![entry("hls", "*.m3u8", 10, 100)];
        let hit = match_route(&rules, "https://cdn.test/a.m3u8?token=abc").unwrap();
        assert_eq!(hit.plugin_id, "hls");
    }

    #[test]
    fn lower_priority_value_wins() {
        let rules = vec![entry("slow", "*.m3u8", 50, 1), entry("fast", "*.m3u8", 10, 200)];
        assert_eq!(match_route(&rules, "https://x/a.m3u8").unwrap().plugin_id, "fast");
    }

    #[test]
    fn plugin_priority_breaks_rule_tie() {
        let rules = vec![entry("b", "*.m3u8", 10, 20), entry("a", "*.m3u8", 10, 5)];
        assert_eq!(match_route(&rules, "https://x/a.m3u8").unwrap().plugin_id, "a");
    }

    #[test]
    fn no_matching_rule_returns_none() {
        let rules = vec![entry("hls", "*.m3u8", 10, 100)];
        assert!(match_route(&rules, "https://x/a.mp4").is_none());
    }

    #[test]
    fn empty_pattern_never_matches() {
        assert!(!matches_url("   ", "https://x/a.m3u8"));
    }
}
