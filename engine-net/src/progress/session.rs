//! `aria2.session` 条目序列化（格式见 docs/c3/Q5-schema.md §5）。
//!
//! 单条目：
//! ```text
//! <uri1>\t<uri2>\t\n
//!  gid=0123456789abcdef\n
//!  pause=true\n
//!  header=Accept: */*\n
//! ```
//! URI 行以 `\t` 分隔、`\n` 结束；选项行以一个空格开头。

use std::path::{Path, PathBuf};

use crate::progress::control::replace_file;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionEntry {
    pub uris: Vec<String>,
    /// `(name, value)`，保持写入顺序（`header` 可重复）。
    pub options: Vec<(String, String)>,
}

impl SessionEntry {
    pub fn gid(&self) -> Option<&str> {
        self.option("gid")
    }

    pub fn is_paused(&self) -> bool {
        self.option("pause") == Some("true")
    }

    pub fn option(&self, name: &str) -> Option<&str> {
        self.options
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFile {
    pub entries: Vec<SessionEntry>,
}

impl SessionFile {
    pub fn encode(&self) -> String {
        let mut out = String::new();
        for entry in &self.entries {
            for uri in &entry.uris {
                out.push_str(uri);
                out.push('\t');
            }
            out.push('\n');
            for (name, value) in &entry.options {
                out.push(' ');
                out.push_str(name);
                out.push('=');
                out.push_str(value);
                out.push('\n');
            }
        }
        out
    }

    /// 宽松解析：空行跳过，未知选项原样保留。
    pub fn decode(text: &str) -> Self {
        let mut entries: Vec<SessionEntry> = Vec::new();
        let mut current = SessionEntry::default();

        let flush = |current: &mut SessionEntry, entries: &mut Vec<SessionEntry>| {
            if !current.uris.is_empty() || !current.options.is_empty() {
                entries.push(std::mem::take(current));
            }
        };

        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            if let Some(option) = line.strip_prefix(' ') {
                if let Some((name, value)) = option.split_once('=') {
                    current
                        .options
                        .push((name.to_string(), value.to_string()));
                }
                continue;
            }

            // URI 行：前一条若已收集选项，则先落盘。
            if !current.uris.is_empty() {
                flush(&mut current, &mut entries);
            }
            let trimmed = line.trim_end_matches('\t');
            for uri in trimmed.split('\t') {
                if !uri.is_empty() {
                    current.uris.push(uri.to_string());
                }
            }
        }
        flush(&mut current, &mut entries);

        Self { entries }
    }

    pub fn load(path: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(Self::decode(&text))),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// `<file>__temp` + 原子替换（Q5 §5）。
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let temp = temp_path(path);
        std::fs::write(&temp, self.encode())?;
        replace_file(&temp, path)
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push("__temp");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_gid_pause_and_headers() {
        let session = SessionFile {
            entries: vec![
                SessionEntry {
                    uris: vec![
                        "http://a.test/1".into(),
                        "http://a.test/2".into(),
                    ],
                    options: vec![
                        ("gid".into(), "fffe000000000001".into()),
                        ("pause".into(), "true".into()),
                        ("header".into(), "Accept: */*".into()),
                        ("header".into(), "Referer: http://x".into()),
                    ],
                },
                SessionEntry {
                    uris: vec!["https://b.test/f".into()],
                    options: vec![("gid".into(), "fffe000000000002".into())],
                },
            ],
        };

        let text = session.encode();
        assert!(text.starts_with("http://a.test/1\thttp://a.test/2\t\n gid="));
        assert_eq!(SessionFile::decode(&text), session);
    }

    #[test]
    fn tolerates_empty_and_trailing_lines() {
        let text = "\nhttp://a.test/x\t\n gid=1\n\n";
        let parsed = SessionFile::decode(text);
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].gid(), Some("1"));
        assert!(!parsed.entries[0].is_paused());
    }
}
