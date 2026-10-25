//! 301/302/303/307/308 处理，`--max-redirect` 上限（aria2 默认 20）。
//!
//! 只支持绝对 URI、`//host/path`、`/path` 与相对路径四种 Location；
//! Phase 1 不做 `?query` 形式的相对合并（记 ISSUES）。

use http::Uri;

use crate::error::NetworkError;

/// 判断是否是重定向状态码。
pub fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// 把 `Location` 解析成绝对 URI（相对当前请求 URI）。
pub fn resolve_location(base: &Uri, location: &str) -> Result<Uri, NetworkError> {
    let loc = location.trim();
    if loc.is_empty() {
        return Err(NetworkError::HttpProtocol("Location 为空".into()));
    }

    if let Ok(abs) = loc.parse::<Uri>() {
        if abs.scheme().is_some() {
            return Ok(abs);
        }
    }

    let scheme = base
        .scheme_str()
        .ok_or_else(|| NetworkError::HttpProtocol("原 URI 缺 scheme".into()))?;
    let authority = base
        .authority()
        .ok_or_else(|| NetworkError::HttpProtocol("原 URI 缺 authority".into()))?;

    let joined = if let Some(rest) = loc.strip_prefix("//") {
        format!("{scheme}://{rest}")
    } else if loc.starts_with('/') {
        format!("{scheme}://{authority}{loc}")
    } else {
        let path = base.path();
        let dir = match path.rfind('/') {
            Some(idx) => &path[..=idx],
            None => "/",
        };
        format!("{scheme}://{authority}{dir}{loc}")
    };

    joined
        .parse::<Uri>()
        .map_err(|err| NetworkError::HttpProtocol(format!("非法 Location `{loc}`：{err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Uri {
        "http://a.test:8080/dir/file.bin?x=1".parse().unwrap()
    }

    #[test]
    fn resolves_absolute_and_relative() {
        assert_eq!(
            resolve_location(&base(), "https://b.test/y").unwrap().to_string(),
            "https://b.test/y"
        );
        assert_eq!(
            resolve_location(&base(), "//b.test/y").unwrap().to_string(),
            "http://b.test/y"
        );
        assert_eq!(
            resolve_location(&base(), "/y").unwrap().to_string(),
            "http://a.test:8080/y"
        );
        assert_eq!(
            resolve_location(&base(), "y").unwrap().to_string(),
            "http://a.test:8080/dir/y"
        );
    }

    #[test]
    fn redirect_status_set() {
        for code in [301, 302, 303, 307, 308] {
            assert!(is_redirect(code));
        }
        assert!(!is_redirect(200));
    }
}
