//! # hydra-wal
//!
//! The coordinator's **commit stream** (spec §2.6a) and **control WAL** (spec §1.2), which may
//! share one physical log (disjoint record types). Implements `WAL-FORMAT.md` exactly: record
//! layout, fsync/dir-sync durability rules, partial-tail discard on open, corruption detection,
//! and effect IDs.
//!
//! - [`record`] — on-disk record framing (§1) + type registry (§2).
//! - [`file`]   — file header (§1).
//! - [`writer`] — append + `fdatasync`, dir-sync on create, segment rotation (§3).
//! - [`reader`] — sequential scan with partial-tail discard vs. mid-stream corruption (§3.4).
//! - [`effect_id`] — replay-deterministic effect IDs (§4).

pub mod effect_id;
pub mod file;
pub mod reader;
pub mod record;
pub mod writer;

pub use file::{FileHeader, FILE_HEADER_LEN};
pub use reader::{RecoveredRecord, WalScan};
pub use record::{encode_record, rec_type, RecordHeader};
pub use writer::WalWriter;

/// Errors from the WAL layer. Framing/checksum failures are structured, never panics.
#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("payload too large: {len} > cap {cap}")]
    PayloadTooLarge { len: u64, cap: u32 },

    #[error("file header truncated: have {have} bytes")]
    TruncatedFileHeader { have: usize },
    #[error("bad file magic: {0:#010x}")]
    BadFileMagic(u32),
    #[error("unsupported file format major: file {file_major}, ours {our_major}")]
    UnsupportedFormatMajor { file_major: u8, our_major: u8 },
    #[error("file header checksum mismatch")]
    BadFileHeaderChecksum,

    /// A record failed its BLAKE3 tag but a valid record follows it — this is not a torn tail,
    /// it is mid-stream corruption. Refuse to open (WAL-FORMAT §3.4).
    #[error("mid-stream corruption at offset {offset} (valid records follow); refusing to open")]
    CorruptMidStream { offset: u64 },

    /// An unknown record type with the CRITICAL flag set (WAL-FORMAT §3.5).
    #[error("unknown CRITICAL record type {record_type} at offset {offset}; refusing to open")]
    UnknownCriticalRecord { record_type: u16, offset: u64 },

    /// A GENERATION_COMMIT whose embedded sampler checkpoint violates I19's equalities
    /// (`generated_through == sampled_pos == last_output_pos`), checked on read (WAL-FORMAT §2).
    #[error("I19 violation at offset {offset}: {detail}")]
    I19Violation { offset: u64, detail: String },
}

/// Segment rotation threshold (WAL-FORMAT §3.6).
pub const SEGMENT_ROTATE_BYTES: u64 = 256 * 1024 * 1024;
