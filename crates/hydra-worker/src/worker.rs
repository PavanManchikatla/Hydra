//! The stage worker: a **thin effect executor** (BLUEPRINT M2 sub-slice A).
//!
//! `recv()` → [`wire::decode`] (F1 fence + envelope hard-limits already gated pre-alloc by the
//! transport) → route:
//!   * **control-plane** bodies map 1:1 to [`StageEvent`] and are stepped through the **real**
//!     `hydra-state` [`Stage`] SM — the DST-tested machine, not a parallel "simple" copy; its
//!     [`StageEffect`]s are encoded straight back to the wire;
//!   * **data-plane** bodies (`APPLY_TOKEN`, `FWD`) are executed by the `hydra-engine-sys` engine
//!     (windowed layer-range apply, boundary extract/inject, unsampled logits) and forwarded or
//!     acked.
//!
//! No protocol state lives in the engine and no compute lives in the SM: the worker is the seam
//! (BLUEPRINT §1.4 / §2 architecture rule). `on_frame` is **pure of I/O** — it takes bytes and
//! returns reply bytes — so it is unit-testable without a socket; the async [`serve_conn`] loop is
//! the only place bytes touch a connection.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use std::net::SocketAddr;

use hydra_engine_sys::{Context, EngineError, Model, ENGINE_AVAILABLE};
use hydra_state::{ActivationKind, ActivationTuple, Epoch, RecoveryId, Stage, StageEffect, StageEvent, StageRank};
use hydra_transport::framed::Conn;
use hydra_transport::tcp_mtls::{ClientConn, TcpMtls, TcpMtlsListener};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::durable::DurableForwarder;
use crate::sampler::{Sampler, SamplerError, SamplingConfig};
use crate::wire::{self, Msg, SessionFence, WireError};
use hydra_transport::roles::PeerRole;

/// `ERR_FENCED` on the wire (`proto::ErrCode::ERR_FENCED`).
const ERR_FENCED: u16 = 1;
/// `ERR_RECOVERY_COMPLETED` (Case B′).
const ERR_RECOVERY_COMPLETED: u16 = 3;
/// `ERR_CHECKPOINT_MISMATCH` — sampler drift (spec §2.6b: fatal, never silently repaired).
const ERR_CHECKPOINT_MISMATCH: u16 = 9;
/// The config-defined initial checkpoint id the coordinator seeds S_P with (spec §1.4 boundary).
pub const INITIAL_CHECKPOINT_ID: u64 = 1;

/// One cached `SAMPLED` — the snapshot ring entry that makes a duplicate `SAMPLE_NEXT` idempotent
/// (I14) without advancing the RNG, and carries `post_sample_state_snapshot(q)` (spec §2.6a).
#[derive(Clone)]
struct SampledEntry {
    token_id: u32,
    snapshot: Vec<u8>,
    state_digest: [u8; 32],
}

/// Everything a worker needs to load a **verified** shard — the manifest path **and the cluster's
/// trusted signing key**, together.
///
/// **Audit C1, structurally.** The trust anchor is not a second optional field beside the path; it
/// is in the same struct, so `shard_manifest: Some(..)` cannot be written without answering *whose
/// signature counts*. The previous shape — `Option<String>` naming only a path — made the anchor
/// something a caller could simply not think about, and the code then fell back to the key embedded
/// in the manifest itself.
///
/// The **H14** half of the binding needs no field here: the expected `manifest_hash` is already in
/// `WorkerConfig::fence` (spec §1.1's fence tuple), which is exactly the value the cluster agreed on
/// for this session. Taking it from anywhere else would be inventing a second source of truth.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ShardManifestConfig {
    /// Path to the signed manifest emitted by `hydra-modelsvc split`.
    pub path: String,
    /// The cluster's Ed25519 manifest-signing public key, provisioned at pairing.
    pub trusted_signer: [u8; 32],
}

/// Static description of one worker's role in the pipeline.


#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub fence: SessionFence,
    pub rank: StageRank,
    /// First hosted layer (`l0`).
    pub layer_first: i32,
    /// Last hosted layer exclusive (`l1`); `-1` == to the model's last layer.
    pub layer_last: i32,
    /// This stage hosts the final range → produces **logits** (a logits context, not embeddings).
    /// A non-final stage emits a **boundary** residual for the next stage.
    pub is_final: bool,
    /// This stage ingests raw tokens (`rank 0`) rather than an upstream boundary.
    pub receives_tokens: bool,
    pub epoch: Epoch,
    pub recovery_id: RecoveryId,
    /// Small GGUF path (dev-box memory discipline; a dev-mode artifact, `hydra-engine-sys` docs).
    /// `None` (or an absent file / unlinked engine) → control-plane-only worker (no compute).
    pub model_path: Option<String>,
    /// `0` = CPU (the deterministic DoD backend); `99` = GPU.
    pub n_gpu_layers: i32,
    pub n_ctx: i32,
    /// The session sampling config — set on the **final** stage (S_P) to enable the sampler
    /// (spec §2.6b). `None` on non-final stages and for the teacher-forced-only anchor.
    pub sampler_config: Option<SamplingConfig>,
    /// A **recovery-replacement** worker starts its stage `FROZEN` (not `FROZEN_READY`) so it can
    /// accept `BEGIN_RECOVERY` **Case A** through the real stage SM (spec §6.2/§6.5). Default `false`
    /// (a fresh session's worker is `FROZEN_READY`).
    pub recovery_start: bool,
    /// P2·10b — path to the **Ed25519-signed shard manifest** (`hydra-modelsvc split` output).
    /// When set, `model_path` is treated as a **per-stage shard** and is loaded only after the
    /// manifest verifies (signature → this shard's entry → the shard's BLAKE3 → the entry's layer
    /// range vs this worker's configured range). Any failure **refuses the shard** — it never
    /// degrades to a full load or a control-plane-only worker, because a silent downgrade is
    /// exactly the attack the signature exists to stop. `None` ⇒ the pre-P2·10b full-model load.
    /// Shard-loading configuration. **`Some` implies a trust anchor**: see [`ShardManifestConfig`].
    pub shard_manifest: Option<ShardManifestConfig>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error(transparent)]
    Wire(#[from] WireError),
    /// **Audit C2.** The peer is authentic and bound to a role, but that role may not send this
    /// message family. Distinct from a fence mismatch: F1 asks *"is this frame for this session?"*,
    /// this asks *"may THIS peer send THIS?"* — and before the gate existed, nothing asked it.
    #[error("REFUSED: a {role} may not send {body:?} (audit C2 role gate)")]
    Unauthorized { role: String, body: String },
    #[error("engine: {0}")]
    Engine(#[from] EngineError),
    /// **Audit C3 / H7 / M5 / 1c.** The frame decoded cleanly, but a *value* in it cannot cross the
    /// FFI on this stage: a declared `n_embd` that is not this engine's, a position count above
    /// `n_batch`, a position outside `[0, n_ctx)` or that does not fit `i32`, a token id outside
    /// `[0, n_vocab)`. Refused **before** the engine is touched — and distinct from
    /// [`WorkerError::Engine`], which is what the engine says *after* being called. A bound that
    /// only the engine enforces is a bound the engine must survive being asked about; the worker
    /// holds the numbers (`n_embd`, `n_batch`, `n_ctx`, `n_vocab` are the engine's own facts) and
    /// so the worker asks first.
    #[error("REFUSED before the FFI: {what} = {value} (bound {bound})")]
    PreFfi { what: &'static str, value: i64, bound: i64 },
    #[error("data-plane frame but no engine linked/loaded on this worker")]
    EngineUnavailable,
    #[error(transparent)]
    ShardRefused(#[from] crate::shard::ShardRefused),
    #[error(transparent)]
    Transport(#[from] hydra_transport::TransportError),
}

/// The engine half of a worker. The [`Model`] is leaked to `'static` (one per worker process, freed
/// at process exit) so the borrowing [`Context`] can be stored beside it without a self-referential
/// struct; the worker owns the engine on a single thread, so the non-`Send` C handle never travels.
struct Engine {
    ctx: Context<'static>,
    n_embd: usize,
    /// True iff this stage extracts a boundary (i.e. it is not the final logits stage).
    emit_boundary: bool,
}

/// `i32::try_from` on a network-derived position, bounded to `[0, n_ctx)` (audit 1c). Every
/// `as i32` on a wire `i64` was a silent truncation: `2^32 + 5` became `5`, and a negative became a
/// position the engine would index with. The value is checked here, once, and the FFI only ever
/// sees an `i32` that was a position.
fn wire_pos(what: &'static str, pos: i64, n_ctx: i32) -> Result<i32, WorkerError> {
    match i32::try_from(pos) {
        Ok(p) if p >= 0 && p < n_ctx => Ok(p),
        _ => Err(WorkerError::PreFfi { what, value: pos, bound: n_ctx as i64 }),
    }
}

impl Engine {
    /// Build the engine for `cfg`, or `None` if the engine isn't linked or the model file is absent
    /// (both dev-environment artifacts — a control-plane-only worker still runs everywhere).
    fn try_new(cfg: &WorkerConfig) -> Result<Option<Engine>, WorkerError> {
        if !ENGINE_AVAILABLE {
            return Ok(None);
        }
        let Some(path) = cfg.model_path.as_deref().filter(|p| std::path::Path::new(p).exists()) else {
            return Ok(None);
        };
        // P2·10b: a configured manifest makes this a SHARD worker. Verification happens before any
        // weights are read, and a failure REFUSES — it never falls back to a full load or to a
        // control-plane-only worker. The missing-model case above is a dev-environment artifact;
        // this case is a trust failure, and the two must not share an outcome.
        let model = match cfg.shard_manifest.as_ref() {
            Some(sm) => {
                // The trust anchor is `sm.trusted_signer` (C1) and the identity binding is the
                // session's own fence tuple (H14) — not a value invented here.
                let verified = crate::shard::verify_shard(
                    &sm.path,
                    path,
                    cfg.layer_first,
                    cfg.layer_last,
                    &crate::shard::TrustedSigner(sm.trusted_signer),
                    &cfg.fence.manifest_hash,
                )?;
                crate::shard::load_verified_shard(&verified, cfg.n_gpu_layers)?
            }
            None => Model::load(path, cfg.n_gpu_layers)?,
        };
        let n_embd = model.n_embd() as usize;
        let model: &'static Model = Box::leak(Box::new(model));
        // A boundary-emitting stage is an embeddings context; the final stage is a logits context.
        let embeddings = !cfg.is_final;
        let ctx = model.context(cfg.layer_first, cfg.layer_last, embeddings, cfg.n_ctx, cfg.n_ctx)?;
        Ok(Some(Engine { ctx, n_embd, emit_boundary: embeddings }))
    }

    /// Apply one token at `pos`. Returns the extracted boundary (emit stages) or `None` (final).
    ///
    /// **M5 (worker side):** `token_id < n_vocab` is checked here, before the FFI. The
    /// coordinator's `Session::push_sampled` holds the same bound before a token becomes
    /// *durable*; this is the bound before it becomes *compute*.
    fn apply_token(&mut self, token_id: u32, input_pos: i64) -> Result<Option<Vec<f32>>, WorkerError> {
        let n_vocab = self.ctx.n_vocab();
        let token = match i32::try_from(token_id) {
            Ok(t) if t < n_vocab => t,
            _ => return Err(WorkerError::PreFfi { what: "APPLY_TOKEN token_id vs n_vocab", value: token_id as i64, bound: n_vocab as i64 }),
        };
        let pos = wire_pos("APPLY_TOKEN input_pos", input_pos, self.ctx.n_ctx())?;
        if self.emit_boundary {
            let mut b = vec![0f32; self.n_embd];
            self.ctx.apply_tokens(&[token], pos, Some(&mut b))?;
            Ok(Some(b))
        } else {
            self.ctx.apply_tokens(&[token], pos, None)?;
            Ok(None)
        }
    }

    /// Inject one boundary position at `pos`. Returns the re-extracted boundary (middle stages) or
    /// `None` (final stage).
    ///
    /// **Audit C3 (the external half) + H7.** `wire::decode` proved the frame is self-consistent
    /// (`activations.len() == n_positions × n_embd`, `n_positions == 1`). This is where the
    /// *declared* `n_embd` meets the **engine's** `n_embd`, and where `n_positions` meets
    /// `n_batch` — both before `hydra_apply` is called. A frame can be perfectly internally
    /// consistent and still describe a model this stage does not hold.
    fn apply_boundary(&mut self, boundary: &[f32], n_positions: u16, n_embd: u32, first_input_pos: i64) -> Result<Option<Vec<f32>>, WorkerError> {
        if n_embd as i64 != self.ctx.n_embd() as i64 {
            return Err(WorkerError::PreFfi { what: "FWD n_embd vs engine n_embd", value: n_embd as i64, bound: self.ctx.n_embd() as i64 });
        }
        if n_positions as i64 > self.ctx.n_batch() as i64 {
            return Err(WorkerError::PreFfi { what: "FWD n_positions vs n_batch (audit H7)", value: n_positions as i64, bound: self.ctx.n_batch() as i64 });
        }
        // Belt and braces: decode guarantees this, but the FFI must never rely on a guarantee made
        // in another module. Cheap, and it keeps `apply_boundary`'s contract local.
        if boundary.len() != n_positions as usize * n_embd as usize {
            return Err(WorkerError::PreFfi { what: "FWD activations.len vs n_positions × n_embd", value: boundary.len() as i64, bound: (n_positions as usize * n_embd as usize) as i64 });
        }
        let pos = wire_pos("FWD first_input_pos", first_input_pos, self.ctx.n_ctx())?;
        if self.emit_boundary {
            let mut b = vec![0f32; self.n_embd];
            self.ctx.apply_boundary(boundary, n_positions as i32, pos, Some(&mut b))?;
            Ok(Some(b))
        } else {
            self.ctx.apply_boundary(boundary, n_positions as i32, pos, None)?;
            Ok(None)
        }
    }

    /// The retained (unsampled, I14) f32 logits for the position just applied. A worker applies
    /// exactly one position per frame, so the logits live at batch-relative output index 0
    /// (`hydra_logits` indexes the most recent apply's enabled outputs, not the absolute position).
    /// Sampling is the caller's job (I14) — the engine never samples.
    fn last_logits(&mut self) -> Result<Vec<f32>, EngineError> {
        self.ctx.logits(0)
    }
}

fn logits_digest(logits: &[f32]) -> [u8; 32] {
    *blake3::hash(&wire::f32_to_bytes_le(logits)).as_bytes()
}

/// A running stage worker: the real `hydra-state` [`Stage`] SM + an optional engine, plus (on S_P)
/// the sampler, the retained logits, and the SAMPLED snapshot ring.
pub struct Worker {
    cfg: WorkerConfig,
    stage: Stage,
    engine: Option<Engine>,
    /// S_P sampler (spec §2.6b); `None` on non-final stages or when no `sampler_config` is set.
    sampler: Option<Sampler>,
    /// The most recent position's retained logits (I14: sample only from retained logits).
    latest_logits: Option<Vec<f32>>,
    /// Snapshot ring / `SAMPLED` cache keyed by output position — makes a duplicate `SAMPLE_NEXT`
    /// idempotent (I14) and holds `snapshot(q)` for each sampled q (spec §2.6a).
    sampled_ring: HashMap<i64, SampledEntry>,
}

impl Worker {
    pub fn new(cfg: WorkerConfig) -> Result<Worker, WorkerError> {
        // Recovery replacement → FROZEN (accepts BEGIN_RECOVERY Case A); fresh → FROZEN_READY.
        let stage = if cfg.recovery_start {
            Stage::frozen(cfg.rank, cfg.epoch, cfg.recovery_id, 0)
        } else {
            Stage::frozen_ready(cfg.rank, cfg.epoch, cfg.recovery_id)
        };
        let engine = Engine::try_new(&cfg)?;
        // The sampler lives only on S_P (the final stage) and only when a config is provided.
        let sampler = if cfg.is_final {
            cfg.sampler_config.clone().map(|c| Sampler::initial(INITIAL_CHECKPOINT_ID, c))
        } else {
            None
        };
        Ok(Worker { cfg, stage, engine, sampler, latest_logits: None, sampled_ring: HashMap::new() })
    }

    pub fn has_engine(&self) -> bool {
        self.engine.is_some()
    }

    pub fn has_sampler(&self) -> bool {
        self.sampler.is_some()
    }

    pub fn stage(&self) -> &Stage {
        &self.stage
    }

    /// Decode one inbound frame, act on it, and return zero or more reply frames (each already a
    /// complete `Frame` payload ready for `Conn::send`). Pure of I/O.
    pub fn on_frame(&mut self, payload: &[u8]) -> Result<Vec<Vec<u8>>, WorkerError> {
        let (view, msg) = wire::decode(payload, &self.cfg.fence)?;
        match msg {
            Msg::ApplyToken { input_pos, token_id, no_sample } => {
                if !self.cfg.receives_tokens {
                    // A non-ingress stage never receives raw tokens (F1/precondition) — drop.
                    return Ok(vec![]);
                }
                let eng = self.engine.as_mut().ok_or(WorkerError::EngineUnavailable)?;
                match eng.apply_token(token_id, input_pos)? {
                    Some(boundary) => Ok(vec![wire::encode_fwd(&self.cfg.fence, view.epoch, input_pos, no_sample, &boundary)]),
                    None => self.retain_and_ack(view.epoch, input_pos, no_sample),
                }
            }
            Msg::Fwd { first_input_pos, no_sample, n_positions, n_embd, activations } => {
                let eng = self.engine.as_mut().ok_or(WorkerError::EngineUnavailable)?;
                match eng.apply_boundary(&activations, n_positions, n_embd, first_input_pos)? {
                    Some(boundary) => Ok(vec![wire::encode_fwd(&self.cfg.fence, view.epoch, first_input_pos, no_sample, &boundary)]),
                    None => self.retain_and_ack(view.epoch, first_input_pos, no_sample),
                }
            }
            Msg::SampleNext { output_pos, sampling_config_hash, expected_sampler_checkpoint_id } => {
                self.on_sample_next(view.epoch, output_pos, &sampling_config_hash, expected_sampler_checkpoint_id)
            }
            Msg::InstallSamplerCheckpoint { checkpoint_id, snapshot } => {
                self.on_install_sampler_checkpoint(view.epoch, checkpoint_id, &snapshot)
            }
            Msg::CommitActivation(t) => Ok(self.step_control(StageEvent::RecvCommit { tuple: t })),
            Msg::FinalizeActivation { attempt } => Ok(self.step_control(StageEvent::RecvFinalize { attempt })),
            Msg::ActivationCommitAbort { aborted_attempt } => Ok(self.step_control(StageEvent::RecvAbort { attempt: aborted_attempt })),
            Msg::BeginRecovery { base, target, recovery_id, truncate_to } => {
                let effects = self.stage.step(StageEvent::RecvBegin { base, target, recovery_id, truncate_to });
                // I7a/I7b (§7.19 (b)): if the freeze is ACCEPTED, a **surviving** stage must discard the
                // provisional tail the rest of the pipeline — rebuilt only to the durable frontier — can
                // no longer justify. Mirror the SM's `applied`-truncation into the engine KV (`kv_truncate`
                // drops positions ≥ its arg, so keep `[0, truncate_to]`), and drop provisional sampled
                // outputs (the sampler itself is restored by the subsequent `INSTALL_SAMPLER_CHECKPOINT`,
                // I15). A killed+replaced stage starts empty, so this is a no-op there; it is load-bearing
                // only for a survivor (e.g. the downstream S_P on a middle-stage kill).
                if effects.iter().any(|e| matches!(e, StageEffect::RecoveryAck { .. })) {
                    if let Some(eng) = self.engine.as_mut() {
                        // `i32::try_from`, not `as i32` (audit 1c): a wire `i64` that does not fit is
                        // refused, never truncated into some other position. The full H3 bound
                        // (`0 ≤ truncate_to < n_ctx`, `target == base+1`) lands in Wave 1d.
                        let keep = truncate_to.saturating_add(1).max(0);
                        let keep = i32::try_from(keep).map_err(|_| WorkerError::PreFfi { what: "BEGIN_RECOVERY truncate_to", value: truncate_to, bound: i32::MAX as i64 })?;
                        eng.ctx.kv_truncate(keep)?;
                    }
                    self.sampled_ring.clear();
                }
                Ok(effects.into_iter().filter_map(|eff| self.encode_effect(eff)).collect())
            }
            Msg::CatchUpContext { goal_input_pos } => Ok(self.catch_up(goal_input_pos)),
            // Acks / errors / SAMPLED are coordinator-inbound; a worker never receives them. The
            // durability-plane acks (DURABILITY_ACK / COMMIT_ACK / COMMIT_SYNC) are consumed by the
            // release-rule logic in the serve loop, not by `on_frame`; a stage worker that is not a
            // durability target ignores an inbound BOUNDARY_COPY (seam 2 gives the target a handler).
            Msg::ActivationCommitted(_)
            | Msg::ActivationFinalized
            | Msg::RecoveryAck { .. }
            | Msg::CatchUpReady { .. }
            | Msg::AppliedAck { .. }
            | Msg::Sampled { .. }
            | Msg::SamplerCheckpointInstalled { .. }
            | Msg::BoundaryCopy { .. }
            | Msg::DurabilityAck { .. }
            | Msg::CommitAck { .. }
            | Msg::CommitSync { .. }
            | Msg::Err { .. } => Ok(vec![]),
        }
    }

    /// Drive the **real stage SM** through catch-up: step `RebuildStep{goal}` until it reaches
    /// `FROZEN_READY` (or stalls), then emit `CATCH_UP_READY`. The engine KV is rebuilt separately by
    /// the preceding `REBUILD_APPLY` (`APPLY_TOKEN` NO_SAMPLE) frames — this advances the SM's
    /// control-plane frontier so activation can commit (spec §6.2). Bounded to `goal+2` steps so a
    /// stuck SM cannot loop forever.
    fn catch_up(&mut self, goal: i64) -> Vec<Vec<u8>> {
        let mut ready: Option<Vec<u8>> = None;
        for _ in 0..goal.max(0) + 2 {
            for eff in self.stage.step(StageEvent::RebuildStep { goal }) {
                if let StageEffect::Ready { recovery_id, applied, .. } = eff {
                    ready = Some(wire::encode_catch_up_ready(&self.cfg.fence, self.stage.epoch(), recovery_id, applied));
                }
            }
            if ready.is_some() {
                break;
            }
        }
        ready.into_iter().collect()
    }

    /// Final-stage apply tail: retain the position's logits (for a later `SAMPLE_NEXT`, I14) and
    /// ack the position.
    ///
    /// **The `output_checksum` is a teacher-forced WITNESS, and it is emitted only for a
    /// teacher-forced (`NO_SAMPLE`) apply.** It exists so an equivalence harness — the rule-14
    /// bit-exact anchor, the shard anchor, the chunked-prefill anchor — can compare a final-stage
    /// logits vector without sampling it. On an **autoregressive decode** apply (`no_sample =
    /// false`) the coordinator's token comes from `SAMPLED`, never from this ack, and nothing in
    /// the protocol reads the field; digesting the whole `n_vocab` vector there is pure cost.
    ///
    /// It is not a small cost: on the dev model that vector is 151 936 floats, and converting it
    /// to little-endian bytes and BLAKE3-hashing it measures **9.607 ms/token**
    /// (`tests/digest_cost.rs`) — which is what PROJECT_STATE §7.25 identified when a calibration
    /// residual that was constant across split points turned out to be the harness's own witness.
    /// Gating it on `no_sample` is what makes a **production-shaped** TPOT measurable at all, and
    /// it removes real per-token work from the deployed decode loop.
    ///
    /// Retention itself is unconditional: I14 requires `SAMPLE_NEXT` to sample only from the
    /// retained logits of that position.
    fn retain_and_ack(&mut self, epoch: Epoch, pos: i64, no_sample: bool) -> Result<Vec<Vec<u8>>, WorkerError> {
        let eng = self.engine.as_mut().ok_or(WorkerError::EngineUnavailable)?;
        let logits = eng.last_logits()?;
        let witness = if no_sample { logits_digest(&logits).to_vec() } else { Vec::new() };
        self.latest_logits = Some(logits);
        Ok(vec![wire::encode_applied_ack(&self.cfg.fence, epoch, pos, &witness)])
    }

    /// `SAMPLE_NEXT` (spec §3, I14): fence the checkpoint id + config hash (drift is fatal), serve a
    /// duplicate from the snapshot ring **without advancing the RNG**, else sample from the retained
    /// logits and cache the result.
    fn on_sample_next(
        &mut self,
        epoch: Epoch,
        output_pos: i64,
        config_hash: &[u8],
        expected_checkpoint_id: u64,
    ) -> Result<Vec<Vec<u8>>, WorkerError> {
        let fence = &self.cfg.fence;
        let Some(sampler) = self.sampler.as_mut() else {
            return Ok(vec![wire::encode_error(fence, epoch, 0, ERR_CHECKPOINT_MISMATCH)]);
        };
        // Fatal drift → reject loudly, never silently repair (spec §2.6b).
        if sampler.check_fence(expected_checkpoint_id, config_hash).is_err() {
            return Ok(vec![wire::encode_error(fence, epoch, 0, ERR_CHECKPOINT_MISMATCH)]);
        }
        // I14: a duplicate SAMPLE_NEXT is served from the SAMPLED cache; the RNG never re-advances.
        if let Some(entry) = self.sampled_ring.get(&output_pos) {
            return Ok(vec![wire::encode_sampled(fence, epoch, output_pos, entry.token_id, &entry.snapshot, &entry.state_digest)]);
        }
        let Some(logits) = self.latest_logits.as_ref() else {
            // No retained logits for this position (I14: sample only from retained logits).
            return Ok(vec![wire::encode_error(fence, epoch, 0, ERR_CHECKPOINT_MISMATCH)]);
        };
        let out = sampler.sample(output_pos, logits);
        self.sampled_ring.insert(
            output_pos,
            SampledEntry { token_id: out.token_id, snapshot: out.snapshot.clone(), state_digest: out.state_digest },
        );
        Ok(vec![wire::encode_sampled(fence, epoch, output_pos, out.token_id, &out.snapshot, &out.state_digest)])
    }

    /// `INSTALL_SAMPLER_CHECKPOINT` (I17): install the exact state into S_P's sampler (idempotent),
    /// then ack. The sampler must exist (a final stage with a config).
    fn on_install_sampler_checkpoint(
        &mut self,
        epoch: Epoch,
        checkpoint_id: u64,
        snapshot: &[u8],
    ) -> Result<Vec<Vec<u8>>, WorkerError> {
        let fence = &self.cfg.fence;
        let Some(sampler) = self.sampler.as_mut() else {
            return Ok(vec![wire::encode_error(fence, epoch, 0, ERR_CHECKPOINT_MISMATCH)]);
        };
        match sampler.install(snapshot) {
            Ok(()) => {
                let digest = *blake3::hash(snapshot).as_bytes();
                Ok(vec![wire::encode_sampler_checkpoint_installed(fence, epoch, checkpoint_id, sampler.sampled_pos(), &digest)])
            }
            Err(SamplerError::BadChecksum) | Err(SamplerError::BadSnapshot(_)) | Err(SamplerError::ConfigDrift) => {
                Ok(vec![wire::encode_error(fence, epoch, 0, ERR_CHECKPOINT_MISMATCH)])
            }
            Err(_) => Ok(vec![wire::encode_error(fence, epoch, 0, ERR_CHECKPOINT_MISMATCH)]),
        }
    }

    /// Step the real Stage SM and encode each emitted effect to the wire.
    fn step_control(&mut self, ev: StageEvent) -> Vec<Vec<u8>> {
        self.stage.step(ev).into_iter().filter_map(|eff| self.encode_effect(eff)).collect()
    }

    fn encode_effect(&self, eff: StageEffect) -> Option<Vec<u8>> {
        let fence = &self.cfg.fence;
        let gen = self.stage.generation();
        match eff {
            StageEffect::Committed { epoch, recovery_id, attempt, .. } => {
                let t = ActivationTuple { kind: ActivationKind::Initial, epoch, recovery_id, attempt, sampler_checkpoint_id: 0 };
                Some(wire::encode_activation_committed(fence, &t, gen))
            }
            StageEffect::Finalized { attempt, .. } => Some(wire::encode_activation_finalized(fence, self.stage.epoch(), attempt)),
            StageEffect::RecoveryAck { target, recovery_id, .. } => {
                Some(wire::encode_recovery_ack(fence, target, recovery_id, self.stage.applied()))
            }
            StageEffect::ResetAck { recovery_id, .. } => {
                Some(wire::encode_recovery_ack(fence, self.stage.epoch(), recovery_id, self.stage.applied()))
            }
            StageEffect::RecoveryCompleted { target, .. } => Some(wire::encode_error(fence, target, 0, ERR_RECOVERY_COMPLETED)),
            StageEffect::Fenced { attempt, .. } => Some(wire::encode_error(fence, self.stage.epoch(), attempt, ERR_FENCED)),
            // `Ready` is an internal catch-up milestone (no wire ack in this slice).
            StageEffect::Ready { .. } => None,
        }
    }
}

/// **Audit C2 — the message-family gate: may `role` send `body` to a worker?**
///
/// The finding was that mTLS answers *"is this peer in the cluster?"* and nothing answered *"which
/// stage is it?"*, so **any certificate holder could send any message family** — a stage worker
/// could issue `COMMIT_ACTIVATION`, the durability target could send `SAMPLED`, and the F1 fence
/// passed all of it because it checks *session* identity, not *sender* role.
///
/// # Why the table is written as an allow-list of (role, family) pairs
///
/// A deny-list is wrong for the same reason a first-match SAN lookup is: the default for anything
/// unlisted must be **refuse**. Written this way, a message family added to the protocol has **no
/// sender** until someone states one, so the failure mode of forgetting is a refusal rather than a
/// silent grant.
///
/// # The v1 topology this encodes (BLUEPRINT §1.5, spec §4)
///
/// * the **coordinator** owns the control plane and the token/sample data plane it drives;
/// * an **upstream stage** sends `FWD` (a boundary) and, on the durability edge, `BOUNDARY_COPY`;
/// * a **downstream stage** answers with `APPLIED_ACK` / `COMMIT_ACK`;
/// * the **durability target** answers with `DURABILITY_ACK`.
///
/// Note that a stage is *not* permitted to send `SAMPLE_NEXT` or any activation frame: sampling is
/// S_P's to answer and the coordinator's to request (spec §1.4's ownership boundary), and the
/// activation transaction is the coordinator's alone.
pub fn role_may_send(role: PeerRole, body: hydra_proto::proto::Body) -> bool {
    use hydra_proto::proto::Body as B;
    match role {
        PeerRole::Coordinator => matches!(
            body,
            // data plane the coordinator drives
            B::ApplyToken | B::Fwd | B::SampleNext | B::CommitSync
                // recovery / reset control plane
                | B::BeginRecovery | B::ResetRecoveryAttempt | B::CatchUpContext
                // sampler + segment checkpoint planes
                | B::InstallSamplerCheckpoint | B::PrepareSegmentCheckpoint
                // the activation transaction
                | B::CommitActivation | B::FinalizeActivation | B::ActivationCommitAbort
                // lifecycle + errors
                | B::Cancel | B::Error
        ),
        // An upstream stage forwards boundaries; a downstream stage acknowledges them. One worker's
        // peer may be either, and the two sets are disjoint, so a single arm covers both without
        // widening what any one connection can do beyond stage-to-stage traffic.
        PeerRole::Stage { .. } => matches!(
            body,
            B::Fwd | B::AppliedAck | B::CommitAck | B::BoundaryCopy | B::DurabilityAck | B::Error
        ),
        PeerRole::DurabilityTarget => matches!(body, B::DurabilityAck | B::Error),
    }
}

/// The role gate, applied to a raw frame before its body is interpreted.
///
/// **Order matters and is the point:** this runs on a `peek_body` (which roots the FlatBuffer and
/// reads one enum) *before* `on_frame` decodes anything. An authorisation check that runs after
/// decoding has already let an unauthorised peer direct work — buffer allocation, tensor copies,
/// engine calls. A frame whose body does not even parse is also refused here, since an
/// unidentifiable message family cannot be shown to be permitted.
pub fn check_role(role: PeerRole, payload: &[u8]) -> Result<(), WorkerError> {
    let body = wire::peek_body(payload).ok_or_else(|| WorkerError::Unauthorized {
        role: role.label(),
        body: "<unparseable body>".to_string(),
    })?;
    if role_may_send(role, body) {
        Ok(())
    } else {
        Err(WorkerError::Unauthorized { role: role.label(), body: format!("{body:?}") })
    }
}

/// Serve one connection: recv → `on_frame` → send each reply, until the peer closes. A wire/engine
/// error on a single frame is surfaced (the caller decides whether to drop the connection); a clean
/// EOF returns `Ok(())`.
pub async fn serve_conn<S>(worker: &mut Worker, conn: &mut Conn<S>, role: PeerRole) -> Result<(), WorkerError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let frame = match conn.recv().await {
            Ok(f) => f,
            // Clean shutdown (peer closed / killed) — not an error at this layer.
            Err(hydra_transport::TransportError::Io(e)) if is_eof(&e) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        check_role(role, &frame.payload)?;
        for reply in worker.on_frame(&frame.payload)? {
            conn.send(0, &reply).await?;
        }
    }
}

/// Serve one upstream connection with **worker→worker direct FWD** (spec §4 data plane): when
/// `on_frame` produces a `FWD` boundary, it is sent **straight to the downstream peer** over `down`
/// — never relayed through the coordinator — and the peer's response (an `APPLIED_ACK`, or its own
/// `FWD` for a 3-stage pipeline) is relayed back upstream. Non-`FWD` replies (control-plane acks) go
/// straight back upstream. This replaces the coordinator-relay interim: the expensive boundary
/// tensor travels S1→S2 directly; only the small ack traverses the coordinator edge.
pub async fn serve_conn_forwarding<U, D>(worker: &mut Worker, up: &mut Conn<U>, down: &mut Conn<D>, role: PeerRole) -> Result<(), WorkerError>
where
    U: AsyncRead + AsyncWrite + Unpin,
    D: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let frame = match up.recv().await {
            Ok(f) => f,
            Err(hydra_transport::TransportError::Io(e)) if is_eof(&e) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        check_role(role, &frame.payload)?;
        for reply in worker.on_frame(&frame.payload)? {
            if wire::is_fwd_frame(&reply) {
                // Direct S1→S2: the boundary tensor never traverses the coordinator on the compute path.
                down.send(0, &reply).await?;
                let resp = down.recv().await?;
                up.send(0, &resp.payload).await?;
            } else {
                up.send(0, &reply).await?;
            }
        }
    }
}

fn is_eof(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe)
}

// --------------------------- multi-connection serve loop (P1·1a) ---------------------------
//
// A worker→worker chained pipeline where the coordinator ALSO samples/controls a stage needs each
// worker to serve **concurrent** inbound connections (seam-3 discovery): S_P serves S1's `FWD`
// (data plane) **and** the coordinator's `SAMPLE_NEXT`/control at the same time; a mid stage serves
// its upstream `FWD` and coordinator control likewise. The sequential accept loop (`serve_conn` in a
// `while accept` loop) serves one connection to completion before accepting the next, so a long-lived
// data connection starves the control connection — a deadlock for that topology.
//
// The engine's C context is not `Send`, so the single `Worker` cannot move between threads; instead
// every connection shares **one** `Worker` on **one** thread behind a `RefCell`. The invariant that
// makes this sound: the borrow is taken **only across the synchronous `on_frame`** and **never held
// across an `.await`**. On a current-thread runtime one task runs at a time and `on_frame` awaits
// nothing, so two connections' frames interleave at frame granularity with no double-borrow.

/// A [`Worker`] shared across concurrent inbound connections on one thread (see the module note on
/// the borrow-never-across-await invariant).
pub type SharedWorker = Rc<RefCell<Worker>>;

/// Wrap a worker for the multi-connection serve loop.
pub fn shared(worker: Worker) -> SharedWorker {
    Rc::new(RefCell::new(worker))
}

/// Serve one inbound connection against a **shared** worker: recv → `on_frame` (borrow scoped to the
/// synchronous call, released before any send) → send each reply, until the peer closes. This is the
/// concurrent-safe analogue of [`serve_conn`]; several of these run at once against one `Worker`.
pub async fn serve_conn_shared<S>(worker: &SharedWorker, conn: &mut Conn<S>, role: PeerRole) -> Result<(), WorkerError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let frame = match conn.recv().await {
            Ok(f) => f,
            Err(hydra_transport::TransportError::Io(e)) if is_eof(&e) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        // Borrow scoped to the synchronous `on_frame` — the replies are owned bytes, so the borrow
        // is dropped before we `.await` a send (the invariant that keeps the `RefCell` sound).
        check_role(role, &frame.payload)?;
        let replies = worker.borrow_mut().on_frame(&frame.payload)?;
        for reply in replies {
            conn.send(0, &reply).await?;
        }
    }
}

/// Accept inbound connections forever, serving each **concurrently** against the one shared `Worker`
/// via `spawn_local` (so a slow/long-lived peer never blocks a second peer — the multi-connection
/// requirement). Must be run inside a `tokio::task::LocalSet` on a current-thread runtime (the shared
/// `Worker`/`Rc` are `!Send`). A per-connection error drops only that connection; a listener error
/// ends the loop.
pub async fn serve_multi_conn(worker: SharedWorker, listener: TcpMtlsListener) -> Result<(), WorkerError> {
    loop {
        // Audit C2: `accept()` yields the connection AND the role its certificate bound to. There
        // is no path here that produces a connection without one.
        let (mut conn, role) = match listener.accept().await {
            Ok(a) => (a.conn, a.peer.role),
            Err(e) => return Err(e.into()),
        };
        let w = worker.clone();
        tokio::task::spawn_local(async move {
            if let Err(e) = serve_conn_shared(&w, &mut conn, role).await {
                eprintln!("hydra-worker: connection ended with error: {e}");
            }
        });
    }
}

// --------------------------- direct-FWD recovery re-link (P1·1a) ---------------------------
//
// In a direct-FWD pipeline the survivor stage forwards each boundary **straight** to its downstream
// peer (worker→worker). When that peer is killed and replaced (a two-stage D1 recovery), the survivor
// must re-link its outbound down-link to the replacement — **without** its own upstream (coordinator)
// connection reconnecting, and **preserving its own KV** (the `Worker` outlives the re-link). The
// coordinator drives the replacement's rebuild (from the durable `BoundaryStore`) + activation, then
// updates the shared [`DownTarget`]; the survivor re-links on its next forward.

/// A shared, updatable downstream target (address + certificate name). The coordinator rewrites it
/// when it brings up a replacement downstream stage; the survivor re-links to the new value.
pub type DownTarget = std::sync::Arc<std::sync::Mutex<(SocketAddr, String)>>;

/// Forward `frame` to the downstream peer named by `down` and return its response, **re-linking on
/// failure**: if the current connection is absent or errors (the downstream died), drop it, re-read
/// the shared `DownTarget`, and reconnect — up to `retries` attempts with a short backoff — before
/// surfacing the error. The survivor's `Worker`/KV and its upstream connection are untouched. This is
/// the direct-FWD recovery re-link primitive (P1·1a).
pub async fn forward_with_relink(
    dc: &mut Option<ClientConn>,
    connector: &TcpMtls,
    down: &DownTarget,
    frame: &[u8],
    retries: usize,
) -> Result<Vec<u8>, WorkerError> {
    let mut last: Option<WorkerError> = None;
    for attempt in 0..=retries {
        // (Re)connect from the CURRENT target — after a failure this picks up the replacement.
        if dc.is_none() {
            let (addr, name) = down.lock().unwrap().clone();
            match connector.connect(addr, &name).await {
                Ok(c) => *dc = Some(c),
                Err(e) => {
                    last = Some(e.into());
                    if attempt < retries {
                        relink_backoff(attempt).await;
                    }
                    continue;
                }
            }
        }
        let conn = dc.as_mut().unwrap();
        match forward_once(conn, frame).await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                *dc = None; // dead link — re-link on the next attempt
                last = Some(e);
                if attempt < retries {
                    relink_backoff(attempt).await;
                }
            }
        }
    }
    Err(last.unwrap_or(WorkerError::EngineUnavailable))
}

async fn forward_once(conn: &mut ClientConn, frame: &[u8]) -> Result<Vec<u8>, WorkerError> {
    conn.send(0, frame).await?;
    Ok(conn.recv().await?.payload)
}

// ------------------- multi-conn + forwarding + durable serve loop (P1·1b seam B) -------------------
//
// The 3-node chained pipeline S1→S2→S_P where a mid stage forwards its boundary downstream AND is
// durably copied. It composes seam-1 (multi-conn: one `Worker` shared across concurrent inbound
// connections) with seam-A durability ([`DurableForwarder`]): on a `FWD` reply, forward it to the
// shared downstream link and copy it to the durability target under the R3′ retention bound.
//
// ┌─ CONCURRENCY / LOCK-ORDERING CONTRACT (binding — the intricate part; structured so a violation
// │  does not compile-or-run rather than merely being discouraged) ────────────────────────────────
// │  Two locks are in play, and they have OPPOSITE await disciplines:
// │
// │   * `Rc<RefCell<Worker>>` (the shared engine state). The `RefCell` borrow is taken ONLY across
// │     the SYNCHRONOUS `on_frame` call and is NEVER held across an `.await`. It is written as an
// │     unnamed temporary — `worker.borrow_mut().on_frame(..)?` — so there is no binding that could
// │     outlive the statement, i.e. it is *awkward to hold it across an await even by accident*.
// │     WHY THIS IS LOAD-BEARING: on the current-thread runtime one task runs at a time, so while a
// │     forward `.await`s, another connection's task runs. If this task held the `Worker` borrow
// │     across that await, the other task's `on_frame` `borrow_mut()` would panic (double borrow).
// │     The archetype is a concurrent `SAMPLE_NEXT` arriving during an in-flight `FWD` — the exact
// │     interleaving `panic_vector_*` loops ~100× to prove sound.
// │
// │   * `Rc<tokio::sync::Mutex<DownstreamState>>` (the down-link + durability conn + forwarder). This
// │     is an ASYNC Mutex precisely because the forward `send`/`recv` IS held across an `.await`; a
// │     `RefCell` here would panic. It is locked ONLY inside the `FWD` branch, so a control frame
// │     (e.g. `SAMPLE_NEXT`) on another connection never contends it and is served concurrently — the
// │     entire reason multi-conn exists (generation is sequential per position; the concurrency we
// │     need is accept-both, not heavy parallelism).
// │
// │  Per FWD step (all inside the `FWD` branch, the borrow already dropped): lock the downstream
// │  Mutex → (already have the owned reply bytes from the sync worker step) → forward send/recv +
// │  durability copy while holding ONLY the link lock. Backpressure on the R3′ bound happens here too
// │  (block on `DURABILITY_ACK`, never drop a copy).
// └─────────────────────────────────────────────────────────────────────────────────────────────────

/// The shared downstream state for a durable forwarding stage: the direct S1→S2 down-link (which is
/// **re-linkable** — seam 2), the background durability connection, and the R3′ [`DurableForwarder`].
/// Behind a `tokio::sync::Mutex` (async) because the forward `send`/`recv` is held across an `.await`
/// — a `RefCell` would be unsound there (see the serve-loop contract). Only the `FWD`-carrying
/// connection ever locks it.
///
/// The down-link is a re-establishable [`forward_with_relink`] pair (`Option<ClientConn>` +
/// connector + updatable [`DownTarget`]), not a fixed connection: when the coordinator kills a
/// downstream stage and brings up a replacement, it rebuilds the replacement from the durable
/// boundaries and rewrites the shared `down_target`; the survivor re-links on its next forward,
/// preserving its own KV and upstream connection (seam C). A run with no recovery simply never
/// updates the target.
pub struct DownstreamState {
    /// The current direct down-link (lazily (re)connected from `down_target`). `None` until the first
    /// forward, and reset to `None` on a forward failure so the next attempt re-links.
    pub down: Option<ClientConn>,
    /// The `(addr, name)` the current `down` connection was opened to — so a forward can detect that
    /// the coordinator **rewrote** `down_target` (a recovery re-target) and re-link proactively, not
    /// only on a hard connection failure. In a real `kill -9` both happen; detecting the target change
    /// makes the re-link correct even when the killed peer's socket has not yet errored.
    pub down_connected_to: Option<(SocketAddr, String)>,
    /// Dials the downstream presenting this stage's identity (trusting the cluster CA).
    pub down_connector: TcpMtls,
    /// The (updatable) downstream address + cert name — the coordinator rewrites it on recovery.
    pub down_target: DownTarget,
    /// Bounded re-link retries before a forward surfaces an error (never hangs).
    pub relink_retries: usize,
    /// Background-class durability connection (`BOUNDARY_COPY` out, `DURABILITY_ACK` back).
    pub dur: ClientConn,
    /// R3′ retention + copy policy (seam A).
    pub forwarder: DurableForwarder,
}

/// A downstream shared across the concurrent serve tasks of one worker (see [`DownstreamState`]).
pub type SharedDown = Rc<tokio::sync::Mutex<DownstreamState>>;

/// Wrap a downstream for the durable forwarding serve loop.
pub fn shared_down(state: DownstreamState) -> SharedDown {
    Rc::new(tokio::sync::Mutex::new(state))
}

/// Forward one `FWD` reply on the (already-locked) downstream, copy it for durability under the R3′
/// bound, and return the downstream's response to relay upstream. This is the FWD sub-step of the
/// serve loop, extracted so the worker borrow is provably not in scope: its only `Worker` input is
/// the already-decoded `(input_pos, boundary)` bytes.
async fn forward_and_copy(d: &mut DownstreamState, reply: &[u8], fence: &SessionFence) -> Result<Vec<u8>, WorkerError> {
    // Decode the boundary out of the FWD reply for the durability copy (the raw bytes are still what
    // we forward downstream — the direct S1→S2 path is byte-preserving).
    let (input_pos, boundary) = match wire::decode(reply, fence)?.1 {
        Msg::Fwd { first_input_pos, activations, .. } => (first_input_pos, activations),
        _ => return Err(WorkerError::Wire(WireError::Malformed("forward_and_copy: reply is not FWD".into()))),
    };

    // BACKPRESSURE (spec §5, R3′ bound): if retention is at the bound, block on DURABILITY_ACKs until
    // a slot frees. Never drop a copy — a dropped boundary is a future recovery hole (slower is safe,
    // lossy is not). `on_applied_ack` advances inline (below) from the downstream response, so at the
    // bound it is durability that must catch up.
    while d.forwarder.is_at_capacity() {
        let ack = d.dur.recv().await?;
        match wire::decode(&ack.payload, fence)?.1 {
            Msg::DurabilityAck { durable_through_input_pos, .. } => d.forwarder.on_durability_ack(durable_through_input_pos),
            _ => return Err(WorkerError::Wire(WireError::Malformed("expected DURABILITY_ACK on the durability link".into()))),
        }
        d.forwarder.release();
    }

    // Re-target detection (seam 2): if the coordinator rewrote `down_target` since we last connected
    // (a recovery brought up a replacement downstream), drop the cached link so the forward re-links
    // to the new peer — even if the old socket has not yet errored (the in-process / not-yet-dead
    // case). A hard failure is still handled inside `forward_with_relink`.
    {
        let target = d.down_target.lock().unwrap().clone();
        if d.down_connected_to.as_ref() != Some(&target) {
            d.down = None;
            d.down_connected_to = Some(target);
        }
    }

    // Direct S1→S2 with re-link (seam 2): the boundary tensor travels worker→worker, never via the
    // coordinator; on a down-link failure (a killed+replaced downstream) it re-reads `down_target`
    // and reconnects to the replacement, bounded, without touching the upstream connection or KV.
    let DownstreamState { down, down_connector, down_target, relink_retries, dur, forwarder, .. } = d;
    let resp_payload = forward_with_relink(down, down_connector, down_target, reply, *relink_retries).await?;

    // The downstream's response to a boundary FWD is its APPLIED_ACK — advance the R3′ downstream
    // frontier from it so release can proceed.
    if let Ok((_, Msg::AppliedAck { cumulative_input_pos, .. })) = wire::decode(&resp_payload, fence) {
        forwarder.on_applied_ack(cumulative_input_pos);
    }

    // Background-class durability copy (BOUNDARY_COPY) + R3′ retain. Fire-and-retain: the matching
    // DURABILITY_ACK is drained lazily (only under backpressure above), so this does not block the
    // forward path in the common case.
    forwarder.copy_and_retain(dur, input_pos, &boundary).await?;
    forwarder.release();

    Ok(resp_payload)
}

/// Serve one inbound connection against a **shared** worker with **durable worker→worker forwarding**:
/// recv → `on_frame` (borrow scoped to the synchronous call) → for each reply, a `FWD` is forwarded to
/// the shared downstream and durably copied ([`forward_and_copy`]); a non-`FWD` reply (a control ack,
/// e.g. `SAMPLE_NEXT`→`SAMPLED` on a coordinator connection) goes straight back upstream. Several of
/// these run at once against one `Worker`/one `DownstreamState`; only FWD-carrying connections contend
/// the downstream lock. See the module contract on the two locks' opposite await disciplines.
pub async fn serve_conn_forwarding_durable_shared<S>(worker: &SharedWorker, down: &SharedDown, conn: &mut Conn<S>, fence: &SessionFence, role: PeerRole) -> Result<(), WorkerError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let frame = match conn.recv().await {
            Ok(f) => f,
            Err(hydra_transport::TransportError::Io(e)) if is_eof(&e) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        // Synchronous worker step: the `RefCell` borrow begins and ends INSIDE this call (unnamed
        // temporary), so it is provably not held across the awaits below.
        check_role(role, &frame.payload)?;
        let replies = worker.borrow_mut().on_frame(&frame.payload)?;
        for reply in replies {
            if wire::is_fwd_frame(&reply) {
                // FWD branch: lock the downstream ONLY here; the worker borrow is already dropped.
                let mut d = down.lock().await;
                let resp = forward_and_copy(&mut d, &reply, fence).await?;
                conn.send(0, &resp).await?;
            } else {
                conn.send(0, &reply).await?;
            }
        }
    }
}

/// Accept inbound connections forever, serving each **concurrently** against the one shared `Worker`
/// and one shared `DownstreamState` (durable worker→worker forwarding). Must run inside a
/// `tokio::task::LocalSet` on a current-thread runtime (the `Worker`/`Rc` are `!Send`). A
/// per-connection error drops only that connection; a listener error ends the loop.
pub async fn serve_multi_conn_forwarding_durable(worker: SharedWorker, down: SharedDown, fence: SessionFence, listener: TcpMtlsListener) -> Result<(), WorkerError> {
    loop {
        let (mut conn, role) = match listener.accept().await {
            Ok(a) => (a.conn, a.peer.role),
            Err(e) => return Err(e.into()),
        };
        let w = worker.clone();
        let d = down.clone();
        let k = fence.clone();
        tokio::task::spawn_local(async move {
            if let Err(e) = serve_conn_forwarding_durable_shared(&w, &d, &mut conn, &k, role).await {
                eprintln!("hydra-worker: durable forwarding connection ended with error: {e}");
            }
        });
    }
}

/// Small bounded backoff between re-link attempts. The coordinator sequences the replacement's
/// readiness before driving the survivor's next forward, so this only smooths a brief window.
async fn relink_backoff(attempt: usize) {
    tokio::time::sleep(std::time::Duration::from_millis(25 * (attempt as u64 + 1))).await;
}

/// Serve one upstream connection with **worker→worker direct FWD and a reconnectable downstream**:
/// like [`serve_conn_forwarding`], but the down-link is re-established from the shared `DownTarget`
/// on failure ([`forward_with_relink`]). A downstream stage can be killed and replaced mid-session
/// and this survivor keeps serving its upstream on the same connection, re-linking to the replacement
/// on its next forward (the direct-FWD recovery re-link, P1·1a). Non-`FWD` replies go straight back.
pub async fn serve_conn_forwarding_relink<U>(
    worker: &mut Worker,
    up: &mut Conn<U>,
    connector: &TcpMtls,
    down: &DownTarget,
    relink_retries: usize,
    role: PeerRole,
) -> Result<(), WorkerError>
where
    U: AsyncRead + AsyncWrite + Unpin,
{
    let mut dc: Option<ClientConn> = None;
    loop {
        let frame = match up.recv().await {
            Ok(f) => f,
            Err(hydra_transport::TransportError::Io(e)) if is_eof(&e) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        check_role(role, &frame.payload)?;
        for reply in worker.on_frame(&frame.payload)? {
            if wire::is_fwd_frame(&reply) {
                let resp = forward_with_relink(&mut dc, connector, down, &reply, relink_retries).await?;
                up.send(0, &resp).await?;
            } else {
                up.send(0, &reply).await?;
            }
        }
    }
}
