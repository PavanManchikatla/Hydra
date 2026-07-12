//! Directed recovery scenarios (M1 slice 3): BEGIN_RECOVERY Cases A/B/B′, RESET_RECOVERY_ATTEMPT,
//! the named "reset-after-catch-up" scenario, and Mut2 (label-only reset) mutation parity.

use hydra_state::invariants::check_stage;
use hydra_state::stage::{StageEffect, StageEvent::*, StageState};
use hydra_state::{ActivationKind, ActivationTuple, Stage};

fn tuple(epoch: u32, attempt: u32) -> ActivationTuple {
    ActivationTuple { kind: ActivationKind::Recovery, epoch, recovery_id: 0, attempt, sampler_checkpoint_id: 1 }
}

fn step_ok(s: &mut Stage, ev: hydra_state::StageEvent) -> Vec<StageEffect> {
    let e = s.step(ev);
    assert!(check_stage(s).is_empty(), "invariant violated in {:?}", s.state());
    e
}

#[test]
fn case_a_first_application_truncates() {
    let mut s = Stage::frozen(0, 0, 0, 5); // epoch 0, applied 5
    let e = step_ok(&mut s, RecvBegin { base: 0, target: 1, recovery_id: 0, truncate_to: 2 });
    assert_eq!(s.state(), StageState::Frozen);
    assert_eq!(s.epoch(), 1);
    assert_eq!(s.applied(), 2, "applied truncated to truncate_to (I7a)");
    assert!(matches!(e[0], StageEffect::RecoveryAck { target: 1, .. }));
}

#[test]
fn case_b_prime_completed_is_locally_decidable() {
    let mut s = Stage::frozen_ready(0, 1, 0);
    step_ok(&mut s, RecvCommit { tuple: tuple(1, 1) });
    step_ok(&mut s, RecvFinalize { attempt: 1 }); // ACTIVE_FINAL with evidence
    let e = s.step(RecvBegin { base: 0, target: 1, recovery_id: 0, truncate_to: 0 });
    assert!(matches!(e[0], StageEffect::RecoveryCompleted { target: 1, .. }));
}

#[test]
fn case_b_pure_replay_ok_when_within_truncate() {
    let mut s = Stage::frozen(0, 1, 0, 1); // at target epoch 1, applied 1 ≤ truncate
    let e = step_ok(&mut s, RecvBegin { base: 0, target: 1, recovery_id: 0, truncate_to: 2 });
    assert!(matches!(e[0], StageEffect::RecoveryAck { .. }));
    assert!(!s.caseb_violated());
}

// The named "reset-after-catch-up" DST scenario: a survivor advanced past truncate_to by
// catch-up must be RESET (truncated) before a Case-B replay, or Case B fatally trips.
#[cfg(not(feature = "mutation_label_reset"))]
#[test]
fn reset_after_catch_up_truncates_then_case_b_ok() {
    let mut s = Stage::frozen(0, 1, 0, 0);
    // catch-up/rebuild past truncate_to (=1): advance applied to 3
    for _ in 0..4 {
        step_ok(&mut s, RebuildStep { goal: 3 });
    }
    assert_eq!(s.applied(), 3);
    assert_eq!(s.state(), StageState::FrozenReady);
    // RESET truncates back to truncate_to
    step_ok(&mut s, RecvReset { target: 1, new_recovery_id: 1, truncate_to: 1 });
    assert_eq!(s.applied(), 1, "RESET truncates the survivor (I23)");
    // now Case B is a clean pure replay
    step_ok(&mut s, RecvBegin { base: 0, target: 1, recovery_id: 1, truncate_to: 1 });
    assert!(!s.caseb_violated());
}

// Mut2 parity: label-only reset leaves the survivor advanced → Case B trips CaseBPure.
#[cfg(feature = "mutation_label_reset")]
#[test]
fn mut2_label_only_reset_trips_case_b() {
    let mut s = Stage::frozen(0, 1, 0, 0);
    for _ in 0..4 {
        s.step(RebuildStep { goal: 3 });
    }
    assert_eq!(s.applied(), 3);
    s.step(RecvReset { target: 1, new_recovery_id: 1, truncate_to: 1 }); // label-only: applied stays 3
    assert_eq!(s.applied(), 3, "Mut2 reset does not truncate");
    s.step(RecvBegin { base: 0, target: 1, recovery_id: 1, truncate_to: 1 }); // Case B: 3 > 1
    let v = check_stage(&s);
    assert!(
        v.iter().any(|x| x.invariant == "CaseBPure"),
        "mutation parity: checker must catch Mut2's Case-B violation; got {v:?}"
    );
}
