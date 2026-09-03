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
use hydra_worker::wire::{self, Msg, SessionFence};
use hydra_worker::worker::{serve_conn, Worker, WorkerConfig};

const W: &str = "worker-r";
const C: &str = "coordinator";

fn spawn_recovery_worker(cfg: WorkerConfig, server_cfg: rustls::ServerConfig) -> SocketAddr {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = TcpMtlsListener::bind_with_config(
                "127.0.0.1:0".parse().unwrap(),
                server_cfg,
                hydra_worker::pair::dev_role_table(),
            )
            .await
            .unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            let mut worker = Worker::new(cfg).expect("worker");
            // Audit C2: the connection arrives with its bound role; `serve_conn` gates each message
            // family against it.
            let a = listener.accept().await.unwrap();
            let (mut conn, role) = (a.conn, a.peer.role);
            let _ = serve_conn(&mut worker, &mut conn, role).await;
        });
    });
    rx.recv().unwrap()
}

#[tokio::test]
async fn recovery_replacement_reaches_active_final_through_the_real_stage_sm() {
    let ca = ClusterCa::new().unwrap();
    let w_id = ca.issue(W).unwrap();
    let c_id = ca.issue(C).unwrap();
    let fence = SessionFence::dev(0xC5);

    // A recovery-replacement worker: starts FROZEN (recovery_start = true), control-plane only.
    let cfg = WorkerConfig {
        fence: fence.clone(), rank: 0, layer_first: 0, layer_last: -1, is_final: true, receives_tokens: true,
        epoch: 0, recovery_id: 0, model_path: None, n_gpu_layers: 0, n_ctx: 64, sampler_config: None,
        recovery_start: true, shard_manifest: None,
    };
    let addr = spawn_recovery_worker(cfg, ca.server_config(&w_id).unwrap());
    let connector = TcpMtls::from_config(ca.client_config(&c_id).unwrap()).unwrap();
    let mut conn = connector.connect(addr, W).await.expect("connect");

    // 1. BEGIN_RECOVERY Case A — base 0 → **target 1**, new recovery_id=1, truncate_to=0 (fresh
    //    replica). **Audit H3 (2026-08-23): this used to send `base = target = 0`** — a degenerate
    //    "transition" the coordinator can never produce (spec §1.3 and the TLA+
    //    `SendBeginRecovery` both emit `target = base + 1` by construction). The harness was
    //    exercising a shape the protocol has no way to generate, which is why nothing here could
    //    have caught the missing `target == base + 1` bound.
    conn.send(0, &wire::encode_begin_recovery(&fence, 0, 1, 1, 0)).await.unwrap();
    match wire::decode(&conn.recv().await.unwrap().payload, &fence).unwrap().1 {
        Msg::RecoveryAck { .. } => {}
        other => panic!("Case A must ack RECOVERY_ACK, got {other:?}"),
    }

    // 2. CATCH_UP_CONTEXT{goal=3} → the stage RebuildStep-s to FROZEN_READY → CATCH_UP_READY.
    conn.send(0, &wire::encode_catch_up_context(&fence, 1, 1, 3)).await.unwrap();
    match wire::decode(&conn.recv().await.unwrap().payload, &fence).unwrap().1 {
        Msg::CatchUpReady { applied_input_pos } => assert_eq!(applied_input_pos, 3, "caught up to goal"),
        other => panic!("catch-up must ack CATCH_UP_READY, got {other:?}"),
    }

    // 3. Activation transaction on the recovered stage: COMMIT → COMMITTED, FINALIZE → FINALIZED.
    let tuple = hydra_state::ActivationTuple {
        kind: hydra_state::ActivationKind::Recovery, epoch: 1, recovery_id: 1, attempt: 0, sampler_checkpoint_id: 0,
    };
    conn.send(0, &wire::encode_commit_activation(&fence, &tuple, 1)).await.unwrap();
    match wire::decode(&conn.recv().await.unwrap().payload, &fence).unwrap().1 {
        Msg::ActivationCommitted(t) => assert_eq!((t.epoch, t.recovery_id, t.attempt), (1, 1, 0)),
        other => panic!("expected ActivationCommitted, got {other:?}"),
    }
    conn.send(0, &wire::encode_finalize_activation(&fence, &tuple, 1)).await.unwrap();
    assert!(
        matches!(wire::decode(&conn.recv().await.unwrap().payload, &fence).unwrap().1, Msg::ActivationFinalized),
        "the recovered stage reaches ACTIVE_FINAL"
    );
}

#[tokio::test]
async fn a_frozen_ready_stage_at_base_takes_case_a_like_a_frozen_one() {
    // Until 2026-09-03 this test asserted the OPPOSITE — "FROZEN_READY is not a Case-A entry state;
    // the recovery-start flag is load-bearing" — a code-level refusal the model never had (its
    // stages start FROZEN and Case A admitted {ACTIVE_FINAL, FROZEN} at base). Spec §6.5a made the
    // refused state reachable and load-bearing the other way: a coordinator that crashes after
    // reconstruction and before COMMIT leaves both stages FROZEN_READY at base, and its restart
    // FENCES FORWARD with `BEGIN_RECOVERY{base, base+1}` — which these stages dropped as Case C,
    // and the product coordinator waited forever (the restart oracle's third window). Spec §1.3
    // Case A now admits REBUILDING / FROZEN_READY at base; the model and the stage SM likewise.
    // What `recovery_start` still decides: a replacement boots FROZEN (it must be rebuilt before it
    // can serve), a normal boot is FROZEN_READY (nothing to rebuild for the INITIAL activation) —
    // a difference in what the stage needs, not in whether it can be fenced forward.
    let mut fresh = hydra_state::Stage::frozen_ready(0, 0, 0);
    let effs = fresh.step(hydra_state::StageEvent::RecvBegin { base: 0, target: 1, recovery_id: 1, truncate_to: 0, n_ctx: 64 });
    assert!(
        effs.iter().any(|e| matches!(e, hydra_state::StageEffect::RecoveryAck { target: 1, recovery_id: 1, .. })),
        "FROZEN_READY at base takes Case A under §6.5a (RECOVERY_ACK at the target); got {effs:?}"
    );

    let mut recovering = hydra_state::Stage::frozen(0, 0, 0, 0);
    let effs = recovering.step(hydra_state::StageEvent::RecvBegin { base: 0, target: 1, recovery_id: 1, truncate_to: 0, n_ctx: 64 });
    assert!(!effs.is_empty(), "FROZEN accepts Case A");

    // What is STILL refused: a stage not at base (Case C, nothing on the wire but ERR_TRANSITION).
    let mut elsewhere = hydra_state::Stage::frozen_ready(0, 3, 0);
    let effs = elsewhere.step(hydra_state::StageEvent::RecvBegin { base: 0, target: 1, recovery_id: 1, truncate_to: 0, n_ctx: 64 });
    assert!(effs.is_empty(), "a stage at another epoch is not at base: Case C");
}

/// **Audit M13 (the auditor's second half) — `RESET_RECOVERY_ATTEMPT` had no wire decode arm.**
///
/// # Standing rule 20: this is gloss drift, found by re-reading the source
///
/// The Wave-1 directive framed M13 purely as the **spec silence** about what a reset does to the
/// attempt floor. That half was amended (§6.4 now says a reset does *not* reset the attempt space,
/// and a stage **retains** its highest accepted attempt) and the SM already matched it. The
/// auditor's M13 has a second half the gloss dropped: *"`RESET_RECOVERY_ATTEMPT` has no wire decode
/// arm"* — so an inbound reset fell through to `UnsupportedBody`, and spec §6.4's
/// reconstruction-invalidating restart was **unreachable over the wire**. The stage SM has
/// implemented `RecvReset` since M1 and nothing could deliver it.
#[tokio::test]
async fn a_reset_recovery_attempt_reaches_the_stage_machine_over_the_wire() {
    use hydra_state::StageState;

    let fence = SessionFence::dev(0x7C);
    let mut w = Worker::new(WorkerConfig {
        fence: fence.clone(),
        rank: 0,
        layer_first: 0,
        layer_last: -1,
        is_final: true,
        receives_tokens: true,
        epoch: 1,
        recovery_id: 0,
        model_path: None, // control-plane only: this is about the SM, not the engine
        n_gpu_layers: 0,
        n_ctx: 64,
        sampler_config: None,
        recovery_start: true, // starts FROZEN, the state a reset is accepted in
        shard_manifest: None,
    })
    .expect("worker");

    assert_eq!(w.stage().state(), StageState::Frozen);
    assert_eq!(w.stage().recovery_id(), 0);

    let frame = wire::encode_reset_recovery_attempt(&fence, 1, 0, 1, 3, 7);
    let replies = w.on_frame(&frame).expect("a reset must decode and step the SM, not fall through to UnsupportedBody");

    assert_eq!(w.stage().recovery_id(), 1, "the stage adopted the new recovery_id (I23)");
    assert_eq!(w.stage().state(), StageState::Frozen);
    assert_eq!(replies.len(), 1, "and it acked");
    match wire::decode(&replies[0], &fence).expect("the ack decodes").1 {
        Msg::RecoveryAck { .. } | Msg::CatchUpReady { .. } => {}
        other => panic!("expected a reset ack, got {other:?}"),
    }

    // The amended §6.4 ruling, asserted where it is easy to regress: a reset does NOT reset the
    // attempt space. `highest_attempt` survives, because the floor is scoped to (session, epoch)
    // and the epoch did not change — restarting it would fence every post-reset activation.
    assert_eq!(
        w.stage().highest_attempt(),
        0,
        "no attempt has been accepted yet, but the point is that the reset did not TOUCH the floor"
    );
}
