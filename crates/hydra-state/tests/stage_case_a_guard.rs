//! **Refinement (b)'s ratification condition (design authority, 2026-09-09): the H3 guard applies
//! UNCHANGED in the states Case A newly admits.** Spec §1.3 with §6.5a admits a stage REBUILDING or
//! FROZEN_READY at base to Case A (a coordinator that crashed after reconstruction and before COMMIT
//! fences forward onto exactly those stages). The audit-H3 bounds — `target == base + 1` and
//! `0 ≤ truncate_to < n_ctx` — sit BEFORE the state match in `Stage::step`'s `RecvBegin` arm, so they
//! run for every state; this file makes that a directed test: from each newly admitted state a BEGIN
//! failing either bound is REFUSED (Case C: no ack, the refusal effect), and a well-formed one is
//! acknowledged at the target.

use hydra_state::stage::{Stage, StageEffect, StageEvent, StageState};

fn begin(base: u32, target: u32, truncate_to: i64, n_ctx: i64) -> StageEvent {
    StageEvent::RecvBegin { base, target, recovery_id: 1, truncate_to, n_ctx }
}

fn acked(effs: &[StageEffect], target: u32) -> bool {
    effs.iter().any(|e| matches!(e, StageEffect::RecoveryAck { target: t, .. } if *t == target))
}

fn refused(effs: &[StageEffect]) -> bool {
    !effs.iter().any(|e| matches!(e, StageEffect::RecoveryAck { .. }))
}

/// A stage caught up for an activation that never committed: FROZEN_READY at base.
fn frozen_ready_at_base() -> Stage {
    Stage::frozen_ready(0, 0, 0)
}

/// A stage still rebuilding at base: FROZEN → one RebuildStep short of its goal.
fn rebuilding_at_base() -> Stage {
    let mut s = Stage::frozen(0, 0, 0, 0);
    let _ = s.step(StageEvent::RebuildStep { goal: 4 });
    assert_eq!(s.state(), StageState::Rebuilding, "fixture: one step below the goal is REBUILDING");
    s
}

#[test]
fn frozen_ready_at_base_refuses_a_begin_whose_target_is_not_base_plus_one() {
    let mut s = frozen_ready_at_base();
    assert!(refused(&s.step(begin(0, 2, 0, 64))), "target = base + 2 is refused (H3)");
    assert!(refused(&s.step(begin(0, 0, 0, 64))), "target = base is refused (H3)");
    assert_eq!(s.state(), StageState::FrozenReady, "a refused BEGIN touches no state");
}

#[test]
fn frozen_ready_at_base_refuses_a_begin_whose_truncate_to_is_out_of_range() {
    let mut s = frozen_ready_at_base();
    assert!(refused(&s.step(begin(0, 1, -1, 64))), "truncate_to = -1 is refused (H3: would discard everything)");
    assert!(refused(&s.step(begin(0, 1, 64, 64))), "truncate_to = n_ctx is refused (H3)");
    assert_eq!(s.state(), StageState::FrozenReady);
}

#[test]
fn frozen_ready_at_base_takes_a_well_formed_begin_as_case_a() {
    let mut s = frozen_ready_at_base();
    assert!(acked(&s.step(begin(0, 1, 0, 64)), 1), "Case A: RECOVERY_ACK at the target");
    assert_eq!(s.state(), StageState::Frozen);
    assert_eq!(s.epoch(), 1);
}

#[test]
fn rebuilding_at_base_refuses_a_begin_whose_target_is_not_base_plus_one() {
    let mut s = rebuilding_at_base();
    assert!(refused(&s.step(begin(0, 2, 0, 64))));
    assert!(refused(&s.step(begin(0, 0, 0, 64))));
    assert_eq!(s.state(), StageState::Rebuilding, "a refused BEGIN touches no state");
}

#[test]
fn rebuilding_at_base_refuses_a_begin_whose_truncate_to_is_out_of_range() {
    let mut s = rebuilding_at_base();
    assert!(refused(&s.step(begin(0, 1, -1, 64))));
    assert!(refused(&s.step(begin(0, 1, 64, 64))));
    assert_eq!(s.state(), StageState::Rebuilding);
}

#[test]
fn rebuilding_at_base_takes_a_well_formed_begin_as_case_a_and_abandons_its_catch_up() {
    let mut s = rebuilding_at_base();
    assert!(acked(&s.step(begin(0, 1, 0, 64)), 1));
    assert_eq!(s.state(), StageState::Frozen);
    assert_eq!(s.epoch(), 1);
    assert!(s.applied() <= 0, "truncated to [0, truncate_to]: the unfinished catch-up is abandoned");
}
