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

use hydra_engine_sys::{Context, EngineError, Model, ENGINE_AVAILABLE};
use hydra_state::{ActivationKind, ActivationTuple, Epoch, RecoveryId, Stage, StageEffect, StageEvent, StageRank};
use hydra_transport::framed::Conn;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::wire::{self, Msg, SessionKeys, WireError};

/// `ERR_FENCED` on the wire (`proto::ErrCode::ERR_FENCED`).
const ERR_FENCED: u16 = 1;
/// `ERR_RECOVERY_COMPLETED` (Case B′).
const ERR_RECOVERY_COMPLETED: u16 = 3;

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

    /// BLAKE3 digest of the retained (unsampled, I14) f32 logits for the position just applied —
    /// the wire-transmittable witness of bit-exactness for the teacher-forced anchor. A worker
    /// applies exactly one position per frame, so the logits live at batch-relative output index 0
    /// (`hydra_logits` indexes the most recent apply's enabled outputs, not the absolute position).
    fn last_logits_digest(&mut self) -> Result<[u8; 32], EngineError> {
        let logits = self.ctx.logits(0)?;
        Ok(*blake3::hash(&wire::f32_to_bytes_le(&logits)).as_bytes())
    }
}

/// A running stage worker: the real `hydra-state` [`Stage`] SM + an optional engine.
pub struct Worker {
    cfg: WorkerConfig,
    stage: Stage,
    engine: Option<Engine>,
}

impl Worker {
    pub fn new(cfg: WorkerConfig) -> Result<Worker, WorkerError> {
        let stage = Stage::frozen_ready(cfg.rank, cfg.epoch, cfg.recovery_id);
        let engine = Engine::try_new(&cfg)?;
        Ok(Worker { cfg, stage, engine })
    }

    pub fn has_engine(&self) -> bool {
        self.engine.is_some()
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
                    None => {
                        let digest = eng.last_logits_digest()?;
                        Ok(vec![wire::encode_applied_ack(&self.cfg.keys, view.epoch, input_pos, &digest)])
                    }
                }
            }
            Msg::Fwd { first_input_pos, no_sample, activations } => {
                let eng = self.engine.as_mut().ok_or(WorkerError::EngineUnavailable)?;
                match eng.apply_boundary(&activations, first_input_pos as i32)? {
                    Some(boundary) => Ok(vec![wire::encode_fwd(&self.cfg.keys, view.epoch, first_input_pos, no_sample, &boundary)]),
                    None => {
                        let digest = eng.last_logits_digest()?;
                        Ok(vec![wire::encode_applied_ack(&self.cfg.keys, view.epoch, first_input_pos, &digest)])
                    }
                }
            }
            Msg::CommitActivation(t) => Ok(self.step_control(StageEvent::RecvCommit { tuple: t })),
            Msg::FinalizeActivation { attempt } => Ok(self.step_control(StageEvent::RecvFinalize { attempt })),
            Msg::ActivationCommitAbort { aborted_attempt } => Ok(self.step_control(StageEvent::RecvAbort { attempt: aborted_attempt })),
            Msg::BeginRecovery { base, target, recovery_id, truncate_to } => {
                Ok(self.step_control(StageEvent::RecvBegin { base, target, recovery_id, truncate_to }))
            }
            // Acks / errors are coordinator-inbound; a worker never receives them.
            Msg::ActivationCommitted(_) | Msg::ActivationFinalized | Msg::RecoveryAck { .. } | Msg::AppliedAck { .. } | Msg::Err { .. } => {
                Ok(vec![])
            }
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

fn is_eof(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe)
}
