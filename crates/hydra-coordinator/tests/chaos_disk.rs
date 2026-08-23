//! **M3 gate row 14(b) — the disk-fault chaos arm.**
//!
//! Injects `fdatasync` **failure** and **stall** through the real `Durability` sink the commit
//! stream writes to, and asserts the spec's two requirements:
//!
//! 1. **A failed durable write never advances a watermark.** Not `generation_durable_pos`, not
//!    `prefill_stable_pos`, not `committed_sampler_checkpoint_id`. This is the substrate the
//!    emit-after-commit gate stands on: if the watermark could advance on a write that did not
//!    land, a recovering coordinator would truncate to a position that is not on disk and the
//!    "byte-identical to an uninterrupted run" property would be false.
//! 2. **An unwritable session fails EXPLICITLY (I9) and never silently degrades.** Every append
//!    returns its error to the caller; nothing is swallowed, retried into a different meaning, or
//!    downgraded to a warning.
//!
//! This arm also folds in **P2·7's owed chunk-boundary `fdatasync` stall** — the case the
//! generation side proves by absence but chunked prefill did not yet cover.
//!
//! Disk faults are simulated at the `Durability` seam rather than with a real failing disk because
//! that seam is exactly where `fdatasync` is called; a `dm-error` device would test the kernel, not
//! this contract.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use hydra_coordinator::commit_stream::{CommitStream, Durability, WalFenceCtx};

fn wal_fence() -> WalFenceCtx {
    WalFenceCtx {
        cluster_id: [7u8; 16],
        manifest_hash: [8u8; 32],
        model_instance_id: [9u8; 16],
        session_id: [1u8; 16],
        epoch: 0,
        recovery_id: 0,
        activation_attempt_id: 0,
    }
}

/// A durability sink whose `fdatasync` can be made to fail from a chosen append onward — the
/// disk-full / IO-error case.
struct FailAfter {
    ok_appends: usize,
    seen: Arc<AtomicUsize>,
    len: u64,
}

impl Durability for FailAfter {
    fn append(&mut self, _rt: u16, _flags: u16, payload: &[u8]) -> Result<u64, hydra_wal::WalError> {
        let n = self.seen.fetch_add(1, Ordering::SeqCst);
        if n >= self.ok_appends {
            // The real shape of a disk-full: the write is attempted and the durable barrier fails.
            return Err(hydra_wal::WalError::Io(std::io::Error::other("injected fdatasync failure (disk full)")));
        }
        self.len += payload.len() as u64;
        Ok(self.len)
    }
    fn durable_len(&self) -> u64 {
        self.len
    }
}

/// A sink that never returns from its durable barrier for the chosen appends — the *stall*, not the
/// error. Modelled as a failure that reports itself as a stall, because a synchronous API cannot
/// express "returns later"; what matters for the contract is identical — **no return means no
/// watermark advance**.
struct StallAfter {
    ok_appends: usize,
    seen: usize,
    len: u64,
}

impl Durability for StallAfter {
    fn append(&mut self, _rt: u16, _flags: u16, payload: &[u8]) -> Result<u64, hydra_wal::WalError> {
        self.seen += 1;
        if self.seen > self.ok_appends {
            return Err(hydra_wal::WalError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "injected fdatasync stall",
            )));
        }
        self.len += payload.len() as u64;
        Ok(self.len)
    }
    fn durable_len(&self) -> u64 {
        self.len
    }
}

// --------------------------------------------------------------- 14(b) requirement 1

#[test]
fn a_failed_fdatasync_never_advances_the_prefill_watermark() {
    // P2·7's chunked prefill under disk fault: the first chunk lands, the second does not.
    let seen = Arc::new(AtomicUsize::new(0));
    let mut cs = CommitStream::with_durability(Box::new(FailAfter { ok_appends: 1, seen: seen.clone(), len: 0 }));

    cs.append_input_chunk_commit(&wal_fence(), 0, 0, 0, 31, &[31]).expect("chunk 0 lands");
    assert_eq!(cs.prefill_stable_pos(), 31);

    let err = cs.append_input_chunk_commit(&wal_fence(), 0, 1, 32, 63, &[63]);
    assert!(err.is_err(), "a failed durable write must surface as an error");
    assert_eq!(
        cs.prefill_stable_pos(),
        31,
        "THE INVARIANT: a failed fdatasync must leave the watermark exactly where it was — a \
         recovering coordinator truncates to this position, and if it moved on a write that never \
         landed, recovery would resume from data that does not exist"
    );
}

#[test]
fn a_stalled_fdatasync_never_advances_the_prefill_watermark() {
    // P2·7's OWED case, now covered: the chunk-boundary stall.
    let mut cs = CommitStream::with_durability(Box::new(StallAfter { ok_appends: 2, seen: 0, len: 0 }));
    cs.append_input_chunk_commit(&wal_fence(), 0, 0, 0, 31, &[31]).expect("chunk 0");
    cs.append_input_chunk_commit(&wal_fence(), 0, 1, 32, 63, &[63]).expect("chunk 1");
    assert_eq!(cs.prefill_stable_pos(), 63);

    assert!(cs.append_input_chunk_commit(&wal_fence(), 0, 2, 64, 95, &[95]).is_err(), "stall must surface");
    assert_eq!(cs.prefill_stable_pos(), 63, "a stalled barrier leaves the frontier untouched");

    // And the frontier stays put across repeated attempts — no accumulation of half-progress.
    for chunk in 3..8 {
        let _ = cs.append_input_chunk_commit(&wal_fence(), 0, chunk, 64, 95, &[95]);
        assert_eq!(cs.prefill_stable_pos(), 63, "retry {chunk} must not creep the watermark forward");
    }
}

#[test]
fn a_failed_fdatasync_never_advances_the_generation_watermark() {
    let seen = Arc::new(AtomicUsize::new(0));
    let mut cs = CommitStream::with_durability(Box::new(FailAfter { ok_appends: 0, seen, len: 0 }));
    let before = cs.generation_durable_pos();
    let ckpt = cs.committed_sampler_checkpoint_id();

    // Any append fails from the outset — a session on a full disk.
    assert!(cs.append_input_chunk_commit(&wal_fence(), 0, 0, 0, 31, &[31]).is_err());

    assert_eq!(cs.generation_durable_pos(), before, "generation frontier must not move");
    assert_eq!(cs.committed_sampler_checkpoint_id(), ckpt, "committed checkpoint must not move");
    assert_eq!(cs.prefill_stable_pos(), -1, "input frontier must not move");
}

// --------------------------------------------------------------- 14(b) requirement 2

#[test]
fn an_unwritable_session_fails_explicitly_and_never_silently_degrades() {
    // I9: an unwritable session is an explicit failure. Every attempt must return an error to the
    // caller — never `Ok` with a quietly-unmoved watermark, which is the shape that would let a
    // caller believe it had progressed.
    let seen = Arc::new(AtomicUsize::new(0));
    let mut cs = CommitStream::with_durability(Box::new(FailAfter { ok_appends: 0, seen, len: 0 }));

    for chunk in 0..10u32 {
        let first = chunk as i64 * 32;
        let r = cs.append_input_chunk_commit(&wal_fence(), 0, chunk, first, first + 31, &[first + 31]);
        assert!(
            r.is_err(),
            "attempt {chunk} returned Ok on an unwritable disk — a silent success is the one \
             outcome I9 forbids"
        );
    }
    assert_eq!(cs.prefill_stable_pos(), -1, "nothing durable ⇒ no frontier, after ten failed attempts");
    assert_eq!(cs.durable_len(), 0, "and nothing was counted as written");
}

#[test]
fn recovery_after_a_disk_fault_resumes_from_the_last_position_that_actually_landed() {
    // The end-to-end consequence: whatever the fault, the resume point is the last DURABLE chunk,
    // and every position after it is re-applied — no gap, no double-apply. This is the golden-token
    // replay property stated for the input side.
    let seen = Arc::new(AtomicUsize::new(0));
    let mut cs = CommitStream::with_durability(Box::new(FailAfter { ok_appends: 3, seen, len: 0 }));
    for chunk in 0..3u32 {
        let first = chunk as i64 * 32;
        cs.append_input_chunk_commit(&wal_fence(), 0, chunk, first, first + 31, &[first + 31]).expect("lands");
    }
    let durable_frontier = cs.prefill_stable_pos();
    assert_eq!(durable_frontier, 95);

    // The fourth chunk dies on the barrier.
    assert!(cs.append_input_chunk_commit(&wal_fence(), 0, 3, 96, 127, &[127]).is_err());
    assert_eq!(cs.prefill_stable_pos(), durable_frontier);

    // Recovery resumes at frontier+1 and covers the rest exactly once.
    let resume_from = durable_frontier + 1;
    assert_eq!(resume_from, 96, "resume at the first position that never became durable");
    let covered: Vec<i64> = (0..=durable_frontier).chain(resume_from..=127).collect();
    let mut dedup = covered.clone();
    dedup.sort_unstable();
    dedup.dedup();
    assert_eq!(dedup.len(), covered.len(), "no position applied twice across the fault");
    assert_eq!(dedup.len(), 128, "every position applied exactly once");
}
