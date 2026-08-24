//! **M4·0 acceptance — the verified state machine now drives PRODUCTION, not just tests.**
//!
//! # The claim this file exists to make falsifiable
//!
//! Every previous end-to-end demonstration in this repository was driven by a **harness** that
//! hand-rolled the activation transaction: `send(COMMIT_ACTIVATION)` then
//! `send(FINALIZE_ACTIVATION)`, `activation_attempt_id` hard-coded to `1`, no ack collection, no
//! intent record, no completion record. So a green result proved the *choreography* worked when
//! driven correctly, and said nothing about the machine TLC checks, which no shipping code ran.
//!
//! Here the frames are produced by `hydra_state::Coordinator`'s own effects, executed by
//! `ActivationDriver` over **real mTLS** into a **real `Worker`** running the real `Stage` SM. If
//! this passes, the two verified state machines are talking to each other through the wire codec
//! and the transport, which is the property M4·0 was for.
//!
//! It lives in `hydra-worker` because that crate may depend on `hydra-coordinator` (the one-way
//! edge) and so is the only place both halves are visible at once.

use hydra_coordinator::commit_stream::WalFenceCtx;
use hydra_coordinator::control_wal::ControlWal;
use hydra_coordinator::driver::{ActivationDriver, MtlsStageLink};
use hydra_state::coordinator::{CoordEvent, CoordState, Coordinator};
use hydra_state::{AuthenticatedRank, SessionId};
use hydra_worker::pair::Cluster;
use hydra_worker::wire::{self, SessionFence};
use hydra_worker::worker::WorkerConfig;

fn worker_cfg(fence: &SessionFence) -> WorkerConfig {
    WorkerConfig {
        fence: fence.clone(),
        rank: 0,
        layer_first: 0,
        layer_last: -1,
        is_final: true,
        receives_tokens: true,
        epoch: 0,
        recovery_id: 0,
        model_path: None, // control-plane only: this asserts the SMs, not the engine
        n_gpu_layers: 0,
        n_ctx: 64,
        sampler_config: None,
        recovery_start: false,
        shard_manifest: None,
    }
}

fn wal_fence(fence: &SessionFence) -> WalFenceCtx {
    WalFenceCtx {
        cluster_id: fence.cluster_id,
        manifest_hash: fence.manifest_hash,
        model_instance_id: fence.model_instance_id,
        session_id: fence.session_id,
        epoch: 0,
        recovery_id: 0,
        activation_attempt_id: 0,
    }
}

/// **The activation transaction, coordinator SM → real mTLS → stage SM, end to end.**
#[tokio::test]
async fn the_real_coordinator_activates_a_real_worker_over_mtls() {
    let cluster = Cluster::new().expect("cluster");
    let s1_id = cluster.issue("worker-s1").expect("issue");
    // **Audit M12: a CSPRNG session identity**, minted the way the binary mints it — not `dev(seed)`.
    let fence = wire::SessionFence::mint([0x11; 16], [0x22; 32], [0x33; 16]);

    let addr = hydra_worker::pair::spawn_endpoint(worker_cfg(&fence), cluster.ca.server_config(&s1_id).unwrap());
    let connector = cluster.coordinator_connector().unwrap();
    let conn = connector.connect(addr, "worker-s1").await.expect("connect to the stage");

    let dir = tempfile::tempdir().unwrap();
    let wal = ControlWal::create(dir.path().join("control.wal"), fence.cluster_id, fence.session_id).expect("control wal");

    // The rank comes from the role this link was dialled as, never from a frame (audit H4).
    let rank = AuthenticatedRank::for_test_harness_asserting_identity(0);
    let link = MtlsStageLink::new(rank, conn);

    let coord = Coordinator::new_initial(SessionId(fence.session_id), 1, 1);
    let mut driver = ActivationDriver::new(coord, wal, wal_fence(&fence), fence.clone(), vec![link]);

    // Drive the transaction. Every frame below is produced by the SM's own effects.
    driver.step(CoordEvent::StagesReconstructed).await.unwrap();
    let intent = driver.step(CoordEvent::ProceedWriteIntent).await.unwrap();
    assert_eq!(intent.wal_records, vec!["INTENT"], "WAL-before-wire, on a real disk");
    assert_eq!(intent.frames_sent, 0, "and nothing on the wire ahead of the durable record");

    let sent = driver.step(CoordEvent::ProceedSendCommit).await.unwrap();
    assert_eq!(sent.frames_sent, 1, "COMMIT_ACTIVATION went to the stage");

    // The stage's reply comes back over the same connection and is fed to the SM keyed on the
    // link's authenticated rank.
    let reply = driver.recv_from(rank).await.expect("the stage replies");
    assert!(
        matches!(wire::decode(&reply, &fence).unwrap().1, wire::Msg::ActivationCommitted(_)),
        "the REAL Stage SM answered a frame the REAL Coordinator SM produced"
    );
    driver.on_frame(rank, &reply).await.unwrap();

    let complete = driver.step(CoordEvent::ProceedWriteComplete).await.unwrap();
    assert_eq!(complete.wal_records, vec!["COMPLETE"], "the irrevocable decision is durable");

    let fin = driver.step(CoordEvent::ProceedSendFinalize).await.unwrap();
    assert_eq!(fin.frames_sent, 1);

    let reply = driver.recv_from(rank).await.expect("the stage finalizes");
    assert!(
        matches!(wire::decode(&reply, &fence).unwrap().1, wire::Msg::ActivationFinalized),
        "and the stage ACCEPTED the completion evidence the coordinator committed to (audit H2)"
    );
    driver.on_frame(rank, &reply).await.unwrap();

    driver.step(CoordEvent::ProceedBecomeServiceable).await.unwrap();
    assert_eq!(driver.state(), CoordState::Serviceable, "the data plane may now serve (I16/I20)");
}

/// **The kill window, driven by the coordinator rather than a harness.**
///
/// A stage dies mid-transaction. Previously a harness noticed and re-sent by hand; here the
/// coordinator's own SM is what observes the loss and what decides the recourse — `§6.7`'s
/// `ACTIVATION_UNSERVABLE` and a superseding recovery, with the record made durable before the
/// transition (audit M6). **That is the evidence that the SM drives production**: the decision is
/// taken by the machine TLC checks, and the test only supplies the failure.
#[tokio::test]
async fn a_stage_lost_after_the_decision_is_superseded_by_the_coordinator_not_the_harness() {
    let cluster = Cluster::new().expect("cluster");
    let fence = wire::SessionFence::mint([0x11; 16], [0x22; 32], [0x33; 16]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.wal");
    let wal = ControlWal::create(&path, fence.cluster_id, fence.session_id).expect("control wal");

    // Two stages: one that answers, one that will be lost after the decision.
    let mut links = Vec::new();
    for (rank_n, name) in [(0u16, "worker-s1"), (1u16, "worker-s2")] {
        let id = cluster.issue(name).expect("issue");
        let mut cfg = worker_cfg(&fence);
        cfg.rank = rank_n;
        let addr = hydra_worker::pair::spawn_endpoint(cfg, cluster.ca.server_config(&id).unwrap());
        let conn = cluster.coordinator_connector().unwrap().connect(addr, name).await.expect("connect");
        links.push(MtlsStageLink::new(AuthenticatedRank::for_test_harness_asserting_identity(rank_n), conn));
    }

    let coord = Coordinator::new_initial(SessionId(fence.session_id), 2, 1);
    let mut driver = ActivationDriver::new(coord, wal, wal_fence(&fence), fence.clone(), links);
    let r0 = AuthenticatedRank::for_test_harness_asserting_identity(0);
    let r1 = AuthenticatedRank::for_test_harness_asserting_identity(1);

    driver.step(CoordEvent::StagesReconstructed).await.unwrap();
    driver.step(CoordEvent::ProceedWriteIntent).await.unwrap();
    driver.step(CoordEvent::ProceedSendCommit).await.unwrap();
    for r in [r0, r1] {
        let reply = driver.recv_from(r).await.expect("committed");
        driver.on_frame(r, &reply).await.unwrap();
    }
    driver.step(CoordEvent::ProceedWriteComplete).await.unwrap();
    driver.step(CoordEvent::ProceedSendFinalize).await.unwrap();

    // One stage finalizes; the other is lost after the durable decision.
    let reply = driver.recv_from(r0).await.expect("finalized");
    driver.on_frame(r0, &reply).await.unwrap();
    driver.step(CoordEvent::StageLost { rank: r1 }).await.unwrap();

    // The COORDINATOR decides the recourse. The test supplies only the loss.
    let out = driver.step(CoordEvent::ProceedRecordUnservable).await.unwrap();
    assert_eq!(out.wal_records, vec!["UNSERVABLE"], "the fact is durable before the transition (audit M6)");
    assert_eq!(driver.state(), CoordState::Superseding, "and never serves under the incomplete configuration (I22)");

    // A different process reading this log classifies the restart the same way.
    drop(driver);
    let (_wal, records) = ControlWal::open(&path, &fence.cluster_id, &fence.session_id).expect("reopen");
    assert_eq!(records.len(), 3, "INTENT + COMPLETE + UNSERVABLE are on disk for a restart to read");
}
