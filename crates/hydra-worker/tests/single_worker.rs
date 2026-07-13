//! M2 sub-slice A — **single-worker echo over real mTLS**.
//!
//! A `hydra-worker` is stood up as a real TCP+mTLS endpoint on localhost (same handshake + framing
//! path as multi-machine); a coordinator connects and drives it. Two things are proven:
//!   1. **control plane** — a `COMMIT_ACTIVATION` / `FINALIZE_ACTIVATION` pair is routed through
//!      the *real* `hydra-state` stage SM (`FROZEN_READY → PREACTIVE → ACTIVE_FINAL`) and the acks
//!      come back over the wire. No parallel "simple" SM exists; this is the DST-tested one.
//!   2. **data plane** (engine-gated) — `APPLY_TOKEN` frames drive the `hydra-engine-sys` engine to
//!      unsampled logits, returned as `APPLIED_ACK{output_checksum}`. Skips cleanly without the
//!      engine/model (dev-environment artifacts), exactly like the engine-sys bit-exact test.
//!
//! The worker owns a non-`Send` engine context, so it runs on its own thread with a current-thread
//! runtime; the coordinator drives it from the test's runtime over a loopback connection.

use std::net::SocketAddr;
use std::sync::mpsc;

use hydra_transport::tcp_mtls::{TcpMtls, TcpMtlsListener};
use hydra_transport::ClusterCa;
use hydra_worker::wire::{self, Msg, SessionKeys};
use hydra_worker::worker::{serve_conn, Worker, WorkerConfig};

const WORKER_NAME: &str = "worker-1";
const COORD_NAME: &str = "coordinator";

/// Stand up a worker on its own thread; return the bound loopback address. The worker serves a
/// single connection (enough for one coordinator in these tests) and then the thread exits.
fn spawn_worker(cfg: WorkerConfig, server_cfg: rustls::ServerConfig) -> SocketAddr {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = TcpMtlsListener::bind_with_config("127.0.0.1:0".parse().unwrap(), server_cfg)
                .await
                .expect("bind worker");
            tx.send(listener.local_addr().unwrap()).unwrap();
            let mut worker = Worker::new(cfg).expect("worker");
            let mut conn = listener.accept().await.expect("accept");
            let _ = serve_conn(&mut worker, &mut conn).await;
        });
    });
    rx.recv().expect("worker addr")
}

fn model_path() -> Option<String> {
    let default = concat!(env!("CARGO_MANIFEST_DIR"), "/../../models/qwen2.5-0.5b-instruct-fp16.gguf");
    std::env::var("HYDRA_TEST_MODEL")
        .ok()
        .filter(|p| std::path::Path::new(p).exists())
        .or_else(|| std::path::Path::new(default).exists().then(|| default.to_string()))
}

#[tokio::test]
async fn control_plane_activation_round_trips_through_the_real_stage_sm() {
    let ca = ClusterCa::new().unwrap();
    let worker_id = ca.issue(WORKER_NAME).unwrap();
    let coord_id = ca.issue(COORD_NAME).unwrap();
    let keys = SessionKeys::dev(1);

    let cfg = WorkerConfig {
        keys: keys.clone(),
        rank: 0,
        layer_first: 0,
        layer_last: -1,
        is_final: true,
        receives_tokens: true,
        epoch: 0,
        recovery_id: 0,
        model_path: None, // control-plane only — no engine needed
        n_gpu_layers: 0,
        n_ctx: 64,
        sampler_config: None,
    };
    let addr = spawn_worker(cfg, ca.server_config(&worker_id).unwrap());

    let connector = TcpMtls::from_config(ca.client_config(&coord_id).unwrap()).unwrap();
    let mut conn = connector.connect(addr, WORKER_NAME).await.expect("connect");

    // COMMIT_ACTIVATION (attempt 0) → ACTIVATION_COMMITTED.
    let tuple = hydra_state::ActivationTuple {
        kind: hydra_state::ActivationKind::Initial,
        epoch: 0,
        recovery_id: 0,
        attempt: 0,
        sampler_checkpoint_id: 0,
    };
    conn.send(0, &wire::encode_commit_activation(&keys, &tuple, 1)).await.unwrap();
    let reply = conn.recv().await.unwrap();
    match wire::decode(&reply.payload, &keys).unwrap().1 {
        Msg::ActivationCommitted(t) => assert_eq!((t.epoch, t.attempt), (0, 0)),
        other => panic!("expected ActivationCommitted, got {other:?}"),
    }

    // FINALIZE_ACTIVATION → ACTIVATION_FINALIZED (stage now ACTIVE_FINAL).
    conn.send(0, &wire::encode_finalize_activation(&keys, &tuple, 1)).await.unwrap();
    let reply = conn.recv().await.unwrap();
    assert!(
        matches!(wire::decode(&reply.payload, &keys).unwrap().1, Msg::ActivationFinalized),
        "expected ActivationFinalized"
    );
}

#[tokio::test]
async fn f1_fence_mismatch_is_rejected_before_action() {
    // A frame carrying a foreign session identity must be dropped by the F1 check, never decoded to
    // an action. (Exercised directly on the codec — the same gate the worker applies per frame.)
    let ours = SessionKeys::dev(2);
    let theirs = SessionKeys::dev(3);
    let tuple = hydra_state::ActivationTuple {
        kind: hydra_state::ActivationKind::Initial,
        epoch: 0,
        recovery_id: 0,
        attempt: 0,
        sampler_checkpoint_id: 0,
    };
    let frame = wire::encode_commit_activation(&theirs, &tuple, 1);
    assert!(matches!(wire::decode(&frame, &ours), Err(wire::WireError::FenceMismatch(_))));
    // Our own frame decodes fine.
    let ok = wire::encode_commit_activation(&ours, &tuple, 1);
    assert!(matches!(wire::decode(&ok, &ours), Ok((_, Msg::CommitActivation(_)))));
}

#[tokio::test]
async fn data_plane_apply_token_echo_over_mtls() {
    let Some(path) = model_path() else {
        eprintln!("SKIP: no model — data-plane echo needs the engine + GGUF (dev-environment artifacts)");
        return;
    };

    let ca = ClusterCa::new().unwrap();
    let worker_id = ca.issue(WORKER_NAME).unwrap();
    let coord_id = ca.issue(COORD_NAME).unwrap();
    let keys = SessionKeys::dev(4);

    // Full-range worker: hosts every layer, ingests tokens, produces logits.
    let cfg = WorkerConfig {
        keys: keys.clone(),
        rank: 0,
        layer_first: 0,
        layer_last: -1,
        is_final: true,
        receives_tokens: true,
        epoch: 0,
        recovery_id: 0,
        model_path: Some(path),
        n_gpu_layers: 0,
        n_ctx: 64,
        sampler_config: None,
    };
    let addr = spawn_worker(cfg, ca.server_config(&worker_id).unwrap());

    let connector = TcpMtls::from_config(ca.client_config(&coord_id).unwrap()).unwrap();
    let mut conn = connector.connect(addr, WORKER_NAME).await.expect("connect");

    // Teacher-forced (NO_SAMPLE) apply of a few fixed token ids; each returns a logits digest.
    let tokens: [u32; 3] = [40, 1770, 374];
    let mut last_digest = Vec::new();
    for (pos, &tok) in tokens.iter().enumerate() {
        conn.send(0, &wire::encode_apply_token(&keys, 0, pos as i64, tok, true)).await.unwrap();
        let reply = conn.recv().await.unwrap();
        match wire::decode(&reply.payload, &keys).unwrap().1 {
            Msg::AppliedAck { cumulative_input_pos, output_checksum } => {
                assert_eq!(cumulative_input_pos, pos as i64, "position echoes back");
                assert_eq!(output_checksum.len(), 32, "32-byte logits digest");
                last_digest = output_checksum;
            }
            other => panic!("expected AppliedAck, got {other:?}"),
        }
    }
    assert!(last_digest.iter().any(|&b| b != 0), "logits digest is non-trivial");
}
