//! File header (WAL-FORMAT.md §1). 72 bytes; BLAKE3 over the first 40.
//!
//! ```text
//! file_header := magic:u32 = 0x4859574C ("HYWL") | format_version:u16 = 0x0100
//!              | flags:u16 | cluster_id:[16]u8 | session_scope:[16]u8
//!              | header_blake3:[32]u8   # of the preceding 40 bytes
//! ```

use crate::WalError;

pub const FILE_MAGIC: u32 = 0x4859_574C; // "HYWL"
pub const FORMAT_VERSION: u16 = 0x0100; // (major<<8)|minor => major 1, minor 0
pub const FILE_HEADER_LEN: usize = 72; // 4 + 2 + 2 + 16 + 16 + 32
const HEADER_SIGNED_LEN: usize = 40; // bytes covered by header_blake3

pub const FLAG_CONTAINS_COMMIT_STREAM: u16 = 0x0001;
pub const FLAG_CONTAINS_CONTROL_WAL: u16 = 0x0002;

#[inline]
fn major(v: u16) -> u8 {
    (v >> 8) as u8
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileHeader {
    pub flags: u16,
    pub cluster_id: [u8; 16],
    /// Zero UUID = multi-session file (WAL-FORMAT §1).
    pub session_scope: [u8; 16],
}

impl FileHeader {
    pub fn encode(&self) -> [u8; FILE_HEADER_LEN] {
        let mut b = [0u8; FILE_HEADER_LEN];
        b[0..4].copy_from_slice(&FILE_MAGIC.to_le_bytes());
        b[4..6].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        b[6..8].copy_from_slice(&self.flags.to_le_bytes());
        b[8..24].copy_from_slice(&self.cluster_id);
        b[24..40].copy_from_slice(&self.session_scope);
        let tag = blake3::hash(&b[..HEADER_SIGNED_LEN]);
        b[40..72].copy_from_slice(tag.as_bytes());
        b
    }

    /// Parse + validate a file header. Major-version mismatch => refuse to open (WAL-FORMAT §3.5);
    /// higher minor, same major => open read-compatible.
    pub fn parse(buf: &[u8]) -> Result<FileHeader, WalError> {
        if buf.len() < FILE_HEADER_LEN {
            return Err(WalError::TruncatedFileHeader { have: buf.len() });
        }
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != FILE_MAGIC {
            return Err(WalError::BadFileMagic(magic));
        }
        let version = u16::from_le_bytes([buf[4], buf[5]]);
        if major(version) != major(FORMAT_VERSION) {
            return Err(WalError::UnsupportedFormatMajor {
                file_major: major(version),
                our_major: major(FORMAT_VERSION),
            });
        }
        let expected = blake3::hash(&buf[..HEADER_SIGNED_LEN]);
        if &buf[40..72] != expected.as_bytes() {
            return Err(WalError::BadFileHeaderChecksum);
        }
        let flags = u16::from_le_bytes([buf[6], buf[7]]);
        let mut cluster_id = [0u8; 16];
        let mut session_scope = [0u8; 16];
        cluster_id.copy_from_slice(&buf[8..24]);
        session_scope.copy_from_slice(&buf[24..40]);
        Ok(FileHeader { flags, cluster_id, session_scope })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = FileHeader {
            flags: FLAG_CONTAINS_COMMIT_STREAM | FLAG_CONTAINS_CONTROL_WAL,
            cluster_id: [7u8; 16],
            session_scope: [0u8; 16],
        };
        let enc = h.encode();
        assert_eq!(FileHeader::parse(&enc).unwrap(), h);
    }

    #[test]
    fn rejects_bad_magic_and_major() {
        let h = FileHeader { flags: 0, cluster_id: [0; 16], session_scope: [0; 16] };
        let mut enc = h.encode();
        let mut bad = enc;
        bad[0] ^= 0xFF;
        assert!(matches!(FileHeader::parse(&bad), Err(WalError::BadFileMagic(_))));
        // bump major byte (high byte of version u16 at offset 4)
        enc[5] = enc[5].wrapping_add(1);
        // re-sign so only the version differs
        let tag = blake3::hash(&enc[..40]);
        enc[40..72].copy_from_slice(tag.as_bytes());
        assert!(matches!(
            FileHeader::parse(&enc),
            Err(WalError::UnsupportedFormatMajor { .. })
        ));
    }
}
