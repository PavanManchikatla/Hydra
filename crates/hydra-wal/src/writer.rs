//! Append path (WAL-FORMAT.md §3): write record bytes → `fdatasync`; `fsync` the parent
//! directory after creating/rotating a segment; a watermark advances only after the
//! `fdatasync` that made its record durable returns.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::file::FileHeader;
use crate::record::encode_record;
use crate::{WalError, SEGMENT_ROTATE_BYTES};

/// `fsync` a directory so a newly created/renamed entry is durable (WAL-FORMAT §3.2).
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    // Opening a directory read-only and fsync'ing it is the portable way to persist the
    // directory entry on Unix.
    File::open(dir)?.sync_all()
}

/// Appends length-prefixed, BLAKE3-tagged records to a single WAL segment with per-record
/// `fdatasync`. One writer owns one segment file.
pub struct WalWriter {
    file: File,
    dir: PathBuf,
    path: PathBuf,
    size: u64,
}

impl WalWriter {
    /// Create a fresh segment: write the file header, `fdatasync` it, then `fsync` the parent
    /// directory so the new file is durable before any record is appended (§3.2). Fails if the
    /// file already exists (never clobber a WAL).
    pub fn create(path: impl AsRef<Path>, header: &FileHeader) -> Result<Self, WalError> {
        let path = path.as_ref().to_path_buf();
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let mut file = OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
        let hdr = header.encode();
        file.write_all(&hdr)?;
        file.sync_data()?;
        sync_dir(&dir)?;
        Ok(Self { file, dir, path, size: hdr.len() as u64 })
    }

    /// Reopen an existing segment for appending, positioned at `durable_len` (typically the
    /// value returned by recovery after partial-tail discard).
    pub fn open_append(path: impl AsRef<Path>, durable_len: u64) -> Result<Self, WalError> {
        let path = path.as_ref().to_path_buf();
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        file.set_len(durable_len)?; // discard any partial tail
        file.sync_data()?;
        use std::io::{Seek, SeekFrom};
        let mut file = file;
        file.seek(SeekFrom::Start(durable_len))?;
        Ok(Self { file, dir, path, size: durable_len })
    }

    /// Append one record and `fdatasync`. Returns the record's start offset. The record is
    /// durable when this returns (WAL-FORMAT §3.1).
    pub fn append(&mut self, record_type: u16, flags: u16, payload: &[u8]) -> Result<u64, WalError> {
        let rec = encode_record(record_type, flags, payload)?;
        let offset = self.size;
        self.file.write_all(&rec)?;
        self.file.sync_data()?; // fdatasync — record is durable after this returns
        self.size += rec.len() as u64;
        Ok(offset)
    }

    /// Current on-disk size (offset of the next append).
    pub fn len(&self) -> u64 {
        self.size
    }

    pub fn is_empty(&self) -> bool {
        false // always has at least a file header
    }

    /// Whether the segment has reached the rotation threshold (WAL-FORMAT §3.6).
    pub fn should_rotate(&self) -> bool {
        self.size >= SEGMENT_ROTATE_BYTES
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}
