//! Recovery scan (WAL-FORMAT.md §3.4): read records sequentially, verifying magic, length,
//! and BLAKE3. The first framing/checksum failure ends the durable log — **truncate to that
//! boundary** (partial-tail discard). A checksum failure that has valid records *after* it is
//! mid-stream corruption: refuse to open. GENERATION_COMMIT records are I19-validated on read.

use std::path::Path;

use crate::file::{FileHeader, FILE_HEADER_LEN};
use crate::record::{
    read_record, rec_type, record_size, ReadStep, MAX_PAYLOAD_LEN, RECORD_HEADER_LEN, RECORD_MAGIC,
};
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

/// How many resync probes to attempt before giving up and refusing (audit H8).
///
/// A probe is only attempted at an offset whose two magic bytes match, and each probe costs at most
/// one BLAKE3 over a declared record. Corrupt data can declare a large length at many offsets, so
/// the work is capped; **exceeding the cap refuses the log** rather than concluding "no valid record
/// follows". Fail-closed is the only safe default here: the alternative is to silently discard
/// however much durable data lay beyond the point we stopped looking.
const MAX_RESYNC_PROBES: usize = 256;

/// **Audit H8 — resync after damage: is there a checksum-valid record anywhere after `from`?**
///
/// # What this replaces, and why it is not the same function
///
/// The previous version stepped forward only over records whose *framing* still parsed
/// (`BadChecksum{total_len}`) and **returned `false` the moment it met `BadFraming` or
/// `Incomplete`** — i.e. as soon as it could not compute how far to jump. That is exactly the case
/// a corrupt frame **header** produces. So a single flipped magic byte in the middle of a log made
/// the scanner answer "nothing valid follows", the scan reported a torn tail, and
/// `WalWriter::open_append` then **truncated the file to that offset** — permanently destroying
/// every record after it, all of which were on the platter and checksum-valid.
///
/// The fix does not need the framing to be intact: **every record starts at an 8-byte-aligned
/// offset** (`FILE_HEADER_LEN` is 72 and `record_size` is always a multiple of 8), so the scan can
/// walk aligned offsets looking for the record magic and try to parse there. A record that parses
/// AND passes its BLAKE3 tag is real durable data by any standard we have.
fn find_valid_record_after(bytes: &[u8], from: usize) -> Option<usize> {
    // Round up to the next 8-aligned offset relative to the file header.
    let mut probe = from.max(FILE_HEADER_LEN);
    let rel = probe - FILE_HEADER_LEN;
    if rel % 8 != 0 {
        probe += 8 - (rel % 8);
    }
    let mut probes = 0usize;
    while probe + RECORD_HEADER_LEN <= bytes.len() {
        if u16::from_le_bytes([bytes[probe], bytes[probe + 1]]) == RECORD_MAGIC {
            probes += 1;
            if probes > MAX_RESYNC_PROBES {
                // Unclassifiable: refuse rather than assume the rest is garbage.
                return Some(probe);
            }
            if let ReadStep::Record { .. } = read_record(&bytes[probe..]) {
                return Some(probe);
            }
        }
        probe += 8;
    }
    None
}

/// **Audit H8 — could the region from `pos` to EOF be a genuine torn tail?**
///
/// A torn tail is **at most one partially-written record**: the writer appends one record at a time
/// and `fdatasync`s between them, so at most one record can be mid-flight when the machine dies.
/// A trailing region longer than the largest record that could ever have been in flight is
/// therefore not a tail, whatever it looks like — and discarding it would silently drop however
/// much durable data it spans.
fn tail_is_bounded(bytes: &[u8], pos: usize) -> bool {
    bytes.len().saturating_sub(pos) <= record_size(MAX_PAYLOAD_LEN as usize)
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
                // Damage. Whether it is a discardable TAIL or fatal MID-STREAM corruption is
                // decided identically for all three kinds (audit H8): a torn tail is at most one
                // partially-written record and has nothing valid after it. Anything else is
                // corruption and the log is refused — never silently truncated.
                ReadStep::Incomplete | ReadStep::BadFraming | ReadStep::BadChecksum { .. } => {
                    // Start the search past this offset: a `BadChecksum` record's own bytes must
                    // not be re-found as "a valid record after the damage".
                    if find_valid_record_after(bytes, pos + 1).is_some() {
                        return Err(WalError::CorruptMidStream { offset: pos as u64 });
                    }
                    if !tail_is_bounded(bytes, pos) {
                        return Err(WalError::CorruptMidStream { offset: pos as u64 });
                    }
                    truncated_tail = true;
                    break;
                }
            }
        }

        Ok(WalScan { header, records, durable_len: pos as u64, truncated_tail })
    }
}
