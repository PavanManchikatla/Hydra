//! Directed tests for the stage-session state machine (M1 slice 2), incl. F2 attempt fencing
//! and its Mut3 mutation parity. Invariants asserted after every step.

use hydra_state::invariants::check_stage;
use hydra_state::stage::{StageEffect, StageEvent::*, StageState};
use hydra_state::{ActivationKind, ActivationTuple, Stage};

fn tuple(attempt: u32) -> ActivationTuple {
    ActivationTuple {
        kind: ActivationKind::Recovery,
        epoch: 0,
        recovery_id: 0,
        attempt,
        sampler_checkpoint_id: 1,
    }
}

fn step_ok(s: &mut Stage, ev: hydra_state::StageEvent) -> Vec<StageEffect> {
    let effs = s.step(ev);
    assert!(check_stage(s).is_empty(), "stage invariant violated in {:?}", s.state());
    effs
}

#[test]
fn commit_then_finalize_reaches_active_final() {
    let mut s = Stage::frozen_ready(0, 0, 0);
    let e = step_ok(&mut s, RecvCommit { tuple: tuple(1) });
    assert_eq!(s.state(), StageState::Preactive);
    assert!(matches!(e[0], StageEffect::Committed { attempt: 1, .. }));
    let e = step_ok(&mut s, RecvFinalize { attempt: 1 });
    assert_eq!(s.state(), StageState::ActiveFinal);
    assert!(s.holds_final_evidence());
    assert!(matches!(e[0], StageEffect::Finalized { attempt: 1, .. }));
}

#[test]
fn commit_replay_is_idempotent() {
    let mut s = Stage::frozen_ready(0, 0, 0);
    step_ok(&mut s, RecvCommit { tuple: tuple(1) });
    let e = step_ok(&mut s, RecvCommit { tuple: tuple(1) }); // replay
    assert_eq!(s.state(), StageState::Preactive);
    assert!(matches!(e[0], StageEffect::Committed { attempt: 1, .. }), "must re-ack");
}

#[test]
fn abort_returns_to_frozen_ready() {
    let mut s = Stage::frozen_ready(0, 0, 0);
    step_ok(&mut s, RecvCommit { tuple: tuple(1) });
    step_ok(&mut s, RecvAbort { attempt: 1 });
    assert_eq!(s.state(), StageState::FrozenReady);
    assert_eq!(s.highest_attempt(), 1, "fence floor persists across abort");
}

// ---- F2 attempt fencing (default build) ----
#[cfg(not(feature = "mutation_no_attempt_fence"))]
#[test]
fn stale_attempt_is_fenced() {
    let mut s = Stage::frozen_ready(0, 0, 0);
    step_ok(&mut s, RecvCommit { tuple: tuple(1) }); // attempt 1
    step_ok(&mut s, RecvAbort { attempt: 1 });
    step_ok(&mut s, RecvCommit { tuple: tuple(2) }); // retry attempt 2 -> PREACTIVE
    assert_eq!(s.attempt(), 2);
    // a delayed COMMIT from the aborted attempt 1 must be fenced (F2), not accepted
    let e = step_ok(&mut s, RecvCommit { tuple: tuple(1) });
    assert!(matches!(e[0], StageEffect::Fenced { attempt: 1, highest: 2, .. }));
    assert_eq!(s.attempt(), 2, "stage must not regress to the stale attempt");
}

// ---- Mut3 parity: no attempt fencing -> stale commit accepted -> checker catches it ----
#[cfg(feature = "mutation_no_attempt_fence")]
#[test]
fn mut3_stale_commit_regression_is_caught_by_checker() {
    let mut s = Stage::frozen_ready(0, 0, 0);
    s.step(RecvCommit { tuple: tuple(1) });
    s.step(RecvAbort { attempt: 1 });
    s.step(RecvCommit { tuple: tuple(2) }); // PREACTIVE at attempt 2, highest 2
    assert_eq!(s.attempt(), 2);
    // With fencing off, the stale attempt-1 COMMIT is (wrongly) accepted → attempt regresses.
    s.step(RecvCommit { tuple: tuple(1) });
    assert_eq!(s.attempt(), 1, "Mut3 regresses the stage onto the stale attempt");
    let v = check_stage(&s);
    assert!(
        v.iter().any(|x| x.invariant == "F2 AttemptFence"),
        "mutation parity: checker must catch Mut3's fence regression; got {v:?}"
    );
}
