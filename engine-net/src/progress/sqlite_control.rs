//! `<file>.sqlite` 控制文件读写（Aria2-Harness 实验特性，`--sqlite-progress=true`）。
//!
//! 与 `aria2-1.37.0/src/SqliteProgressInfoFile.cc` 逐项对齐：
//!
//! - 库结构：`CREATE TABLE IF NOT EXISTS progress (info_hash TEXT PRIMARY KEY, data BLOB)`
//! - HTTP 任务的主键固定为 `ctrl`；BT 任务用 infoHash 十六进制串（Phase 2 不涉及 BT）。
//! - `data` BLOB **就是 legacy `*.aria2` 的完整字节流**（父类序列化的结果），
//!   因此解析/编码完全复用 [`ControlFile`]，本模块只负责 SQLite 这层壳。
//! - 写入节流与 `.aria2` 同款：SHA-1 未变则不写库。
//! - 删除主库时一并清理 WAL 伴生的 `-wal` / `-shm`。
//!
//! 兼容策略（R5 修订后）：读取优先 `.sqlite`，不存在时回退 legacy `*.aria2`；
//! 写入沿用「载入时命中的格式」，全新任务按 `--sqlite-progress` 决定。

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use sha1::{Digest, Sha1};

use crate::progress::control::{ControlFile, ControlFileError};

/// 产品后缀（`DefaultBtProgressInfoFile::getSuffix()` 的 sqlite 分支）。
pub const SQLITE_SUFFIX: &str = ".sqlite";
/// legacy 后缀（R5 原口径）。
pub const LEGACY_SUFFIX: &str = ".aria2";
/// HTTP 任务在 `progress` 表中的主键（`SqliteProgressInfoFile::primaryKey()`）。
pub const HTTP_KEY: &str = "ctrl";

const CREATE_TABLE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS progress (info_hash TEXT PRIMARY KEY, data BLOB);";

/// `<file>` → `<file>.sqlite`。
pub fn sqlite_path(dest: &Path) -> PathBuf {
    with_suffix(dest, SQLITE_SUFFIX)
}

/// `<file>` → `<file>.aria2`。
pub fn legacy_path(dest: &Path) -> PathBuf {
    with_suffix(dest, LEGACY_SUFFIX)
}

fn with_suffix(dest: &Path, suffix: &str) -> PathBuf {
    let mut name = dest.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// SQLite 控制文件存储。
#[derive(Debug)]
pub struct SqliteControlStore {
    path: PathBuf,
    key: String,
    last_digest: Option<[u8; 20]>,
}

impl SqliteControlStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            key: HTTP_KEY.to_string(),
            last_digest: None,
        }
    }

    /// BT 任务用 infoHash 十六进制串作主键（Phase 2 暂不生成 BT 任务，仅保留能力）。
    pub fn with_key(path: PathBuf, key: impl Into<String>) -> Self {
        Self {
            path,
            key: key.into(),
            last_digest: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 与 aria2 `exists()` 同义：文件存在**且**表里有该主键的记录。
    pub fn exists(&self) -> bool {
        if !self.path.is_file() {
            return false;
        }
        let Ok(conn) = self.open(false) else {
            return false;
        };
        conn.query_row(
            "SELECT 1 FROM progress WHERE info_hash = ?1 LIMIT 1;",
            params![self.key],
            |_| Ok(()),
        )
        .optional()
        .map(|row| row.is_some())
        .unwrap_or(false)
    }

    pub fn load(&self) -> Result<Option<ControlFile>, ControlFileError> {
        if !self.path.is_file() {
            return Ok(None);
        }
        let conn = self.open(false)?;
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT data FROM progress WHERE info_hash = ?1 LIMIT 1;",
                params![self.key],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(sqlite_error)?;
        match blob {
            None => Ok(None),
            Some(bytes) if bytes.is_empty() => Ok(None),
            Some(bytes) => ControlFile::decode(&bytes).map(Some),
        }
    }

    /// 返回是否真的写了库（false = SHA-1 未变）。
    pub fn save(&mut self, control: &ControlFile) -> Result<bool, ControlFileError> {
        let bytes = control.encode();
        let digest: [u8; 20] = Sha1::digest(&bytes).into();
        if self.last_digest.as_ref() == Some(&digest) {
            return Ok(false);
        }

        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(io_error)?;
            }
        }

        let conn = self.open(true)?;
        conn.execute(
            "INSERT OR REPLACE INTO progress (info_hash, data) VALUES (?1, ?2);",
            params![self.key, bytes],
        )
        .map_err(sqlite_error)?;
        self.last_digest = Some(digest);
        Ok(true)
    }

    pub fn remove(&self) -> std::io::Result<()> {
        for path in [
            self.path.clone(),
            sidecar(&self.path, "-wal"),
            sidecar(&self.path, "-shm"),
        ] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    /// 打开数据库。`create=true` 时建表（对齐 aria2 的 `openDatabase(path, create)`）。
    fn open(&self, create: bool) -> Result<Connection, ControlFileError> {
        let flags = if create {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        let conn = Connection::open_with_flags(&self.path, flags).map_err(sqlite_error)?;
        // 与 C++ 侧一致：WAL + synchronous=NORMAL（60s 级提交节拍下足够安全）。
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        if create {
            conn.execute_batch(CREATE_TABLE_SQL).map_err(sqlite_error)?;
        }
        Ok(conn)
    }
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn sqlite_error(err: rusqlite::Error) -> ControlFileError {
    ControlFileError::Io(format!("SQLite：{err}"))
}

fn io_error(err: std::io::Error) -> ControlFileError {
    ControlFileError::Io(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let unique = format!(
            "engine-net-sqlite-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn roundtrip_and_digest_throttle() {
        let dir = temp_dir();
        let path = sqlite_path(&dir.join("file.bin"));
        let mut store = SqliteControlStore::new(path.clone());
        assert!(!store.exists());
        assert_eq!(store.load().unwrap(), None);

        let mut control = ControlFile::new(4 * 1024 * 1024, 1024 * 1024);
        control.mark_piece_done(0);
        assert!(store.save(&control).unwrap());
        // 同内容重复保存必须被 SHA-1 节流拦住。
        assert!(!store.save(&control).unwrap());
        assert!(store.exists());

        let loaded = store.load().unwrap().expect("应能读回");
        assert_eq!(loaded, control);
        assert_eq!(loaded.completed_bytes(), 1024 * 1024);

        store.remove().unwrap();
        assert!(!path.exists());
        assert!(!store.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 最硬的一条：`data` BLOB 必须与 legacy `*.aria2` 的字节流逐字节相同
    /// （C++ 侧就是把 `DefaultBtProgressInfoFile::save(IOFile&)` 的产物塞进 BLOB）。
    #[test]
    fn blob_is_byte_identical_to_legacy_stream() {
        let dir = temp_dir();
        let dest = dir.join("blob.bin");
        let sqlite_file = sqlite_path(&dest);
        let legacy_file = legacy_path(&dest);

        // 20 片 → 位图 3 字节，故意让完成位跨字节且不连续（多连接时就是这样）。
        let mut control = ControlFile::new(20 * 1024 * 1024, 1024 * 1024);
        for index in [0u64, 1, 7, 8, 13, 19] {
            control.mark_piece_done(index);
        }
        // in-flight piece 的位图按 16KiB/block 编码：1MiB → 64 block → 8 字节。
        control.in_flight.push(crate::progress::control::InFlight {
            index: 5,
            length: 1024 * 1024,
            bitfield: vec![0b1000_0000, 0, 0, 0, 0, 0, 0, 0],
        });

        let mut sqlite = SqliteControlStore::new(sqlite_file.clone());
        assert!(sqlite.save(&control).unwrap());
        let mut legacy = crate::progress::control::ControlFileStore::new(legacy_file.clone());
        legacy.save(&control).unwrap();

        let blob = read_blob(&sqlite_file);
        let legacy_bytes = std::fs::read(&legacy_file).unwrap();
        assert_eq!(blob, control.encode(), "BLOB 必须就是 legacy 序列化字节流");
        assert_eq!(blob, legacy_bytes, "BLOB 与 .aria2 文件必须逐字节相同");

        // 读回必须位级相等（含 in-flight 位图）。
        let loaded = SqliteControlStore::new(sqlite_file.clone())
            .load()
            .unwrap()
            .expect("应能读回");
        assert_eq!(loaded, control);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「哪怕 1 bit 不一致」：改动 BLOB 中单个 bit，读回必须只差这一位，
    /// 证明我们既没有归一化位图，也没有丢 in-flight。
    #[test]
    fn single_bit_difference_is_preserved() {
        let dir = temp_dir();
        let path = sqlite_path(&dir.join("bit.bin"));
        let mut control = ControlFile::new(20 * 1024 * 1024, 1024 * 1024);
        control.mark_piece_done(3);

        let mut store = SqliteControlStore::new(path.clone());
        store.save(&control).unwrap();

        let mut blob = read_blob(&path);
        // 布局：version(2) + extension(4) + infoHashLength(4) + pieceLength(4)
        //      + totalLength(8) + uploadLength(8) + bitfieldLength(4) = 34
        const BITFIELD_OFFSET: usize = 34;
        blob[BITFIELD_OFFSET] ^= 0b0000_0001; // 翻转第 7 片的 1 bit
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO progress (info_hash, data) VALUES (?1, ?2);",
            params![HTTP_KEY, blob],
        )
        .unwrap();
        drop(conn);

        let loaded = store.load().unwrap().expect("应能读回");
        assert!(loaded.is_piece_done(7), "被翻转的 bit 必须生效");
        assert!(loaded.is_piece_done(3), "其余 bit 必须原样保留");
        assert_eq!(
            loaded.encode()[BITFIELD_OFFSET..BITFIELD_OFFSET + 3],
            [0b0001_0001, 0, 0],
            "位图内容必须是「原值 XOR 0b1」，不允许任何归一化"
        );
        assert_eq!(loaded.in_flight, control.in_flight);

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn read_blob(path: &Path) -> Vec<u8> {
        let conn = Connection::open(path).unwrap();
        conn.query_row(
            "SELECT data FROM progress WHERE info_hash = ?1 LIMIT 1;",
            params![HTTP_KEY],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .unwrap()
    }

    #[test]
    fn table_layout_matches_aria2() {
        let dir = temp_dir();
        let path = sqlite_path(&dir.join("a.bin"));
        let mut store = SqliteControlStore::new(path.clone());
        store.save(&ControlFile::new(2048, 1024)).unwrap();

        // 直接按 aria2 的 SQL 读，确认库结构/主键完全一致。
        let conn = Connection::open(&path).unwrap();
        let key: String = conn
            .query_row("SELECT info_hash FROM progress LIMIT 1;", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(key, HTTP_KEY);
        let size: i64 = conn
            .query_row("SELECT length(data) FROM progress LIMIT 1;", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(size > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
