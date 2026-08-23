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
    /// **Audit H9 — the writer is poisoned by any I/O error and never writes again.**
    ///
    /// A failed append is not a failed *operation*, it is an **unknown log state**: `write_all` can
    /// return an error after a short write, and `sync_data` can fail after part of the record has
    /// been written back. Either way some prefix of a record may be on the platter, and the writer
    /// cannot find out how much without reading its own file back.
    ///
    /// Accepting the next append is then actively destructive: it places a checksum-valid record
    /// immediately behind a partial one, which is **mid-stream corruption by construction** — and
    /// since H8 that is a log which refuses to open at all. So one transient `ENOSPC` would cost
    /// the whole session's ledger rather than the single write that failed. The writer therefore
    /// stops at the first error and says so, and recovery goes through `WalScan` +
    /// [`WalWriter::open_append`], which is the only path that establishes what is actually durable.
    poisoned: Option<String>,
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
        Ok(Self { file, dir, path, size: hdr.len() as u64, poisoned: None })
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
        Ok(Self { file, dir, path, size: durable_len, poisoned: None })
    }

    /// Append one record and `fdatasync`. Returns the record's start offset. The record is
    /// durable when this returns (WAL-FORMAT §3.1).
    pub fn append(&mut self, record_type: u16, flags: u16, payload: &[u8]) -> Result<u64, WalError> {
        if let Some(why) = &self.poisoned {
            return Err(WalError::WriterPoisoned { why: why.clone() });
        }
        let rec = encode_record(record_type, flags, payload)?;
        let offset = self.size;
        // From here to the end of `sync_data`, a failure leaves the on-disk state UNKNOWN — an
        // unknown number of bytes may have landed. Every such error poisons the writer (H9).
        if let Err(e) = self.file.write_all(&rec) {
            return Err(self.poison(format!("write failed at offset {offset}: {e}")));
        }
        if let Err(e) = self.file.sync_data() {
            return Err(self.poison(format!("fdatasync failed for the record at offset {offset}: {e}")));
        }
        self.size += rec.len() as u64;
        Ok(offset)
    }

    /// Poison the writer and return the error to report. Idempotent: the FIRST cause is kept,
    /// because that is the one that explains the state of the file.
    fn poison(&mut self, why: String) -> WalError {
        if self.poisoned.is_none() {
            self.poisoned = Some(why.clone());
        }
        WalError::WriterPoisoned { why: self.poisoned.clone().unwrap_or(why) }
    }

    /// Whether this writer has been poisoned by an I/O error (audit H9).
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.is_some()
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
