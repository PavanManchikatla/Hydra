//! M2 slice 5 sub-slice C (part 1) — **worker recovery support**, driven through the *real*
//! `hydra-state` stage SM over real mTLS. Control-plane only (no engine) → runs everywhere.
//!
//! Proves the three C-discovery orchestration fixes compose into a valid recovery entry:
//!   1. a recovery-replacement worker starts `FROZEN` and accepts `BEGIN_RECOVERY` **Case A**;
//!   2. `CATCH_UP_CONTEXT{goal}` drives the stage's `RebuildStep` to `FROZEN_READY`;
//!   3. the worker emits `CATCH_UP_READY`; then the activation transaction commits and finalizes.
//!
//! Every transition is a real `Stage::step` — no shortcut around the DST-tested path.

use std::net::SocketAddr;
use std::sync::mpsc;

use hydra_transport::tcp_mtls::{TcpMtls, TcpMtlsListener};
use hydra_transport::ClusterCa;
use hydra_worker::wire::{self, Msg, SessionKeys};
use hydra_worker::worker::{serve_conn, Worker, WorkerConfig};

const W: &str = "worker-r";
const C: &str = "coordinator";

fn spawn_recovery_worker(cfg: WorkerConfig, server_cfg: rustls::ServerConfig) -> SocketAddr {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = TcpMtlsListener::bind_with_config("127.0.0.1:0".parse().unwrap(), server_cfg).await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            let mut worker = Worker::new(cfg).expect("worker");
            let mut conn = listener.accept().await.unwrap();
            let _ = serve_conn(&mut worker, &mut conn).await;
        });
    });
    rx.recv().unwrap()
}

#[tokio::test]
async fn recovery_replacement_reaches_active_final_through_the_real_stage_sm() {
    let ca = ClusterCa::new().unwrap();
    let w_id = ca.issue(W).unwrap();
    let c_id = ca.issue(C).unwrap();
    let keys = SessionKeys::dev(0xC5);

    // A recovery-replacement worker: starts FROZEN (recovery_start = true), control-plane only.
    let cfg = WorkerConfig {
        keys: keys.clone(), rank: 0, layer_first: 0, layer_last: -1, is_final: true, receives_tokens: true,
        epoch: 0, recovery_id: 0, model_path: None, n_gpu_layers: 0, n_ctx: 64, sampler_config: None,
        recovery_start: true,
    };
    let addr = spawn_recovery_worker(cfg, ca.server_config(&w_id).unwrap());
    let connector = TcpMtls::from_config(ca.client_config(&c_id).unwrap()).unwrap();
    let mut conn = connector.connect(addr, W).await.expect("connect");

    // 1. BEGIN_RECOVERY Case A (base=target=0, new recovery_id=1, truncate_to=0 for a fresh replica).
    conn.send(0, &wire::encode_begin_recovery(&keys, 0, 0, 1, 0)).await.unwrap();
    match wire::decode(&conn.recv().await.unwrap().payload, &keys).unwrap().1 {
        Msg::RecoveryAck { .. } => {}
        other => panic!("Case A must ack RECOVERY_ACK, got {other:?}"),
    }

    // 2. CATCH_UP_CONTEXT{goal=3} → the stage RebuildStep-s to FROZEN_READY → CATCH_UP_READY.
    conn.send(0, &wire::encode_catch_up_context(&keys, 0, 1, 3)).await.unwrap();
    match wire::decode(&conn.recv().await.unwrap().payload, &keys).unwrap().1 {
        Msg::CatchUpReady { applied_input_pos } => assert_eq!(applied_input_pos, 3, "caught up to goal"),
        other => panic!("catch-up must ack CATCH_UP_READY, got {other:?}"),
    }

    // 3. Activation transaction on the recovered stage: COMMIT → COMMITTED, FINALIZE → FINALIZED.
    let tuple = hydra_state::ActivationTuple {
        kind: hydra_state::ActivationKind::Recovery, epoch: 0, recovery_id: 1, attempt: 0, sampler_checkpoint_id: 0,
    };
    conn.send(0, &wire::encode_commit_activation(&keys, &tuple, 1)).await.unwrap();
    match wire::decode(&conn.recv().await.unwrap().payload, &keys).unwrap().1 {
        Msg::ActivationCommitted(t) => assert_eq!((t.epoch, t.recovery_id, t.attempt), (0, 1, 0)),
        other => panic!("expected ActivationCommitted, got {other:?}"),
    }
    conn.send(0, &wire::encode_finalize_activation(&keys, &tuple, 1)).await.unwrap();
    assert!(
        matches!(wire::decode(&conn.recv().await.unwrap().payload, &keys).unwrap().1, Msg::ActivationFinalized),
        "the recovered stage reaches ACTIVE_FINAL"
    );
}

#[tokio::test]
async fn fresh_worker_does_not_accept_case_a() {
    // Guard: a NON-recovery worker (FROZEN_READY) must not satisfy Case A — the recovery-start flag
    // is load-bearing, not cosmetic. Exercised directly on the Stage to keep it engine-free.
    let mut fresh = hydra_state::Stage::frozen_ready(0, 0, 0);
    let effs = fresh.step(hydra_state::StageEvent::RecvBegin { base: 0, target: 0, recovery_id: 1, truncate_to: 0 });
    assert!(effs.is_empty(), "FROZEN_READY is not a Case-A entry state (no RECOVERY_ACK)");

    let mut recovering = hydra_state::Stage::frozen(0, 0, 0, 0);
    let effs = recovering.step(hydra_state::StageEvent::RecvBegin { base: 0, target: 0, recovery_id: 1, truncate_to: 0 });
    assert!(!effs.is_empty(), "FROZEN accepts Case A");
}
