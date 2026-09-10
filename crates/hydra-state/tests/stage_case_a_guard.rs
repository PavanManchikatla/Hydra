//! **Refinement (b)'s ratification condition (design authority, 2026-09-09): the H3 guard applies
//! UNCHANGED in the states Case A newly admits.** Spec §1.3 with §6.5a admits a stage REBUILDING or
//! FROZEN_READY at base to Case A (a coordinator that crashed after reconstruction and before COMMIT
//! fences forward onto exactly those stages). The audit-H3 bounds — `target == base + 1` and
//! `0 ≤ truncate_to < n_ctx` — sit BEFORE the state match in `Stage::step`'s `RecvBegin` arm, so they
//! run for every state; this file makes that a directed test: from each newly admitted state a BEGIN
//! failing either bound is REFUSED (Case C: no ack, the refusal effect), and a well-formed one is
//! acknowledged at the target.

//!
//! **Spec v0.10.5 (2026-09-10, ruling item 1): the sentinel case.** `truncate_to = EMPTY` (`-1`) is
//! the ONE admitted negative — Case A with it discards everything (`applied = -1`) — from each of the
//! admitted states; every other negative stays refused, and the sentinel does not relax the `target`
//! bound.

use hydra_state::stage::{Stage, StageEffect, StageEvent, StageState, TRUNCATE_TO_EMPTY};

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
    assert!(refused(&s.step(begin(0, 1, -2, 64))), "truncate_to = -2 is refused (H3: only the EMPTY sentinel is admitted below 0)");
    assert!(refused(&s.step(begin(0, 1, i64::MIN, 64))), "truncate_to = i64::MIN is refused (H3)");
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
    assert!(refused(&s.step(begin(0, 1, -2, 64))));
    assert!(refused(&s.step(begin(0, 1, i64::MIN, 64))));
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

// ---- spec v0.10.5: the EMPTY sentinel ----

#[test]
fn the_sentinel_is_minus_one_and_nothing_else_below_zero_is_admitted() {
    assert_eq!(TRUNCATE_TO_EMPTY, -1, "the wire/WAL value the spec names");
}

#[test]
fn frozen_ready_at_base_takes_an_empty_begin_as_case_a_and_discards_everything() {
    let mut s = frozen_ready_at_base();
    assert!(acked(&s.step(begin(0, 1, TRUNCATE_TO_EMPTY, 64)), 1), "Case A with EMPTY: RECOVERY_ACK at the target");
    assert_eq!(s.state(), StageState::Frozen);
    assert_eq!(s.epoch(), 1);
    assert_eq!(s.applied(), -1, "EMPTY discards everything: the fresh-shard value, the rebuild starts at 0");
}

#[test]
fn rebuilding_at_base_takes_an_empty_begin_as_case_a_and_discards_everything() {
    let mut s = rebuilding_at_base();
    assert!(acked(&s.step(begin(0, 1, TRUNCATE_TO_EMPTY, 64)), 1));
    assert_eq!(s.state(), StageState::Frozen);
    assert_eq!(s.applied(), -1);
}

#[test]
fn an_active_survivor_takes_an_empty_begin_as_case_a_and_discards_its_whole_prefix() {
    // The product case: S1 ACTIVE_FINAL holding the whole prefix, S_P (downstream) lost in D0.
    let mut s = Stage::frozen(0, 0, 0, 0);
    let _ = s.step(StageEvent::RebuildStep { goal: 7 });
    while s.state() != StageState::FrozenReady { let _ = s.step(StageEvent::RebuildStep { goal: 7 }); }
    assert_eq!(s.applied(), 7);
    assert!(acked(&s.step(begin(0, 1, TRUNCATE_TO_EMPTY, 64)), 1));
    assert_eq!(s.applied(), -1, "a survivor that cannot re-emit its prefix (§2.3d) gives all of it up");
    // and a non-EMPTY BEGIN would have kept it (the contrast the sentinel exists for)
    let mut k = Stage::frozen(0, 0, 0, 0);
    while k.state() != StageState::FrozenReady { let _ = k.step(StageEvent::RebuildStep { goal: 7 }); }
    assert!(acked(&k.step(begin(0, 1, 7, 64)), 1));
    assert_eq!(k.applied(), 7, "truncate_to = 7 keeps positions 0..=7");
}

#[test]
fn the_sentinel_does_not_relax_the_target_bound() {
    let mut s = frozen_ready_at_base();
    assert!(refused(&s.step(begin(0, 2, TRUNCATE_TO_EMPTY, 64))), "target = base + 2 with EMPTY is still refused");
    assert_eq!(s.state(), StageState::FrozenReady, "a refused BEGIN touches no state");
}
