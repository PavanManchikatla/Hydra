//! Recovery scan (WAL-FORMAT.md §3.4): read records sequentially, verifying magic, length,
//! and BLAKE3. The first framing/checksum failure ends the durable log — **truncate to that
//! boundary** (partial-tail discard). A checksum failure that has valid records *after* it is
//! mid-stream corruption: refuse to open. GENERATION_COMMIT records are I19-validated on read.

use std::path::Path;

use crate::file::{FileHeader, FILE_HEADER_LEN};
use crate::record::{read_record, rec_type, ReadStep};
use crate::WalError;

/// A record recovered by the scan (payload copied out).
#[derive(Debug, Clone)]
pub struct RecoveredRecord {
    pub record_type: u16,
    pub flags: u16,
    pub offset: u64,
    pub payload: Vec<u8>,
}

/// Result of scanning a WAL segment for recovery.
#[derive(Debug)]
pub struct WalScan {
    pub header: FileHeader,
    /// Known records (unknown non-critical types are skipped per §3.5).
    pub records: Vec<RecoveredRecord>,
    /// Byte length of the durable prefix — the file should be truncated to this on reopen.
    pub durable_len: u64,
    /// True if a partial/torn trailing record was discarded.
    pub truncated_tail: bool,
}

fn is_known_type(t: u16) -> bool {
    use rec_type::*;
    matches!(
        t,
        INITIAL_COMMIT
            | SEGMENT_COMMIT
            | GENERATION_COMMIT
            | INPUT_CHUNK_COMMIT
            | BOUNDARY_COPY
            | BEGIN_RECOVERY
            | RESET_RECOVERY_ATTEMPT
            | ACTIVATION_COMMIT_INTENT
            | ACTIVATION_COMPLETE
            | ACTIVATION_ABORT
            | ACTIVATION_UNSERVABLE
            | SESSION_TERMINATE
            | CANCEL_CUTOFF
            | PLACEMENT_INSTALL
            | EVENT_LOG
    )
}

/// Does a checksum-valid record exist at or after `pos`? Advances over intact-but-corrupt
/// records (framing readable, tag bad) by their length; stops at a torn/garbage boundary.
fn any_valid_record_after(bytes: &[u8], mut pos: usize) -> bool {
    while pos < bytes.len() {
        match read_record(&bytes[pos..]) {
            ReadStep::Record { .. } => return true,
            ReadStep::BadChecksum { total_len } => pos += total_len,
            ReadStep::Incomplete | ReadStep::BadFraming => return false,
        }
    }
    false
}

impl WalScan {
    pub fn open(path: impl AsRef<Path>) -> Result<WalScan, WalError> {
        let bytes = std::fs::read(path)?;
        Self::from_bytes(&bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<WalScan, WalError> {
        let header = FileHeader::parse(bytes)?;
        let mut pos = FILE_HEADER_LEN;
        let mut records = Vec::new();
        let mut truncated_tail = false;

        while pos < bytes.len() {
            match read_record(&bytes[pos..]) {
                ReadStep::Record { header: rh, payload, total_len } => {
                    if !is_known_type(rh.record_type) {
                        if rh.is_critical() {
                            // §3.5: unknown + CRITICAL => refuse to open.
                            return Err(WalError::UnknownCriticalRecord {
                                record_type: rh.record_type,
                                offset: pos as u64,
                            });
                        }
                        // §3.5: unknown, non-critical => skip (length-prefixed), keep scanning.
                        pos += total_len;
                        continue;
                    }
                    if rh.record_type == rec_type::GENERATION_COMMIT {
                        // §2: I19 equalities validated on read.
                        hydra_proto::validate_generation_commit_i19(payload).map_err(|detail| {
                            WalError::I19Violation { offset: pos as u64, detail }
                        })?;
                    }
                    records.push(RecoveredRecord {
                        record_type: rh.record_type,
                        flags: rh.flags,
                        offset: pos as u64,
                        payload: payload.to_vec(),
                    });
                    pos += total_len;
                }
                ReadStep::Incomplete | ReadStep::BadFraming => {
                    // Torn/garbage tail: the durable log ends at `pos`.
                    truncated_tail = true;
                    break;
                }
                ReadStep::BadChecksum { total_len } => {
                    if any_valid_record_after(bytes, pos + total_len) {
                        // §3.4: not the tail — mid-stream corruption. Refuse to open.
                        return Err(WalError::CorruptMidStream { offset: pos as u64 });
                    }
                    // Bad tag with nothing valid after => treat as torn tail, discard.
                    truncated_tail = true;
                    break;
                }
            }
        }

        Ok(WalScan { header, records, durable_len: pos as u64, truncated_tail })
    }
}
