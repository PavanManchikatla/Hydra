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

use hydra_engine_sys::{Context, EngineError, Model, ENGINE_AVAILABLE};
use hydra_state::{ActivationKind, ActivationTuple, Epoch, RecoveryId, Stage, StageEffect, StageEvent, StageRank};
use hydra_transport::framed::Conn;
use hydra_transport::tcp_mtls::TcpMtlsListener;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::sampler::{Sampler, SamplerError, SamplingConfig};
use crate::wire::{self, Msg, SessionKeys, WireError};

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

/// Static description of one worker's role in the pipeline.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub keys: SessionKeys,
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
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error("engine: {0}")]
    Engine(#[from] EngineError),
    #[error("data-plane frame but no engine linked/loaded on this worker")]
    EngineUnavailable,
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

impl Engine {
    /// Build the engine for `cfg`, or `None` if the engine isn't linked or the model file is absent
    /// (both dev-environment artifacts — a control-plane-only worker still runs everywhere).
    fn try_new(cfg: &WorkerConfig) -> Result<Option<Engine>, EngineError> {
        if !ENGINE_AVAILABLE {
            return Ok(None);
        }
        let Some(path) = cfg.model_path.as_deref().filter(|p| std::path::Path::new(p).exists()) else {
            return Ok(None);
        };
        let model = Model::load(path, cfg.n_gpu_layers)?;
        let n_embd = model.n_embd() as usize;
        let model: &'static Model = Box::leak(Box::new(model));
        // A boundary-emitting stage is an embeddings context; the final stage is a logits context.
        let embeddings = !cfg.is_final;
        let ctx = model.context(cfg.layer_first, cfg.layer_last, embeddings, cfg.n_ctx, cfg.n_ctx)?;
        Ok(Some(Engine { ctx, n_embd, emit_boundary: embeddings }))
    }

    /// Apply one token at `pos`. Returns the extracted boundary (emit stages) or `None` (final).
    fn apply_token(&mut self, token: i32, pos: i32) -> Result<Option<Vec<f32>>, EngineError> {
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
    fn apply_boundary(&mut self, boundary: &[f32], pos: i32) -> Result<Option<Vec<f32>>, EngineError> {
        if self.emit_boundary {
            let mut b = vec![0f32; self.n_embd];
            self.ctx.apply_boundary(boundary, pos, Some(&mut b))?;
            Ok(Some(b))
        } else {
            self.ctx.apply_boundary(boundary, pos, None)?;
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
        let (view, msg) = wire::decode(payload, &self.cfg.keys)?;
        match msg {
            Msg::ApplyToken { input_pos, token_id, no_sample } => {
                if !self.cfg.receives_tokens {
                    // A non-ingress stage never receives raw tokens (F1/precondition) — drop.
                    return Ok(vec![]);
                }
                let eng = self.engine.as_mut().ok_or(WorkerError::EngineUnavailable)?;
                match eng.apply_token(token_id as i32, input_pos as i32)? {
                    Some(boundary) => Ok(vec![wire::encode_fwd(&self.cfg.keys, view.epoch, input_pos, no_sample, &boundary)]),
                    None => self.retain_and_ack(view.epoch, input_pos),
                }
            }
            Msg::Fwd { first_input_pos, no_sample, activations } => {
                let eng = self.engine.as_mut().ok_or(WorkerError::EngineUnavailable)?;
                match eng.apply_boundary(&activations, first_input_pos as i32)? {
                    Some(boundary) => Ok(vec![wire::encode_fwd(&self.cfg.keys, view.epoch, first_input_pos, no_sample, &boundary)]),
                    None => self.retain_and_ack(view.epoch, first_input_pos),
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
                Ok(self.step_control(StageEvent::RecvBegin { base, target, recovery_id, truncate_to }))
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
                    ready = Some(wire::encode_catch_up_ready(&self.cfg.keys, self.stage.epoch(), recovery_id, applied));
                }
            }
            if ready.is_some() {
                break;
            }
        }
        ready.into_iter().collect()
    }

    /// Final-stage apply tail: retain the position's logits (for a later `SAMPLE_NEXT`, I14) and
    /// ack with their digest (the teacher-forced bit-exact anchor's witness).
    fn retain_and_ack(&mut self, epoch: Epoch, pos: i64) -> Result<Vec<Vec<u8>>, WorkerError> {
        let eng = self.engine.as_mut().ok_or(WorkerError::EngineUnavailable)?;
        let logits = eng.last_logits()?;
        let digest = logits_digest(&logits);
        self.latest_logits = Some(logits);
        Ok(vec![wire::encode_applied_ack(&self.cfg.keys, epoch, pos, &digest)])
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
        let keys = &self.cfg.keys;
        let Some(sampler) = self.sampler.as_mut() else {
            return Ok(vec![wire::encode_error(keys, epoch, 0, ERR_CHECKPOINT_MISMATCH)]);
        };
        // Fatal drift → reject loudly, never silently repair (spec §2.6b).
        if sampler.check_fence(expected_checkpoint_id, config_hash).is_err() {
            return Ok(vec![wire::encode_error(keys, epoch, 0, ERR_CHECKPOINT_MISMATCH)]);
        }
        // I14: a duplicate SAMPLE_NEXT is served from the SAMPLED cache; the RNG never re-advances.
        if let Some(entry) = self.sampled_ring.get(&output_pos) {
            return Ok(vec![wire::encode_sampled(keys, epoch, output_pos, entry.token_id, &entry.snapshot, &entry.state_digest)]);
        }
        let Some(logits) = self.latest_logits.as_ref() else {
            // No retained logits for this position (I14: sample only from retained logits).
            return Ok(vec![wire::encode_error(keys, epoch, 0, ERR_CHECKPOINT_MISMATCH)]);
        };
        let out = sampler.sample(output_pos, logits);
        self.sampled_ring.insert(
            output_pos,
            SampledEntry { token_id: out.token_id, snapshot: out.snapshot.clone(), state_digest: out.state_digest },
        );
        Ok(vec![wire::encode_sampled(keys, epoch, output_pos, out.token_id, &out.snapshot, &out.state_digest)])
    }

    /// `INSTALL_SAMPLER_CHECKPOINT` (I17): install the exact state into S_P's sampler (idempotent),
    /// then ack. The sampler must exist (a final stage with a config).
    fn on_install_sampler_checkpoint(
        &mut self,
        epoch: Epoch,
        checkpoint_id: u64,
        snapshot: &[u8],
    ) -> Result<Vec<Vec<u8>>, WorkerError> {
        let keys = &self.cfg.keys;
        let Some(sampler) = self.sampler.as_mut() else {
            return Ok(vec![wire::encode_error(keys, epoch, 0, ERR_CHECKPOINT_MISMATCH)]);
        };
        match sampler.install(snapshot) {
            Ok(()) => {
                let digest = *blake3::hash(snapshot).as_bytes();
                Ok(vec![wire::encode_sampler_checkpoint_installed(keys, epoch, checkpoint_id, sampler.sampled_pos(), &digest)])
            }
            Err(SamplerError::BadChecksum) | Err(SamplerError::BadSnapshot(_)) | Err(SamplerError::ConfigDrift) => {
                Ok(vec![wire::encode_error(keys, epoch, 0, ERR_CHECKPOINT_MISMATCH)])
            }
            Err(_) => Ok(vec![wire::encode_error(keys, epoch, 0, ERR_CHECKPOINT_MISMATCH)]),
        }
    }

    /// Step the real Stage SM and encode each emitted effect to the wire.
    fn step_control(&mut self, ev: StageEvent) -> Vec<Vec<u8>> {
        self.stage.step(ev).into_iter().filter_map(|eff| self.encode_effect(eff)).collect()
    }

    fn encode_effect(&self, eff: StageEffect) -> Option<Vec<u8>> {
        let keys = &self.cfg.keys;
        let gen = self.stage.generation();
        match eff {
            StageEffect::Committed { epoch, recovery_id, attempt, .. } => {
                let t = ActivationTuple { kind: ActivationKind::Initial, epoch, recovery_id, attempt, sampler_checkpoint_id: 0 };
                Some(wire::encode_activation_committed(keys, &t, gen))
            }
            StageEffect::Finalized { attempt, .. } => Some(wire::encode_activation_finalized(keys, self.stage.epoch(), attempt)),
            StageEffect::RecoveryAck { target, recovery_id, .. } => {
                Some(wire::encode_recovery_ack(keys, target, recovery_id, self.stage.applied()))
            }
            StageEffect::ResetAck { recovery_id, .. } => {
                Some(wire::encode_recovery_ack(keys, self.stage.epoch(), recovery_id, self.stage.applied()))
            }
            StageEffect::RecoveryCompleted { target, .. } => Some(wire::encode_error(keys, target, 0, ERR_RECOVERY_COMPLETED)),
            StageEffect::Fenced { attempt, .. } => Some(wire::encode_error(keys, self.stage.epoch(), attempt, ERR_FENCED)),
            // `Ready` is an internal catch-up milestone (no wire ack in this slice).
            StageEffect::Ready { .. } => None,
        }
    }
}

/// Serve one connection: recv → `on_frame` → send each reply, until the peer closes. A wire/engine
/// error on a single frame is surfaced (the caller decides whether to drop the connection); a clean
/// EOF returns `Ok(())`.
pub async fn serve_conn<S>(worker: &mut Worker, conn: &mut Conn<S>) -> Result<(), WorkerError>
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
pub async fn serve_conn_forwarding<U, D>(worker: &mut Worker, up: &mut Conn<U>, down: &mut Conn<D>) -> Result<(), WorkerError>
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
pub async fn serve_conn_shared<S>(worker: &SharedWorker, conn: &mut Conn<S>) -> Result<(), WorkerError>
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
        let mut conn = match listener.accept().await {
            Ok(c) => c,
            Err(e) => return Err(e.into()),
        };
        let w = worker.clone();
        tokio::task::spawn_local(async move {
            if let Err(e) = serve_conn_shared(&w, &mut conn).await {
                eprintln!("hydra-worker: connection ended with error: {e}");
            }
        });
    }
}
