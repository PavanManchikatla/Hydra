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
    /// `BEGIN_RECOVERY{base, target, recovery_id, truncate_to}` — the three-case transition (I11).
    RecvBegin { base: Epoch, target: Epoch, recovery_id: RecoveryId, truncate_to: i64 },
    /// `RESET_RECOVERY_ATTEMPT{target, new_recovery_id, truncate_to}` (I23).
    RecvReset { target: Epoch, new_recovery_id: RecoveryId, truncate_to: i64 },
    /// Catch-up/rebuild toward `goal` (advances `applied`; TLA+ `StageRebuildStep`).
    RebuildStep { goal: i64 },
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
    /// `RECOVERY_ACK` (Case A/B).
    RecoveryAck { rank: StageRank, target: Epoch, recovery_id: RecoveryId },
    /// `ERR_RECOVERY_COMPLETED` (Case B′ — locally-decidable completed activation).
    RecoveryCompleted { rank: StageRank, target: Epoch },
    /// `RESET_ACK`.
    ResetAck { rank: StageRank, recovery_id: RecoveryId },
    /// `READY` after catch-up/rebuild reaches goal.
    Ready { rank: StageRank, recovery_id: RecoveryId, applied: i64 },
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
    /// Applied/KV frontier for this shard (spec §2.3; abstract position).
    applied: i64,
    /// Set if a Case-B replay ever saw `applied > truncate_to` — a fatal I11/I23 violation
    /// (the CaseBPure detector; Mut2's label-only reset trips this).
    caseb_violated: bool,
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
            applied: 0,
            caseb_violated: false,
        }
    }

    /// A `FROZEN` stage at (epoch, recovery_id) with a given applied frontier — the state a
    /// survivor is in as recovery begins.
    pub fn frozen(rank: StageRank, epoch: Epoch, recovery_id: RecoveryId, applied: i64) -> Self {
        Self {
            rank,
            state: StageState::Frozen,
            epoch,
            recovery_id,
            gen: 1,
            attempt: 0,
            highest_attempt: 0,
            final_evidence: false,
            applied,
            caseb_violated: false,
        }
    }

    pub fn applied(&self) -> i64 {
        self.applied
    }
    pub fn caseb_violated(&self) -> bool {
        self.caseb_violated
    }
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }
    pub fn recovery_id(&self) -> RecoveryId {
        self.recovery_id
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
            RecvBegin { base, target, recovery_id: r, truncate_to } => {
                // Case B′: a completed activation is locally decidable — ERR_RECOVERY_COMPLETED.
                if self.state == ActiveFinal && self.epoch == target && self.final_evidence {
                    return vec![StageEffect::RecoveryCompleted { rank: self.rank, target }];
                }
                // Case A: first application at base — freeze, adopt target, truncate (I7a).
                if matches!(self.state, ActiveFinal | Frozen) && self.epoch == base {
                    self.state = Frozen;
                    self.epoch = target;
                    self.recovery_id = r;
                    self.applied = self.applied.min(truncate_to); // truncate applied > truncate_to
                    self.final_evidence = false;
                    return vec![StageEffect::RecoveryAck { rank: self.rank, target, recovery_id: r }];
                }
                // Case B: PURE replay to a FROZEN stage already under this transition.
                if self.state == Frozen && self.epoch == target && r >= self.recovery_id {
                    // Case B asserts applied ≤ truncate_to; legitimate post-catch-up advancement is
                    // handled by RESET, never Case B. If this trips, RESET failed to truncate (I11/I23).
                    if self.applied > truncate_to {
                        self.caseb_violated = true;
                    }
                    self.recovery_id = r;
                    return vec![StageEffect::RecoveryAck { rank: self.rank, target, recovery_id: r }];
                }
                // Case C: invalid transition (→ ERR_TRANSITION on the wire).
                Vec::new()
            }
            RecvReset { target, new_recovery_id: nr, truncate_to } => {
                let acceptable = matches!(self.state, Frozen | Rebuilding | FrozenReady | Preactive)
                    && !self.final_evidence; // PREACTIVE only if no COMPLETE evidence
                if acceptable && self.epoch == target && nr > self.recovery_id {
                    self.state = Frozen;
                    self.recovery_id = nr;
                    self.attempt = 0;
                    // ResetTruncates (Mut2 = FALSE → label-only r-bump, leaving applied > truncate_to).
                    if !cfg!(feature = "mutation_label_reset") {
                        self.applied = self.applied.min(truncate_to);
                    }
                    return vec![StageEffect::ResetAck { rank: self.rank, recovery_id: nr }];
                }
                Vec::new()
            }
            RebuildStep { goal } => {
                if matches!(self.state, Frozen | Rebuilding) {
                    if self.applied < goal {
                        self.state = Rebuilding;
                        self.applied += 1;
                    } else {
                        self.state = FrozenReady;
                        return vec![StageEffect::Ready {
                            rank: self.rank,
                            recovery_id: self.recovery_id,
                            applied: self.applied,
                        }];
                    }
                }
                Vec::new()
            }
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
                self.applied = 0;
                self.final_evidence = false;
                Vec::new()
            }
        }
    }
}
