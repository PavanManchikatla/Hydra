//! Coordinator activation-transaction state machine (spec §6.6), mirroring the TLA+ actions
//! `CoordWriteIntent / CoordSendCommit / StageRecvCommit(ack) / CoordAbortActivation /
//! CoordWriteComplete / CoordSendFinalize / CoordBecomeServiceable / CoordCrash / CoordRestart`.
//!
//! WAL-before-wire is explicit: a WAL write is an [`Effect::WriteWal`]; the coordinator does not
//! act on the record until it observes [`CoordEvent::WalDurable`]. Because the write, its
//! durability, and the subsequent send are distinct steps, every "decided but not yet told"
//! crash window is reachable — exactly the window TLC-1 exploited.

use std::collections::BTreeSet;

use crate::{
    ActivationKind, ActivationTuple, CheckpointId, CompletionId, ControlMsg, Effect, EffectId,
    EffectKind, Epoch, RecoveryId, SessionId, StageRank, WalRecord,
};

/// Which durable write completed (carried on `WalDurable`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WalKindTag {
    Intent,
    Complete,
    Abort,
    /// **Audit M6 (2026-08-23).** `ACTIVATION_UNSERVABLE` used to be recorded *atomically* — the
    /// SM pushed it into its durable WAL in the same step that emitted the write effect, i.e. it
    /// treated the write as instantly durable. Spec §6.7 step 1 says *"C **fsyncs**
    /// ACTIVATION_UNSERVABLE"* and §1.2 lists it under WAL-before-wire, so the atomic form was the
    /// **code and the model agreeing with each other and not with the spec**. It now goes through
    /// `WalDurable` like every other durable decision, which makes the crash window between write
    /// and durability reachable — the window §6.5's restart classification depends on.
    Unservable,
    /// `SESSION_TERMINATE`, on the same footing (spec §1.2).
    Terminate,
    /// `BEGIN_RECOVERY` (M4·0b). §6.5's classifier reads this record, so it must be durable
    /// before the message goes out — the same WAL-before-wire discipline as the intent.
    BeginRecovery,
    /// `RESET_RECOVERY_ATTEMPT` (M4·0b; audit M13's sender half).
    Reset,
}

/// Coordinator activation state (subset of TLA+ `cState` for the transition-core slice).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CoordState {
    /// `BEGIN_RECOVERY` written, awaiting its fdatasync (M4·0b).
    RecoveryStartedPending,
    /// `RESET_RECOVERY_ATTEMPT` written, awaiting its fdatasync (M4·0b).
    ResetPending,
    /// `SESSION_TERMINATE` written, awaiting its fdatasync (M4·0b).
    TerminatePending,
    /// **M4·0b — `BEGIN_RECOVERY` is durable and not yet sent** (TLA+ `RECOVERY_STARTED`).
    ///
    /// The state a coordinator is in between deciding to recover and telling anyone. It is a real
    /// crash window and §6.5 classifies it from the durable `BEGIN` record.
    RecoveryStarted,
    /// Recovery/reconstruction in progress (stages catching up); INITIAL starts here.
    Reconstructing,
    /// All stages `FROZEN_READY`; ready to attempt activation.
    ReadyAll,
    /// `ACTIVATION_COMMIT_INTENT` written, awaiting its fdatasync.
    IntentPending,
    /// Intent durable; `COMMIT_ACTIVATION` not yet sent (crash window).
    IntentDurable,
    /// `COMMIT_ACTIVATION` sent; collecting `ACTIVATION_COMMITTED` acks.
    Committing,
    /// `ACTIVATION_COMMIT_ABORT` written, awaiting its fdatasync.
    AbortPending,
    /// `ACTIVATION_COMPLETE` written, awaiting its fdatasync (the irrevocable decision).
    CompletePending,
    /// Complete durable; `FINALIZE_ACTIVATION` not yet sent.
    ActivationComplete,
    /// `FINALIZE_ACTIVATION` sent; collecting `ACTIVATION_FINALIZED` acks.
    Finalizing,
    /// Finalized everywhere; data plane may serve (I16/I20).
    Serviceable,
    /// `ACTIVATION_UNSERVABLE` written, awaiting its fdatasync (audit M6).
    ///
    /// **TLA+ correspondence (BLUEPRINT §4.3), and why the model does not change:** the model's
    /// `Wal(r) == wal' = wal ∪ {r}` makes every write **atomically durable**, i.e. its `wal` *is*
    /// the durable set — a record is in it iff it survived. `CoordRecordUnservable` is therefore
    /// one atomic action, and a crash "in the write→fdatasync window" is indistinguishable in the
    /// model from a crash *before* the action: the record is simply absent, which is what a real
    /// disk gives you too. The defect was that the **code violated that abstraction** by putting
    /// the record into its own durable set before the fdatasync returned. This state maps onto the
    /// model's *pre-action* state (`ACTIVATION_COMPLETE`/`FINALIZING`), exactly as `IntentPending`
    /// and `CompletePending` already do, and the model action is taken at `WalDurable`. **The model
    /// semantics do not move, so per rule 11 no full TLC rerun is owed — this is the code
    /// conforming to the model, the same shape as H2/H3.** A crash here loses the
    /// record — which is precisely why §6.5 classifies a restart with no durable UNSERVABLE as
    /// *still finalizing*, and why the record must be durable before the transition is taken.
    UnservablePending,
    /// `ACTIVATION_UNSERVABLE` **durable**: the decision stands but is not served (§6.7).
    Superseding,
    Crashed,
    Terminal,
}

/// Events driving the coordinator. `Proceed*` are the coordinator's own spontaneous actions
/// (enabled TLA+ actions); the simulator schedules them like TLC picks enabled transitions.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CoordEvent {
    /// All stages reached `FROZEN_READY`.
    StagesReconstructed,
    ProceedWriteIntent,
    WalDurable(WalKindTag),
    ProceedSendCommit,
    /// Audit H4: the rank must come from an authenticated peer identity — see
    /// [`crate::AuthenticatedRank`]. The wire carries no rank, so a `StageRank` here would be a
    /// value the coordinator invented or the sender implied.
    StageCommitted { rank: crate::AuthenticatedRank, attempt: crate::AttemptId },
    ProceedAbort,
    ProceedWriteComplete,
    ProceedSendFinalize,
    /// Audit H4: authenticated rank (see [`crate::AuthenticatedRank`]).
    StageFinalized { rank: crate::AuthenticatedRank, attempt: crate::AttemptId },
    ProceedBecomeServiceable,
    /// A required participant is permanently lost (new stage_generation / removal).
    /// Audit H4: authenticated rank (see [`crate::AuthenticatedRank`]).
    StageLost { rank: crate::AuthenticatedRank },
    /// §6.7: record ACTIVATION_UNSERVABLE for a decided-but-unservable activation.
    ProceedRecordUnservable,
    /// §6.7 step 3: open the superseding recovery at epoch+1.
    ProceedStartSuperseding,
    /// **M4·0b — begin a semantic recovery** (TLA+ `CoordBeginRecovery`): a participant was lost
    /// while the session was serviceable. Writes the durable `BEGIN_RECOVERY` record.
    ProceedBeginRecovery { truncate_to: i64 },
    /// **M4·0b — send the `BEGIN_RECOVERY`** the record already committed to (TLA+
    /// `SendBeginRecovery`). Separate from the write, because that is where the crash window is.
    ProceedSendBeginRecovery,
    /// **M4·0b — restart the reconstruction attempt** (spec §6.4; TLA+ `CoordResetAttempt`).
    ProceedResetAttempt { truncate_to: i64 },
    /// **M4·0b — terminate the session** (spec §1.2/§11): the `SESSION_TERMINATE` path, which had a
    /// durable record and a `WalKindTag` and nothing that ever emitted one.
    ProceedTerminate,
    Crash,
    Restart,
}

/// The coordinator's activation transaction for one (session, epoch, recovery_id).
#[derive(Clone, Debug)]
pub struct Coordinator {
    session: SessionId,
    n_stages: u16,
    kind: ActivationKind,
    epoch: Epoch,
    recovery_id: RecoveryId,
    checkpoint: CheckpointId,

    state: CoordState,
    attempt: crate::AttemptId, // last-used activation attempt for (epoch, recovery_id)
    tuple: Option<ActivationTuple>,
    committed: BTreeSet<StageRank>,
    finalized: BTreeSet<StageRank>,
    /// Participants permanently lost after the durable decision (drives §6.7).
    lost: BTreeSet<StageRank>,
    next_completion_id: CompletionId,

    /// Durable coordinator WAL (the coordinator's persistent truth).
    wal: Vec<WalRecord>,
    /// The recovery whose `BEGIN` record is in flight (M4·0b): `(base, target, truncate_to, recovery_id)`.
    /// The recovery id is `0` for a recovery opened from SERVICEABLE and `derived + 1` for one a
    /// restart fences forward with (spec §6.5a, 2026-09-02).
    pending_recovery: Option<(Epoch, Epoch, i64, RecoveryId)>,
    /// The reset whose record is in flight (M4·0b).
    pending_reset: Option<(Epoch, RecoveryId, RecoveryId, i64)>,
    /// The commit stream's witness that the completed activation has served (spec §6.5a
    /// refinement): set only by `restart_from`; a served activation's crash fences forward.
    served: bool,
    /// Per-(session, epoch) monotonic counter owned by the SM, feeding effect ids (WAL-FORMAT §4).
    monotonic_seq: u64,
}

impl Coordinator {
    /// **A coordinator PROCESS that restarted (spec §6.5a, 2026-09-02): everything it knows is
    /// `records`, the durable control log.** Construct, then feed `CoordEvent::Restart`: the
    /// derivation and the fence-forward live in [`Self::restart`], mirroring the model's
    /// `CoordRestart`, so no decision is taken by whoever calls this (rule 21: the placement was
    /// reachable — `hydra-state` owns it). Until `Restart` is fed the volatile fields are the
    /// model's ⊥ and must not be read.
    /// `served`: the commit stream's witness that the completed activation has already served (a
    /// generation commit exists). Spec §6.5a refinement (2026-09-03): a durable COMPLETE re-enters
    /// finalization only while the activation has NOT served; a crash after service is outside any
    /// transaction and fences forward — the stages' data-plane tail beyond the durable frontier
    /// needs the BEGIN's truncation, which no re-entered finalization performs.
    pub fn restart_from(session: SessionId, n_stages: u16, checkpoint: CheckpointId, records: Vec<WalRecord>, served: bool) -> Self {
        let mut c = Self::new_initial(session, n_stages, checkpoint);
        c.served = served;
        c.wal = records;
        c.state = CoordState::Crashed;
        c
    }

    /// A session admitted with an INITIAL activation pending at epoch 0 (TLA+ `Init`).
    pub fn new_initial(session: SessionId, n_stages: u16, checkpoint: CheckpointId) -> Self {
        Self {
            session,
            n_stages,
            kind: ActivationKind::Initial,
            epoch: 0,
            recovery_id: 0,
            checkpoint,
            state: CoordState::Reconstructing,
            attempt: 0,
            tuple: None,
            committed: BTreeSet::new(),
            finalized: BTreeSet::new(),
            lost: BTreeSet::new(),
            next_completion_id: 1,
            pending_recovery: None,
            pending_reset: None,
            served: false,
            wal: Vec::new(),
            monotonic_seq: 0,
        }
    }

    pub fn state(&self) -> CoordState {
        self.state
    }
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }
    /// **Test/driver seam: enter the state a coordinator is in when a session was already
    /// serving.** A recovery begins from SERVICEABLE (TLA+ `CoordBeginRecovery`), and a freshly
    /// constructed `Coordinator` has not been through an activation. Named for what it does so a
    /// call site outside a recovery entry reads as the mistake it would be.
    pub fn force_state_for_recovery_entry(&mut self) {
        self.state = CoordState::Serviceable;
    }

    /// The reconstruction attempt this coordinator is on (spec §6.4; M4·0b).
    pub fn recovery_id(&self) -> crate::RecoveryId {
        self.recovery_id
    }

    pub fn attempt(&self) -> crate::AttemptId {
        self.attempt
    }
    pub fn wal(&self) -> &[WalRecord] {
        &self.wal
    }
    /// A durable COMPLETE exists for the **current epoch** (a superseding recovery advances the
    /// epoch, so the predecessor's COMPLETE must not leak into the new transaction).
    pub fn completed(&self) -> bool {
        self.wal.iter().any(
            |r| matches!(r, WalRecord::ActivationComplete { tuple, .. } if tuple.epoch == self.epoch),
        )
    }

    /// True iff a durable ABORT exists for `(epoch, recovery_id, attempt)` (I25 predicate).
    pub fn attempt_aborted(&self, attempt: crate::AttemptId) -> bool {
        self.wal.iter().any(|r| {
            matches!(r, WalRecord::ActivationAbort { epoch, recovery_id, attempt: a }
                if *epoch == self.epoch && *recovery_id == self.recovery_id && *a == attempt)
        })
    }

    fn next_effect_id(&mut self, kind: EffectKind) -> EffectId {
        let seq = self.monotonic_seq;
        self.monotonic_seq += 1;
        EffectId::compute(self.session, self.epoch, self.recovery_id, self.attempt, kind, seq)
    }

    fn all_committed(&self) -> bool {
        self.committed.len() as u16 == self.n_stages
    }
    fn all_finalized(&self) -> bool {
        self.finalized.len() as u16 == self.n_stages
    }

    /// Whether a spontaneous `Proceed*` event is enabled now (so a scheduler only fires enabled
    /// actions, like TLC's `Next`). External events (`WalDurable`, `Stage*`, `Crash`, `Restart`)
    /// are always deliverable.
    pub fn enabled(&self, ev: &CoordEvent) -> bool {
        use CoordEvent::*;
        match ev {
            StagesReconstructed => self.state == CoordState::Reconstructing,
            ProceedWriteIntent => self.state == CoordState::ReadyAll,
            ProceedSendCommit => self.state == CoordState::IntentDurable,
            ProceedAbort => self.state == CoordState::Committing && !self.completed(),
            ProceedWriteComplete => {
                self.state == CoordState::Committing
                    && self.all_committed()
                    && !self.completed()
                    && (cfg!(feature = "mutation_no_abort_finality")
                        || !self.attempt_aborted(self.attempt)) // I25 / TLC-1 guard
            }
            ProceedSendFinalize => self.state == CoordState::ActivationComplete,
            ProceedBecomeServiceable => self.state == CoordState::Finalizing && self.all_finalized(),
            ProceedRecordUnservable => {
                // §6.7: a durable decision, a required participant lost, not yet all finalized —
                // supersede instead of blocking. Mut1 removes this recourse.
                cfg!(not(feature = "mutation_no_unservable"))
                    && matches!(self.state, CoordState::ActivationComplete | CoordState::Finalizing)
                    && self.completed()
                    && !self.all_finalized()
                    && !self.lost.is_empty()
            }
            ProceedStartSuperseding => self.state == CoordState::Superseding,
            // TLA+ `CoordBeginRecovery`: serviceable, and a participant is lost — a reason to
            // recover. The guard is the model's, not an invention here.
            ProceedBeginRecovery { .. } => self.state == CoordState::Serviceable && !self.lost.is_empty(),
            ProceedSendBeginRecovery => self.state == CoordState::RecoveryStarted,
            // TLA+ `CoordResetAttempt`: only before the decision is durable. After it, §6.7 governs
            // and a reset is forbidden — the same boundary I25 draws for aborts.
            ProceedResetAttempt { .. } => {
                !self.completed()
                    && matches!(
                        self.state,
                        CoordState::Reconstructing | CoordState::ReadyAll | CoordState::IntentDurable | CoordState::Committing
                    )
            }
            ProceedTerminate => !matches!(self.state, CoordState::Terminal),
            // M6: while the UNSERVABLE record is in flight the coordinator is committed to the
            // decision but must not act on it, exactly like IntentPending/CompletePending.
            // external events: deliverable in any live (non-crashed/terminal) state
            _ => !matches!(self.state, CoordState::Terminal),
        }
    }

    /// **Post-decision liveness (I22 / Mut1 detector):** a durable decision with a permanently
    /// lost participant that can neither finalize nor supersede is a stuck state — the
    /// `PostDecisionLoss` liveness hole. With the unservable path present this never holds
    /// (`ProceedRecordUnservable` is enabled); Mut1 removes it and this fires. The simulator uses
    /// it as a deadlock watchdog.
    pub fn post_decision_deadlock(&self) -> bool {
        self.completed()
            && !self.all_finalized()
            && !self.lost.is_empty()
            && matches!(self.state, CoordState::Finalizing)
            && !self.enabled(&CoordEvent::ProceedRecordUnservable)
            && !self.enabled(&CoordEvent::ProceedStartSuperseding)
    }

    /// Apply one event. Disabled events are no-ops (return no effects), mirroring TLC firing only
    /// enabled actions. Never performs I/O.
    pub fn step(&mut self, ev: CoordEvent) -> Vec<Effect> {
        use CoordEvent::*;
        if !self.enabled(&ev) {
            return Vec::new();
        }
        match ev {
            StagesReconstructed => {
                self.state = CoordState::ReadyAll;
                Vec::new()
            }
            ProceedWriteIntent => {
                self.attempt += 1;
                self.committed.clear();
                self.finalized.clear();
                let tuple = ActivationTuple {
                    kind: self.kind,
                    epoch: self.epoch,
                    recovery_id: self.recovery_id,
                    attempt: self.attempt,
                    sampler_checkpoint_id: self.checkpoint,
                };
                self.tuple = Some(tuple.clone());
                let id = self.next_effect_id(EffectKind::WriteWal);
                self.state = CoordState::IntentPending;
                vec![Effect::WriteWal { id, record: WalRecord::ActivationCommitIntent { tuple } }]
            }
            WalDurable(tag) => self.on_wal_durable(tag),
            ProceedSendCommit => {
                let id = self.next_effect_id(EffectKind::SendMsg);
                let tuple = self.tuple.clone().expect("tuple set by intent");
                self.state = CoordState::Committing;
                vec![Effect::Send { id, msg: ControlMsg::CommitActivation { tuple } }]
            }
            StageCommitted { rank, attempt } => {
                // count only acks for the CURRENT attempt (stale pre-abort acks never count — I25)
                if attempt == self.attempt && self.state == CoordState::Committing {
                    self.committed.insert(rank.rank());
                }
                Vec::new()
            }
            ProceedAbort => {
                let wal_id = self.next_effect_id(EffectKind::WriteWal);
                let send_id = self.next_effect_id(EffectKind::SendMsg);
                let (e, r, a) = (self.epoch, self.recovery_id, self.attempt);
                self.state = CoordState::AbortPending;
                vec![
                    Effect::WriteWal {
                        id: wal_id,
                        record: WalRecord::ActivationAbort { epoch: e, recovery_id: r, attempt: a },
                    },
                    Effect::Send {
                        id: send_id,
                        msg: ControlMsg::ActivationCommitAbort { epoch: e, recovery_id: r, attempt: a },
                    },
                ]
            }
            ProceedWriteComplete => {
                let id = self.next_effect_id(EffectKind::WriteWal);
                let tuple = self.tuple.clone().expect("tuple set by intent");
                let completion_id = self.next_completion_id;
                self.state = CoordState::CompletePending;
                vec![Effect::WriteWal {
                    id,
                    record: WalRecord::ActivationComplete { tuple, completion_id },
                }]
            }
            ProceedSendFinalize => {
                let id = self.next_effect_id(EffectKind::SendMsg);
                let tuple = self.tuple.clone().expect("tuple set by intent");
                let completion_id = self.completion_id().expect("complete durable");
                self.state = CoordState::Finalizing;
                vec![Effect::Send { id, msg: ControlMsg::FinalizeActivation { tuple, completion_id } }]
            }
            StageFinalized { rank, attempt } => {
                if attempt == self.attempt && self.state == CoordState::Finalizing {
                    self.finalized.insert(rank.rank());
                }
                Vec::new()
            }
            ProceedBecomeServiceable => {
                self.state = CoordState::Serviceable;
                Vec::new()
            }
            StageLost { rank } => {
                self.lost.insert(rank.rank());
                self.finalized.remove(&rank.rank()); // a lost participant's finalize evidence is gone
                Vec::new()
            }
            ProceedRecordUnservable => {
                let id = self.next_effect_id(EffectKind::WriteWal);
                let completion_id = self.completion_id().unwrap_or(0);
                // F-UNSERVABLE: record the ACTIVATION_UNSERVABLE fact in the durable WAL *and*
                // transition, atomically — mirroring TLA+ `CoordRecordUnservable` (Wal(UNSERVABLE)
                // ∧ unservable'=TRUE ∧ cState'=SUPERSEDING). Without the durable record, a restart
                // — which derives everything from the durable log (spec §6.5a; `restart()`) and
                // evaluates the superseding branch *before* the COMPLETE branch — can never
                // classify SUPERSEDING, so a crash in the window before the superseding
                // BEGIN_RECOVERY would restart into finalization and reopen the I22 hole. This
                // durability was missing; the WAL effect alone was not enough.
                // Mut5 (`mutation_unservable_restart`) reintroduces exactly that omission: the
                // WriteWal effect below is still emitted (a real disk / the sim's virtual WAL records
                // it), but `self.wal` does not — so restart misclassifies. The sim re-finds it via
                // the WAL-codec cross-check (monotone-mutation rule).
                // **Audit M6: the transition now waits for durability.** This used to push the
                // record into `self.wal` (the SM's durable truth) and enter SUPERSEDING in the same
                // step as emitting the write — treating the fdatasync as instantaneous, and making
                // the crash window between them unreachable in both the code and the sim (whose
                // hard-coded fsync mirrored the same assumption). The record is now applied in
                // `on_wal_durable`, so a crash in the window leaves NO durable UNSERVABLE, and
                // §6.5's restart classification correctly reports "still finalizing" — which is
                // what the spec's "C fsyncs ACTIVATION_UNSERVABLE" always implied.
                self.state = CoordState::UnservablePending;
                vec![Effect::WriteWal {
                    id,
                    record: WalRecord::ActivationUnservable { completion_id },
                }]
            }
            ProceedStartSuperseding => {
                // §6.7 step 3: open a superseding recovery at epoch+1 (base = completed epoch),
                // restoring an enabled transition (I22). Reachable survivors take Case A normally.
                //
                // **M4·0b — this now WRITES THE `BEGIN_RECOVERY` RECORD, which the model always
                // did** (`CoordStartSuperseding` is `Wal([t |-> "BEGIN", ...])` ∧ the transition).
                // The code advanced the epoch and told nobody and recorded nothing, so a crash in
                // the superseding window left §6.5 with no `BEGIN` to classify from — the code was
                // weaker than the model it mirrors, in the one place I22 depends on.
                let id = self.next_effect_id(EffectKind::WriteWal);
                let (base, target) = (self.epoch, self.epoch + 1);
                self.pending_recovery = Some((base, target, 0, 0));
                self.state = CoordState::RecoveryStartedPending;
                vec![Effect::WriteWal {
                    id,
                    record: WalRecord::BeginRecovery { base, target, recovery_id: 0, truncate_to: 0 },
                }]
            }
            ProceedBeginRecovery { truncate_to } => {
                // TLA+ `CoordBeginRecovery`. WAL-before-wire: the record is written here and the
                // message is sent by `ProceedSendBeginRecovery`, **after** `WalDurable` — so a
                // crash between them leaves a durable BEGIN that §6.5 classifies, which is the
                // entire reason the record exists.
                let id = self.next_effect_id(EffectKind::WriteWal);
                let (base, target) = (self.epoch, self.epoch + 1);
                self.state = CoordState::RecoveryStartedPending;
                self.pending_recovery = Some((base, target, truncate_to, 0));
                vec![Effect::WriteWal {
                    id,
                    record: WalRecord::BeginRecovery { base, target, recovery_id: 0, truncate_to },
                }]
            }
            ProceedSendBeginRecovery => {
                let id = self.next_effect_id(EffectKind::SendMsg);
                let (base, target, truncate_to, _rid) = self.pending_recovery.expect("RECOVERY_STARTED implies a pending recovery");
                self.state = CoordState::Reconstructing;
                vec![Effect::Send {
                    id,
                    msg: ControlMsg::BeginRecovery { base, target, recovery_id: self.recovery_id, truncate_to },
                }]
            }
            ProceedResetAttempt { truncate_to } => {
                // TLA+ `CoordResetAttempt`: WAL *and* wire in one action in the model, which is
                // sound there because the model's `Wal` is atomically durable. Here the record is
                // written and the send follows on `WalDurable`, for the same reason as the intent.
                let id = self.next_effect_id(EffectKind::WriteWal);
                let (old_r, new_r) = (self.recovery_id, self.recovery_id + 1);
                self.pending_reset = Some((self.epoch, old_r, new_r, truncate_to));
                self.state = CoordState::ResetPending;
                vec![Effect::WriteWal {
                    id,
                    record: WalRecord::ResetRecoveryAttempt { target: self.epoch, old_recovery_id: old_r, new_recovery_id: new_r, truncate_to },
                }]
            }
            ProceedTerminate => {
                let id = self.next_effect_id(EffectKind::WriteWal);
                self.state = CoordState::TerminatePending;
                vec![Effect::WriteWal { id, record: WalRecord::SessionTerminate }]
            }
            Crash => {
                self.state = CoordState::Crashed;
                Vec::new()
            }
            Restart => self.restart(),
        }
    }

    fn completion_id(&self) -> Option<CompletionId> {
        self.wal.iter().find_map(|r| match r {
            WalRecord::ActivationComplete { tuple, completion_id } if tuple.epoch == self.epoch => {
                Some(*completion_id)
            }
            _ => None,
        })
    }

    fn on_wal_durable(&mut self, tag: WalKindTag) -> Vec<Effect> {
        match tag {
            WalKindTag::Intent => {
                if self.state == CoordState::IntentPending {
                    let tuple = self.tuple.clone().expect("tuple");
                    self.wal.push(WalRecord::ActivationCommitIntent { tuple });
                    self.state = CoordState::IntentDurable;
                }
            }
            WalKindTag::Complete => {
                if self.state == CoordState::CompletePending {
                    // TLC-1 / I25: never write COMPLETE for an attempt with a durable ABORT.
                    debug_assert!(
                        cfg!(feature = "mutation_no_abort_finality") || !self.attempt_aborted(self.attempt),
                        "I25 violated: COMPLETE for aborted attempt"
                    );
                    let tuple = self.tuple.clone().expect("tuple");
                    let completion_id = self.next_completion_id;
                    self.next_completion_id += 1;
                    self.wal.push(WalRecord::ActivationComplete { tuple, completion_id });
                    self.state = CoordState::ActivationComplete;
                }
            }
            WalKindTag::Unservable => {
                if self.state == CoordState::UnservablePending {
                    let completion_id = self.completion_id().unwrap_or(0);
                    // Mut5 (`mutation_unservable_restart`) reintroduces the original omission: the
                    // WriteWal effect was emitted (a real disk / the sim's virtual WAL records it)
                    // but `self.wal` does not, so a restart misclassifies. The sim re-finds it via
                    // the WAL-codec cross-check (monotone-mutation rule).
                    #[cfg(not(feature = "mutation_unservable_restart"))]
                    self.wal.push(WalRecord::ActivationUnservable { completion_id });
                    let _ = completion_id;
                    self.state = CoordState::Superseding;
                }
            }
            WalKindTag::BeginRecovery => {
                if self.state == CoordState::RecoveryStartedPending {
                    let (_base, target, _t, rid) = self.pending_recovery.expect("pending recovery");
                    // The model's post-state, applied only once the record is durable.
                    self.epoch = target;
                    self.recovery_id = rid;
                    self.attempt = 0;
                    self.kind = ActivationKind::Recovery;
                    self.tuple = None;
                    self.committed.clear();
                    self.finalized.clear();
                    self.lost.clear();
                    self.wal.push(WalRecord::BeginRecovery { base: target - 1, target, recovery_id: rid, truncate_to: _t });
                    self.state = CoordState::RecoveryStarted;
                }
            }
            WalKindTag::Reset => {
                if self.state == CoordState::ResetPending {
                    let (target, old_r, new_r, truncate_to) = self.pending_reset.expect("pending reset");
                    self.recovery_id = new_r;
                    self.attempt = 0;
                    self.committed.clear();
                    self.finalized.clear();
                    self.wal.push(WalRecord::ResetRecoveryAttempt { target, old_recovery_id: old_r, new_recovery_id: new_r, truncate_to });
                    self.state = CoordState::Reconstructing;
                    // The RESET message follows the durable record (spec §6.4).
                    let id = self.next_effect_id(EffectKind::SendMsg);
                    return vec![Effect::Send {
                        id,
                        msg: ControlMsg::ResetRecoveryAttempt { target, new_recovery_id: new_r, truncate_to },
                    }];
                }
            }
            WalKindTag::Terminate => {
                self.wal.push(WalRecord::SessionTerminate);
                // `SESSION_TERMINATE` is durable-then-terminal for the same reason (spec §1.2).
                //
                // **Stated honestly: nothing in this SM emits that record yet** — the tag exists
                // so the sim's WAL interpretation is total and so the transition is written down
                // where the rest of the durability choreography lives, rather than being invented
                // later by whoever first needs it. It is unreached code, and calling it "covered"
                // because it compiles would be exactly the over-promise §7.31 is about.
                self.state = CoordState::Terminal;
            }
            WalKindTag::Abort => {
                if self.state == CoordState::AbortPending {
                    let (e, r, a) = (self.epoch, self.recovery_id, self.attempt);
                    self.wal.push(WalRecord::ActivationAbort { epoch: e, recovery_id: r, attempt: a });
                    // abort ⇒ FROZEN_READY, retry activation at attempt+1, same recovery_id (I21)
                    self.committed.clear();
                    self.state = CoordState::ReadyAll;
                }
            }
        }
        Vec::new()
    }

    /// **Restart (spec §6.5 + §6.5a, 2026-09-02): derive from the durable WAL, classify, fence
    /// forward.** Mirrors the model's `CoordRestart` branch for branch:
    ///
    /// * `dTarget`  = max `target` over durable `BEGIN_RECOVERY` records (0 if none — INITIAL);
    /// * `dRId`     = max `recovery_id` over BEGINs / `new_recovery_id` over RESETs at `dTarget`;
    /// * `dAttempt` = max `attempt` over durable INTENTs at `(dTarget, dRId)` (0 if none);
    /// * `dTrunc`   = `truncate_to` of the latest BEGIN at `dTarget`;
    /// * `dComplete`/`dComplId` = the durable COMPLETE at `(dTarget, dRId, dAttempt)` / max id;
    /// * `dUnserv`  = a durable UNSERVABLE naming `dComplId`.
    ///
    /// Then: UNSERVABLE → `Superseding` (F-UNSERVABLE order); COMPLETE → `ActivationComplete`
    /// (the decision stands, I22); **everything else fences forward** — a `BEGIN_RECOVERY` at
    /// `(dTarget + 1, dRId + 1)` is written (an effect; the send follows `WalDurable`), and an
    /// intent without completion evidence is never resumed. "Sent implies durable" (INTENT and
    /// BEGIN are fsynced before they are sent) is what makes the derived maxima a fence.
    ///
    /// The model's bound-exhaustion branch (→ TERMINAL) is a property of TLC's finite constants;
    /// the implementation has no such bound, so it is not mirrored here.
    fn restart(&mut self) -> Vec<Effect> {
        debug_assert_eq!(self.state, CoordState::Crashed, "Restart is only meaningful from CRASHED");
        // ---- derive ----
        let d_target: Epoch = self
            .wal
            .iter()
            .filter_map(|r| match r {
                WalRecord::BeginRecovery { target, .. } => Some(*target),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        let d_rid: RecoveryId = self
            .wal
            .iter()
            .filter_map(|r| match r {
                WalRecord::BeginRecovery { target, recovery_id, .. } if *target == d_target => Some(*recovery_id),
                WalRecord::ResetRecoveryAttempt { target, new_recovery_id, .. } if *target == d_target => Some(*new_recovery_id),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        let d_attempt: crate::AttemptId = self
            .wal
            .iter()
            .filter_map(|r| match r {
                WalRecord::ActivationCommitIntent { tuple } if tuple.epoch == d_target && tuple.recovery_id == d_rid => Some(tuple.attempt),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        let d_trunc: i64 = self
            .wal
            .iter()
            .filter_map(|r| match r {
                WalRecord::BeginRecovery { target, recovery_id, truncate_to, .. } if *target == d_target => Some((*recovery_id, *truncate_to)),
                _ => None,
            })
            .max_by_key(|(rid, _)| *rid)
            .map(|(_, t)| t)
            .unwrap_or(0);
        let d_complete = self.wal.iter().find_map(|r| match r {
            WalRecord::ActivationComplete { tuple, completion_id }
                if tuple.epoch == d_target && tuple.recovery_id == d_rid && tuple.attempt == d_attempt =>
            {
                Some((tuple.clone(), *completion_id))
            }
            _ => None,
        });
        let d_compl_id: CompletionId = self
            .wal
            .iter()
            .filter_map(|r| match r {
                WalRecord::ActivationComplete { completion_id, .. } => Some(*completion_id),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        // Only an UNSERVABLE naming THIS target's completion classifies the restart as superseding
        // (spec §6.5a). A superseded completion of an earlier target is finished business: the
        // recovery its supersession opened is already in the log as a later BEGIN_RECOVERY, and
        // re-entering SUPERSEDING off it would claim a decision this target never made. The DST
        // found exactly that on 2026-09-03 (`DecisionMonotone — post-decision state Superseding
        // without a durable COMPLETE`); `d_compl_id` above stays the GLOBAL maximum so ids remain
        // monotone across restarts.
        let d_unserv = match &d_complete {
            Some((_, cid)) => self.wal.iter().any(|r| matches!(r, WalRecord::ActivationUnservable { completion_id } if completion_id == cid)),
            None => false,
        };

        // ---- re-derive the volatile state: no survivors ----
        self.epoch = d_target;
        self.recovery_id = d_rid;
        self.attempt = d_attempt;
        self.kind = if d_target == 0 { ActivationKind::Initial } else { ActivationKind::Recovery };
        self.tuple = d_complete.as_ref().map(|(t, _)| t.clone());
        self.next_completion_id = d_compl_id + 1;
        self.committed.clear();
        self.finalized.clear();
        self.lost.clear();
        self.pending_recovery = None;
        self.pending_reset = None;

        // ---- classify (§6.5 order is load-bearing), then fence forward ----
        if d_unserv {
            self.state = CoordState::Superseding;
            Vec::new()
        } else if d_complete.is_some() && !self.served {
            // §6.5 branch 2 — only while the activation has NOT served (§6.5a refinement): a crash
            // after service is outside any transaction and falls through to the fence, because the
            // stages hold a data-plane tail beyond the durable frontier that only the BEGIN's
            // truncation removes (§2.3d), and a re-entered finalization would leave the old epoch
            // unfenced.
            self.state = CoordState::ActivationComplete;
            Vec::new()
        } else {
            let (base, target, rid) = (d_target, d_target + 1, d_rid + 1);
            let id = self.next_effect_id(EffectKind::WriteWal);
            self.pending_recovery = Some((base, target, d_trunc, rid));
            self.state = CoordState::RecoveryStartedPending;
            vec![Effect::WriteWal { id, record: WalRecord::BeginRecovery { base, target, recovery_id: rid, truncate_to: d_trunc } }]
        }
    }
}
