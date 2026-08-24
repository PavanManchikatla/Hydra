//! Directed tests for the stage-session state machine (M1 slice 2), incl. F2 attempt fencing
//! and its Mut3 mutation parity. Invariants asserted after every step.

use hydra_state::invariants::check_stage;
use hydra_state::stage::{StageEffect, StageEvent::*, StageState};
use hydra_state::{ActivationKind, ActivationTuple, Stage};

fn tuple(attempt: u32) -> ActivationTuple {
    tuple_at(0, attempt)
}

fn tuple_at(epoch: u32, attempt: u32) -> ActivationTuple {
    ActivationTuple {
        kind: ActivationKind::Recovery,
        epoch,
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
    let e = step_ok(&mut s, RecvFinalize { epoch: 0, attempt: 1, completion_id: 0, complete_record_hash: [0u8; 32] });
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

// Mixed-epoch retransmit (F1): a straggler COMMIT_ACTIVATION from a *previous epoch* must be
// rejected by fence-tuple epoch matching (spec §4 / I4-F1), independent of attempt fencing (F2).
// This is the epoch-level companion to `stale_attempt_is_fenced` (which covers F2).
#[test]
fn mixed_epoch_commit_is_rejected_by_f1() {
    // A stage that has advanced to epoch 1 (e.g. after a recovery to target epoch 1).
    let mut s = Stage::frozen_ready(0, 1, 0);
    // A delayed COMMIT for epoch 0 (a mixed-epoch retransmit) must be a no-op — not applied.
    let e = step_ok(&mut s, RecvCommit { tuple: tuple(1) }); // tuple() is epoch 0
    assert!(e.is_empty(), "a COMMIT for the wrong epoch produces no ack (F1 reject)");
    assert_eq!(s.state(), StageState::FrozenReady, "stage does not act on a mixed-epoch message");
    assert_eq!(s.epoch(), 1, "stage stays at its own epoch");
    // the matching-epoch COMMIT is accepted normally, proving the reject was epoch-specific
    let e = step_ok(&mut s, RecvCommit { tuple: tuple_at(1, 1) });
    assert_eq!(s.state(), StageState::Preactive);
    assert!(matches!(e[0], StageEffect::Committed { epoch: 1, .. }));
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

/// **Audit H1 — an attempt ABOVE the window is fenced, and (the real property) the floor DOES NOT
/// MOVE.**
///
/// The fence used to be one-sided: `attempt >= highest_attempt`. That is **unforgeable-past but
/// forgeable-future**. A single `COMMIT_ACTIVATION` carrying a far-future id satisfied it, was
/// accepted, and — because acceptance *adopts* the value as the new floor — **permanently fenced
/// every legitimate attempt that followed**. The session could then never activate, never recover,
/// and §6.4's bound-exhaustion path terminates it: a **silent, permanent denial of activation in
/// which the stage behaved exactly as specified.**
///
/// **The second assertion is the one that matters.** A rejection that still advanced the floor would
/// reproduce the whole defect while *looking* like a fix — the frame is refused, the log says
/// `Fenced`, and the session is dead anyway. Asserting only the rejection would be an
/// over-promising test in exactly the §7.31 sense.
#[test]
fn an_attempt_above_the_window_is_fenced_and_does_not_move_the_floor() {
    let mut s = Stage::frozen_ready(0, 0, 0);
    step_ok(&mut s, RecvCommit { tuple: tuple(1) }); // accept attempt 1; floor := 1
    assert_eq!(s.highest_attempt(), 1);

    // A far-future attempt: the exact shape that used to be accepted.
    for hostile in [3u32, 99, u32::MAX] {
        let e = step_ok(&mut s, RecvCommit { tuple: tuple(hostile) });
        assert!(
            matches!(e[0], StageEffect::Fenced { .. }),
            "attempt {hostile} is above the window {{1, 2}} and must be FENCED, got {e:?}"
        );
        assert_eq!(
            s.highest_attempt(),
            1,
            "a fenced attempt must NOT advance the floor — otherwise the refusal still consumes the \
             attempt space and the denial-of-activation survives the fix (audit H1)"
        );
    }

    // Control: the next legitimate attempt is still accepted, so the window did not close the door
    // on the protocol's own retry path.
    step_ok(&mut s, RecvAbort { attempt: 1 });
    let e = step_ok(&mut s, RecvCommit { tuple: tuple(2) });
    assert!(!matches!(e.first(), Some(StageEffect::Fenced { .. })), "attempt 2 = floor+1 must be accepted");
    assert_eq!(s.attempt(), 2);
    assert_eq!(s.highest_attempt(), 2);
}

/// The window's lower edge is unchanged, so H1 narrows without weakening: an idempotent replay at
/// exactly the floor is still accepted (spec §6.6 step 2 *requires* the stage to re-ack it), and
/// anything below is still stale.
#[test]
fn the_window_still_accepts_an_idempotent_replay_and_still_rejects_a_stale_attempt() {
    let mut s = Stage::frozen_ready(0, 0, 0); // floor starts at 0
    // Reach attempt 1 the way the protocol reaches it — one increment. (Writing `tuple(2)` here
    // fails, and correctly so: the window refuses a jump from floor 0 to attempt 2. That is the
    // amendment biting on the test's own premise, which is worth knowing.)
    step_ok(&mut s, RecvCommit { tuple: tuple(1) }); // floor := 1
    assert_eq!(s.highest_attempt(), 1);

    // replay at the floor — must be re-acked, not fenced (I18 convergence, §6.6 step 2)
    let e = step_ok(&mut s, RecvCommit { tuple: tuple(1) });
    assert!(!matches!(e.first(), Some(StageEffect::Fenced { .. })), "a replay at the floor must re-ack");

    // below the floor — still stale, exactly as before the amendment
    let e = step_ok(&mut s, RecvCommit { tuple: tuple(0) });
    assert!(matches!(e[0], StageEffect::Fenced { attempt: 0, highest: 1, .. }), "got {e:?}");
    assert_eq!(s.highest_attempt(), 1);
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

/// **Audit H2 — a finalize whose evidence names a DIFFERENT activation is refused.**
///
/// # Why this test exists specifically to bound the rollout allowance
///
/// `ACCEPT_LEGACY_ZERO_COMPLETION_HASH` lets an all-zero hash through, so a mixed-version cluster
/// can finish a rollout. The risk in any such allowance is that it quietly widens: someone reads
/// "the hash is not always checked" and concludes the check is advisory. It is not. A **non-zero**
/// hash that does not match the tuple this stage is PREACTIVE on is refused, and the stage stays
/// PREACTIVE rather than becoming serviceable on evidence about some other activation.
///
/// The case is a legitimate coordinator's **stale** finalize — matching epoch, matching attempt,
/// different tuple — which is exactly what the dropped fields left open.
#[test]
fn a_finalize_whose_evidence_names_a_different_activation_is_refused() {
    use hydra_state::stage::ACCEPT_LEGACY_ZERO_COMPLETION_HASH;
    use hydra_state::{ActivationKind, ActivationTuple};

    let mut s = Stage::frozen_ready(0, 0, 0);
    let tuple = ActivationTuple { kind: ActivationKind::Initial, epoch: 0, recovery_id: 0, attempt: 1, sampler_checkpoint_id: 7 };
    step_ok(&mut s, RecvCommit { tuple: tuple.clone() });
    assert_eq!(s.state(), StageState::Preactive);

    // Evidence for a DIFFERENT activation: same epoch, same attempt, different checkpoint — so the
    // attempt fence and the epoch check both pass and only the evidence can catch it.
    let other = ActivationTuple { sampler_checkpoint_id: 9, ..tuple.clone() };
    let wrong = other.completion_hash();
    assert_ne!(wrong, tuple.completion_hash(), "the fixture must actually differ, or this proves nothing");

    let effects = s.step(RecvFinalize { epoch: 0, attempt: 1, completion_id: 5, complete_record_hash: wrong });
    assert!(effects.is_empty(), "a finalize carrying another activation's evidence must not finalize this one");
    assert_eq!(s.state(), StageState::Preactive, "and the stage stays PREACTIVE — never serviceable on foreign evidence");
    assert!(!s.holds_final_evidence());

    // The matching evidence finalizes.
    let effects = s.step(RecvFinalize { epoch: 0, attempt: 1, completion_id: 5, complete_record_hash: tuple.completion_hash() });
    assert_eq!(effects.len(), 1, "the correct evidence finalizes");
    assert_eq!(s.state(), StageState::ActiveFinal);
    assert_eq!(s.completion_id(), 5, "and the completion id is adopted (spec §6.6 step 4)");

    // The allowance is exactly one value, and it is dated. Asserted as a runtime comparison so the
    // compiler does not fold it away: when the constant goes `false` this test fails and tells the
    // next reader to delete the §8 v1-blocking row and this block together.
    assert_eq!(
        ACCEPT_LEGACY_ZERO_COMPLETION_HASH,
        std::hint::black_box(true),
        "the zero-hash rollout allowance is now off — delete the §8 v1-blocking row and this assertion"
    );
}
