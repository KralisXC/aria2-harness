//! 轻量自动更新检查（无代码签名版）。
//!
//! 设计取舍：
//! - **不引入完整更新框架**（自替换 / 差分升级 / 签名校验都不做），只解决"用户知道有新版本"。
//! - 启动后延迟 5s 再请求，绝不与引擎握手、插件加载、首帧渲染抢启动窗口。
//! - 单次检查、单线程、失败静默：断网 / 镜像 404 / 机场 DNS 污染都不影响正常使用。
//! - 只在**远端版本严格大于**本地版本时向前端发事件，同版本 / 降级一律忽略。
//!
//! 远端 JSON 契约（`version.json`）：
//! ```json
//! { "version": "0.9.4", "url": "https://example.com/Aria2-Harness-Setup-0.9.4-x64.exe",
//!   "changelog": "修复托盘唤醒偶发白屏" }
//! ```
//! 可用 `ARIA2_HARNESS_UPDATE_URL` 覆盖请求地址（内网镜像 / 灰度验证用）。

use std::time::Duration;

use serde_json::{json, Value};

/// 远端版本清单的默认地址（发布时指向仓库根目录的 `version.json`）。
pub const DEFAULT_UPDATE_URL: &str =
    "https://raw.githubusercontent.com/lingxi/aria2-harness/main/version.json";
/// 启动后延迟多久开始检查：避开启动期的一切关键路径。
const CHECK_DELAY: Duration = Duration::from_secs(5);
/// 单次请求的总时限（含 DNS / TLS / 读体），超时即放弃本次检查。
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);
/// 向前端派发的事件名（`App.vue` 全局监听）。
pub const EVENT_UPDATE_AVAILABLE: &str = "app://update-available";
/// 环境变量：覆盖检查地址（留空即用默认值）。
const UPDATE_URL_ENV: &str = "ARIA2_HARNESS_UPDATE_URL";

/// 远端清单。
struct RemoteRelease {
    version: String,
    url: String,
    changelog: String,
}

/// 启动一次后台检查（幂等由调用方保证：`bootstrap` 每个进程只调用一次）。
///
/// 用独立 std 线程而非 tokio 任务：`ureq` 是阻塞客户端，塞进运行时需要
/// `spawn_blocking`，反而多一层调度；这里延迟 5s 的语义本身就是线程级的。
pub fn spawn_check() {
    let spawned = std::thread::Builder::new()
        .name("update-check".to_string())
        .spawn(|| {
            std::thread::sleep(CHECK_DELAY);
            let Some(release) = fetch_release() else { return };

            let current = env!("CARGO_PKG_VERSION");
            if !is_newer(&release.version, current) {
                tracing::info!(
                    "更新检查完成：远端 v{} 不高于当前 v{current}，无需提示",
                    release.version
                );
                return;
            }

            tracing::info!(
                "发现新版本 v{}（当前 v{current}）→ {}",
                release.version,
                release.url
            );
            if let Some(host) = crate::host::host() {
                host.emit(
                    EVENT_UPDATE_AVAILABLE,
                    json!({
                        "version": release.version,
                        "url": release.url,
                        "changelog": release.changelog,
                        "current": current,
                    }),
                );
            }
        });
    if let Err(err) = spawned {
        tracing::warn!("更新检查线程启动失败：{err}");
    }
}

/// 实际请求地址：环境变量优先（空白视为未设置）。
fn update_url() -> String {
    std::env::var(UPDATE_URL_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_UPDATE_URL.to_string())
}

/// 拉取并解析远端清单；任何环节失败都返回 `None`（只记 debug 日志，不打扰用户）。
fn fetch_release() -> Option<RemoteRelease> {
    let url = update_url();
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .user_agent(concat!("Aria2-Harness/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();

    let body = match agent.get(&url).call() {
        Ok(mut response) => match response.body_mut().read_to_string() {
            Ok(text) => text,
            Err(err) => {
                tracing::debug!("更新检查读取响应失败：{err}");
                return None;
            }
        },
        Err(err) => {
            tracing::debug!("更新检查请求失败（{url}）：{err}");
            return None;
        }
    };

    let value: Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(err) => {
            tracing::debug!("更新清单不是合法 JSON：{err}");
            return None;
        }
    };

    let version = value.get("version")?.as_str()?.trim().to_string();
    let url = value.get("url")?.as_str()?.trim().to_string();
    if !is_http_url(&url) {
        tracing::debug!("更新清单的下载地址不是 http(s)：{url}");
        return None;
    }
    let changelog = value
        .get("changelog")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    Some(RemoteRelease {
        version,
        url,
        changelog,
    })
}

/// 只接受 http / https：清单可能被中间人替换，绝不能下发 `file:` / 任意协议。
pub fn is_http_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// 版本号比较：`1.0.10 > 1.0.9`（按段做数值比较，而非字符串比较）。
///
/// 规则：先剥掉 `-` / `+` 之后的预发布与构建元数据（`1.0.0-beta.1` → `1.0.0`）
/// → 按 `.` 切段 → 逐段取前导数字 → 缺段补 0 → 第一个不等段决定大小。
/// 无法解析的版本号一律视为"不比当前新"。
fn is_newer(remote: &str, current: &str) -> bool {
    let parse = |text: &str| -> Option<Vec<u64>> {
        // 先去掉预发布 / 构建元数据（`1.0.0-beta.1` → `1.0.0`）。
        // 不能直接按 `.` 切：`beta.1` 的尾巴会被当成第 4 段版本号，
        // 导致同版本号的预发布版被误判为"更新"（例如 1.0.0-beta.1 > 1.0.0）。
        let core = text
            .trim()
            .trim_start_matches('v')
            .split(['-', '+'])
            .next()
            .unwrap_or_default();
        let mut parts = Vec::new();
        for segment in core.split('.') {
            let digits: String = segment.chars().take_while(|c| c.is_ascii_digit()).collect();
            parts.push(digits.parse::<u64>().ok()?);
        }
        if parts.is_empty() {
            return None;
        }
        Some(parts)
    };
    let (Some(mut remote), Some(mut current)) = (parse(remote), parse(current)) else {
        return false;
    };
    let width = remote.len().max(current.len());
    remote.resize(width, 0);
    current.resize(width, 0);
    remote > current
}

#[cfg(test)]
mod tests {
    use super::{is_http_url, is_newer};

    #[test]
    fn numeric_compare_not_lexicographic() {
        assert!(is_newer("1.0.10", "1.0.9"));
        assert!(is_newer("1.1", "1.0.99"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("0.9.9", "1.0.0"));
    }

    #[test]
    fn tolerates_prefix_and_suffix() {
        assert!(is_newer("v1.2.0", "1.1.9"));
        assert!(!is_newer("1.0.0-beta.1", "1.0.0"));
        assert!(!is_newer("not-a-version", "1.0.0"));
    }

    #[test]
    fn only_http_schemes() {
        assert!(is_http_url("https://example.com/a.exe"));
        assert!(is_http_url("http://127.0.0.1:8080/a"));
        assert!(!is_http_url("file:///C:/a.exe"));
        assert!(!is_http_url("javascript:alert(1)"));
    }
}
