//! M2 slice 5 sub-slice C — **D1 recovery, coordinator-side disk + client truth (CI-safe half).**
//!
//! The engine-gated end-to-end (real kill -9 of a subprocess S_P through the real recovery
//! machinery, byte-identical vs an uninterrupted seeded run) lives in
//! `hydra-worker/tests/d1_recovery.rs`. Here we prove the two assertions that do **not** need the
//! engine, at the coordinator layer, so they run everywhere:
//!
//!   (c) **disk truth** — the commit stream reads back **I19-valid with no output position
//!       committed twice**, and the recovery inputs (prompt, committed tokens, the restore
//!       checkpoint, `generation_durable_pos`) reconstruct from the durable ledger alone (I3);
//!   (a) **client truth** — the SSE event log is a **pure function of the durable ledger prefix**,
//!       so a reconstruction after a kill yields dense, gap-free, non-repeating ids and a
//!       byte-identical stream, and `Last-Event-ID` resume at the kill boundary is byte-identical.

use std::sync::atomic::{AtomicU32, Ordering};

use flatbuffers::FlatBufferBuilder;
use hydra_coordinator::recovery::{self, RecoveryError};
use hydra_coordinator::{CommitStream, PieceSource, SampledToken, Session, WalFenceCtx};
use hydra_proto::wal;
use hydra_tokenizer::Admission;

static SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_path(tag: &str) -> std::path::PathBuf {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("hydra-d1-{}-{tag}-{seq}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join("commit-stream.wal")
}

fn fence() -> WalFenceCtx {
    WalFenceCtx {
        cluster_id: [1; 16],
        session_id: [2; 16],
        model_instance_id: [3; 16],
        manifest_hash: [4; 32],
        epoch: 0,
        recovery_id: 0,
        activation_attempt_id: 0,
    }
}

/// A minimal valid `SamplerCheckpointRec` for output position `pos` (I19: generated==sampled==pos).
/// `rng_counter` is set to `pos` so the reconstructed checkpoint is position-distinguishable.
fn snapshot(checkpoint_id: u64, pos: i64) -> Vec<u8> {
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
            rng_counter: pos.max(0) as u64,
            generated_through_output_pos: pos,
            serialized_grammar_state: grammar,
            serialized_penalty_state: penalty,
            sampled_output_pos: pos,
            sampling_config_hash: cfg,
            state_checksum: sum,
        },
    );
    fbb.finish(rec, None);
    fbb.finished_data().to_vec()
}

fn admission(prompt: &[u32]) -> Admission {
    Admission {
        tokenizer_hash: [0xA1; 32],
        chat_template_hash: [0xB2; 32],
        rendered_prompt_bytes_hash: [0xC3; 32],
        rendered_prompt: "<|im_start|>user\nhi<|im_end|>\n<|im_start|>assistant\n".to_string(),
        prompt_tokens: prompt.to_vec(),
    }
}

// --------------------------- (c) disk truth ---------------------------

#[test]
fn recovery_state_reconstructs_from_the_durable_ledger_and_is_i19_no_double() {
    let path = temp_path("disk");
    let prompt = [10u32, 20, 30];
    let mut cs = CommitStream::create(&path, [1; 16], [2; 16]).unwrap();
    cs.append_initial_commit(&fence(), &admission(&prompt), &snapshot(1, -1), 1).unwrap();
    // Three group commits: positions [0..=2], [3..=5], [6..=8]; tokens 100+pos.
    cs.append_generation_commit(&fence(), 0, 2, &[(0, 100), (1, 101), (2, 102)], &snapshot(1, 2)).unwrap();
    cs.append_generation_commit(&fence(), 3, 5, &[(3, 103), (4, 104), (5, 105)], &snapshot(1, 5)).unwrap();
    cs.append_generation_commit(&fence(), 6, 8, &[(6, 106), (7, 107), (8, 108)], &snapshot(1, 8)).unwrap();
    drop(cs);

    // Reconstruct recovery inputs from the durable ledger alone (I3).
    let state = recovery::read(&path).expect("read recovery state");
    assert_eq!(state.prompt_tokens, prompt);
    assert_eq!(state.generation_durable_pos, 8);
    assert_eq!(state.last_commit_id, 3);
    assert_eq!(state.generated_token_ids(), (100..=108).collect::<Vec<u32>>());
    // input frontier = prompt(3) + generated(9) = 12; resume samples output position 9.
    assert_eq!(state.input_frontier(), 12);
    // The restore checkpoint is snapshot(8): rng_counter carries position 8 (the last durable state).
    let rec = flatbuffers::root::<wal::SamplerCheckpointRec>(&state.last_checkpoint).unwrap();
    assert_eq!(rec.rng_counter(), 8, "the restore checkpoint is snapshot(generation_durable_pos)");

    // (c) disk-truth verifier: I19-valid, positions strictly increasing (never a repeat).
    let stats = recovery::verify(&path).expect("verify");
    assert_eq!(stats.committed_positions, 9);
    assert_eq!(stats.max_position, 8);
    assert!(stats.positions_strictly_increasing);
}

#[test]
fn a_position_committed_twice_is_rejected_on_read() {
    // A retry that re-appends an already-committed position is the failure `read` must catch — the
    // durable-truth half of "no position twice". CommitStream's per-record I19 does not forbid it
    // across records, so this is a real guard, not a tautology.
    let path = temp_path("double");
    let mut cs = CommitStream::create(&path, [1; 16], [2; 16]).unwrap();
    cs.append_initial_commit(&fence(), &admission(&[10, 20]), &snapshot(1, -1), 1).unwrap();
    cs.append_generation_commit(&fence(), 0, 2, &[(0, 100), (1, 101), (2, 102)], &snapshot(1, 2)).unwrap();
    // A "retry" whose group overlaps position 2 (duplicate) — each record is individually I19-valid.
    cs.append_generation_commit(&fence(), 2, 4, &[(2, 102), (3, 103), (4, 104)], &snapshot(1, 4)).unwrap();
    drop(cs);

    match recovery::read(&path) {
        Err(RecoveryError::DuplicatePosition(2)) => {}
        other => panic!("expected DuplicatePosition(2), got {other:?}"),
    }
}

// --------------------------- (a) client truth ---------------------------

/// A deterministic stub `PieceSource`: token `t` renders as `"<t>"` (always complete UTF-8, so the
/// detok never buffers — the multibyte-straddle path is covered separately in `session_http`).
struct StubPieces;
impl PieceSource for StubPieces {
    fn piece(&self, token: u32) -> Vec<u8> {
        format!("<{token}>").into_bytes()
    }
}

/// Drive a session over a fresh commit-stream file: commit `tokens` (as output positions
/// `0..tokens.len()`) in k-sized groups and return `(events, full_text)`.
fn run_session(path: &std::path::Path, prompt: &[u32], tokens: &[u32], k: usize) -> (Vec<(u64, String)>, String) {
    let mut cs = CommitStream::create(path, [1; 16], [2; 16]).unwrap();
    cs.append_initial_commit(&fence(), &admission(prompt), &snapshot(1, -1), 1).unwrap();
    let mut s = Session::new(cs, fence(), Box::new(StubPieces), k, 1_000_000);
    let mut events = Vec::new();
    for (pos, &tok) in tokens.iter().enumerate() {
        s.push_sampled(SampledToken { output_pos: pos as i64, token_id: tok, snapshot: snapshot(1, pos as i64) });
        if let hydra_coordinator::CommitOutcome::Committed(evs) = s.try_commit_by_count().unwrap() {
            events.extend(evs.into_iter().map(|e| (e.id, e.data)));
        }
    }
    for e in s.finish().unwrap() {
        events.push((e.id, e.data));
    }
    let full_text = events.iter().map(|(_, d)| d.as_str()).collect::<String>();
    (events, full_text)
}

#[test]
fn sse_ids_are_continuous_and_bytes_identical_across_a_reconstruction() {
    let tokens: Vec<u32> = (100..112).collect(); // 12 generated tokens
    let prompt = [10u32, 20, 30];
    let k = 4;

    // Uninterrupted reference.
    let (events_ref, text_ref) = run_session(&temp_path("ref"), &prompt, &tokens, k);
    let ids_ref: Vec<u64> = events_ref.iter().map(|(id, _)| *id).collect();
    // Dense 1..=N, no gap/repeat.
    assert_eq!(ids_ref, (1..=ids_ref.len() as u64).collect::<Vec<_>>());

    // Interrupted: durable through the first 6 positions, then the S_P is lost.
    let kill_path = temp_path("kill");
    {
        let mut cs = CommitStream::create(&kill_path, [1; 16], [2; 16]).unwrap();
        cs.append_initial_commit(&fence(), &admission(&prompt), &snapshot(1, -1), 1).unwrap();
        let mut s = Session::new(cs, fence(), Box::new(StubPieces), k, 1_000_000);
        for pos in 0..6i64 {
            s.push_sampled(SampledToken { output_pos: pos, token_id: tokens[pos as usize], snapshot: snapshot(1, pos) });
            let _ = s.try_commit_by_count().unwrap();
        }
        s.finish().unwrap();
        // drop `s` == the coordinator session is gone; only the durable file survives.
    }

    // RECOVER: reconstruct from the durable ledger, replay it into a fresh session (the event log is
    // a pure function of the ledger), then continue with the SAME remaining tokens (seeded recovery
    // re-produces them identically — proven with the real sampler in the engine-gated harness).
    let state = recovery::read(&kill_path).expect("read");
    assert_eq!(state.generation_durable_pos, 5, "durable through output position 5");
    assert_eq!(state.generated_token_ids(), tokens[..6].to_vec());

    let recovered_path = temp_path("recovered");
    let mut cs = CommitStream::create(&recovered_path, [1; 16], [2; 16]).unwrap();
    cs.append_initial_commit(&fence(), &admission(&state.prompt_tokens), &snapshot(1, -1), 1).unwrap();
    let mut s = Session::new(cs, fence(), Box::new(StubPieces), k, 1_000_000);
    let mut events_rec = Vec::new();
    // Replay the durable prefix (reconstruct the pre-kill events identically).
    for (pos, tok) in state.generated_tokens.iter().copied() {
        s.push_sampled(SampledToken { output_pos: pos, token_id: tok, snapshot: snapshot(1, pos) });
        if let hydra_coordinator::CommitOutcome::Committed(evs) = s.try_commit_by_count().unwrap() {
            events_rec.extend(evs.into_iter().map(|e| (e.id, e.data)));
        }
    }
    // Continue past the kill with the remaining tokens.
    for pos in 6..tokens.len() as i64 {
        s.push_sampled(SampledToken { output_pos: pos, token_id: tokens[pos as usize], snapshot: snapshot(1, pos) });
        if let hydra_coordinator::CommitOutcome::Committed(evs) = s.try_commit_by_count().unwrap() {
            events_rec.extend(evs.into_iter().map(|e| (e.id, e.data)));
        }
    }
    for e in s.finish().unwrap() {
        events_rec.push((e.id, e.data));
    }

    // (a) SSE id continuity: dense, gap-free, non-repeating — and byte-identical to the uninterrupted run.
    let ids_rec: Vec<u64> = events_rec.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids_rec, (1..=ids_rec.len() as u64).collect::<Vec<_>>(), "ids dense with no gap/repeat");
    assert_eq!(ids_rec, ids_ref, "recovered id sequence matches uninterrupted");
    let text_rec = events_rec.iter().map(|(_, d)| d.as_str()).collect::<String>();
    assert_eq!(text_rec, text_ref, "post-recovery stream is byte-identical to the uninterrupted run");

    // Last-Event-ID resume at the kill boundary (id 3 = through output pos 5 at k=4? compute) is a
    // byte-identical suffix: prefix(cut) + suffix(cut) == full, for EVERY cut.
    for cut in 0..=ids_rec.len() {
        let prefix: String = events_rec[..cut].iter().map(|(_, d)| d.as_str()).collect();
        let suffix: String = events_rec[cut..].iter().map(|(_, d)| d.as_str()).collect();
        assert_eq!(format!("{prefix}{suffix}"), text_ref, "resume at event {cut} is byte-identical");
    }
}
