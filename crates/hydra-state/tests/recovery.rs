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

// F-LIVENESS-FAIR family 3 (spec §1.3): a PREACTIVE stage receiving BEGIN_RECOVERY for its epoch
// reverts (per the abort rule; PREACTIVE is reversible) rather than being marooned. Regression for
// the model-fidelity gap the checker found post-supersession.
#[test]
fn preactive_stage_reverts_on_begin_recovery() {
    let mut s = Stage::frozen_ready(0, 0, 0);
    step_ok(&mut s, RecvCommit { tuple: tuple(0, 1) }); // -> PREACTIVE at epoch 0, attempt 1
    assert_eq!(s.state(), StageState::Preactive);
    // BEGIN_RECOVERY for the next epoch, base = the stage's epoch: must revert, not be rejected.
    let e = step_ok(&mut s, RecvBegin { base: 0, target: 1, recovery_id: 0, truncate_to: 0 });
    assert_eq!(s.state(), StageState::Frozen, "PREACTIVE reverts to FROZEN (spec §1.3)");
    assert_eq!(s.epoch(), 1, "adopts the target epoch");
    assert!(!s.holds_final_evidence());
    assert!(matches!(e[0], StageEffect::RecoveryAck { target: 1, .. }), "acks the recovery");
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

// ================= TLC-trace replay: Mut2 (gate evidence (d)) =================
// TLC counterexample: `verification/smoke/Mut2-CaseBPure.cfg`, 8-state trace (`CaseBPure`
// violated). Event-sequence fidelity replay of the stage-side portion — the impl `Stage` is
// driven through the mapped ordered sequence; the faithful build (RESET truncates) and the Mut2
// build (label-only reset) walk the identical sequence to opposite outcomes. The pivotal spot
// assertion is `applied` after the RESET (truncated=1 faithful vs advanced=3 Mut2).
//
//   TLC state : action              -> impl event (stage-side realization)
//   S2        : CoordResetAttempt    -> (coordinator orchestration; the RESET the stage sees at S7)
//   S3–S4     : CoordCrash/Restart   -> (coordinator restart; not stage-visible)
//   S5        : SendBeginRecovery    -> (coordinator emits BEGIN_RECOVERY; the stage sees it at S8)
//   S6        : StageRebuildStep     -> RebuildStep{goal:3}  (catch up applied past truncate_to)
//   S7        : StageRecvResetAt      -> RecvReset{…,truncate_to:1}
//               faithful: applied 3 -> 1 (truncates) | Mut2: applied stays 3 (label-only)
//   S8        : StageRecvBeginAt      -> RecvBegin Case B{truncate_to:1}
//               => Mut2: `CaseBPure` violated at the mapped step S8 (applied 3 > truncate_to 1).

// Faithful build (RESET truncates): the identical sequence completes clean — the replay's dual.
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
