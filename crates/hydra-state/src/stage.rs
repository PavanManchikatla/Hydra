//! Stage-session state machine (spec §10 STAGE-SESSION), mirroring the TLA+ stage actions
//! `StageRecvCommitAt / StageRecvFinalizeAt / StageRecvAbortAt` and the activation-attempt
//! fencing rule **F2** (a stage rejects any activation control message whose
//! `activation_attempt_id` is below its highest accepted attempt for the (session, epoch)).
//!
//! This slice covers the activation side (`FROZEN_READY → PREACTIVE → ACTIVE_FINAL`, abort back
//! to `FROZEN_READY`, idempotent COMMIT replay). Recovery Cases A/B/B′/C and reset land next.

use crate::{ActivationTuple, AttemptId, Epoch, RecoveryId, StageRank};

/// Per-stage activation state (TLA+ `stState`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StageState {
    ActiveFinal,
    Frozen,
    Rebuilding,
    FrozenReady,
    Preactive,
    Lost,
}

/// Events a stage receives (control plane).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StageEvent {
    /// `COMMIT_ACTIVATION{tuple}` — carries the activation attempt.
    RecvCommit { tuple: ActivationTuple },
    /// `FINALIZE_ACTIVATION` for `attempt`.
    RecvFinalize { attempt: AttemptId },
    /// `ACTIVATION_COMMIT_ABORT` for `attempt`.
    RecvAbort { attempt: AttemptId },
    /// Shard loss: LOST + new stage generation.
    Crash,
}

/// Effects a stage emits (acks back to the coordinator).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StageEffect {
    Committed { rank: StageRank, epoch: Epoch, recovery_id: RecoveryId, attempt: AttemptId },
    Finalized { rank: StageRank, attempt: AttemptId },
    /// F2 rejection (would carry `ERR_FENCED{FenceState}` on the wire).
    Fenced { rank: StageRank, attempt: AttemptId, highest: AttemptId },
}

/// One stage's participation in the activation transaction for a (session, epoch, recovery_id).
#[derive(Clone, Debug)]
pub struct Stage {
    rank: StageRank,
    state: StageState,
    epoch: Epoch,
    recovery_id: RecoveryId,
    gen: u64,
    /// The attempt currently accepted (bound into PREACTIVE/ACTIVE_FINAL).
    attempt: AttemptId,
    /// Highest activation attempt ever accepted for (epoch) — the F2 fence floor.
    highest_attempt: AttemptId,
    final_evidence: bool,
}

impl Stage {
    /// A stage that has finished reconstruction and is `FROZEN_READY` at (epoch, recovery_id).
    pub fn frozen_ready(rank: StageRank, epoch: Epoch, recovery_id: RecoveryId) -> Self {
        Self {
            rank,
            state: StageState::FrozenReady,
            epoch,
            recovery_id,
            gen: 1,
            attempt: 0,
            highest_attempt: 0,
            final_evidence: false,
        }
    }

    pub fn state(&self) -> StageState {
        self.state
    }
    pub fn attempt(&self) -> AttemptId {
        self.attempt
    }
    pub fn highest_attempt(&self) -> AttemptId {
        self.highest_attempt
    }
    pub fn generation(&self) -> u64 {
        self.gen
    }
    pub fn holds_final_evidence(&self) -> bool {
        self.final_evidence
    }

    /// F2 fence: accept an activation-control attempt iff it is not below the highest accepted
    /// (unless the Mut3 mutation disables fencing).
    fn attempt_passes_fence(&self, attempt: AttemptId) -> bool {
        cfg!(feature = "mutation_no_attempt_fence") || attempt >= self.highest_attempt
    }

    pub fn step(&mut self, ev: StageEvent) -> Vec<StageEffect> {
        use StageEvent::*;
        use StageState::*;
        match ev {
            RecvCommit { tuple } => {
                if tuple.epoch != self.epoch || tuple.recovery_id != self.recovery_id {
                    return Vec::new(); // wrong (epoch, recovery_id) — F1/precondition
                }
                if !self.attempt_passes_fence(tuple.attempt) {
                    // F2: fence a stale attempt.
                    return vec![StageEffect::Fenced {
                        rank: self.rank,
                        attempt: tuple.attempt,
                        highest: self.highest_attempt,
                    }];
                }
                match self.state {
                    FrozenReady => {
                        self.state = Preactive;
                        self.attempt = tuple.attempt;
                        self.highest_attempt = self.highest_attempt.max(tuple.attempt);
                    }
                    Preactive if self.attempt == tuple.attempt => {
                        // idempotent replay ⇒ re-ack (I18); no state change.
                    }
                    Preactive => {
                        // a different (fence-passing, i.e. ≥) attempt supersedes the reconstruction
                        self.attempt = tuple.attempt;
                        self.highest_attempt = self.highest_attempt.max(tuple.attempt);
                    }
                    _ => return Vec::new(),
                }
                vec![StageEffect::Committed {
                    rank: self.rank,
                    epoch: self.epoch,
                    recovery_id: self.recovery_id,
                    attempt: tuple.attempt,
                }]
            }
            RecvFinalize { attempt } => {
                if self.state == Preactive
                    && (cfg!(feature = "mutation_no_attempt_fence") || attempt == self.attempt)
                {
                    self.state = ActiveFinal;
                    self.final_evidence = true;
                    return vec![StageEffect::Finalized { rank: self.rank, attempt }];
                }
                Vec::new()
            }
            RecvAbort { attempt } => {
                // abort ⇒ FROZEN_READY, next attempt fence (I21). A finalized stage is never aborted.
                if self.state == Preactive && self.attempt == attempt && !self.final_evidence {
                    self.state = FrozenReady;
                    // fence floor stays: the next attempt must exceed the aborted one.
                    self.highest_attempt = self.highest_attempt.max(attempt);
                }
                Vec::new()
            }
            Crash => {
                self.state = Lost;
                self.gen += 1;
                self.final_evidence = false;
                Vec::new()
            }
        }
    }
}
