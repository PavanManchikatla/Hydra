//! # hydra-state
//!
//! **PURE, synchronous** Hydra state machines (BLUEPRINT §2 architecture rule): inputs are
//! `(state, event)` pairs, outputs are `(state′, effects[])`. **No I/O, no clocks, no
//! randomness** — the only nondeterminism is what arrives in events. All networking, disk, and
//! engine work is done by the binaries executing the emitted [`Effect`]s. Any protocol behavior
//! implemented outside this crate is a defect.
//!
//! This crate mirrors `verification/HydraActivationCore.tla` (the machine-checked transition
//! core) action-for-action, then extends it with the spec's watermark/ledger/data-plane layer.
//! Invariants I1–I25 are checked by [`invariants::check`] over the whole state.
//!
//! **Scope of this module (M1 slice 1):** the coordinator activation transaction (spec §6.6) —
//! `ACTIVATION_COMMIT_INTENT → COMMIT_ACTIVATION → ACTIVATION_COMPLETE → FINALIZE_ACTIVATION →
//! ACTIVE_FINAL`, its abort reversal (I21), and the **I25 abort-finality** guard that TLC-1
//! found. Stage-session machines, recovery Cases A/B/B′/C, reset, unservable/supersession, and
//! the watermark/ledger layer land in subsequent slices.

pub mod coordinator;
pub mod invariants;

pub use coordinator::{Coordinator, CoordEvent, CoordState};

// ---------- identifiers ----------

pub type Epoch = u32;
pub type RecoveryId = u32;
pub type AttemptId = u32;
pub type CompletionId = u64;
pub type StageRank = u16;
pub type CheckpointId = u64;

/// 16-byte session id.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SessionId(pub [u8; 16]);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ActivationKind {
    Initial,
    Recovery,
}

/// The activation tuple (spec §6.6 / `proto.ActivationTuple`), reduced to the fields the
/// transition core fences and commits on. `(epoch, recovery_id, attempt)` is its identity for
/// I25 (abort/complete mutual exclusion).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ActivationTuple {
    pub kind: ActivationKind,
    pub epoch: Epoch,
    pub recovery_id: RecoveryId,
    pub attempt: AttemptId,
    pub sampler_checkpoint_id: CheckpointId,
}

impl ActivationTuple {
    /// The `(epoch, recovery_id, attempt)` identity used by I25.
    pub fn attempt_key(&self) -> (Epoch, RecoveryId, AttemptId) {
        (self.epoch, self.recovery_id, self.attempt)
    }
}

// ---------- effects (executed by the binaries; never performed here) ----------

/// Effect-kind tag — the domain-separation byte in the effect id (WAL-FORMAT §4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u16)]
pub enum EffectKind {
    WriteWal = 1,
    SendMsg = 2,
}

/// Stable effect id (WAL-FORMAT §4): `blake3(session_id || epoch || recovery_id ||
/// attempt || effect_kind || monotonic_seq)` truncated to u64. Identical (state, event) inputs
/// yield identical ids, so the runtime deduplicates effect execution across restarts.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct EffectId(pub u64);

impl EffectId {
    pub fn compute(
        session: SessionId,
        epoch: Epoch,
        recovery_id: RecoveryId,
        attempt: AttemptId,
        kind: EffectKind,
        monotonic_seq: u64,
    ) -> EffectId {
        let mut h = blake3::Hasher::new();
        h.update(&session.0);
        h.update(&epoch.to_le_bytes());
        h.update(&recovery_id.to_le_bytes());
        h.update(&attempt.to_le_bytes());
        h.update(&(kind as u16).to_le_bytes());
        h.update(&monotonic_seq.to_le_bytes());
        EffectId(u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap()))
    }
}

/// Durable coordinator WAL records (subset the transition core writes; WAL-FORMAT §2 registry).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WalRecord {
    ActivationCommitIntent { tuple: ActivationTuple },
    ActivationComplete { tuple: ActivationTuple, completion_id: CompletionId },
    ActivationAbort { epoch: Epoch, recovery_id: RecoveryId, attempt: AttemptId },
    SessionTerminate,
}

/// Control-plane messages the coordinator sends to stages (spec §4).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ControlMsg {
    CommitActivation { tuple: ActivationTuple },
    ActivationCommitAbort { epoch: Epoch, recovery_id: RecoveryId, attempt: AttemptId },
    FinalizeActivation { tuple: ActivationTuple, completion_id: CompletionId },
}

/// An effect emitted by a state machine, to be executed (idempotently, by id) by the runtime.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Effect {
    /// WAL-before-wire: the coordinator must observe this record durable
    /// ([`CoordEvent::WalDurable`]) before acting on it.
    WriteWal { id: EffectId, record: WalRecord },
    Send { id: EffectId, msg: ControlMsg },
}

impl Effect {
    pub fn id(&self) -> EffectId {
        match self {
            Effect::WriteWal { id, .. } | Effect::Send { id, .. } => *id,
        }
    }
}
