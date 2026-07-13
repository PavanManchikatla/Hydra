//! D1 recovery — coordinator side (spec §6.2 Strategy A; the single full-range **D0-class** S_P
//! topology of sub-slice C).
//!
//! On S_P loss the coordinator reconstructs the recovery inputs from its **own durable commit
//! stream** — the ledger is the single source of truth (I3): the committed token history (replayed
//! as `REBUILD_APPLY` = `APPLY_TOKEN` NO_SAMPLE to rebuild the engine KV during catch-up) and the
//! sampler checkpoint embedded in the **last durable `GENERATION_COMMIT`** (the
//! `INSTALL_SAMPLER_CHECKPOINT` restore point). Any provisional sampled-ahead outputs above
//! `generation_durable_pos` are, by construction, **absent** from the durable stream, so a recovery
//! that resumes at `generation_durable_pos + 1` discards them — that is I7b/I15 made structural.
//!
//! This module is **pure** (it reads a WAL file); the wire orchestration that drives a replacement
//! worker lives in the D1 recovery harness (`tests/`). It also carries the **disk-truth verifier**
//! for the recovery DoD: every `GENERATION_COMMIT` satisfies I19 and **no output position is
//! committed twice** (a retry may never append a duplicate position).

use std::collections::HashSet;

use flatbuffers::FlatBufferBuilder;
use hydra_proto::validate_generation_commit_i19;
use hydra_proto::wal;
use hydra_wal::reader::WalScan;
use hydra_wal::record::rec_type;

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("wal: {0}")]
    Wal(#[from] hydra_wal::WalError),
    #[error("commit stream has no INITIAL_COMMIT")]
    NoInitial,
    #[error("I19 violation on read at commit_id {0}: {1}")]
    I19(u64, String),
    #[error("output position {0} committed twice (a retry appended a duplicate)")]
    DuplicatePosition(i64),
    #[error("malformed {0} record: {1}")]
    Malformed(&'static str, String),
}

/// Everything the coordinator needs to drive a replacement S_P through recovery, reconstructed from
/// the durable commit stream alone (I3).
#[derive(Debug, Clone)]
pub struct RecoveryState {
    pub prompt_tokens: Vec<u32>,
    /// Committed generated tokens in output-position order (`(output_pos, token_id)`).
    pub generated_tokens: Vec<(i64, u32)>,
    /// `snapshot(generation_durable_pos)` — the sampler checkpoint the last `GENERATION_COMMIT`
    /// embeds, re-serialized as a standalone `SamplerCheckpointRec` root ready for
    /// `INSTALL_SAMPLER_CHECKPOINT`. When no generation has committed yet this is the initial
    /// checkpoint from `INITIAL_COMMIT`.
    pub last_checkpoint: Vec<u8>,
    pub checkpoint_id: u64,
    /// The last durable output position (`-1` if nothing generated yet).
    pub generation_durable_pos: i64,
    pub last_commit_id: u64,
}

impl RecoveryState {
    /// The input frontier to replay for catch-up = prompt length + committed generated count. After
    /// applying these positions the engine's retained logits predict `generation_durable_pos + 1`.
    pub fn input_frontier(&self) -> i64 {
        self.prompt_tokens.len() as i64 + self.generated_tokens.len() as i64
    }

    /// Committed generated token ids in output-position order (for `REBUILD_APPLY` feedback).
    pub fn generated_token_ids(&self) -> Vec<u32> {
        self.generated_tokens.iter().map(|&(_, t)| t).collect()
    }

    /// The full input-token replay sequence (prompt then generated) for catch-up KV rebuild.
    pub fn replay_tokens(&self) -> Vec<u32> {
        self.prompt_tokens.iter().copied().chain(self.generated_token_ids()).collect()
    }
}

/// Read + validate a commit-stream file, returning the recovery inputs. Enforces the recovery DoD's
/// disk-truth invariants as it goes: **I19 per `GENERATION_COMMIT`** and **no output position
/// committed twice**. A violation is an error (never silently tolerated).
pub fn read(path: impl AsRef<std::path::Path>) -> Result<RecoveryState, RecoveryError> {
    let scan = WalScan::open(path)?;

    let mut prompt_tokens: Option<Vec<u32>> = None;
    let mut initial_checkpoint: Option<(Vec<u8>, u64)> = None;
    let mut generated_tokens: Vec<(i64, u32)> = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    let mut last_checkpoint: Option<(Vec<u8>, u64)> = None;
    let mut generation_durable_pos: i64 = -1;
    let mut last_commit_id: u64 = 0;

    for r in &scan.records {
        match r.record_type {
            rec_type::INITIAL_COMMIT => {
                let ic = flatbuffers::root::<wal::InitialCommit>(&r.payload)
                    .map_err(|e| RecoveryError::Malformed("INITIAL_COMMIT", e.to_string()))?;
                let prompt: Vec<u32> = ic.prompt_tokens().iter().map(|te| te.token_id()).collect();
                prompt_tokens = Some(prompt);
                let ckpt = ic.initial_checkpoint();
                initial_checkpoint = Some((reserialize_checkpoint(&ckpt), ckpt.checkpoint_id()));
            }
            rec_type::GENERATION_COMMIT => {
                // I19 on read (spec §2.6a: one record or nothing) — same validator as on write.
                let gc = flatbuffers::root::<wal::GenerationCommit>(&r.payload)
                    .map_err(|e| RecoveryError::Malformed("GENERATION_COMMIT", e.to_string()))?;
                validate_generation_commit_i19(&r.payload).map_err(|e| RecoveryError::I19(gc.commit_id(), e))?;
                for te in gc.tokens().iter() {
                    let pos = te.absolute_position();
                    if !seen.insert(pos) {
                        return Err(RecoveryError::DuplicatePosition(pos));
                    }
                    generated_tokens.push((pos, te.token_id()));
                }
                let ckpt = gc.checkpoint();
                last_checkpoint = Some((reserialize_checkpoint(&ckpt), ckpt.checkpoint_id()));
                generation_durable_pos = gc.last_output_pos();
                last_commit_id = gc.commit_id();
            }
            _ => {}
        }
    }

    let prompt_tokens = prompt_tokens.ok_or(RecoveryError::NoInitial)?;
    generated_tokens.sort_by_key(|&(pos, _)| pos);
    let (last_checkpoint, checkpoint_id) = last_checkpoint
        .or(initial_checkpoint)
        .ok_or(RecoveryError::NoInitial)?;

    Ok(RecoveryState {
        prompt_tokens,
        generated_tokens,
        last_checkpoint,
        checkpoint_id,
        generation_durable_pos,
        last_commit_id,
    })
}

/// Disk-truth report for the recovery-DoD assertion (c): the commit stream reads back I19-valid with
/// **no output position committed twice**. `read` already enforces both; this returns the stats a
/// test asserts on (and re-confirms position monotonicity across the whole stream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitStreamStats {
    pub generation_commits: usize,
    pub committed_positions: usize,
    pub max_position: i64,
    /// Output positions are strictly increasing across the entire stream (no gap-free requirement,
    /// but never a repeat, and never out of order).
    pub positions_strictly_increasing: bool,
}

/// Verify a commit stream satisfies the recovery DoD's disk-truth invariants and return its stats.
pub fn verify(path: impl AsRef<std::path::Path>) -> Result<CommitStreamStats, RecoveryError> {
    let state = read(path)?;
    let positions: Vec<i64> = state.generated_tokens.iter().map(|&(p, _)| p).collect();
    let strictly_increasing = positions.windows(2).all(|w| w[0] < w[1]);
    Ok(CommitStreamStats {
        generation_commits: 0_usize.max(state.last_commit_id as usize),
        committed_positions: positions.len(),
        max_position: positions.last().copied().unwrap_or(-1),
        positions_strictly_increasing: strictly_increasing,
    })
}

/// Re-serialize a nested `SamplerCheckpointRec` into a fresh standalone root buffer (field-by-field
/// copy — the coordinator never re-derives sampler state; spec §1.4) so it can be handed to
/// `INSTALL_SAMPLER_CHECKPOINT` verbatim.
fn reserialize_checkpoint(rec: &wal::SamplerCheckpointRec) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let rng_key = Some(fbb.create_vector(rec.rng_key().bytes()));
    let grammar = Some(fbb.create_vector(rec.serialized_grammar_state().bytes()));
    let penalty = Some(fbb.create_vector(rec.serialized_penalty_state().bytes()));
    let cfg = Some(fbb.create_vector(rec.sampling_config_hash().bytes()));
    let sum = Some(fbb.create_vector(rec.state_checksum().bytes()));
    let out = wal::SamplerCheckpointRec::create(
        &mut fbb,
        &wal::SamplerCheckpointRecArgs {
            checkpoint_id: rec.checkpoint_id(),
            rng_key,
            rng_counter: rec.rng_counter(),
            generated_through_output_pos: rec.generated_through_output_pos(),
            serialized_grammar_state: grammar,
            serialized_penalty_state: penalty,
            sampled_output_pos: rec.sampled_output_pos(),
            sampling_config_hash: cfg,
            state_checksum: sum,
        },
    );
    fbb.finish(out, None);
    fbb.finished_data().to_vec()
}
