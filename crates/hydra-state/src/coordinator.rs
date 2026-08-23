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
}

/// Coordinator activation state (subset of TLA+ `cState` for the transition-core slice).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CoordState {
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
    /// Per-(session, epoch) monotonic counter owned by the SM, feeding effect ids (WAL-FORMAT §4).
    monotonic_seq: u64,
}

impl Coordinator {
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

    fn unservable_recorded(&self) -> bool {
        self.wal.iter().any(|r| matches!(r, WalRecord::ActivationUnservable { .. })) && self.completed()
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
                // ∧ unservable'=TRUE ∧ cState'=SUPERSEDING). Without the durable record,
                // `unservable_recorded()` (and thus §6.5's restart-superseding branch, which is
                // evaluated *before* the COMPLETE branch) can never fire, so a crash in the window
                // before the superseding BEGIN_RECOVERY would restart into finalization and reopen
                // the I22 hole. This durability was missing; the WAL effect alone was not enough.
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
                self.epoch += 1;
                self.recovery_id = 0;
                self.attempt = 0;
                self.kind = ActivationKind::Recovery;
                self.tuple = None;
                self.committed.clear();
                self.finalized.clear();
                self.lost.clear();
                self.state = CoordState::Reconstructing;
                Vec::new()
            }
            Crash => {
                self.state = CoordState::Crashed;
                Vec::new()
            }
            Restart => {
                self.restart();
                Vec::new()
            }
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
            WalKindTag::Terminate => {
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

    /// Phase-specific restart (spec §6.5), driven purely by the durable WAL — no clocks, no I/O.
    fn restart(&mut self) {
        let complete = self.completed();
        let intent = self.wal.iter().any(|r| {
            matches!(r, WalRecord::ActivationCommitIntent { tuple }
                if tuple.attempt == self.attempt)
        });
        self.committed.clear();
        self.finalized.clear();
        // AbortGuardEnabled = the I25 guard (off only under the Mut4 mutation).
        let abort_guard = !cfg!(feature = "mutation_no_abort_finality");
        self.state = if self.unservable_recorded() {
            // §6.7: an unservable activation was recorded → resume the superseding recovery.
            CoordState::Superseding
        } else if complete {
            // decision stands; finalize.
            CoordState::ActivationComplete
        } else if abort_guard && self.attempt_aborted(self.attempt) {
            // TLC-1 / I25: durable ABORT for the current attempt, no COMPLETE ⇒ attempt terminal;
            // proceed to a NEW intent at attempt+1. NEVER replay COMMIT for the aborted attempt.
            // (Mut4 disables this branch → falls through to replay COMMIT → reproduces TLC-1.)
            CoordState::ReadyAll
        } else if intent {
            // intent durable, no complete ⇒ replay COMMIT (converge, I18)
            CoordState::IntentDurable
        } else {
            CoordState::Reconstructing
        };
    }
}
