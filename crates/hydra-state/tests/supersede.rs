//! Post-decision participant loss (spec §6.7, I22) and its Mut1 mutation parity.
//! The named DST scenario: a participant lost *after* the durable decision must be superseded
//! (never served), restoring progress. Mut1 removes the supersession path → the coordinator
//! wedges in a post-decision deadlock (the `PostDecisionLoss` liveness hole).

use hydra_state::CoordEvent::*;
use hydra_state::{invariants, CoordState, Coordinator, SessionId};

fn sid() -> SessionId {
    SessionId([1u8; 16])
}

// ================= TLC-trace replay: Mut1 (gate evidence (d)) =================
// TLC counterexample: `Mut1Unservable.cfg`, 18-state stuttering lasso (`PostDecisionLoss`/Progress
// liveness violated — a LOST participant after the durable decision, no supersession under
// EnableUnservable=FALSE, so Progress is never restored). Event-sequence fidelity replay of the
// defect-bearing tail; the early recovery setup (S2–S9) is orchestration the coordinator
// transition core abstracts as its Reconstructing→ReadyAll edge (`StagesReconstructed`), so it is
// documented-as-abstracted rather than driven. The impl checks the lasso via the
// `post_decision_deadlock()` watchdog (liveness is not modeled directly): the faithful build
// (supersession available) escapes it; the Mut1 build wedges in it — the stuttering lasso.
//
//   TLC state : action                        -> impl event(s)
//   S2–S9     : StageCrash/Rejoin/ResetAt/     -> (recovery orchestration; abstracted as
//               RebuildStep×4                       `StagesReconstructed` in the transition core)
//   S10       : CoordWriteIntent               -> ProceedWriteIntent ; WalDurable(Intent)
//   S11       : CoordSendCommit                -> ProceedSendCommit
//   S12,S14   : StageRecvCommitAt ×2           -> StageCommitted{rank:0} ; StageCommitted{rank:1}
//   S15       : CoordWriteComplete             -> ProceedWriteComplete ; WalDurable(Complete)  (decision durable)
//   S16       : CoordSendFinalize              -> ProceedSendFinalize
//   S17       : StageRecvFinalizeAt            -> StageFinalized{rank:0}   (one stage finalizes)
//   S13       : StageCrash (post-commit loss)  -> StageLost{rank:1}        (the other is lost)
//   S18       : Stuttering (no Progress)       -> post_decision_deadlock(): faithful=false, Mut1=true

/// Drive a 2-stage session to `Finalizing` with the durable decision made, stage 0 finalized,
/// and stage 1 permanently lost (the post-decision-loss state) — the mapped tail (S10–S17,S13) of
/// the Mut1 lasso above.
fn drive_to_post_decision_loss() -> Coordinator {
    let mut c = Coordinator::new_initial(sid(), 2, 1);
    c.step(StagesReconstructed);
    c.step(ProceedWriteIntent);
    c.step(WalDurable(hydra_state::coordinator::WalKindTag::Intent));
    c.step(ProceedSendCommit);
    c.step(StageCommitted { rank: hydra_state::AuthenticatedRank::for_test_harness_asserting_identity(0), attempt: 1 });
    c.step(StageCommitted { rank: hydra_state::AuthenticatedRank::for_test_harness_asserting_identity(1), attempt: 1 });
    c.step(ProceedWriteComplete);
    c.step(WalDurable(hydra_state::coordinator::WalKindTag::Complete));
    c.step(ProceedSendFinalize); // -> Finalizing (decision is durable)
    c.step(StageFinalized { rank: hydra_state::AuthenticatedRank::for_test_harness_asserting_identity(0), attempt: 1 });
    c.step(StageLost { rank: hydra_state::AuthenticatedRank::for_test_harness_asserting_identity(1) }); // required participant lost after the decision
    assert!(c.completed());
    assert_eq!(c.state(), CoordState::Finalizing);
    c
}

// ---- default build: supersession restores progress (I22) ----
#[cfg(not(feature = "mutation_no_unservable"))]
#[test]
fn post_decision_loss_supersedes_and_recovers() {
    let mut c = drive_to_post_decision_loss();
    assert!(!c.post_decision_deadlock(), "supersession path is available → not stuck");
    // Record ACTIVATION_UNSERVABLE → **the record is written, not yet durable** (audit M6). The
    // transition waits for the fdatasync, exactly as the intent and complete writes do; this step
    // used to be atomic, which is what made the crash window below unreachable.
    c.step(ProceedRecordUnservable);
    assert_eq!(c.state(), CoordState::UnservablePending, "written, awaiting fdatasync");
    c.step(WalDurable(hydra_state::coordinator::WalKindTag::Unservable));
    assert_eq!(c.state(), CoordState::Superseding);
    assert!(invariants::check(&c).is_empty());
    // open the superseding recovery at epoch+1 → progress restored
    let base_epoch = c.epoch();
    // **M4·0b — superseding now WRITES ITS `BEGIN_RECOVERY` RECORD, which the model always did**
    // (`CoordStartSuperseding` is `Wal([t |-> "BEGIN", ...])` ∧ the transition). The code used to
    // advance the epoch, tell nobody, and record nothing — so a crash in the superseding window
    // left §6.5 with no BEGIN to classify from. This step is that record becoming durable.
    let effs = c.step(ProceedStartSuperseding);
    assert!(
        matches!(effs[..], [hydra_state::Effect::WriteWal { .. }]),
        "supersession writes BEGIN_RECOVERY before it transitions"
    );
    assert_eq!(c.state(), CoordState::RecoveryStartedPending, "written, awaiting fdatasync");
    c.step(WalDurable(hydra_state::coordinator::WalKindTag::BeginRecovery));
    assert_eq!(c.state(), CoordState::RecoveryStarted);
    c.step(ProceedSendBeginRecovery);
    assert_eq!(c.state(), CoordState::Reconstructing);
    assert_eq!(c.epoch(), base_epoch + 1, "superseding recovery is at epoch+1");
    assert!(!c.completed(), "predecessor's COMPLETE does not leak into the new epoch");
    assert!(invariants::check(&c).is_empty());
}

// ---- F-UNSERVABLE regression: crash in the superseding window restarts to SUPERSEDING ----
// A coordinator crash after ACTIVATION_UNSERVABLE is durable but before the superseding
// BEGIN_RECOVERY must restart to SUPERSEDING (spec §6.5, evaluated *before* the COMPLETE branch)
// and must NEVER re-enter finalization. Regression for the durability gap the hydra-wal sim found:
// the unservable fact was emitted as a WAL effect but never recorded in the coordinator's durable
// WAL, so restart misclassified to ACTIVATION_COMPLETE and reopened the I22 hole.
// Asserts the FIX; Mut5 (`mutation_unservable_restart`) deliberately reintroduces the defect, so
// exclude it there — the sim's WAL-codec cross-check covers that build (mutation-parity convention).
#[cfg(not(any(feature = "mutation_no_unservable", feature = "mutation_unservable_restart")))]
#[test]
fn f_unservable_crash_in_superseding_window_restarts_to_superseding() {
    let mut c = drive_to_post_decision_loss();
    let effs = c.step(ProceedRecordUnservable);
    assert!(no_finalize(&effs), "recording unservable must not emit FINALIZE");
    c.step(WalDurable(hydra_state::coordinator::WalKindTag::Unservable));
    assert_eq!(c.state(), CoordState::Superseding);
    // crash + restart while still in the superseding window (epoch not yet advanced)
    c.step(Crash);
    let effs = c.step(Restart);
    assert_eq!(
        c.state(),
        CoordState::Superseding,
        "durable ACTIVATION_UNSERVABLE must classify restart as SUPERSEDING, never re-finalize"
    );
    assert!(no_finalize(&effs), "restart in the superseding window must not emit FINALIZE");
    assert!(invariants::check(&c).is_empty());
    // and the superseding recovery still opens cleanly at epoch+1 after the restart
    let base = c.epoch();
    let effs = c.step(ProceedStartSuperseding);
    assert!(no_finalize(&effs));
    c.step(WalDurable(hydra_state::coordinator::WalKindTag::BeginRecovery));
    let effs = c.step(ProceedSendBeginRecovery);
    assert_eq!(c.state(), CoordState::Reconstructing);
    assert_eq!(c.epoch(), base + 1);
    assert!(!c.completed(), "predecessor COMPLETE does not leak past supersession");
    assert!(no_finalize(&effs));
}

fn no_finalize(effs: &[hydra_state::Effect]) -> bool {
    !effs.iter().any(|e| {
        matches!(
            e,
            hydra_state::Effect::Send { msg: hydra_state::ControlMsg::FinalizeActivation { .. }, .. }
        )
    })
}

// ---- Mut1 parity: no supersession → post-decision deadlock ----
#[cfg(feature = "mutation_no_unservable")]
#[test]
fn mut1_post_decision_loss_deadlocks() {
    let c = drive_to_post_decision_loss();
    // With the unservable/supersession path removed, there is no productive action: the lost
    // stage can never finalize, and supersession is disabled. The watchdog flags the liveness hole.
    assert!(!c.enabled(&ProceedRecordUnservable), "Mut1 removes the supersession recourse");
    assert!(
        c.post_decision_deadlock(),
        "mutation parity: the sim's deadlock watchdog must flag Mut1's PostDecisionLoss hole"
    );
}

// ---------------------------------------------------------------------------------------------
// **Audit M6 — the crash window between writing ACTIVATION_UNSERVABLE and its fdatasync.**
//
// # What the oracles could not see
//
// Two of them, agreeing with each other and with neither the spec nor a real disk:
//
// * **The coordinator SM** pushed the UNSERVABLE record into its durable WAL *in the same step*
//   that emitted the write effect — treating the fdatasync as instantaneous.
// * **The DST sim** called `vwal.fsync()` the instant such a record was written, "to keep the
//   virtual disk's durable set == self.wal". It was keeping the sim consistent with the SM's
//   mistake.
//
// So no schedule in 10M+ steps could place a crash between the write and its durability — the
// simulator was blind by construction to the exact window the record exists to survive. Spec §6.7
// step 1 says *"C **fsyncs** ACTIVATION_UNSERVABLE"* and §1.2 lists it under WAL-before-wire, so
// the atomic form was code and model agreeing against the spec.
//
// The test below is the window itself, and it asserts the *opposite* outcome from its sibling
// above: a crash BEFORE the record is durable must NOT classify the restart as superseding,
// because on a real disk there is nothing to classify from.
// ---------------------------------------------------------------------------------------------
#[cfg(not(any(feature = "mutation_no_unservable", feature = "mutation_unservable_restart")))]
#[test]
fn a_crash_before_the_unservable_record_is_durable_leaves_no_unservable_fact() {
    let mut c = drive_to_post_decision_loss();

    // The write is issued; the fdatasync has NOT returned.
    let effs = c.step(ProceedRecordUnservable);
    assert_eq!(c.state(), CoordState::UnservablePending);
    assert!(no_finalize(&effs), "the write must not emit FINALIZE");

    // The machine dies in the window. Nothing durable was written.
    c.step(Crash);
    let effs = c.step(Restart);
    assert_ne!(
        c.state(),
        CoordState::Superseding,
        "with no DURABLE unservable record there is no unservable fact to restart into — claiming \
         one would be the coordinator believing a write that may never have reached the platter"
    );
    assert!(no_finalize(&effs), "restart must not emit FINALIZE for a lost participant either");
    assert!(invariants::check(&c).is_empty(), "the state after the lost write is still legal");

    // And the recourse is intact: the coordinator can record it again, and this time it lands.
    c.step(ProceedRecordUnservable);
    c.step(WalDurable(hydra_state::coordinator::WalKindTag::Unservable));
    assert_eq!(c.state(), CoordState::Superseding, "re-recording after the lost write converges");
    assert!(invariants::check(&c).is_empty());
}
