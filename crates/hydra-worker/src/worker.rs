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

/// The shared downstream state for a durable forwarding stage: the direct S1→S2 down-link, the
/// background durability connection, and the R3′ [`DurableForwarder`]. Behind a `tokio::sync::Mutex`
/// (async) because the forward `send`/`recv` is held across an `.await` — a `RefCell` would be unsound
/// there (see the serve-loop contract). Only the `FWD`-carrying connection ever locks it.
pub struct DownstreamState {
    /// Direct worker→worker down-link (the boundary tensor travels here, never via the coordinator).
    pub down: ClientConn,
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
async fn forward_and_copy(d: &mut DownstreamState, reply: &[u8], keys: &SessionKeys) -> Result<Vec<u8>, WorkerError> {
    // Decode the boundary out of the FWD reply for the durability copy (the raw bytes are still what
    // we forward downstream — the direct S1→S2 path is byte-preserving).
    let (input_pos, boundary) = match wire::decode(reply, keys)?.1 {
        Msg::Fwd { first_input_pos, activations, .. } => (first_input_pos, activations),
        _ => return Err(WorkerError::Wire(WireError::Malformed("forward_and_copy: reply is not FWD".into()))),
    };

    // BACKPRESSURE (spec §5, R3′ bound): if retention is at the bound, block on DURABILITY_ACKs until
    // a slot frees. Never drop a copy — a dropped boundary is a future recovery hole (slower is safe,
    // lossy is not). `on_applied_ack` advances inline (below) from the downstream response, so at the
    // bound it is durability that must catch up.
    while d.forwarder.is_at_capacity() {
        let ack = d.dur.recv().await?;
        match wire::decode(&ack.payload, keys)?.1 {
            Msg::DurabilityAck { durable_through_input_pos, .. } => d.forwarder.on_durability_ack(durable_through_input_pos),
            _ => return Err(WorkerError::Wire(WireError::Malformed("expected DURABILITY_ACK on the durability link".into()))),
        }
        d.forwarder.release();
    }

    // Direct S1→S2: the boundary tensor travels worker→worker, never via the coordinator.
    let DownstreamState { down, dur, forwarder } = d;
    down.send(0, reply).await?;
    let resp = down.recv().await?;

    // The downstream's response to a boundary FWD is its APPLIED_ACK — advance the R3′ downstream
    // frontier from it so release can proceed.
    if let Ok((_, Msg::AppliedAck { cumulative_input_pos, .. })) = wire::decode(&resp.payload, keys) {
        forwarder.on_applied_ack(cumulative_input_pos);
    }

    // Background-class durability copy (BOUNDARY_COPY) + R3′ retain. Fire-and-retain: the matching
    // DURABILITY_ACK is drained lazily (only under backpressure above), so this does not block the
    // forward path in the common case.
    forwarder.copy_and_retain(dur, input_pos, &boundary).await?;
    forwarder.release();

    Ok(resp.payload)
}

/// Serve one inbound connection against a **shared** worker with **durable worker→worker forwarding**:
/// recv → `on_frame` (borrow scoped to the synchronous call) → for each reply, a `FWD` is forwarded to
/// the shared downstream and durably copied ([`forward_and_copy`]); a non-`FWD` reply (a control ack,
/// e.g. `SAMPLE_NEXT`→`SAMPLED` on a coordinator connection) goes straight back upstream. Several of
/// these run at once against one `Worker`/one `DownstreamState`; only FWD-carrying connections contend
/// the downstream lock. See the module contract on the two locks' opposite await disciplines.
pub async fn serve_conn_forwarding_durable_shared<S>(worker: &SharedWorker, down: &SharedDown, conn: &mut Conn<S>, keys: &SessionKeys) -> Result<(), WorkerError>
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
        let replies = worker.borrow_mut().on_frame(&frame.payload)?;
        for reply in replies {
            if wire::is_fwd_frame(&reply) {
                // FWD branch: lock the downstream ONLY here; the worker borrow is already dropped.
                let mut d = down.lock().await;
                let resp = forward_and_copy(&mut d, &reply, keys).await?;
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
pub async fn serve_multi_conn_forwarding_durable(worker: SharedWorker, down: SharedDown, keys: SessionKeys, listener: TcpMtlsListener) -> Result<(), WorkerError> {
    loop {
        let mut conn = match listener.accept().await {
            Ok(c) => c,
            Err(e) => return Err(e.into()),
        };
        let w = worker.clone();
        let d = down.clone();
        let k = keys.clone();
        tokio::task::spawn_local(async move {
            if let Err(e) = serve_conn_forwarding_durable_shared(&w, &d, &mut conn, &k).await {
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
