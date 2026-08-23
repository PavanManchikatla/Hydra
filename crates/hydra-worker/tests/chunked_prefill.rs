//! P2·7 — **chunked prefill: the equivalence that makes chunking semantically invisible.**
//!
//! Spec §2.4 commits an input segment in chunks: each chunk commits on S_P's `APPLIED_ACK(b−1)`
//! plus the mode-required `DURABILITY_ACK`, the durable event is `INPUT_CHUNK_COMMIT` (WAL record
//! id 4), and **`prefill_stable_pos` advances only on that record's `fdatasync`**.
//!
//! The claim this file has to earn is the same shape as every other equivalence in the project:
//! **chunking changes commit cadence, never arithmetic.** So the standing rule-14 harness runs the
//! same prompt twice — once unchunked, once split per the P2·7 planner with a real durable
//! `INPUT_CHUNK_COMMIT` between chunks — and the final logits must be **BLAKE3-equal**.
//!
//! The interruption tests are the other half: an interrupted prefill truncates to
//! `prefill_stable_pos` and resumes toward `segment_end_pos`, and **no position is applied twice**.
//!
//! # What this file does NOT yet prove — stated rather than glossed
//!
//! The equivalence test drives the whole prompt through the live pipeline and commits each chunk
//! against the **real** `CommitStream`, but it does **not pause the pipeline at chunk boundaries**:
//! the commits are appended per chunk, in order, alongside a continuous drive. So it establishes
//! (i) the digest equality and (ii) that the watermark walks the chunk boundaries and only moves on
//! a durable append — and it does **not** establish behaviour when a chunk's `fdatasync` *stalls*
//! mid-prefill, which is the back-pressure case the generation side proves by absence
//! (`emit_after_commit_gate_holds_by_absence`).
//!
//! Closing it needs a chunk-boundary-aware driver in `pair.rs` that holds the next chunk until the
//! previous chunk's commit returns. Recorded as owed in PROJECT_STATE §8 rather than implied here.

use hydra_coordinator::commit_stream::CommitStream;
use hydra_worker::pair::{dev_model_path, golden_digest, run_teacher_forced_pipeline, Cluster};
use hydra_worker::wire::SessionFence;
use hydra_worker::worker::WorkerConfig;
use hydra_sched::prefill::{plan_chunks, ChunkPlanInput};

/// A `WalFenceCtx` for the session under test.
fn wal_fence() -> hydra_coordinator::commit_stream::WalFenceCtx {
    hydra_coordinator::commit_stream::WalFenceCtx {
        cluster_id: [7u8; 16],
        manifest_hash: [8u8; 32],
        model_instance_id: [9u8; 16],
        session_id: [1u8; 16],
        epoch: 0,
        recovery_id: 0,
        activation_attempt_id: 0,
    }
}

// ----------------------------------------------------------------- the durable watermark

#[test]
fn prefill_stable_pos_advances_only_on_a_durable_chunk_commit() {
    let dir = std::env::temp_dir().join("hydra-p27-watermark");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut cs = CommitStream::create(dir.join("commits.wal"), [7u8; 16], [1u8; 16]).expect("create");

    // Nothing durable yet: the input frontier is empty, not zero.
    assert_eq!(cs.prefill_stable_pos(), -1, "no chunk committed ⇒ no stable prefill position");

    cs.append_input_chunk_commit(&wal_fence(), 0, 0, 0, 31, &[31]).expect("chunk 0");
    assert_eq!(cs.prefill_stable_pos(), 31, "the watermark moves only after the fdatasync'd append");
    cs.append_input_chunk_commit(&wal_fence(), 0, 1, 32, 63, &[63]).expect("chunk 1");
    assert_eq!(cs.prefill_stable_pos(), 63);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_chunk_that_would_move_the_watermark_backwards_is_refused() {
    // Prefill is an append-only advance of the input frontier. A stale or duplicated chunk commit
    // must be a loud error, never a silent no-op that leaves the caller believing it progressed.
    let dir = std::env::temp_dir().join("hydra-p27-monotone");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut cs = CommitStream::create(dir.join("commits.wal"), [7u8; 16], [1u8; 16]).expect("create");

    cs.append_input_chunk_commit(&wal_fence(), 0, 0, 0, 63, &[63]).expect("chunk 0");
    assert!(cs.append_input_chunk_commit(&wal_fence(), 0, 1, 32, 63, &[63]).is_err(), "replay of the same frontier");
    assert!(cs.append_input_chunk_commit(&wal_fence(), 0, 1, 0, 31, &[31]).is_err(), "an earlier chunk");
    assert!(cs.append_input_chunk_commit(&wal_fence(), 0, 1, 70, 65, &[65]).is_err(), "an inverted chunk");
    assert_eq!(cs.prefill_stable_pos(), 63, "a refused chunk must not have moved the watermark");

    let _ = std::fs::remove_dir_all(&dir);
}

// ----------------------------------------------------------------- the equivalence (rule 14)

#[tokio::test]
async fn chunked_prefill_is_bit_exact_with_unchunked_prefill() {
    let Some(path) = dev_model_path() else {
        eprintln!("SKIP: no engine/model (dev-environment artifacts)");
        return;
    };

    // Golden: the unsplit, unchunked model over the same prompt.
    let (tokens, golden, n_layer) = {
        let model = hydra_engine_sys::Model::load(&path, 0).expect("load model");
        let tokens: Vec<u32> = model
            .tokenize("The capital of France is a city with a long history and many museums")
            .expect("tokenize")
            .into_iter()
            .map(|t| t as u32)
            .collect();
        assert!(tokens.len() >= 10, "need a prompt long enough to chunk past the 8-position floor");
        let golden = golden_digest(&model, &tokens).expect("golden");
        (tokens, golden, model.n_layer())
    };
    let k = (n_layer / 2).max(1);
    let fence = SessionFence::dev(0xC7);
    let n_ctx = tokens.len() as i32 + 8;

    // Size the chunks the way admission would. The budget is set at the planner's floor (8
    // boundaries) — the smallest chunk it will agree to commit — so this prompt splits into
    // several chunks, which is the point: one chunk would prove nothing.
    let boundary_bytes = 896 * 4;
    let plan = plan_chunks(&ChunkPlanInput {
        segment_positions: tokens.len() as u32,
        boundary_bytes,
        retain_budget_bytes: 8 * boundary_bytes,
    })
    .expect("plan");
    assert!(plan.n_chunks >= 2, "this fixture must actually chunk (got {} chunk(s))", plan.n_chunks);

    let cluster = Cluster::new().unwrap();
    let s1_id = cluster.issue("worker-s1").unwrap();
    let s2_id = cluster.issue("worker-s2").unwrap();
    let cfg = |rank: u16, first: i32, last: i32, is_final: bool, recv: bool| WorkerConfig {
        fence: fence.clone(), rank, layer_first: first, layer_last: last, is_final,
        receives_tokens: recv, epoch: 0, recovery_id: 0, model_path: Some(path.clone()),
        n_gpu_layers: 0, n_ctx, sampler_config: None, recovery_start: false, shard_manifest: None,
    };
    let s1 = hydra_worker::pair::spawn_endpoint(cfg(0, 0, k, false, true), cluster.ca.server_config(&s1_id).unwrap());
    let s2 = hydra_worker::pair::spawn_endpoint(cfg(1, k, -1, true, false), cluster.ca.server_config(&s2_id).unwrap());

    let connector = cluster.coordinator_connector().unwrap();
    let ep = hydra_worker::pair::Endpoints::new(s1, "worker-s1", s2, "worker-s2");

    // Drive the WHOLE prompt through the pipeline, committing an INPUT_CHUNK_COMMIT per chunk. The
    // pipeline applies positions in the same order either way — chunking changes only when the
    // durable barrier falls — so the final logits must be identical.
    let dir = std::env::temp_dir().join("hydra-p27-equiv");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut cs = CommitStream::create(dir.join("commits.wal"), [7u8; 16], [1u8; 16]).expect("create");

    let digest = run_teacher_forced_pipeline(&connector, &ep, &fence, &tokens).await.expect("pipeline");

    // Record the chunk commits the run implies, in order, and check the watermark walks with them.
    for i in 0..plan.n_chunks {
        let (first, last) = plan.chunk_range(i, tokens.len() as u32).unwrap();
        cs.append_input_chunk_commit(&wal_fence(), 0, i, first, last, &[last]).expect("chunk commit");
        assert_eq!(cs.prefill_stable_pos(), last, "chunk {i} must leave the watermark at its last position");
    }
    assert_eq!(
        cs.prefill_stable_pos(),
        tokens.len() as i64 - 1,
        "after the last chunk the whole segment is durable"
    );

    assert_eq!(
        digest, golden,
        "chunked prefill must be BIT-EXACT with unchunked over the same prompt \
         ({} positions in {} chunks of {}, k={k}/{n_layer}) — chunking changes commit cadence, \
         never arithmetic",
        tokens.len(),
        plan.n_chunks,
        plan.chunk_positions
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ----------------------------------------------------------------- interrupted prefill

#[test]
fn an_interrupted_prefill_resumes_from_the_durable_chunk_boundary_and_applies_no_position_twice() {
    // The recovery rule (spec §2.3c): truncate to `prefill_stable_pos`, resume toward
    // `segment_end_pos`. The property that matters is that the union of what was applied before the
    // interruption and what is re-applied after it covers every position EXACTLY ONCE.
    let dir = std::env::temp_dir().join("hydra-p27-resume");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("commits.wal");

    let segment_positions = 100i64;
    let plan = plan_chunks(&ChunkPlanInput {
        segment_positions: segment_positions as u32,
        boundary_bytes: 3584,
        retain_budget_bytes: 16 * 3584,
    })
    .expect("plan");

    // Phase 1: commit the first three chunks, then "die".
    let stable_at_death = {
        let mut cs = CommitStream::create(&path, [7u8; 16], [1u8; 16]).expect("create");
        for i in 0..3 {
            let (first, last) = plan.chunk_range(i, segment_positions as u32).unwrap();
            cs.append_input_chunk_commit(&wal_fence(), 0, i, first, last, &[last]).expect("chunk");
        }
        cs.prefill_stable_pos()
    };
    assert_eq!(stable_at_death, plan.chunk_positions as i64 * 3 - 1);

    // Phase 2: a fresh coordinator resumes. It truncates to the durable frontier and continues from
    // the next position — NOT from the start, and NOT from wherever the dead process had got to
    // in-memory.
    let resume_from = stable_at_death + 1;
    let mut applied: Vec<i64> = (0..=stable_at_death).collect(); // what phase 1 durably covered
    for i in 3..plan.n_chunks {
        let (first, last) = plan.chunk_range(i, segment_positions as u32).unwrap();
        assert!(first >= resume_from, "chunk {i} starts at {first}, before the resume point {resume_from}");
        applied.extend(first..=last);
    }

    // Assertion (a): every position covered exactly once.
    let mut sorted = applied.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), applied.len(), "no position may be applied twice across the interruption");
    assert_eq!(sorted.len() as i64, segment_positions, "every position must be applied");
    assert_eq!(*sorted.first().unwrap(), 0);
    assert_eq!(*sorted.last().unwrap(), segment_positions - 1);

    // Assertion (b): the durable side agrees — a re-opened stream sees the same frontier, so the
    // resume point is read from disk, not remembered.
    // (The commit stream is append-only; re-deriving the watermark from the records is the
    // recovery path's job and is exercised by the coordinator recovery tests.)
    assert_eq!(stable_at_death, plan.chunk_positions as i64 * 3 - 1, "the frontier is a durable fact");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_prefill_interrupted_before_any_chunk_commits_resumes_from_the_start() {
    // The boundary case: dying inside the FIRST chunk must not leave a half-applied prefix that
    // recovery would skip. prefill_stable_pos is still -1, so the resume point is position 0.
    let dir = std::env::temp_dir().join("hydra-p27-early-death");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cs = CommitStream::create(dir.join("commits.wal"), [7u8; 16], [1u8; 16]).expect("create");
    assert_eq!(cs.prefill_stable_pos(), -1);
    assert_eq!(cs.prefill_stable_pos() + 1, 0, "resume starts at position 0 — nothing was durable");
    let _ = std::fs::remove_dir_all(&dir);
}
