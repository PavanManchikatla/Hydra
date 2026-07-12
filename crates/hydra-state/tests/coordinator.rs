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

// This asserts the FIX; the Mut4 mutation deliberately breaks it, so exclude it there (the
// `mut4_*` test below covers that build). This is the mutation-parity convention.
#[cfg(not(feature = "mutation_no_abort_finality"))]
#[test]
fn tlc1_crash_after_abort_never_completes_aborted_attempt() {
    // The TLC-1 counter-example, replayed with the fix (default build; I25 guard on).
    let mut c = Coordinator::new_initial(sid(), 2, 1);
    step_ok(&mut c, StagesReconstructed);
    step_ok(&mut c, ProceedWriteIntent); // attempt 1
    step_ok(&mut c, WalDurable(Intent));
    step_ok(&mut c, ProceedSendCommit);
    // both stages ack COMMIT for attempt 1 — these acks linger in the network
    step_ok(&mut c, StageCommitted { rank: 0, attempt: 1 });
    step_ok(&mut c, StageCommitted { rank: 1, attempt: 1 });
    // coordinator durably ABORTs attempt 1 instead of completing it
    step_ok(&mut c, ProceedAbort);
    step_ok(&mut c, WalDurable(Abort));
    // crash, then restart: attempt 1 must be classified terminal, its COMMIT never replayed
    step_ok(&mut c, Crash);
    step_ok(&mut c, Restart);
    assert_eq!(c.state(), CoordState::ReadyAll, "restart must not resurrect aborted attempt 1");
    // feeding the lingering stale attempt-1 acks completes nothing
    step_ok(&mut c, StageCommitted { rank: 0, attempt: 1 });
    step_ok(&mut c, StageCommitted { rank: 1, attempt: 1 });
    assert!(!c.completed());
    assert!(invariants::check(&c).is_empty());
}

// ---- mutation parity (Mut4 = no abort finality); run with `--features mutation_no_abort_finality` ----
#[cfg(feature = "mutation_no_abort_finality")]
#[test]
fn mut4_completion_after_abort_is_caught_by_checker() {
    let mut c = Coordinator::new_initial(sid(), 2, 1);
    c.step(StagesReconstructed);
    c.step(ProceedWriteIntent); // attempt 1
    c.step(WalDurable(Intent));
    c.step(ProceedSendCommit);
    c.step(StageCommitted { rank: 0, attempt: 1 });
    c.step(StageCommitted { rank: 1, attempt: 1 });
    c.step(ProceedAbort);
    c.step(WalDurable(Abort));
    // Mut4 restart does NOT treat the durable abort as terminal → replays the attempt.
    c.step(Crash);
    c.step(Restart);
    assert_eq!(c.state(), CoordState::IntentDurable, "Mut4 resurrects the aborted attempt");
    c.step(ProceedSendCommit); // replay COMMIT for attempt 1
    c.step(StageCommitted { rank: 0, attempt: 1 }); // stale acks now counted
    c.step(StageCommitted { rank: 1, attempt: 1 });
    c.step(ProceedWriteComplete); // guard off → allowed
    c.step(WalDurable(Complete)); // COMPLETE written for an aborted attempt
    let v = invariants::check(&c);
    assert!(
        v.iter().any(|x| x.invariant == "I25 AbortFinality"),
        "mutation parity: checker must catch Mut4's I25 violation; got {v:?}"
    );
}
