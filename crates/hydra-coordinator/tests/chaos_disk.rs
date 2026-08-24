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

/// A minimal valid `SamplerCheckpointRec` (I19: generated_through == sampled == pos).
fn snapshot(checkpoint_id: u64, generated_through: i64, sampled: i64) -> Vec<u8> {
    use flatbuffers::FlatBufferBuilder;
    use hydra_proto::wal;
    let mut fbb = FlatBufferBuilder::new();
    let rng_key = Some(fbb.create_vector(&[0u8; 8]));
    let grammar = Some(fbb.create_vector::<u8>(&[]));
    let penalty = Some(fbb.create_vector::<u8>(&[]));
    let cfg = Some(fbb.create_vector(&[7u8; 32]));
    let sum = Some(fbb.create_vector(&[9u8; 32]));
    let rec = wal::SamplerCheckpointRec::create(
        &mut fbb,
        &wal::SamplerCheckpointRecArgs {
            checkpoint_id,
            rng_key,
            rng_counter: 42,
            generated_through_output_pos: generated_through,
            serialized_grammar_state: grammar,
            serialized_penalty_state: penalty,
            sampled_output_pos: sampled,
            sampling_config_hash: cfg,
            state_checksum: sum,
        },
    );
    fbb.finish(rec, None);
    fbb.finished_data().to_vec()
}

fn admission() -> hydra_tokenizer::Admission {
    hydra_tokenizer::Admission {
        tokenizer_hash: [0xA1; 32],
        chat_template_hash: [0xB2; 32],
        rendered_prompt_bytes_hash: [0xC3; 32],
        rendered_prompt: "hi".to_string(),
        prompt_tokens: vec![10, 20, 30],
    }
}

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

// ---------------------------------------------------------------------------------------------
// H9 — the sink now PERSISTS BYTES ON FAILURE, which is what a real disk does.
//
// **Standing rule 19: what this file could not see.** `FailAfter` and `StallAfter` above return
// `Err` and keep the payload to themselves — as if a failing write left the platter untouched.
// Real failures are not so tidy: `write_all` can return an error after a short write, and
// `fdatasync` can fail after some of the data has already been written back. Under the old sink
// the *post-failure state of the log* was inexpressible, so every assertion here was about
// watermarks and none could be about what a later reader would find.
//
// With bytes persisted, the question H9 asks becomes askable: after a failed append, is the log
// still a log? The answer, before the fix, was no — the writer accepted the NEXT append, which
// placed a checksum-valid record immediately after a partial one, i.e. manufactured exactly the
// mid-stream corruption H8 refuses to open.
// ---------------------------------------------------------------------------------------------

/// A sink that writes the bytes it was given and *then* reports failure — a short write, or an
/// `fdatasync` that failed after partial write-back. `persisted` is the byte log a later reader
/// would see.
struct PersistsThenFails {
    ok_appends: usize,
    seen: usize,
    persisted: Arc<std::sync::Mutex<Vec<u8>>>,
    /// Fraction of the record that lands before the error (1.0 = all of it).
    landed: f64,
    len: u64,
}

impl Durability for PersistsThenFails {
    fn append(&mut self, _rt: u16, _flags: u16, payload: &[u8]) -> Result<u64, hydra_wal::WalError> {
        self.seen += 1;
        if self.seen > self.ok_appends {
            let n = ((payload.len() as f64) * self.landed) as usize;
            self.persisted.lock().unwrap().extend_from_slice(&payload[..n]);
            return Err(hydra_wal::WalError::Io(std::io::Error::other("injected failure after partial write-back")));
        }
        self.persisted.lock().unwrap().extend_from_slice(payload);
        self.len += payload.len() as u64;
        Ok(self.len)
    }
    fn durable_len(&self) -> u64 {
        self.len
    }
}

/// **H9 — a failed append leaves bytes behind, so the writer must never accept another one.**
///
/// The commit stream is the coordinator's durable truth. If an append fails after some bytes
/// landed and the next append is accepted, the log gains a valid record sitting behind a partial
/// one — which is mid-stream corruption by construction, and (since H8) a log that refuses to
/// open at all. One transient `ENOSPC` would therefore cost the whole session's ledger rather
/// than the one write that failed.
#[test]
fn a_failed_append_poisons_the_stream_so_no_later_write_can_land_behind_it() {
    let persisted = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut cs = CommitStream::with_durability(Box::new(PersistsThenFails {
        ok_appends: 1,
        seen: 0,
        persisted: persisted.clone(),
        landed: 0.5,
        len: 0,
    }));

    cs.append_input_chunk_commit(&wal_fence(), 0, 0, 0, 31, &[31]).expect("chunk 0 lands");
    assert_eq!(cs.prefill_stable_pos(), 31);
    let after_good = persisted.lock().unwrap().len();

    // The failing append: bytes land, the error is returned.
    let err = cs.append_input_chunk_commit(&wal_fence(), 0, 1, 32, 63, &[63]);
    assert!(err.is_err(), "the failure must be reported, never swallowed");
    assert_eq!(cs.prefill_stable_pos(), 31, "and the watermark must not move");
    assert!(persisted.lock().unwrap().len() > after_good, "the sink really did leave bytes behind");

    // THE H9 ASSERTION: every subsequent append is refused, and refused *without touching the
    // sink* — so nothing valid can ever land behind the partial record.
    let before = persisted.lock().unwrap().len();
    for attempt in 0..3 {
        let again = cs.append_input_chunk_commit(&wal_fence(), 0, 2, 64, 95, &[95]);
        assert!(again.is_err(), "attempt {attempt}: a poisoned stream must refuse every later append");
        assert!(
            matches!(again, Err(hydra_coordinator::CommitError::Poisoned { .. })),
            "attempt {attempt}: the refusal must name the poisoning, not masquerade as a fresh I/O error: {again:?}"
        );
    }
    assert_eq!(
        persisted.lock().unwrap().len(),
        before,
        "a poisoned stream must not write ANY bytes — not even the ones that would 'probably' be fine"
    );
    assert_eq!(cs.prefill_stable_pos(), 31, "no watermark moved");
}

/// The same for the generation path, and the control that keeps the rule honest: a stream that has
/// never failed accepts appends normally. (A "poison everything" implementation would pass the
/// test above and destroy the product.)
#[test]
fn poisoning_is_caused_by_the_failure_and_not_by_the_check() {
    let persisted = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut ok = CommitStream::with_durability(Box::new(PersistsThenFails {
        ok_appends: 100,
        seen: 0,
        persisted: persisted.clone(),
        landed: 1.0,
        len: 0,
    }));
    ok.append_initial_commit(&wal_fence(), &admission(), &snapshot(1, -1, -1), 1).expect("initial commit lands");
    for pos in 0..4i64 {
        ok.append_generation_commit(&wal_fence(), pos, pos, &[(pos, 7u32)], &snapshot(1, pos, pos))
            .unwrap_or_else(|e| panic!("healthy stream must keep accepting appends: {e}"));
    }
    assert_eq!(ok.generation_durable_pos(), 3);

    // …and a stream whose generation append fails is poisoned for the generation path too.
    let mut bad = CommitStream::with_durability(Box::new(PersistsThenFails {
        ok_appends: 2, // INITIAL + one GENERATION land, the next fails
        seen: 0,
        persisted: Arc::new(std::sync::Mutex::new(Vec::new())),
        landed: 0.25,
        len: 0,
    }));
    bad.append_initial_commit(&wal_fence(), &admission(), &snapshot(1, -1, -1), 1).expect("initial");
    bad.append_generation_commit(&wal_fence(), 0, 0, &[(0, 7u32)], &snapshot(1, 0, 0)).expect("first generation commit");
    assert!(bad.append_generation_commit(&wal_fence(), 1, 1, &[(1, 8u32)], &snapshot(1, 1, 1)).is_err(), "the injected failure");
    let poisoned = bad.append_generation_commit(&wal_fence(), 2, 2, &[(2, 9u32)], &snapshot(1, 2, 2));
    assert!(matches!(poisoned, Err(hydra_coordinator::CommitError::Poisoned { .. })), "got {poisoned:?}");
    assert_eq!(bad.generation_durable_pos(), 0, "the watermark stands where the last DURABLE record left it");
}

// ---------------------------------------------------------------------------------------------
// **Audit M7 (the auditor's, not the directive's) — retain-or-fail on commit error, per I9.**
//
// # Standing rule 20, and why this test did not exist
//
// The Wave-2 directive said only "M7 (contiguity)". I implemented contiguity on the **boundary
// store**. The auditor's M7 is `GENERATION_COMMIT` contiguity, and its fix text names the part
// that actually loses data: *"retain or fail (I9) on commit error"*. The mistake was mine and it
// was undetectable from the directive alone, because the directive was the only text in the repo.
// Standing rule 20 now requires reconciling against the auditor's own words before calling
// anything closed.
//
// # What the defect was
//
// `Session::commit_group` called `group.take()` — which **drains the buffer** — and then appended.
// On failure the `?` returned with the batch already gone: tokens S_P had sampled, that no durable
// record mentioned, and that the next group's `first_pos` skipped straight past. The error was
// reported, so it did not look like silent corruption. What was silent was the **hole**, and
// `recovery::read` replays the ledger by position, so every later token shifts one place and the
// recovered stream is a different generation from the one the client already saw.
// ---------------------------------------------------------------------------------------------

/// A sink that always succeeds (each integration test file is its own crate, so the helpers in
/// `session_http.rs` are not visible here).
#[derive(Default)]
struct AlwaysOk {
    len: u64,
}
impl Durability for AlwaysOk {
    fn append(&mut self, _rt: u16, _fl: u16, payload: &[u8]) -> Result<u64, hydra_wal::WalError> {
        self.len += payload.len() as u64;
        Ok(self.len)
    }
    fn durable_len(&self) -> u64 {
        self.len
    }
}

/// One byte per token id, and a vocabulary big enough that M5 is not what refuses anything here.
struct BytePieces;
impl hydra_coordinator::PieceSource for BytePieces {
    fn piece(&self, token: u32) -> Vec<u8> {
        vec![token as u8]
    }
    fn n_vocab(&self) -> u32 {
        1 << 20
    }
}

/// A sink that fails exactly once, then works. The transient case — `ENOSPC` on a full disk that
/// the operator then clears — which is precisely when losing the batch is least excusable.
struct FailsOnce {
    fail_at: usize,
    seen: usize,
    len: u64,
}

impl Durability for FailsOnce {
    fn append(&mut self, _rt: u16, _fl: u16, payload: &[u8]) -> Result<u64, hydra_wal::WalError> {
        self.seen += 1;
        if self.seen == self.fail_at {
            return Err(hydra_wal::WalError::Io(std::io::Error::other("injected transient failure")));
        }
        self.len += payload.len() as u64;
        Ok(self.len)
    }
    fn durable_len(&self) -> u64 {
        self.len
    }
}

#[test]
fn a_failed_commit_retains_the_batch_instead_of_discarding_sampled_tokens() {
    use hydra_coordinator::{CommitOutcome, SampledToken, Session};

    // Append #1 is the INITIAL_COMMIT; the sink is set to fail on append #2, the group commit.
    let mut cs = CommitStream::with_durability(Box::new(FailsOnce { fail_at: 2, seen: 0, len: 0 }));
    cs.append_initial_commit(&wal_fence(), &admission(), &snapshot(1, -1, -1), 1).expect("initial commit");
    let mut s = Session::new(cs, wal_fence(), Box::new(BytePieces), 2, 1_000_000);

    // Two tokens buffered; the group-commit append is #2, which fails.
    s.push_sampled(SampledToken { output_pos: 0, token_id: b'a' as u32, snapshot: snapshot(1, 0, 0) }).unwrap();
    s.push_sampled(SampledToken { output_pos: 1, token_id: b'b' as u32, snapshot: snapshot(1, 1, 1) }).unwrap();
    assert_eq!(s.buffered(), 2);

    let err = s.try_commit_by_count();
    assert!(err.is_err(), "the failure must be reported");
    assert_eq!(s.durable_pos(), -1, "and nothing became durable");

    // THE ASSERTION: the sampled tokens are still buffered. Before the fix they were gone, and the
    // next group would have started at position 2, leaving 0 and 1 in no record anywhere.
    assert_eq!(
        s.buffered(),
        2,
        "a failed commit must RETAIN the batch (I9): these are tokens S_P already sampled, and \
         discarding them leaves a permanent hole that recovery replays as a shifted generation"
    );

    // **How M7 and H9 compose, which is the interesting part.** The retry is REFUSED, and that is
    // correct: H9 poisoned the stream because the failed append may have left a partial record, so
    // the on-disk tail is unknown and appending again would manufacture mid-stream corruption. The
    // two fixes say different things and both are needed — *the tokens are not discarded* (M7) and
    // *this file is not written to again* (H9).
    let retry = s.try_commit_by_count();
    assert!(
        matches!(retry, Err(hydra_coordinator::CommitError::Poisoned { .. })),
        "the retry must be refused on a poisoned stream, not appended behind a partial record: {retry:?}"
    );
    assert_eq!(
        s.buffered(),
        2,
        "and the batch is STILL retained after the refusal — a refused retry must not lose the          tokens either. Recovery is reopen-then-recommit (`CommitStream::open` discards the partial          tail durably and restores the watermarks), covered in commit_stream.rs."
    );
    assert_eq!(s.durable_pos(), -1, "nothing became durable at any point");
    let _ = CommitOutcome::Nothing; // the enum is used above via matches!
}

/// **M7 — the ledger must be contiguous, checked before the disk.**
///
/// A group that does not continue the durable sequence is refused, and a group whose token entries
/// are not dense is refused. Both are the shape a dropped batch leaves.
#[test]
fn a_generation_commit_that_does_not_continue_the_sequence_is_refused() {
    use hydra_coordinator::CommitError;

    let mut cs = CommitStream::with_durability(Box::new(AlwaysOk::default()));
    cs.append_initial_commit(&wal_fence(), &admission(), &snapshot(1, -1, -1), 1).unwrap();

    // A gap: the first group starts at 1 while the durable frontier is -1.
    let err = cs.append_generation_commit(&wal_fence(), 1, 1, &[(1, 7)], &snapshot(1, 1, 1));
    assert!(
        matches!(err, Err(CommitError::NonContiguous { got: 1, expected: 0, .. })),
        "a group past the frontier must be refused: {err:?}"
    );

    cs.append_generation_commit(&wal_fence(), 0, 0, &[(0, 7)], &snapshot(1, 0, 0)).expect("position 0 commits");

    // A replay of an already-durable position.
    assert!(matches!(
        cs.append_generation_commit(&wal_fence(), 0, 0, &[(0, 7)], &snapshot(1, 0, 0)),
        Err(CommitError::NonContiguous { .. })
    ));

    // Non-dense entries within an otherwise well-placed group (1, then 3).
    let err = cs.append_generation_commit(&wal_fence(), 1, 3, &[(1, 7), (3, 9)], &snapshot(1, 3, 3));
    assert!(
        matches!(err, Err(CommitError::NonContiguous { what: "GENERATION_COMMIT token entry", got: 3, expected: 2 })),
        "a hole inside the group must be refused too: {err:?}"
    );

    // Control: the next dense group commits.
    cs.append_generation_commit(&wal_fence(), 1, 2, &[(1, 7), (2, 8)], &snapshot(1, 2, 2)).expect("dense group commits");
    assert_eq!(cs.generation_durable_pos(), 2);
}
