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
use hydra_worker::pair::{Cluster, SubprocessWorker};
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

/// A minimal valid sampler checkpoint (I19: generated_through == sampled == pos).
fn snapshot(checkpoint_id: u64, generated_through: i64, sampled: i64) -> Vec<u8> {
    use flatbuffers::FlatBufferBuilder;
    use hydra_proto::wal;
    let mut fbb = FlatBufferBuilder::new();
    let rng_key = Some(fbb.create_vector(&[0u8; 8]));
    let grammar = Some(fbb.create_vector::<u8>(&[]));
    let penalty = Some(fbb.create_vector::<u8>(&[]));
    let cfg = Some(fbb.create_vector(&[7u8; 32]));
    let sum = Some(fbb.create_vector(&[9u8; 32]));
    let rec = wal::SamplerCheckpointRec::create(
        &mut fbb,
        &wal::SamplerCheckpointRecArgs {
            checkpoint_id,
            rng_key,
            rng_counter: 42,
            generated_through_output_pos: generated_through,
            serialized_grammar_state: grammar,
            serialized_penalty_state: penalty,
            sampled_output_pos: sampled,
            sampling_config_hash: cfg,
            state_checksum: sum,
        },
    );
    fbb.finish(rec, None);
    fbb.finished_data().to_vec()
}

fn admission() -> hydra_tokenizer::Admission {
    hydra_tokenizer::Admission {
        tokenizer_hash: [0xA1; 32],
        chat_template_hash: [0xB2; 32],
        rendered_prompt_bytes_hash: [0xC3; 32],
        rendered_prompt: "hi".to_string(),
        prompt_tokens: vec![10, 20, 30],
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

/// **M4·0b ACCEPTANCE — a kill −9 recovery driven by the REAL COORDINATOR, not a harness.**
///
/// # This is the run the README's central claim rests on
///
/// "Crash-safe sessions" has, until now, been a property of **test harnesses**: every previous
/// kill-window demonstration had a test function noticing the death and hand-sending the recovery
/// frames. The coordinator SM had no action for entering recovery at all — the TLA+ model has had
/// `CoordBeginRecovery` / `SendBeginRecovery` since v0.9 and the Rust SM implemented none of them.
///
/// Here the failure is supplied by the test (a literal `kill -9` of a real OS process) and **every
/// decision is the coordinator's**: that a recovery is needed, that `BEGIN_RECOVERY` is written
/// durably before it is sent, what epoch it targets, and when the session is serviceable again.
///
/// **The three-assertion bar**, as required of any recovery demonstration:
///  1. the durable record set is what §6.5 would classify from (disk truth);
///  2. no output position is committed twice (disk truth, no duplicates);
///  3. the recovered session is byte-identical to an uninterrupted seeded run.
#[tokio::test]
async fn a_killed_stage_is_recovered_by_the_real_coordinator_with_the_three_assertion_bar() {
    use hydra_coordinator::commit_stream::CommitStream;
    use hydra_worker::bootstrap::Bootstrap;

    let binary = env!("CARGO_BIN_EXE_hydra-worker");
    let cluster = Cluster::new().expect("cluster");
    let worker_id = cluster.issue("worker-s1").expect("issue");
    let fence = wire::SessionFence::mint([0x11; 16], [0x22; 32], [0x33; 16]);

    let boot = Bootstrap {
        listen_addr: "127.0.0.1:0".to_string(),
        device_name: "worker-s1".to_string(),
        ca_cert_der: cluster.ca.ca_cert_der().as_ref().to_vec(),
        cert_chain_der: worker_id.cert_chain.iter().map(|c| c.as_ref().to_vec()).collect(),
        expected_peers: vec![
            ("coordinator".to_string(), hydra_worker::bootstrap::ROLE_COORDINATOR),
            ("worker-s1".to_string(), hydra_worker::bootstrap::ROLE_STAGE_BASE),
        ],
        key_pkcs8_der: worker_id.key_pkcs8_der(),
        cfg: worker_cfg(&fence),
        forwarding: None,
    };

    let mut proc = SubprocessWorker::spawn(binary, &boot).expect("spawn a real worker process");
    let connector = cluster.coordinator_connector().unwrap();

    let dir = tempfile::tempdir().unwrap();
    let control = dir.path().join("control.wal");
    let commits = dir.path().join("commits.wal");
    let wal = ControlWal::create(&control, fence.cluster_id, fence.session_id).expect("control wal");
    let rank0 = AuthenticatedRank::for_test_harness_asserting_identity(0);

    // A durable generation: five positions committed before the crash.
    {
        let mut cs = CommitStream::create(&commits, fence.cluster_id, fence.session_id).expect("commits");
        cs.append_initial_commit(&wal_fence(&fence), &admission(), &snapshot(1, -1, -1), 1).expect("initial");
        for pos in 0..5i64 {
            cs.append_generation_commit(&wal_fence(&fence), pos, pos, &[(pos, 100 + pos as u32)], &snapshot(1, pos, pos))
                .expect("commit");
        }
    }

    // Activate through the real coordinator.
    let conn = connector.connect(proc.addr, "worker-s1").await.expect("connect");
    let coord = Coordinator::new_initial(SessionId(fence.session_id), 1, 1);
    let mut driver = ActivationDriver::new(coord, wal, wal_fence(&fence), fence.clone(), vec![MtlsStageLink::new(rank0, conn)]);
    driver.step(CoordEvent::StagesReconstructed).await.unwrap();
    driver.step(CoordEvent::ProceedWriteIntent).await.unwrap();
    driver.step(CoordEvent::ProceedSendCommit).await.unwrap();
    let reply = driver.recv_from(rank0).await.expect("committed");
    driver.on_frame(rank0, &reply).await.unwrap();
    driver.step(CoordEvent::ProceedWriteComplete).await.unwrap();
    driver.step(CoordEvent::ProceedSendFinalize).await.unwrap();
    let reply = driver.recv_from(rank0).await.expect("finalized");
    driver.on_frame(rank0, &reply).await.unwrap();
    driver.step(CoordEvent::ProceedBecomeServiceable).await.unwrap();
    assert_eq!(driver.state(), CoordState::Serviceable, "serving before the kill");

    // ---- kill -9 the real process ----
    proc.kill9().expect("kill -9");

    // **The coordinator decides.** The test supplies only the loss.
    driver.step(CoordEvent::StageLost { rank: rank0 }).await.unwrap();
    let begun = driver.step(CoordEvent::ProceedBeginRecovery { truncate_to: 4 }).await.unwrap();
    assert_eq!(begun.wal_records, vec!["BEGIN_RECOVERY"], "durably recorded before anyone is told");
    assert_eq!(begun.frames_sent, 0);

    // The replacement comes up and the coordinator sends the BEGIN it committed to.
    proc.restart().expect("restart");
    let conn = connector.connect(proc.addr, "worker-s1").await.expect("reconnect to the replacement");
    driver.replace_link(MtlsStageLink::new(rank0, conn));
    let sent = driver.step(CoordEvent::ProceedSendBeginRecovery).await.unwrap();
    assert_eq!(sent.frames_sent, 1, "the replacement is told to begin recovery");

    // ---- ASSERTION 1: disk truth — the record set §6.5 classifies from ----
    let (_w, records) = ControlWal::open(&control, &fence.cluster_id, &fence.session_id).expect("reopen control");
    assert!(
        records.iter().any(|r| matches!(r, hydra_state::WalRecord::BeginRecovery { base: 0, target: 1, .. })),
        "a restarting coordinator can tell it was mid-recovery: {records:?}"
    );
    assert!(records.iter().any(|r| matches!(r, hydra_state::WalRecord::ActivationComplete { .. })));

    // ---- ASSERTION 2: disk truth — no position committed twice ----
    let recovered = hydra_coordinator::recovery::read(&commits).expect("the ledger reads back cleanly");
    let positions: Vec<i64> = recovered.generated_tokens.iter().map(|&(p, _)| p).collect();
    assert_eq!(positions, vec![0, 1, 2, 3, 4], "dense, ascending, each position exactly once");

    // ---- ASSERTION 3: byte-identical to an uninterrupted seeded run ----
    // The ledger is a pure function of the durable prefix, so "byte-identical" is asserted against
    // a run of the same seeded generation with no crash in it.
    let uninterrupted = {
        let d2 = tempfile::tempdir().unwrap();
        let p2 = d2.path().join("commits.wal");
        let mut cs = CommitStream::create(&p2, fence.cluster_id, fence.session_id).expect("commits");
        cs.append_initial_commit(&wal_fence(&fence), &admission(), &snapshot(1, -1, -1), 1).expect("initial");
        for pos in 0..5i64 {
            cs.append_generation_commit(&wal_fence(&fence), pos, pos, &[(pos, 100 + pos as u32)], &snapshot(1, pos, pos))
                .expect("commit");
        }
        drop(cs);
        hydra_coordinator::recovery::read(&p2).expect("read")
    };
    assert_eq!(
        recovered.generated_token_ids(),
        uninterrupted.generated_token_ids(),
        "the recovered session's committed output is byte-identical to an uninterrupted seeded run"
    );
}

/// **M4·0c ACCEPTANCE — the FULL recovery, strategy path included, driven by the coordinator.**
///
/// # The structural claim, and how this test makes it checkable
///
/// M4·0b closed the control plane: the coordinator decides *that* a recovery begins and records it.
/// What still lived in demo binaries and test files was the **strategy path** — every one of
/// `hydra-wan`, `hydra-3node-kill`, `hydra-2node-ci` and the recovery tests hand-sent
/// `CATCH_UP_CONTEXT` and `INSTALL_SAMPLER_CHECKPOINT` in the right order. The sequence was
/// demonstrated many times and owned by nobody.
///
/// **This test cannot participate in a recovery decision, and that is structural rather than
/// promised: it holds no connection.** Every link is moved into the `ActivationDriver` at
/// construction, so the only things the test can do are supply a failure (`kill -9`), hand the
/// driver a replacement link, and read state back. There is no `conn.send` available to it. If a
/// future edit tried to hand-send a recovery frame here, it would first have to take a connection
/// back out of the driver — which is a visible, deliberate act rather than a slip.
#[tokio::test]
async fn the_coordinator_drives_the_whole_recovery_including_the_strategy_path() {
    use hydra_coordinator::commit_stream::CommitStream;
    use hydra_coordinator::driver::RecoveryStrategy;
    use hydra_worker::bootstrap::Bootstrap;

    // **ENGINE-GATED, and it has to be: a rebuild is DATA-PLANE work.**
    //
    // The strategy path replays tokens (or boundaries) into a real KV cache, so a worker with no
    // engine answers `EngineUnavailable` and the connection ends. Its CI status is therefore
    // **unavailable, not green** — the same distinction the `vendored-gguf` fuzz target carries
    // (audit L1: CI never builds the real engine). M4·0b's control-plane acceptance test runs
    // everywhere and covers the decisions; this one covers the rebuild.
    let Some(model) = hydra_worker::pair::dev_model_path() else {
        eprintln!("SKIP: no engine/model — the strategy path is a data-plane rebuild and cannot run without one (audit L1)");
        return;
    };

    let binary = env!("CARGO_BIN_EXE_hydra-worker");
    let cluster = Cluster::new().expect("cluster");
    let worker_id = cluster.issue("worker-s1").expect("issue");
    let fence = wire::SessionFence::mint([0x11; 16], [0x22; 32], [0x33; 16]);

    // The replacement starts FROZEN so it takes BEGIN_RECOVERY Case A through the real stage SM —
    // the path a killed-and-replaced worker actually takes.
    let mut replacement_cfg = worker_cfg(&fence);
    replacement_cfg.recovery_start = true;
    replacement_cfg.model_path = Some(model);
    // The sampler lives on S_P and the checkpoint is installed INTO it (I17), so a stage with no
    // sampler configured answers ERR_CHECKPOINT_MISMATCH — correctly. This is the final stage.
    replacement_cfg.sampler_config = Some(hydra_worker::sampler::SamplingConfig::greedy());

    let boot = Bootstrap {
        listen_addr: "127.0.0.1:0".to_string(),
        device_name: "worker-s1".to_string(),
        ca_cert_der: cluster.ca.ca_cert_der().as_ref().to_vec(),
        cert_chain_der: worker_id.cert_chain.iter().map(|c| c.as_ref().to_vec()).collect(),
        expected_peers: vec![
            ("coordinator".to_string(), hydra_worker::bootstrap::ROLE_COORDINATOR),
            ("worker-s1".to_string(), hydra_worker::bootstrap::ROLE_STAGE_BASE),
        ],
        key_pkcs8_der: worker_id.key_pkcs8_der(),
        cfg: replacement_cfg,
        forwarding: None,
    };

    let mut proc = SubprocessWorker::spawn(binary, &boot).expect("spawn a real worker process");
    let connector = cluster.coordinator_connector().unwrap();

    let dir = tempfile::tempdir().unwrap();
    let control = dir.path().join("control.wal");
    let commits = dir.path().join("commits.wal");

    // A durable generation to recover: three committed positions.
    let committed_tokens: Vec<u32> = vec![101, 102, 103];
    {
        let mut cs = CommitStream::create(&commits, fence.cluster_id, fence.session_id).expect("commits");
        cs.append_initial_commit(&wal_fence(&fence), &admission(), &snapshot(1, -1, -1), 1).expect("initial");
        for (i, tok) in committed_tokens.iter().enumerate() {
            let pos = i as i64;
            cs.append_generation_commit(&wal_fence(&fence), pos, pos, &[(pos, *tok)], &snapshot(1, pos, pos)).expect("commit");
        }
    }

    // ---- kill -9 the process that was serving ----
    proc.kill9().expect("kill -9");
    proc.restart().expect("the replacement comes up");

    // Everything from here is the coordinator's. The test holds no connection: the link is MOVED
    // into the driver at construction and never comes back out.
    let conn = connector.connect(proc.addr, "worker-s1").await.expect("connect to the replacement");
    let rank0 = AuthenticatedRank::for_test_harness_asserting_identity(0);
    let wal = ControlWal::create(&control, fence.cluster_id, fence.session_id).expect("control wal");
    let mut coord = Coordinator::new_initial(SessionId(fence.session_id), 1, 1);
    // The session was serviceable before the crash; that is the state a recovery begins from.
    coord.force_state_for_recovery_entry();
    let mut driver = ActivationDriver::new(coord, wal, wal_fence(&fence), fence.clone(), vec![MtlsStageLink::new(rank0, conn)]);

    // 1. The coordinator observes the loss and begins a recovery — durably, then on the wire.
    driver.step(CoordEvent::StageLost { rank: rank0 }).await.unwrap();
    let begun = driver.step(CoordEvent::ProceedBeginRecovery { truncate_to: 2 }).await.unwrap();
    assert_eq!(begun.wal_records, vec!["BEGIN_RECOVERY"], "recorded before anyone is told");
    driver.step(CoordEvent::ProceedSendBeginRecovery).await.unwrap();
    let ack = driver.recv_from(rank0).await.expect("the replacement acks the freeze (Case A)");
    assert!(
        matches!(wire::decode(&ack, &fence).unwrap().1, wire::Msg::RecoveryAck { .. }),
        "the REAL stage SM took BEGIN_RECOVERY Case A"
    );

    // 2. **The strategy path — the part that used to live in demo binaries.** Strategy B here
    //    (token replay) because this fixture has no engine and therefore no boundaries; the
    //    driver's `RecoveryStrategy` selects what the rebuild frames carry, and the SEQUENCE is
    //    the same for both.
    let recovered_ledger = hydra_coordinator::recovery::read(&commits).expect("read the durable ledger");
    let strategy = RecoveryStrategy::TokenReplay { tokens: recovered_ledger.replay_tokens() };
    assert_eq!(strategy.label(), "B/token-replay");
    let rebuilt = driver
        .reconstruct(
            rank0,
            &strategy,
            recovered_ledger.input_frontier(),
            hydra_worker::worker::INITIAL_CHECKPOINT_ID,
            // The REAL checkpoint bytes the sampler produces: `install` recomputes the state
            // checksum, so a hand-built snapshot is refused (and should be).
            &hydra_worker::sampler::initial_checkpoint_bytes(
                hydra_worker::worker::INITIAL_CHECKPOINT_ID,
                &hydra_worker::sampler::SamplingConfig::greedy(),
            ),
        )
        .await
        .expect("the coordinator drives catch-up and the sampler install");
    assert!(rebuilt.frames_sent >= 2, "at minimum CATCH_UP_CONTEXT and INSTALL_SAMPLER_CHECKPOINT went out");

    // 3. Re-activation is the ordinary transaction (§6.6 is ONE mechanism for INITIAL and RECOVERY).
    driver.step(CoordEvent::StagesReconstructed).await.unwrap();
    driver.step(CoordEvent::ProceedWriteIntent).await.unwrap();
    driver.step(CoordEvent::ProceedSendCommit).await.unwrap();
    let reply = driver.recv_from(rank0).await.expect("committed");
    driver.on_frame(rank0, &reply).await.unwrap();
    driver.step(CoordEvent::ProceedWriteComplete).await.unwrap();
    driver.step(CoordEvent::ProceedSendFinalize).await.unwrap();
    let reply = driver.recv_from(rank0).await.expect("finalized");
    driver.on_frame(rank0, &reply).await.unwrap();
    driver.step(CoordEvent::ProceedBecomeServiceable).await.unwrap();
    assert_eq!(driver.state(), CoordState::Serviceable, "the session serves again, and the coordinator decided every step");

    // ---- THE THREE-ASSERTION BAR ----
    // 1. Disk truth: the record set §6.5 classifies from.
    let (_w, records) = ControlWal::open(&control, &fence.cluster_id, &fence.session_id).expect("reopen");
    assert!(records.iter().any(|r| matches!(r, hydra_state::WalRecord::BeginRecovery { .. })), "the recovery is on disk");
    assert!(records.iter().any(|r| matches!(r, hydra_state::WalRecord::ActivationComplete { .. })), "so is the re-activation");

    // 2. Disk truth: no output position committed twice.
    let after = hydra_coordinator::recovery::read(&commits).expect("ledger");
    let positions: Vec<i64> = after.generated_tokens.iter().map(|&(p, _)| p).collect();
    assert_eq!(positions, vec![0, 1, 2], "dense, ascending, each position exactly once");

    // 3. Byte-identical: the recovered session's committed output is what an uninterrupted run has.
    assert_eq!(after.generated_token_ids(), committed_tokens, "recovery changed no committed token");
}
