//! Directed tests for the coordinator activation transaction (M1 slice 1). Invariants are
//! asserted after every step, mirroring the simulator's per-step `check`. The TLC-1 trace and
//! its Mut4 mutation are replayed here as directed scenarios (BLUEPRINT §4 doctrine).

use hydra_state::coordinator::WalKindTag::*;
use hydra_state::CoordEvent::*;
use hydra_state::{invariants, CoordEvent, CoordState, Coordinator, SessionId, WalRecord};

fn sid() -> SessionId {
    SessionId([1u8; 16])
}

/// Step and assert all invariants hold afterward; hands back the effects for spot assertions.
fn step_ok(c: &mut Coordinator, ev: CoordEvent) -> Vec<hydra_state::Effect> {
    let effs = c.step(ev);
    let v = invariants::check(c);
    assert!(v.is_empty(), "invariant violated {v:?} in state {:?}", c.state());
    effs
}

fn drive_happy_path(n: u16) -> Coordinator {
    let mut c = Coordinator::new_initial(sid(), n, 1);
    step_ok(&mut c, StagesReconstructed);
    step_ok(&mut c, ProceedWriteIntent);
    step_ok(&mut c, WalDurable(Intent));
    step_ok(&mut c, ProceedSendCommit);
    for r in 0..n {
        step_ok(&mut c, StageCommitted { rank: hydra_state::AuthenticatedRank::for_test_harness_asserting_identity(r), attempt: 1 });
    }
    step_ok(&mut c, ProceedWriteComplete);
    step_ok(&mut c, WalDurable(Complete));
    step_ok(&mut c, ProceedSendFinalize);
    for r in 0..n {
        step_ok(&mut c, StageFinalized { rank: hydra_state::AuthenticatedRank::for_test_harness_asserting_identity(r), attempt: 1 });
    }
    step_ok(&mut c, ProceedBecomeServiceable);
    c
}

#[test]
fn initial_activation_reaches_serviceable() {
    let c = drive_happy_path(2);
    assert_eq!(c.state(), CoordState::Serviceable);
    assert!(c.completed());
    assert!(invariants::check(&c).is_empty());
}

#[test]
fn disabled_events_are_noops() {
    // Firing a not-yet-enabled action does nothing (TLC only fires enabled actions).
    let mut c = Coordinator::new_initial(sid(), 1, 1);
    assert!(!c.enabled(&ProceedWriteComplete));
    c.step(ProceedWriteComplete);
    assert_eq!(c.state(), CoordState::Reconstructing);
    assert!(c.wal().is_empty());
}

#[test]
fn abort_returns_to_ready_at_next_attempt() {
    let mut c = Coordinator::new_initial(sid(), 2, 1);
    step_ok(&mut c, StagesReconstructed);
    step_ok(&mut c, ProceedWriteIntent); // attempt 1
    step_ok(&mut c, WalDurable(Intent));
    step_ok(&mut c, ProceedSendCommit);
    step_ok(&mut c, ProceedAbort);
    step_ok(&mut c, WalDurable(Abort));
    assert_eq!(c.state(), CoordState::ReadyAll);
    assert!(c.attempt_aborted(1));
    step_ok(&mut c, ProceedWriteIntent); // attempt 2
    assert_eq!(c.attempt(), 2);
}

// ================= TLC-trace replay: TLC-1 / Mut4 (gate evidence (d)) =================
// TLC counterexample: `verification/smoke/Mut4-AbortFinality.cfg`, 14-state trace (the Mut4
// reproduction of the original TLC-1 defect, `AbortFinality`/I25 violated). Event-sequence
// fidelity replay — the impl coordinator is driven through the mapped ordered sequence; the
// faithful build (I25 guard on) and the Mut4 build (guard off) walk the identical sequence to
// opposite outcomes. Full model↔impl state equality is not asserted (abstractions differ).
//
//   TLC state : action                         -> impl event(s)
//   S2–S5     : StageRebuildStep (catch-up)     -> StagesReconstructed
//   S6        : CoordWriteIntent (attempt 1)    -> ProceedWriteIntent ; WalDurable(Intent)
//   S7        : CoordSendCommit                 -> ProceedSendCommit
//   (pre-abort acks linger in the network)      -> StageCommitted{rank,attempt:1} ×2
//   S8        : CoordAbortActivation            -> ProceedAbort ; WalDurable(Abort)
//   S9        : CoordCrash                      -> Crash
//   S10       : CoordRestart                    -> Restart
//               §6.5a (2026-09-03): -> RECOVERY_STARTED — the restart derives from the WAL and
//               FENCES FORWARD to (epoch+1, rid+1); the aborted attempt is terminal AND fenced.
//               (Before §6.5a: faithful -> READY_ALL | Mut4 -> ACTIVATION_INTENT_DURABLE, the
//               resurrection this trace existed to catch.)
//   S11–S14   : the Mut4 replay (CoordSendCommit, stale StageRecvCommitAt ×2, CoordWriteComplete)
//               is UNREACHABLE under fence-forward: no state after S10 accepts an attempt-1 ack.
//               => Mut4 is SUBSUMED by §6.5a — escalated, PROJECT_STATE §7.77; see `mut4_…` below.

// Faithful build (I25 guard on): the aborted attempt stays terminal across the restart, the stale
// attempt-1 acks are rejected by the fenced state, and no COMPLETE can ever exist for attempt 1.
#[cfg(not(feature = "mutation_no_abort_finality"))]
#[test]
fn tlc1_crash_after_abort_never_completes_aborted_attempt() {
    let mut c = Coordinator::new_initial(sid(), 2, 1);
    step_ok(&mut c, StagesReconstructed); // S2–S5
    step_ok(&mut c, ProceedWriteIntent); // S6: attempt 1
    step_ok(&mut c, WalDurable(Intent));
    step_ok(&mut c, ProceedSendCommit); // S7
    // pre-abort attempt-1 acks — these linger in the network past the abort
    step_ok(&mut c, StageCommitted { rank: hydra_state::AuthenticatedRank::for_test_harness_asserting_identity(0), attempt: 1 });
    step_ok(&mut c, StageCommitted { rank: hydra_state::AuthenticatedRank::for_test_harness_asserting_identity(1), attempt: 1 });
    step_ok(&mut c, ProceedAbort); // S8: durably ABORT attempt 1
    step_ok(&mut c, WalDurable(Abort));
    step_ok(&mut c, Crash); // S9
    let effs = step_ok(&mut c, Restart); // S10
    // pivotal spot assertions on trace-relevant fields (S10 under §6.5a):
    assert_eq!(c.state(), CoordState::RecoveryStartedPending, "S10: the restart fences forward — it never resumes the aborted attempt");
    assert!(
        effs.iter().any(|e| matches!(e, hydra_state::Effect::WriteWal { record: WalRecord::BeginRecovery { base: 0, target: 1, recovery_id: 1, .. }, .. })),
        "the fence-forward BEGIN is the restart's only effect; got {effs:?}"
    );
    // The new (epoch, rid) take effect when the BEGIN is DURABLE — WAL-before-wire; the model's
    // CoordRestart writes and advances in one action, the code splits the fsync out.
    step_ok(&mut c, WalDurable(BeginRecovery));
    assert_eq!(c.state(), CoordState::RecoveryStarted);
    assert_eq!((c.epoch(), c.recovery_id(), c.attempt()), (1, 1, 0), "a NEW recovery at (epoch+1, rid+1); the attempt space restarts");
    assert!(
        c.wal().iter().any(|r| matches!(r, WalRecord::ActivationAbort { epoch: 0, recovery_id: 0, attempt: 1 })),
        "the durable ABORT for attempt 1 persists across restart (the log is the state)"
    );
    // the lingering stale attempt-1 acks (TLC S12–S13) are REJECTED by the fenced state — not
    // merely not counted: the events are not accepted at all.
    assert!(c.step(StageCommitted { rank: hydra_state::AuthenticatedRank::for_test_harness_asserting_identity(0), attempt: 1 }).is_empty());
    assert!(c.step(StageCommitted { rank: hydra_state::AuthenticatedRank::for_test_harness_asserting_identity(1), attempt: 1 }).is_empty());
    assert_eq!(c.state(), CoordState::RecoveryStarted, "stale acks move nothing");
    assert!(!c.completed(), "no COMPLETE for an aborted attempt (I25)");
    assert!(invariants::check(&c).is_empty());
}

// ---- mutation parity (Mut4 = no abort finality); run with `--features mutation_no_abort_finality` ----
// ⛔ ESCALATED-SUBSUMED (2026-09-03, PROJECT_STATE §7.77). Mut4's designed trace needed S10 to
// RESUME the aborted attempt (CoordRestart → ACTIVATION_INTENT_DURABLE) so that a replayed COMMIT
// and the stale acks could reach CoordWriteComplete. Spec §6.5a removed that restart: a restart
// derives from the WAL and fences forward, so with the I25 guard OFF the sequence still cannot
// complete attempt 1 — there is no state after S10 that accepts an attempt-1 ack. TLC reports the
// same (`Mut4RestartMin` drains clean: `verdict=ESCALATED-SUBSUMED`). This test therefore asserts
// the SUBSUMPTION — the mutation is unreachable, not "caught" — so a reader running the feature
// build sees the fact rather than a green "caught by checker" the checker never earned. What the
// mutation should sabotage instead is the design authority's ruling; until then this is the record.
#[cfg(feature = "mutation_no_abort_finality")]
#[test]
fn mut4_is_unreachable_under_fence_forward_escalated_subsumed() {
    let mut c = Coordinator::new_initial(sid(), 2, 1);
    c.step(StagesReconstructed); // S2–S5
    c.step(ProceedWriteIntent); // S6: attempt 1
    c.step(WalDurable(Intent));
    c.step(ProceedSendCommit); // S7
    c.step(StageCommitted { rank: hydra_state::AuthenticatedRank::for_test_harness_asserting_identity(0), attempt: 1 });
    c.step(StageCommitted { rank: hydra_state::AuthenticatedRank::for_test_harness_asserting_identity(1), attempt: 1 });
    c.step(ProceedAbort); // S8
    c.step(WalDurable(Abort));
    c.step(Crash); // S9
    c.step(Restart); // S10: guard off — and STILL a fence-forward, not a resurrection
    assert_eq!(c.state(), CoordState::RecoveryStartedPending, "ESCALATED-SUBSUMED: with the guard off the restart still fences forward (§6.5a)");
    c.step(WalDurable(BeginRecovery));
    assert_eq!((c.epoch(), c.attempt()), (1, 0), "attempt 1 is fenced behind epoch 1 — it cannot be resurrected");
    // S11–S14 as designed: nothing is accepted, nothing completes.
    assert!(c.step(ProceedSendCommit).is_empty(), "S11 unreachable");
    assert!(c.step(StageCommitted { rank: hydra_state::AuthenticatedRank::for_test_harness_asserting_identity(0), attempt: 1 }).is_empty());
    assert!(c.step(StageCommitted { rank: hydra_state::AuthenticatedRank::for_test_harness_asserting_identity(1), attempt: 1 }).is_empty());
    assert!(c.step(ProceedWriteComplete).is_empty(), "S14 unreachable");
    assert!(!c.completed(), "no COMPLETE for the aborted attempt — the mutation had nothing to sabotage");
    assert!(invariants::check(&c).is_empty(), "and the checker sees a legal state, because it IS one: the I25 hole is closed by the fence, not the guard");
    // Red by design (rule 25: a verdict token must express failure), exactly as the TLC smoke is,
    // until the design authority rules what Mut4 should sabotage under fence-forward.
    panic!("verdict=ESCALATED-SUBSUMED (Mut4 is unreachable under spec §6.5a fence-forward: the facts above hold, the mutation had nothing to sabotage — PROJECT_STATE §7.77)");
}
