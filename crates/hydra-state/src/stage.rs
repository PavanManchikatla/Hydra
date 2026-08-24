//! Stage-session state machine (spec §10 STAGE-SESSION), mirroring the TLA+ stage actions
//! `StageRecvCommitAt / StageRecvFinalizeAt / StageRecvAbortAt` and the activation-attempt
//! fencing rule **F2** (a stage rejects any activation control message whose
//! `activation_attempt_id` is below its highest accepted attempt for the (session, epoch)).
//!
//! This slice covers the activation side (`FROZEN_READY → PREACTIVE → ACTIVE_FINAL`, abort back
//! to `FROZEN_READY`, idempotent COMMIT replay). Recovery Cases A/B/B′/C and reset land next.

use crate::{ActivationKind, ActivationTuple, AttemptId, Epoch, RecoveryId, StageRank};

/// **⛔ V1-BLOCKING ROLLOUT ALLOWANCE — set this to `false` before v1 ships (§8).**
///
/// A pre-H2 encoder writes `complete_record_hash = [0; 32]`, because the field was populated with
/// zeros and dropped at decode. Accepting that value lets a mixed-version cluster finish a rollout
/// without a flag day. **While it is `true`, an all-zero hash is a valid finalize from any peer the
/// role gate admits — which is most of what H2 closed**, so this is a dated allowance and not a
/// design choice.
///
/// It is a **named constant rather than a comment** so that deleting it is a one-line change with a
/// compiler-visible effect, and so the §8 row that tracks it points at something real.
/// `stage.rs::a_finalize_whose_evidence_names_a_different_activation_is_refused` asserts that a
/// **non-zero** mismatch is refused, so this cannot widen into "any hash passes".
pub const ACCEPT_LEGACY_ZERO_COMPLETION_HASH: bool = true;

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
    ///
    /// **Audit H3:** `n_ctx` accompanies the message because the stage must bound `truncate_to`
    /// against *its own* context window before acting on it — see [`Stage::step`]'s `RecvBegin`
    /// arm. It is the stage's fact, never the sender's, so it is not a wire field.
    RecvBegin { base: Epoch, target: Epoch, recovery_id: RecoveryId, truncate_to: i64, n_ctx: i64 },
    /// `RESET_RECOVERY_ATTEMPT{target, new_recovery_id, truncate_to}` (I23).
    RecvReset { target: Epoch, new_recovery_id: RecoveryId, truncate_to: i64 },
    /// Catch-up/rebuild toward `goal` (advances `applied`; TLA+ `StageRebuildStep`).
    RebuildStep { goal: i64 },
    /// `COMMIT_ACTIVATION{tuple}` — carries the activation attempt.
    RecvCommit { tuple: ActivationTuple },
    /// `FINALIZE_ACTIVATION` for `attempt` **in `epoch`** (audit H2 — the epoch is checked; the
    /// TLA+ action `StageRecvFinalizeAt` has always required `m.tgt = stEpoch[s]` and the code did
    /// not, so the implementation was strictly weaker than the model it mirrors).
    /// **Audit H2, second half:** the completion **evidence** travels too, and is checked.
    RecvFinalize { epoch: Epoch, attempt: AttemptId, completion_id: u64, complete_record_hash: [u8; 32] },
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
    /// **Audit M10 — a refusal that SAYS SO.**
    ///
    /// Every arm below that used to `return Vec::new()` was a refusal the sender never heard
    /// about. The spec defines the codes (§4's `ErrCode`) and names the cases (§1.3: an invalid
    /// transition is `ERR_TRANSITION`); the code simply never emitted them, so a refused control
    /// frame produced **silence**.
    ///
    /// **The cost, measured rather than argued:** M4·0's acceptance test **hung** instead of
    /// failing, because a stage refusing a finalize on mismatched completion evidence just did not
    /// reply — and a hang reads as a network fault, so it is debugged as one. The debugging cost of
    /// silence grows with the size of the system; the cost of a reply is one frame.
    Refused { rank: StageRank, code: RefusalCode, detail: &'static str },
}

/// Why a control frame was refused (audit M10). Maps 1:1 onto `hydra-proto`'s `ErrCode`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RefusalCode {
    /// The frame is for a different epoch/attempt than this stage holds (`ERR_FENCED`).
    Fenced,
    /// The transition is not one this stage can take from here (`ERR_TRANSITION`; spec §1.3 Case C).
    Transition,
}

/// A refusal, as an effect (audit M10). A free function so every site reads identically and a new
/// refusal cannot accidentally be written as a bare `Vec::new()`.
fn refused(rank: StageRank, code: RefusalCode, detail: &'static str) -> Vec<StageEffect> {
    vec![StageEffect::Refused { rank, code, detail }]
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
    /// The kind and checkpoint of the tuple this stage is PREACTIVE on, kept so the completion
    /// evidence can be recomputed when the finalize arrives (audit H2).
    tuple_kind: ActivationKind,
    tuple_checkpoint: crate::CheckpointId,
    /// The `completion_id` adopted at finalize (spec §6.6 step 4).
    completion_id: u64,
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
            tuple_kind: ActivationKind::Initial,
            tuple_checkpoint: 0,
            completion_id: 0,
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
            tuple_kind: ActivationKind::Initial,
            tuple_checkpoint: 0,
            completion_id: 0,
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
    /// The `completion_id` adopted at finalize (spec §6.6 step 4; audit H2).
    pub fn completion_id(&self) -> u64 {
        self.completion_id
    }
    pub fn holds_final_evidence(&self) -> bool {
        self.final_evidence
    }

    /// **F2 fence (amended 2026-08-23 — audit H1): the accepted attempt window is
    /// `{highest_accepted, highest_accepted + 1}`, bounded on BOTH sides.**
    ///
    /// It used to be `attempt >= self.highest_attempt` — a one-sided floor, which is
    /// **unforgeable-past but forgeable-future**. A single activation control message carrying a
    /// far-future id (say `u32::MAX`) satisfied it, was accepted, and — because acceptance *adopts*
    /// the value as the new floor (`highest_attempt.max(tuple.attempt)`) — **permanently fenced
    /// every subsequent legitimate attempt.** The session could then never activate, never recover,
    /// and §6.4's bound-exhaustion path terminates it. **A silent, permanent denial of activation in
    /// which the stage behaves exactly as previously specified**, which is why no test caught it:
    /// there was nothing to catch, the spec said this.
    ///
    /// Under the honest-worker assumption (BLUEPRINT §1.9) this is not an attack — and that is
    /// *why it still had to be fixed*: it is squarely an **accident** case, reachable by a corrupted
    /// field that survives the BLAKE3 frame tag, a buggy peer, or a replay from a future epoch. The
    /// assumption excuses malice, never accidents.
    ///
    /// **Why a window of exactly two is sufficient:** spec §6.4 gives the coordinator exactly one
    /// way to advance an attempt — `+1`. A stage that has accepted attempt *n* can therefore only
    /// ever legitimately be offered *n* (an idempotent replay, which §6.6 step 2 *requires* it to
    /// re-ack) or *n+1* (the next retry). No legitimate message carries *n+2*.
    ///
    /// The Mut3 mutation still disables the fence wholesale, so the mutation's designed
    /// counterexample is unchanged — narrowing the window must not silence it, and the CI legs
    /// assert that it still fires.
    fn attempt_passes_fence(&self, attempt: AttemptId) -> bool {
        cfg!(feature = "mutation_no_attempt_fence")
            || attempt == self.highest_attempt
            || attempt == self.highest_attempt.saturating_add(1)
    }

    pub fn step(&mut self, ev: StageEvent) -> Vec<StageEffect> {
        use StageEvent::*;
        use StageState::*;
        match ev {
            RecvBegin { base, target, recovery_id: r, truncate_to, n_ctx } => {
                // **Audit H3 — bounds, before any state is touched.** `BEGIN_RECOVERY` freezes a
                // serving stage and truncates its KV, so a malformed one is destructive even when
                // it is honest: the code checked only `epoch == base` and accepted **any**
                // `target` and **any** `truncate_to`, including negatives — and `truncate_to = -1`
                // (or `i64::MIN`) discards the entire context via `applied.min(truncate_to)`.
                //
                // The model has never been able to express either defect: `SendBeginRecovery`
                // emits `base |-> recTarget - 1, tgt |-> recTarget` by construction, so `tgt =
                // base + 1` holds in every reachable state, and `trunc` ranges over a small
                // non-negative bound. **The implementation was weaker than the model it mirrors**,
                // which is why no mutation could catch it: TLC never generates the frame.
                //
                // Spec §1.3 gives exactly one epoch relation (`base e → target e+1`) and §2.3's
                // positions are non-negative and bounded by the context window. Both are checked
                // here and the message is dropped (→ `ERR_TRANSITION` on the wire, Case C), never
                // clamped: a clamp would let a wrong frame produce a *plausible* truncation.
                if target != base.saturating_add(1) {
                    return refused(self.rank, RefusalCode::Transition, "BEGIN_RECOVERY target must be base + 1");
                }
                if truncate_to < 0 || truncate_to >= n_ctx {
                    return refused(self.rank, RefusalCode::Transition, "BEGIN_RECOVERY truncate_to outside [0, n_ctx)");
                }
                // Case B′: a completed activation is locally decidable — ERR_RECOVERY_COMPLETED.
                if self.state == ActiveFinal && self.epoch == target && self.final_evidence {
                    return vec![StageEffect::RecoveryCompleted { rank: self.rank, target }];
                }
                // Case A: first application at base — freeze, adopt target, truncate (I7a).
                // PREACTIVE is included per spec §1.3 ("a stage in PREACTIVE receiving
                // BEGIN_RECOVERY for the next attempt/epoch treats it per the abort rule:
                // PREACTIVE is reversible"): a still-preactive stage reverts (discard the preactive
                // tuple, freeze at target, adopt r), so a post-supersession stage is never marooned
                // (F-LIVENESS-FAIR family 3; the model carried the same gap).
                if matches!(self.state, ActiveFinal | Frozen | Preactive) && self.epoch == base {
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
                    // Audit M10: a fenced commit now says so instead of vanishing.
                    return refused(self.rank, RefusalCode::Fenced, "COMMIT_ACTIVATION for another (epoch, recovery_id)");
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
                        self.tuple_kind = tuple.kind;
                        self.tuple_checkpoint = tuple.sampler_checkpoint_id;
                        self.attempt = tuple.attempt;
                        self.highest_attempt = self.highest_attempt.max(tuple.attempt);
                    }
                    Preactive if self.attempt == tuple.attempt => {
                        // idempotent replay ⇒ re-ack (I18); no state change.
                    }
                    Preactive => {
                        // a different (fence-passing, i.e. ≥) attempt supersedes the reconstruction
                        self.tuple_kind = tuple.kind;
                        self.tuple_checkpoint = tuple.sampler_checkpoint_id;
                        self.attempt = tuple.attempt;
                        self.highest_attempt = self.highest_attempt.max(tuple.attempt);
                    }
                    _ => return refused(self.rank, RefusalCode::Transition, "COMMIT_ACTIVATION in a state that cannot accept it"),
                }
                vec![StageEffect::Committed {
                    rank: self.rank,
                    epoch: self.epoch,
                    recovery_id: self.recovery_id,
                    attempt: tuple.attempt,
                }]
            }
            RecvFinalize { epoch, attempt, completion_id, complete_record_hash } => {
                // **Audit H2 — the epoch check the model always had.** `StageRecvFinalizeAt`
                // requires `m.tgt = stEpoch[s]`; the code checked only `state == Preactive &&
                // attempt == self.attempt`. Post-C2 a *forged* finalize is closed by the role
                // gate, but **a legitimate coordinator's stale finalize still lands**: an
                // in-flight `FINALIZE_ACTIVATION` from epoch e, delivered after the stage has
                // moved to e+1 under a recovery, would finalize the new epoch's preactive tuple on
                // the old epoch's evidence — a stage-side I25 violation and a later Case-B′ fatal
                // audit event. The attempt id does not save it: attempt space is per (session,
                // epoch), so the same id recurs in the next epoch.
                if epoch != self.epoch {
                    return refused(self.rank, RefusalCode::Fenced, "FINALIZE_ACTIVATION for another epoch");
                }
                // **Audit H2's OTHER half — the evidence is now CHECKED, not merely carried.**
                //
                // `completion_id` and `complete_record_hash` were dropped at decode because
                // nothing produced real ones: there was no coordinator writing an
                // `ACTIVATION_COMPLETE` record for them to refer to. With M4·0's driver both
                // exist, and the hash is derived from the **tuple** — which this stage is already
                // PREACTIVE on — so it can be recomputed here and compared. No second source of
                // truth, and the stage does not need the coordinator's WAL.
                //
                // It answers one question: *"is this finalize about the activation I committed
                // to?"* The epoch check above catches a stale finalize from another epoch; this
                // catches one that matches on epoch and attempt but names a different tuple.
                let expected = ActivationTuple {
                    kind: self.tuple_kind,
                    epoch: self.epoch,
                    recovery_id: self.recovery_id,
                    attempt: self.attempt,
                    sampler_checkpoint_id: self.tuple_checkpoint,
                }
                .completion_hash();
                let evidence_ok = complete_record_hash == expected
                    || (ACCEPT_LEGACY_ZERO_COMPLETION_HASH && complete_record_hash == [0u8; 32]);
                if self.state == Preactive
                    && (cfg!(feature = "mutation_no_attempt_fence") || attempt == self.attempt)
                    && evidence_ok
                {
                    self.state = ActiveFinal;
                    self.final_evidence = true;
                    self.completion_id = completion_id;
                    return vec![StageEffect::Finalized { rank: self.rank, attempt }];
                }
                // Audit M10 — and this is the exact refusal that made M4·0's acceptance test HANG
                // rather than fail. It is a message now.
                refused(self.rank, RefusalCode::Fenced, "FINALIZE_ACTIVATION: wrong attempt, state, or completion evidence")
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
