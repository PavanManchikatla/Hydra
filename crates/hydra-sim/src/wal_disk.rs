//! A **virtual disk** that drives the coordinator's durability through the *real* `hydra-wal`
//! codec, so the M0 §5 torn-write contract protects the sim's durability model instead of an
//! abstract stand-in.
//!
//! The disk is an in-memory `Vec<u8>` — the codec is pure over bytes (`encode_record` +
//! `WalScan::from_bytes`), so no real file I/O is needed and the sim stays deterministic. Each
//! coordinator `WriteWal` effect is encoded (real framing: magic, length, 8-byte pad, BLAKE3 tag)
//! into a *pending* buffer; `fsync` (the coordinator's `WalDurable`) flushes pending → durable;
//! and a crash **before** the `fdatasync` returns tears the in-flight write.
//!
//! **Torn-write model (faithful to WAL-FORMAT's durability point).** An un-`fdatasync`'d write, if
//! the coordinator crashes, is modeled as *non-durable*: it reaches disk as nothing, as a
//! truncated (incomplete) prefix, or as a full-length but bit-flipped (checksum-failed) tail —
//! **never** as a clean, complete, valid record. All three are discardable tails under the real
//! scanner (§3.4 partial-tail discard), so recovery yields exactly the `fdatasync`'d records —
//! which must equal the coordinator's own durable WAL. A clean complete record is deliberately
//! excluded: that would be a legitimately-recoverable pre-fsync write, a safe *superset* the
//! abstract coordinator conservatively drops, and asserting strict equality there would false-flag.

use hydra_state::{ActivationKind, ActivationTuple, WalRecord};
use hydra_wal::file::FLAG_CONTAINS_CONTROL_WAL;
use hydra_wal::{encode_record, rec_type, FileHeader, WalScan};

use crate::rng::Rng;

/// Encode a coordinator [`WalRecord`] to its `(record_type, payload)` for the real codec. The
/// payload is a compact, reversible little-endian encoding of the record's fields — the codec
/// treats payloads opaquely (only `GENERATION_COMMIT`, which the activation core never writes, is
/// content-validated on read), so this exercises the framing/torn-write contract, not a schema.
fn encode_wal(rec: &WalRecord) -> (u16, Vec<u8>) {
    let mut p = Vec::new();
    match rec {
        WalRecord::ActivationCommitIntent { tuple } => {
            put_tuple(&mut p, tuple);
            (rec_type::ACTIVATION_COMMIT_INTENT, p)
        }
        WalRecord::ActivationComplete { tuple, completion_id } => {
            put_tuple(&mut p, tuple);
            p.extend_from_slice(&completion_id.to_le_bytes());
            (rec_type::ACTIVATION_COMPLETE, p)
        }
        WalRecord::ActivationAbort { epoch, recovery_id, attempt } => {
            p.extend_from_slice(&epoch.to_le_bytes());
            p.extend_from_slice(&recovery_id.to_le_bytes());
            p.extend_from_slice(&attempt.to_le_bytes());
            (rec_type::ACTIVATION_ABORT, p)
        }
        WalRecord::ActivationUnservable { completion_id } => {
            p.extend_from_slice(&completion_id.to_le_bytes());
            (rec_type::ACTIVATION_UNSERVABLE, p)
        }
        WalRecord::SessionTerminate => (rec_type::SESSION_TERMINATE, p),
    }
}

/// Decode a recovered `(record_type, payload)` back to a [`WalRecord`]. Errors on a payload that
/// is too short or an unexpected type — these should never occur on a codec-validated record, so
/// they surface as a sim failure rather than a panic.
fn decode_wal(record_type: u16, payload: &[u8]) -> Result<WalRecord, String> {
    let need = |n: usize| {
        if payload.len() < n {
            Err(format!("type {record_type}: payload {} < {n} bytes", payload.len()))
        } else {
            Ok(())
        }
    };
    match record_type {
        rec_type::ACTIVATION_COMMIT_INTENT => {
            need(21)?;
            Ok(WalRecord::ActivationCommitIntent { tuple: get_tuple(payload)? })
        }
        rec_type::ACTIVATION_COMPLETE => {
            need(29)?;
            let tuple = get_tuple(payload)?;
            let completion_id = u64::from_le_bytes(payload[21..29].try_into().unwrap());
            Ok(WalRecord::ActivationComplete { tuple, completion_id })
        }
        rec_type::ACTIVATION_ABORT => {
            need(12)?;
            Ok(WalRecord::ActivationAbort {
                epoch: u32::from_le_bytes(payload[0..4].try_into().unwrap()),
                recovery_id: u32::from_le_bytes(payload[4..8].try_into().unwrap()),
                attempt: u32::from_le_bytes(payload[8..12].try_into().unwrap()),
            })
        }
        rec_type::ACTIVATION_UNSERVABLE => {
            need(8)?;
            Ok(WalRecord::ActivationUnservable {
                completion_id: u64::from_le_bytes(payload[0..8].try_into().unwrap()),
            })
        }
        rec_type::SESSION_TERMINATE => Ok(WalRecord::SessionTerminate),
        other => Err(format!("unexpected record type {other} recovered")),
    }
}

fn put_tuple(p: &mut Vec<u8>, t: &ActivationTuple) {
    p.push(match t.kind {
        ActivationKind::Initial => 0,
        ActivationKind::Recovery => 1,
    });
    p.extend_from_slice(&t.epoch.to_le_bytes());
    p.extend_from_slice(&t.recovery_id.to_le_bytes());
    p.extend_from_slice(&t.attempt.to_le_bytes());
    p.extend_from_slice(&t.sampler_checkpoint_id.to_le_bytes());
}

fn get_tuple(b: &[u8]) -> Result<ActivationTuple, String> {
    let kind = match b[0] {
        0 => ActivationKind::Initial,
        1 => ActivationKind::Recovery,
        k => return Err(format!("bad activation kind byte {k}")),
    };
    Ok(ActivationTuple {
        kind,
        epoch: u32::from_le_bytes(b[1..5].try_into().unwrap()),
        recovery_id: u32::from_le_bytes(b[5..9].try_into().unwrap()),
        attempt: u32::from_le_bytes(b[9..13].try_into().unwrap()),
        sampler_checkpoint_id: u64::from_le_bytes(b[13..21].try_into().unwrap()),
    })
}

/// What a recovery scan disagreed on (surfaced as a sim `Failure`).
#[derive(Debug)]
pub enum RecoverErr {
    /// `WalScan` refused to open the durable prefix (bad header, or mid-stream corruption — the
    /// latter must never happen when only the tail is torn).
    Scan(String),
    /// A recovered record failed to decode.
    Decode(String),
}

/// The virtual WAL disk backing one coordinator instance.
pub struct VirtualWal {
    /// `fdatasync`'d bytes: the file header followed by durable records. Safe across a crash.
    durable: Vec<u8>,
    /// Bytes written but not yet `fdatasync`'d — the in-flight write exposed to a torn crash.
    pending: Vec<u8>,
}

impl VirtualWal {
    /// A fresh single-segment control-WAL scoped to `session`.
    pub fn new(session: [u8; 16]) -> Self {
        let hdr = FileHeader {
            flags: FLAG_CONTAINS_CONTROL_WAL,
            cluster_id: [7u8; 16],
            session_scope: session,
        }
        .encode();
        VirtualWal { durable: hdr.to_vec(), pending: Vec::new() }
    }

    /// A coordinator `WriteWal` effect: encode the record into the pending (not-yet-durable) tail.
    pub fn write(&mut self, rec: &WalRecord) {
        let (t, payload) = encode_wal(rec);
        // Our payloads are tiny; the only `encode_record` error is >64 MiB, unreachable here.
        let bytes = encode_record(t, 0, &payload).expect("record encodes");
        self.pending.extend_from_slice(&bytes);
    }

    /// `fdatasync`: the pending write is now durable (WAL-FORMAT §3.1).
    pub fn fsync(&mut self) {
        self.durable.append(&mut self.pending);
    }

    /// A crash **before** `fdatasync` returns. The in-flight write does not survive as a clean
    /// complete record — it is absent, truncated, or checksum-corrupt (see the module torn-write
    /// model). `rng` chooses which, so the marathon continuously exercises the real scanner's
    /// partial-tail-discard and bit-flip paths at arbitrary offsets.
    pub fn crash_tear(&mut self, rng: &mut Rng) {
        if self.pending.is_empty() {
            return;
        }
        let full = self.pending.len();
        match rng.below(3) {
            0 => { /* nothing reached the platter */ }
            1 => {
                // a strictly-incomplete prefix reached disk (0..full-1 bytes) → torn tail
                let survive = rng.below(full);
                self.durable.extend_from_slice(&self.pending[..survive]);
            }
            _ => {
                // full-length bytes reached disk but a bit flipped → BadChecksum tail
                let mut b = self.pending.clone();
                let i = rng.below(b.len());
                b[i] ^= 1u8 << rng.below(8);
                self.durable.extend_from_slice(&b);
            }
        }
        self.pending.clear();
    }

    /// Coordinator restart: scan the durable bytes with the real codec (partial-tail discard),
    /// truncate the disk to the recovered durable prefix, and return the recovered records. Any
    /// torn/lost pending write is discarded here — the result must equal the coordinator's own
    /// durable WAL.
    pub fn recover(&mut self) -> Result<Vec<WalRecord>, RecoverErr> {
        let scan = WalScan::from_bytes(&self.durable).map_err(|e| RecoverErr::Scan(e.to_string()))?;
        self.durable.truncate(scan.durable_len as usize);
        // A crash also drops any not-yet-durable in-flight write.
        self.pending.clear();
        scan.records
            .iter()
            .map(|r| decode_wal(r.record_type, &r.payload).map_err(RecoverErr::Decode))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(attempt: u32) -> WalRecord {
        WalRecord::ActivationCommitIntent {
            tuple: ActivationTuple {
                kind: ActivationKind::Initial,
                epoch: 0,
                recovery_id: 0,
                attempt,
                sampler_checkpoint_id: 1,
            },
        }
    }

    #[test]
    fn durable_records_round_trip_through_real_codec() {
        let mut w = VirtualWal::new([9u8; 16]);
        w.write(&intent(1));
        w.fsync();
        w.write(&WalRecord::ActivationComplete {
            tuple: ActivationTuple {
                kind: ActivationKind::Initial,
                epoch: 0,
                recovery_id: 0,
                attempt: 1,
                sampler_checkpoint_id: 1,
            },
            completion_id: 42,
        });
        w.fsync();
        let recovered = w.recover().expect("clean recovery");
        assert_eq!(recovered.len(), 2);
        assert_eq!(recovered[0], intent(1));
        assert!(matches!(recovered[1], WalRecord::ActivationComplete { completion_id: 42, .. }));
    }

    #[test]
    fn torn_pending_write_is_discarded_all_variants() {
        // Every torn variant must recover exactly the one durable record (never the torn one).
        for seed in 0..200u64 {
            let mut w = VirtualWal::new([1u8; 16]);
            w.write(&intent(1));
            w.fsync(); // durable
            w.write(&intent(2)); // in-flight, about to be torn by a crash
            let mut rng = Rng::new(seed);
            w.crash_tear(&mut rng);
            let recovered = w.recover().expect("torn tail is discardable");
            assert_eq!(recovered, vec![intent(1)], "seed {seed}: torn write must not survive");
        }
    }
}
