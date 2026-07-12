//! Directed tests for the coordinator activation transaction (M1 slice 1). Invariants are
//! asserted after every step, mirroring the simulator's per-step `check`. The TLC-1 trace and
//! its Mut4 mutation are replayed here as directed scenarios (BLUEPRINT §4 doctrine).

use hydra_state::coordinator::WalKindTag::*;
use hydra_state::CoordEvent::*;
use hydra_state::{invariants, CoordEvent, CoordState, Coordinator, SessionId};

fn sid() -> SessionId {
    SessionId([1u8; 16])
}

/// Step and assert all invariants hold afterward.
fn step_ok(c: &mut Coordinator, ev: CoordEvent) {
    c.step(ev);
    let v = invariants::check(c);
    assert!(v.is_empty(), "invariant violated {v:?} in state {:?}", c.state());
}

fn drive_happy_path(n: u16) -> Coordinator {
    let mut c = Coordinator::new_initial(sid(), n, 1);
    step_ok(&mut c, StagesReconstructed);
    step_ok(&mut c, ProceedWriteIntent);
    step_ok(&mut c, WalDurable(Intent));
    step_ok(&mut c, ProceedSendCommit);
    for r in 0..n {
        step_ok(&mut c, StageCommitted { rank: r, attempt: 1 });
    }
    step_ok(&mut c, ProceedWriteComplete);
    step_ok(&mut c, WalDurable(Complete));
    step_ok(&mut c, ProceedSendFinalize);
    for r in 0..n {
        step_ok(&mut c, StageFinalized { rank: r, attempt: 1 });
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
//               faithful: -> READY_ALL (attempt terminal, I25)  | Mut4: -> ACTIVATION_INTENT_DURABLE
//   S11       : CoordSendCommit (replay)        -> ProceedSendCommit                  (Mut4 only)
//   S12–S13   : StageRecvCommitAt (stale acks)  -> StageCommitted{…,attempt:1} ×2     (Mut4 only)
//   S14       : CoordWriteComplete              -> ProceedWriteComplete ; WalDurable(Complete)
//               => Mut4: `AbortFinality`/I25 violated at the mapped step S14.

// Faithful build (I25 guard on): the replay diverges from TLC at S10 (READY_ALL) and completes
// with no violation. The Mut4 build is covered by `mut4_…` below (mutation-parity convention).
#[cfg(not(feature = "mutation_no_abort_finality"))]
#[test]
fn tlc1_crash_after_abort_never_completes_aborted_attempt() {
    let mut c = Coordinator::new_initial(sid(), 2, 1);
    step_ok(&mut c, StagesReconstructed); // S2–S5
    step_ok(&mut c, ProceedWriteIntent); // S6: attempt 1
    step_ok(&mut c, WalDurable(Intent));
    step_ok(&mut c, ProceedSendCommit); // S7
    // pre-abort attempt-1 acks — these linger in the network past the abort
    step_ok(&mut c, StageCommitted { rank: 0, attempt: 1 });
    step_ok(&mut c, StageCommitted { rank: 1, attempt: 1 });
    step_ok(&mut c, ProceedAbort); // S8: durably ABORT attempt 1
    step_ok(&mut c, WalDurable(Abort));
    step_ok(&mut c, Crash); // S9
    step_ok(&mut c, Restart); // S10
    // pivotal spot assertions on trace-relevant fields (faithful branch of S10):
    assert_eq!(c.state(), CoordState::ReadyAll, "S10 faithful: aborted attempt is terminal");
    assert_eq!(c.attempt(), 1, "attempt id unchanged across the aborted-attempt restart");
    assert!(c.attempt_aborted(1), "durable ABORT for attempt 1 persists across restart");
    // the lingering stale attempt-1 acks (TLC S12–S13) complete nothing under the guard
    step_ok(&mut c, StageCommitted { rank: 0, attempt: 1 });
    step_ok(&mut c, StageCommitted { rank: 1, attempt: 1 });
    assert!(!c.completed(), "no COMPLETE for an aborted attempt (I25)");
    assert!(invariants::check(&c).is_empty());
}

// ---- mutation parity (Mut4 = no abort finality); run with `--features mutation_no_abort_finality` ----
// Same mapped sequence as above; with the guard off, S10 goes to ACTIVATION_INTENT_DURABLE and the
// replayed commit + stale acks reach CoordWriteComplete (S14), violating I25 exactly as TLC reports.
#[cfg(feature = "mutation_no_abort_finality")]
#[test]
fn mut4_completion_after_abort_is_caught_by_checker() {
    let mut c = Coordinator::new_initial(sid(), 2, 1);
    c.step(StagesReconstructed); // S2–S5
    c.step(ProceedWriteIntent); // S6: attempt 1
    c.step(WalDurable(Intent));
    c.step(ProceedSendCommit); // S7
    c.step(StageCommitted { rank: 0, attempt: 1 });
    c.step(StageCommitted { rank: 1, attempt: 1 });
    c.step(ProceedAbort); // S8
    c.step(WalDurable(Abort));
    c.step(Crash); // S9
    c.step(Restart); // S10: guard off → resurrects the attempt
    assert_eq!(c.state(), CoordState::IntentDurable, "S10 Mut4: aborted attempt resurrected");
    assert_eq!(c.attempt(), 1, "the resurrected attempt is the aborted attempt 1");
    c.step(ProceedSendCommit); // S11: replay COMMIT for attempt 1
    c.step(StageCommitted { rank: 0, attempt: 1 }); // S12–S13: stale acks now counted
    c.step(StageCommitted { rank: 1, attempt: 1 });
    c.step(ProceedWriteComplete); // S14: guard off → allowed
    c.step(WalDurable(Complete)); // COMPLETE written for an aborted attempt
    let v = invariants::check(&c);
    assert!(
        v.iter().any(|x| x.invariant == "I25 AbortFinality"),
        "mutation parity: checker must catch Mut4's I25 violation at S14; got {v:?}"
    );
}
