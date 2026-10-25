//! `*.aria2` 控制文件读写（字节布局见 docs/c3/Q5-schema.md §2）。
//!
//! 与 `aria2-1.37.0/src/DefaultBtProgressInfoFile.cc` 逐字段对齐：
//! version 1 为大端；`version=0` 为旧版主机序（仅读兼容）。HTTP 下载固定
//! `extension=0`、`infoHashLength=0`、`uploadLength=0`、`in_flight` 为空。

use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

use crate::error::NetworkError;

/// aria2 `OptionHandlerFactory.cc:992` 的默认 piece length。
pub const DEFAULT_PIECE_LENGTH: u32 = 1024 * 1024;

/// 防止伪造文件把内存打爆：in-flight 条目数上限（Q5 §7「先做上界检查再分配」）。
const MAX_IN_FLIGHT: u32 = 1_000_000;

/// aria2 `Piece::BLOCK_LENGTH`：in-flight piece 的位图以 16KiB 为一个 block。
const ARIA2_BLOCK_LENGTH: u64 = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlight {
    pub index: u32,
    pub length: u32,
    pub bitfield: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlFile {
    /// 0 = 旧版主机序，1 = 大端。新建一律为 1。
    pub version: u16,
    pub piece_length: u32,
    pub total_length: u64,
    pub bitfield: Vec<u8>,
    pub in_flight: Vec<InFlight>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ControlFileError {
    #[error("不支持的控制文件版本：{0}")]
    UnsupportedVersion(u16),
    #[error("piece length 不能为 0")]
    ZeroPieceLength,
    #[error("控制文件被截断")]
    Truncated,
    #[error("长度字段非法（过大或溢出）")]
    InvalidLength,
    #[error("bitfield 长度不匹配：期望 {expected}，实际 {actual}")]
    BitfieldLengthMismatch { expected: u32, actual: u32 },
    #[error("piece index 越界：{0}")]
    PieceIndexOutOfRange(u32),
    #[error("in-flight piece 长度越界：{0}")]
    PieceLengthOutOfRange(u32),
    #[error("in-flight bitfield 长度不匹配：期望 {expected}，实际 {actual}")]
    InFlightBitfieldLengthMismatch { expected: u32, actual: u32 },
    #[error("总长不匹配：期望 {expected}，实际 {actual}")]
    TotalLengthMismatch { expected: u64, actual: u64 },
    #[error("检测到 piece length 变化")]
    PieceLengthChanged,
    #[error("控制文件 IO 错误：{0}")]
    Io(String),
}

impl ControlFileError {
    /// 与 aria2 语义对齐：piece length 变化 → 10，其余续传失败 → 8（CANNOT_RESUME）。
    pub fn to_aria2_code(&self) -> i32 {
        match self {
            ControlFileError::PieceLengthChanged => 10,
            _ => 8,
        }
    }
}

impl From<ControlFileError> for NetworkError {
    fn from(err: ControlFileError) -> Self {
        NetworkError::Network(err.to_string())
    }
}

pub fn num_pieces(total_length: u64, piece_length: u32) -> u64 {
    if piece_length == 0 {
        return 0;
    }
    total_length.div_ceil(piece_length as u64)
}

pub fn bitfield_len(total_length: u64, piece_length: u32) -> u32 {
    ((num_pieces(total_length, piece_length) + 7) / 8) as u32
}

impl ControlFile {
    /// 新建一个全空的 HTTP 控制文件（in_flight 为空，对齐 Q5 §9.2）。
    pub fn new(total_length: u64, piece_length: u32) -> Self {
        Self {
            version: 1,
            piece_length,
            total_length,
            bitfield: vec![0u8; bitfield_len(total_length, piece_length) as usize],
            in_flight: Vec::new(),
        }
    }

    pub fn num_pieces(&self) -> u64 {
        num_pieces(self.total_length, self.piece_length)
    }

    pub fn completed_bytes(&self) -> u64 {
        (0..self.num_pieces())
            .filter(|idx| self.is_piece_done(*idx))
            .map(|idx| {
                let start = idx * self.piece_length as u64;
                (self.total_length - start).min(self.piece_length as u64)
            })
            .sum()
    }

    /// 该 piece 的实际长度（最后一片可能不足）。
    pub fn piece_length_at(&self, index: u64) -> u64 {
        let start = index.saturating_mul(self.piece_length as u64);
        if start >= self.total_length {
            0
        } else {
            (self.total_length - start).min(self.piece_length as u64)
        }
    }

    /// 从 0 起连续完成的字节数（单连接续传起点；位图允许空洞）。
    pub fn contiguous_prefix(&self) -> u64 {
        let mut done = 0u64;
        for index in 0..self.num_pieces() {
            if !self.is_piece_done(index) {
                break;
            }
            done += self.piece_length_at(index);
        }
        done
    }

    pub fn is_piece_done(&self, index: u64) -> bool {
        if index >= self.num_pieces() {
            return false;
        }
        let byte = (index / 8) as usize;
        let bit = 7 - (index % 8) as u32; // 高位在前
        self.bitfield
            .get(byte)
            .map(|b| b & (1 << bit) != 0)
            .unwrap_or(false)
    }

    pub fn mark_piece_done(&mut self, index: u64) {
        if index >= self.num_pieces() {
            return;
        }
        let byte = (index / 8) as usize;
        let bit = 7 - (index % 8) as u32;
        if let Some(slot) = self.bitfield.get_mut(byte) {
            *slot |= 1 << bit;
        }
    }

    /// 按「已连续下载的字节数」补记整片完成；返回是否有新片被标记。
    ///
    /// 最后一片只有在 `completed == total_length` 时才算完成（与 aria2 位图语义一致）。
    pub fn mark_completed_bytes(&mut self, completed: u64) -> bool {
        let complete = completed.min(self.total_length);
        if self.piece_length == 0 {
            return false;
        }
        let mut changed = false;
        let full = complete / self.piece_length as u64;
        for index in 0..full.min(self.num_pieces()) {
            if !self.is_piece_done(index) {
                self.mark_piece_done(index);
                changed = true;
            }
        }
        if complete == self.total_length && self.num_pieces() > 0 {
            let last = self.num_pieces() - 1;
            if !self.is_piece_done(last) {
                self.mark_piece_done(last);
                changed = true;
            }
        }
        changed
    }

    pub fn is_complete(&self) -> bool {
        self.completed_bytes() == self.total_length
    }

    /// 逐字段编码（version 1 大端；version 0 主机序）。
    pub fn encode(&self) -> Vec<u8> {
        let be = self.version >= 1;
        let mut out = Vec::with_capacity(64 + self.bitfield.len());
        put_u16(&mut out, self.version, be);
        put_u32(&mut out, 0, be); // extension：HTTP 固定 0
        put_u32(&mut out, 0, be); // infoHashLength
        put_u32(&mut out, self.piece_length, be);
        put_u64(&mut out, self.total_length, be);
        put_u64(&mut out, 0, be); // uploadLength
        put_u32(&mut out, self.bitfield.len() as u32, be);
        out.extend_from_slice(&self.bitfield);
        put_u32(&mut out, self.in_flight.len() as u32, be);
        for piece in &self.in_flight {
            put_u32(&mut out, piece.index, be);
            put_u32(&mut out, piece.length, be);
            put_u32(&mut out, piece.bitfield.len() as u32, be);
            out.extend_from_slice(&piece.bitfield);
        }
        out
    }

    /// 只做格式自洽校验（version / pieceLength / bitfield 长度 / in-flight 范围）。
    /// 与具体文件的比对见 `validate_against`。
    pub fn decode(bytes: &[u8]) -> Result<Self, ControlFileError> {
        let mut r = Reader::new(bytes);
        let version = r.u16(true)?;
        if version > 1 {
            return Err(ControlFileError::UnsupportedVersion(version));
        }
        let be = version >= 1;

        let _extension = r.u32(be)?;
        let info_hash_len = r.u32(be)?;
        if info_hash_len > 20 {
            return Err(ControlFileError::InvalidLength);
        }
        r.take(info_hash_len as usize)?;

        let piece_length = r.u32(be)?;
        if piece_length == 0 {
            return Err(ControlFileError::ZeroPieceLength);
        }
        let total_length = r.u64(be)?;
        let _upload_length = r.u64(be)?;

        let bitfield_length = r.u32(be)?;
        let expected = bitfield_len(total_length, piece_length);
        if bitfield_length != expected {
            return Err(ControlFileError::BitfieldLengthMismatch {
                expected,
                actual: bitfield_length,
            });
        }
        let bitfield = r.take(bitfield_length as usize)?.to_vec();

        let num_in_flight = r.u32(be)?;
        if num_in_flight > MAX_IN_FLIGHT {
            return Err(ControlFileError::InvalidLength);
        }
        let mut in_flight = Vec::with_capacity(num_in_flight as usize);
        for _ in 0..num_in_flight {
            let index = r.u32(be)?;
            if index as u64 >= num_pieces(total_length, piece_length) {
                return Err(ControlFileError::PieceIndexOutOfRange(index));
            }
            let length = r.u32(be)?;
            if length > piece_length {
                return Err(ControlFileError::PieceLengthOutOfRange(length));
            }
            let piece_bitfield_length = r.u32(be)?;
            // aria2 的 in-flight piece 位图按 Piece::BLOCK_LENGTH(16KiB) 分块：
            // 每块 1 bit，再向上取整到字节（Piece.cc 构造 BitfieldMan 的语义）。
            let blocks = (length as u64).div_ceil(ARIA2_BLOCK_LENGTH);
            let expected_piece_bitfield = blocks.div_ceil(8) as u32;
            if piece_bitfield_length != expected_piece_bitfield {
                return Err(ControlFileError::InFlightBitfieldLengthMismatch {
                    expected: expected_piece_bitfield,
                    actual: piece_bitfield_length,
                });
            }
            let bitfield = r.take(piece_bitfield_length as usize)?.to_vec();
            in_flight.push(InFlight {
                index,
                length,
                bitfield,
            });
        }

        Ok(Self {
            version,
            piece_length,
            total_length,
            bitfield,
            in_flight,
        })
    }

    /// 与磁盘上的实际文件比对（aria2 `load()` 的 249-413 行等价逻辑）。
    pub fn validate_against(
        &self,
        actual_total_length: u64,
        current_piece_length: u32,
        allow_piece_length_change: bool,
    ) -> Result<(), ControlFileError> {
        if self.total_length != actual_total_length {
            return Err(ControlFileError::TotalLengthMismatch {
                expected: actual_total_length,
                actual: self.total_length,
            });
        }
        if self.piece_length != current_piece_length
            && (self.completed_bytes() > 0 || !self.in_flight.is_empty())
            && !allow_piece_length_change
        {
            return Err(ControlFileError::PieceLengthChanged);
        }
        Ok(())
    }
}

fn put_u16(out: &mut Vec<u8>, value: u16, be: bool) {
    out.extend_from_slice(&if be {
        value.to_be_bytes()
    } else {
        value.to_ne_bytes()
    });
}

fn put_u32(out: &mut Vec<u8>, value: u32, be: bool) {
    out.extend_from_slice(&if be {
        value.to_be_bytes()
    } else {
        value.to_ne_bytes()
    });
}

fn put_u64(out: &mut Vec<u8>, value: u64, be: bool) {
    out.extend_from_slice(&if be {
        value.to_be_bytes()
    } else {
        value.to_ne_bytes()
    });
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ControlFileError> {
        let end = self.pos.checked_add(n).ok_or(ControlFileError::InvalidLength)?;
        if end > self.data.len() {
            return Err(ControlFileError::Truncated);
        }
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u16(&mut self, be: bool) -> Result<u16, ControlFileError> {
        let b: [u8; 2] = self.take(2)?.try_into().map_err(|_| ControlFileError::Truncated)?;
        Ok(if be {
            u16::from_be_bytes(b)
        } else {
            u16::from_ne_bytes(b)
        })
    }

    fn u32(&mut self, be: bool) -> Result<u32, ControlFileError> {
        let b: [u8; 4] = self.take(4)?.try_into().map_err(|_| ControlFileError::Truncated)?;
        Ok(if be {
            u32::from_be_bytes(b)
        } else {
            u32::from_ne_bytes(b)
        })
    }

    fn u64(&mut self, be: bool) -> Result<u64, ControlFileError> {
        let b: [u8; 8] = self.take(8)?.try_into().map_err(|_| ControlFileError::Truncated)?;
        Ok(if be {
            u64::from_be_bytes(b)
        } else {
            u64::from_ne_bytes(b)
        })
    }
}

/// 控制文件落盘：`<file>__temp` + 原子替换；内容未变（SHA-1 相同）则不写。
#[derive(Debug)]
pub struct ControlFileStore {
    path: PathBuf,
    last_digest: Option<[u8; 20]>,
}

impl ControlFileStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            last_digest: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    pub fn load(&self) -> Result<Option<ControlFile>, ControlFileError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => ControlFile::decode(&bytes).map(Some),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(ControlFileError::Io(err.to_string())),
        }
    }

    /// 返回是否真的写了盘（false = SHA-1 未变，跳过）。
    pub fn save(&mut self, control: &ControlFile) -> Result<bool, ControlFileError> {
        let bytes = control.encode();
        let digest: [u8; 20] = Sha1::digest(&bytes).into();
        if self.last_digest.as_ref() == Some(&digest) {
            return Ok(false);
        }

        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| ControlFileError::Io(err.to_string()))?;
            }
        }

        let temp = temp_path(&self.path);
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&temp)
                .map_err(|err| ControlFileError::Io(err.to_string()))?;
            file.write_all(&bytes)
                .map_err(|err| ControlFileError::Io(err.to_string()))?;
            file.sync_all()
                .map_err(|err| ControlFileError::Io(err.to_string()))?;
        }
        replace_file(&temp, &self.path)
            .map_err(|err| ControlFileError::Io(err.to_string()))?;
        self.last_digest = Some(digest);
        Ok(true)
    }

    pub fn remove(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push("__temp");
    PathBuf::from(name)
}

/// 原子替换（Windows 用 `MoveFileExW(REPLACE_EXISTING)`，与 aria2 一致）。
#[cfg(windows)]
pub(crate) fn replace_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let src_wide: Vec<u16> = src.as_os_str().encode_wide().chain(Some(0)).collect();
    let dst_wide: Vec<u16> = dst.as_os_str().encode_wide().chain(Some(0)).collect();
    let ok = unsafe {
        MoveFileExW(
            src_wide.as_ptr(),
            dst_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn replace_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::rename(src, dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitfield_roundtrip_and_completion() {
        let mut control = ControlFile::new(10, 5);
        assert_eq!(control.bitfield.len(), 1);
        control.mark_piece_done(0);
        assert_eq!(control.completed_bytes(), 5);
        assert!(!control.is_complete());
        control.mark_piece_done(1);
        assert!(control.is_complete());

        let decoded = ControlFile::decode(&control.encode()).expect("解码");
        assert_eq!(decoded, control);
        assert_eq!(decoded.completed_bytes(), 10);
    }

    #[test]
    fn rejects_truncated_and_absurd_files() {
        let control = ControlFile::new(10, 5);
        let bytes = control.encode();
        assert_eq!(
            ControlFile::decode(&bytes[..bytes.len() - 1]),
            Err(ControlFileError::Truncated)
        );

        // 伪造 numInFlight = u32::MAX：必须在分配前拒绝。
        let mut forged = bytes.clone();
        let len = forged.len();
        forged[len - 4..].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(
            ControlFile::decode(&forged),
            Err(ControlFileError::InvalidLength)
        );
    }

    #[test]
    fn marks_whole_pieces_by_completed_bytes() {
        let mut control = ControlFile::new(10, 5);
        assert!(!control.mark_completed_bytes(4));
        assert_eq!(control.completed_bytes(), 0);
        assert!(control.mark_completed_bytes(5));
        assert_eq!(control.completed_bytes(), 5);
        // 最后一片不完整：即使写了 9 字节也不能标记。
        assert!(!control.mark_completed_bytes(9));
        assert_eq!(control.completed_bytes(), 5);
        assert!(control.mark_completed_bytes(10));
        assert!(control.is_complete());

        // 非整除总长：最后一片在 total 处标记。
        let mut odd = ControlFile::new(12, 5);
        assert!(odd.mark_completed_bytes(11));
        assert_eq!(odd.completed_bytes(), 10);
        assert!(odd.mark_completed_bytes(12));
        assert!(odd.is_complete());
    }

    #[test]
    fn validates_total_length_and_piece_length() {
        let control = ControlFile::new(10, 5);
        assert_eq!(
            control.validate_against(11, 5, false),
            Err(ControlFileError::TotalLengthMismatch {
                expected: 11,
                actual: 10
            })
        );

        let mut partial = ControlFile::new(10, 5);
        partial.mark_piece_done(0);
        assert_eq!(
            partial.validate_against(10, 1024, false),
            Err(ControlFileError::PieceLengthChanged)
        );
        assert!(partial.validate_against(10, 1024, true).is_ok());
    }
}
