//! Post-decision participant loss (spec §6.7, I22) and its Mut1 mutation parity.
//! The named DST scenario: a participant lost *after* the durable decision must be superseded
//! (never served), restoring progress. Mut1 removes the supersession path → the coordinator
//! wedges in a post-decision deadlock (the `PostDecisionLoss` liveness hole).

use hydra_state::CoordEvent::*;
use hydra_state::{invariants, CoordState, Coordinator, SessionId};

fn sid() -> SessionId {
    SessionId([1u8; 16])
}

/// Drive a 2-stage session to `Finalizing` with the durable decision made, stage 0 finalized,
/// and stage 1 permanently lost (the post-decision-loss state).
fn drive_to_post_decision_loss() -> Coordinator {
    let mut c = Coordinator::new_initial(sid(), 2, 1);
    c.step(StagesReconstructed);
    c.step(ProceedWriteIntent);
    c.step(WalDurable(hydra_state::coordinator::WalKindTag::Intent));
    c.step(ProceedSendCommit);
    c.step(StageCommitted { rank: 0, attempt: 1 });
    c.step(StageCommitted { rank: 1, attempt: 1 });
    c.step(ProceedWriteComplete);
    c.step(WalDurable(hydra_state::coordinator::WalKindTag::Complete));
    c.step(ProceedSendFinalize); // -> Finalizing (decision is durable)
    c.step(StageFinalized { rank: 0, attempt: 1 });
    c.step(StageLost { rank: 1 }); // required participant lost after the decision
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
    // record ACTIVATION_UNSERVABLE → SUPERSEDING (decision stands; nothing served)
    c.step(ProceedRecordUnservable);
    assert_eq!(c.state(), CoordState::Superseding);
    assert!(invariants::check(&c).is_empty());
    // open the superseding recovery at epoch+1 → progress restored
    let base_epoch = c.epoch();
    c.step(ProceedStartSuperseding);
    assert_eq!(c.state(), CoordState::Reconstructing);
    assert_eq!(c.epoch(), base_epoch + 1, "superseding recovery is at epoch+1");
    assert!(!c.completed(), "predecessor's COMPLETE does not leak into the new epoch");
    assert!(invariants::check(&c).is_empty());
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
