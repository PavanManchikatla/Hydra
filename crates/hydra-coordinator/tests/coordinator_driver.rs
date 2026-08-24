//! **M4·0 acceptance — the coordinator driver, tested adversarially (standing rule 19).**
//!
//! # Why a happy-path driver here would re-create the exact blindness this slice removes
//!
//! Before M4·0 the activation transaction was hand-rolled in test drivers: `send(COMMIT)` then
//! `send(FINALIZE)`, `activation_attempt_id` hard-coded to `1`, no ack collection, no intent
//! record, no completion record, no abort path. Those drivers **always drove correctly**, so every
//! green result was a statement about correct driving. `hydra_state::Coordinator` — the SM TLC
//! checks — was constructed nowhere outside the simulator.
//!
//! So the tests below do the thing no previous driver could: they **stop the coordinator in the
//! middle**. Each of §6.5's crash windows is a state a real machine can be in and, until this
//! file, nothing in the repository could produce one.
//!
//! | Window | The question §6.5 answers |
//! |---|---|
//! | after INTENT is written, before it is durable | is there an activation in flight at all? |
//! | after INTENT durable, before COMMIT is sent | resume the same attempt, or start another? |
//! | after acks, before COMPLETE is durable | may the decision still be abandoned? |
//! | after COMPLETE durable, before FINALIZE | **the decision is irrevocable; never abort** |
//! | in the superseding window | SUPERSEDING, never re-finalize (I22) |

use std::sync::{Arc, Mutex};

use hydra_coordinator::commit_stream::WalFenceCtx;
use hydra_coordinator::control_wal::ControlWal;
use hydra_coordinator::driver::{ActivationDriver, DriverError, StageLink};
use hydra_state::coordinator::{CoordEvent, CoordState, Coordinator};
use hydra_state::{AuthenticatedRank, SessionId};
use hydra_wire::SessionFence;

fn wal_fence() -> WalFenceCtx {
    WalFenceCtx {
        cluster_id: [7u8; 16],
        manifest_hash: [8u8; 32],
        model_instance_id: [9u8; 16],
        session_id: [1u8; 16],
        epoch: 0,
        recovery_id: 0,
        activation_attempt_id: 0,
    }
}

/// A stage link that records what it was sent. The **rank comes from the link**, exactly as it
/// comes from the authenticated peer in production (audit H4) — a test cannot smuggle a rank in
/// through a frame here any more than a peer can on the wire.
struct RecordingLink {
    rank: AuthenticatedRank,
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl StageLink for RecordingLink {
    fn rank(&self) -> AuthenticatedRank {
        self.rank
    }
    fn send(&mut self, frame: Vec<u8>) -> Result<(), DriverError> {
        self.sent.lock().unwrap().push(frame);
        Ok(())
    }
}

struct Harness {
    driver: ActivationDriver<RecordingLink>,
    sent: Vec<Arc<Mutex<Vec<Vec<u8>>>>>,
    path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn harness(n_stages: u16) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.wal");
    let wal = ControlWal::create(&path, [7u8; 16], [1u8; 16]).expect("control wal");
    let mut sent = Vec::new();
    let links: Vec<RecordingLink> = (0..n_stages)
        .map(|r| {
            let buf = Arc::new(Mutex::new(Vec::new()));
            sent.push(buf.clone());
            RecordingLink { rank: AuthenticatedRank::for_test_harness_asserting_identity(r), sent: buf }
        })
        .collect();
    let coord = Coordinator::new_initial(SessionId([1u8; 16]), n_stages, 1);
    let driver = ActivationDriver::new(coord, wal, wal_fence(), SessionFence::dev(0x40), links);
    Harness { driver, sent, path, _dir: dir }
}

fn rank(r: u16) -> AuthenticatedRank {
    AuthenticatedRank::for_test_harness_asserting_identity(r)
}

/// **The transaction runs end to end through the real SM, with real durable records.**
///
/// The control, and it is not a formality: every assertion below about a *partial* transaction is
/// only meaningful if the whole one works.
#[test]
fn the_activation_transaction_runs_through_the_real_state_machine() {
    let mut h = harness(2);

    h.driver.step(CoordEvent::StagesReconstructed).unwrap();
    let intent = h.driver.step(CoordEvent::ProceedWriteIntent).unwrap();
    assert_eq!(intent.wal_records, vec!["INTENT"], "WAL-before-wire: the intent is durable first");
    assert_eq!(h.driver.state(), CoordState::IntentDurable, "and the SM advanced on the durability, not the write");

    let commit = h.driver.step(CoordEvent::ProceedSendCommit).unwrap();
    assert_eq!(commit.frames_sent, 2, "COMMIT_ACTIVATION goes to every stage");

    // Acks arrive keyed on the AUTHENTICATED rank — the wire carries none (audit H4).
    h.driver.step(CoordEvent::StageCommitted { rank: rank(0), attempt: 1 }).unwrap();
    h.driver.step(CoordEvent::StageCommitted { rank: rank(1), attempt: 1 }).unwrap();

    let complete = h.driver.step(CoordEvent::ProceedWriteComplete).unwrap();
    assert_eq!(complete.wal_records, vec!["COMPLETE"], "the irrevocable decision is durable");

    let finalize = h.driver.step(CoordEvent::ProceedSendFinalize).unwrap();
    assert_eq!(finalize.frames_sent, 2);

    h.driver.step(CoordEvent::StageFinalized { rank: rank(0), attempt: 1 }).unwrap();
    h.driver.step(CoordEvent::StageFinalized { rank: rank(1), attempt: 1 }).unwrap();
    h.driver.step(CoordEvent::ProceedBecomeServiceable).unwrap();
    assert_eq!(h.driver.state(), CoordState::Serviceable, "and only now may the data plane serve (I16/I20)");
}

/// **§6.5 window 1 — the machine dies after the INTENT is written and before it is durable.**
///
/// On a real disk the record may or may not have landed. The classification must be driven by what
/// the log actually holds, and this is the only place in the repository that can produce the state.
#[test]
fn a_crash_between_writing_the_intent_and_its_durability_is_classified_from_the_log() {
    let mut h = harness(2);
    h.driver.step(CoordEvent::StagesReconstructed).unwrap();

    // The write happens; the acknowledgement does not.
    let out = h.driver.step_without_acknowledging_durability(CoordEvent::ProceedWriteIntent).unwrap();
    assert_eq!(out.wal_records, vec!["INTENT"]);
    assert_eq!(
        h.driver.state(),
        CoordState::IntentPending,
        "the SM is still PENDING: it has not been told the write landed, which is the whole point of \
         WAL-before-wire"
    );
    assert_eq!(out.frames_sent, 0, "and NOTHING went on the wire ahead of the durable record");

    // What a restarting coordinator reads.
    drop(h.driver);
    let (_wal, records) = ControlWal::open(&h.path).expect("reopen the control log");
    assert_eq!(records.len(), 1, "the intent is on disk, so a restart sees an activation in flight");
}

/// **§6.5 window 2 — INTENT durable, COMMIT_ACTIVATION not yet sent.**
///
/// The gap TLC-1 exploited: decided-but-not-told. The coordinator must resume the *same* attempt
/// rather than starting a new one, or two attempts exist for one intent.
#[test]
fn a_crash_after_the_intent_is_durable_but_before_the_commit_is_sent_resumes_the_same_attempt() {
    let mut h = harness(2);
    h.driver.step(CoordEvent::StagesReconstructed).unwrap();
    h.driver.step(CoordEvent::ProceedWriteIntent).unwrap();
    assert_eq!(h.driver.state(), CoordState::IntentDurable);
    let attempt_before = h.driver.coordinator().attempt();
    for buf in &h.sent {
        assert!(buf.lock().unwrap().is_empty(), "no frame left the coordinator before the record was durable");
    }

    h.driver.step(CoordEvent::Crash).unwrap();
    let after = h.driver.step(CoordEvent::Restart).unwrap();
    assert_eq!(
        h.driver.state(),
        CoordState::IntentDurable,
        "a durable intent with no completion means the attempt is still in flight (§6.5)"
    );
    assert_eq!(h.driver.coordinator().attempt(), attempt_before, "the SAME attempt resumes; a new one would fence the old");
    assert_eq!(after.frames_sent, 0, "restart classification sends nothing by itself");
}

/// **§6.5 window 3 — all acks in, COMPLETE not yet durable: the decision may still be abandoned.**
///
/// This is the last moment an abort is legal. One step later it is forbidden forever (I25), and
/// the two states are one `fdatasync` apart.
#[test]
fn a_crash_before_the_complete_record_is_durable_leaves_the_decision_abandonable() {
    let mut h = harness(2);
    h.driver.step(CoordEvent::StagesReconstructed).unwrap();
    h.driver.step(CoordEvent::ProceedWriteIntent).unwrap();
    h.driver.step(CoordEvent::ProceedSendCommit).unwrap();
    h.driver.step(CoordEvent::StageCommitted { rank: rank(0), attempt: 1 }).unwrap();
    h.driver.step(CoordEvent::StageCommitted { rank: rank(1), attempt: 1 }).unwrap();

    // Write COMPLETE, do not acknowledge it.
    let out = h.driver.step_without_acknowledging_durability(CoordEvent::ProceedWriteComplete).unwrap();
    assert_eq!(out.wal_records, vec!["COMPLETE"]);
    assert_eq!(h.driver.state(), CoordState::CompletePending);
    assert_eq!(out.frames_sent, 0, "no FINALIZE may precede the durable decision");

    h.driver.step(CoordEvent::Crash).unwrap();
    h.driver.step(CoordEvent::Restart).unwrap();
    assert_ne!(
        h.driver.state(),
        CoordState::Serviceable,
        "a coordinator must not serve on a decision it never made durable"
    );
}

/// **§6.5 window 4 — COMPLETE durable, FINALIZE not sent: the decision is IRREVOCABLE.**
///
/// The restart must re-enter finalization and must never abort. This is the I25 boundary, and it
/// is the reason the COMPLETE record exists at all.
#[test]
fn a_crash_after_the_complete_record_is_durable_re_enters_finalization_and_never_aborts() {
    let mut h = harness(2);
    h.driver.step(CoordEvent::StagesReconstructed).unwrap();
    h.driver.step(CoordEvent::ProceedWriteIntent).unwrap();
    h.driver.step(CoordEvent::ProceedSendCommit).unwrap();
    h.driver.step(CoordEvent::StageCommitted { rank: rank(0), attempt: 1 }).unwrap();
    h.driver.step(CoordEvent::StageCommitted { rank: rank(1), attempt: 1 }).unwrap();
    h.driver.step(CoordEvent::ProceedWriteComplete).unwrap();
    assert_eq!(h.driver.state(), CoordState::ActivationComplete);

    h.driver.step(CoordEvent::Crash).unwrap();
    h.driver.step(CoordEvent::Restart).unwrap();
    assert_eq!(
        h.driver.state(),
        CoordState::ActivationComplete,
        "a durable COMPLETE with unfinalized stages resumes finalization (§6.5), never an abort (I25)"
    );

    // And the record is on disk for a *different process* to classify from.
    let (_wal, records) = ControlWal::open(&h.path).expect("reopen");
    assert_eq!(records.len(), 2, "INTENT + COMPLETE survive the restart");

    // Finalization completes normally afterwards.
    h.driver.step(CoordEvent::ProceedSendFinalize).unwrap();
    h.driver.step(CoordEvent::StageFinalized { rank: rank(0), attempt: 1 }).unwrap();
    h.driver.step(CoordEvent::StageFinalized { rank: rank(1), attempt: 1 }).unwrap();
    h.driver.step(CoordEvent::ProceedBecomeServiceable).unwrap();
    assert_eq!(h.driver.state(), CoordState::Serviceable);
}

/// **§6.5 window 5 — the superseding window (I22).**
///
/// A participant is lost after the durable decision. The recourse is `ACTIVATION_UNSERVABLE` and a
/// superseding recovery; a restart in that window must resume SUPERSEDING and must never re-enter
/// finalization, which would serve under the incomplete configuration.
#[test]
fn a_crash_in_the_superseding_window_resumes_superseding_and_never_re_finalizes() {
    let mut h = harness(2);
    h.driver.step(CoordEvent::StagesReconstructed).unwrap();
    h.driver.step(CoordEvent::ProceedWriteIntent).unwrap();
    h.driver.step(CoordEvent::ProceedSendCommit).unwrap();
    h.driver.step(CoordEvent::StageCommitted { rank: rank(0), attempt: 1 }).unwrap();
    h.driver.step(CoordEvent::StageCommitted { rank: rank(1), attempt: 1 }).unwrap();
    h.driver.step(CoordEvent::ProceedWriteComplete).unwrap();
    h.driver.step(CoordEvent::ProceedSendFinalize).unwrap();
    h.driver.step(CoordEvent::StageFinalized { rank: rank(0), attempt: 1 }).unwrap();
    // Stage 1 is lost after the decision.
    h.driver.step(CoordEvent::StageLost { rank: rank(1) }).unwrap();

    let out = h.driver.step(CoordEvent::ProceedRecordUnservable).unwrap();
    assert_eq!(out.wal_records, vec!["UNSERVABLE"], "the fact is made durable (M6)");
    assert_eq!(h.driver.state(), CoordState::Superseding);

    h.driver.step(CoordEvent::Crash).unwrap();
    let after = h.driver.step(CoordEvent::Restart).unwrap();
    assert_eq!(
        h.driver.state(),
        CoordState::Superseding,
        "the durable UNSERVABLE classifies the restart as SUPERSEDING — re-entering finalization \
         would reopen the I22 hole"
    );
    assert_eq!(after.frames_sent, 0, "and no FINALIZE is emitted on the way");

    let (_wal, records) = ControlWal::open(&h.path).expect("reopen");
    assert_eq!(records.len(), 3, "INTENT + COMPLETE + UNSERVABLE are all on disk");
}

/// **Audit H4 — the quorum counts AUTHENTICATED ranks, and one peer cannot forge it.**
///
/// `ACTIVATION_COMMITTED` carries **no rank on the wire**. The auditor marked H4 SUSPICIOUS
/// because no production coordinator driver existed to inherit the defect. This is that driver,
/// and the rank it counts comes from the link — the peer's authenticated identity — so two acks
/// from the *same* stage cannot complete a two-stage quorum.
#[test]
fn two_acks_from_the_same_authenticated_stage_are_not_a_quorum() {
    let mut h = harness(2);
    h.driver.step(CoordEvent::StagesReconstructed).unwrap();
    h.driver.step(CoordEvent::ProceedWriteIntent).unwrap();
    h.driver.step(CoordEvent::ProceedSendCommit).unwrap();

    h.driver.step(CoordEvent::StageCommitted { rank: rank(0), attempt: 1 }).unwrap();
    h.driver.step(CoordEvent::StageCommitted { rank: rank(0), attempt: 1 }).unwrap();

    // Writing COMPLETE is not enabled: the SM has one ack, not two.
    let out = h.driver.step(CoordEvent::ProceedWriteComplete).unwrap();
    assert!(
        out.wal_records.is_empty(),
        "one stage acking twice is one ack. If this ever writes COMPLETE, a single compromised or \
         buggy peer can carry the whole quorum (audit H4)"
    );
    assert_eq!(h.driver.state(), CoordState::Committing);

    // The real second stage acks, and the transaction proceeds.
    h.driver.step(CoordEvent::StageCommitted { rank: rank(1), attempt: 1 }).unwrap();
    let out = h.driver.step(CoordEvent::ProceedWriteComplete).unwrap();
    assert_eq!(out.wal_records, vec!["COMPLETE"], "control: two DISTINCT stages are a quorum");
}
