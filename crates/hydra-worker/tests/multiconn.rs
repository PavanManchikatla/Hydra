//! P1·1a — the **multi-connection serve loop**: a worker serves concurrent inbound connections
//! against one shared [`Worker`] (the seam-3 requirement for a direct-FWD pipeline where the
//! coordinator also controls/samples a stage — S_P serves S1's `FWD` **and** the coordinator's
//! `SAMPLE_NEXT` at once).
//!
//! These are **control-plane only** (no engine/model), so they run everywhere in CI. The engine-gated
//! demonstration — S_P serving a live `FWD` stream and `SAMPLE_NEXT` concurrently during real
//! generation — is the P1·1a real-pair seam.

use std::net::SocketAddr;
use std::time::Duration;

use hydra_state::{ActivationKind, ActivationTuple};
use hydra_transport::tcp_mtls::TcpMtls;
use hydra_transport::ClusterCa;
use hydra_worker::pair::spawn_multiconn_endpoint;
use hydra_worker::wire::{self, Msg, SessionKeys};
use hydra_worker::worker::WorkerConfig;

const WORKER_NAME: &str = "s_p";
const COORD_NAME: &str = "coordinator";

fn control_plane_cfg(keys: SessionKeys) -> WorkerConfig {
    WorkerConfig {
        keys,
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
        recovery_start: false,
    }
}

fn tuple() -> ActivationTuple {
    ActivationTuple { kind: ActivationKind::Initial, epoch: 0, recovery_id: 0, attempt: 0, sampler_checkpoint_id: 0 }
}

async fn connect(connector: &TcpMtls, addr: SocketAddr) -> hydra_transport::tcp_mtls::ClientConn {
    // A generous timeout turns the sequential-accept deadlock into a fast, deterministic failure
    // instead of a hung test: under the old `while accept { serve_conn }` loop the server, already
    // blocked serving conn A, never accepts a second peer, so this handshake would never complete.
    tokio::time::timeout(Duration::from_secs(10), connector.connect(addr, WORKER_NAME))
        .await
        .expect("connect timed out — a second connection was not accepted (sequential-accept deadlock?)")
        .expect("connect")
}

/// The core proof: connection **A** is opened and left **idle** (its serve task parks on `recv`), and
/// connection **B** is then served to completion — a duplex control handshake — **without** A ever
/// sending a byte. Under the sequential accept loop B would deadlock (A owns the single accept). The
/// handshake also spans both connections against the **one** shared `Worker`: B commits the activation
/// and A finalizes it, so A's `FINALIZE` only succeeds because it steps the *same* stage SM B committed.
#[tokio::test]
async fn a_second_connection_is_served_while_the_first_is_held_open() {
    let ca = ClusterCa::new().unwrap();
    let worker_id = ca.issue(WORKER_NAME).unwrap();
    let coord_id = ca.issue(COORD_NAME).unwrap();
    let keys = SessionKeys::dev(11);

    let addr = spawn_multiconn_endpoint(control_plane_cfg(keys.clone()), ca.server_config(&worker_id).unwrap());
    let connector = TcpMtls::from_config(ca.client_config(&coord_id).unwrap()).unwrap();

    // A: connect and hold open, idle (no frames) — occupies a serve task, parked on recv.
    let mut conn_a = connect(&connector, addr).await;

    // B: connect *while A is open* and drive COMMIT_ACTIVATION → ACTIVATION_COMMITTED.
    let mut conn_b = connect(&connector, addr).await;
    conn_b.send(0, &wire::encode_commit_activation(&keys, &tuple(), 1)).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(10), conn_b.recv())
        .await
        .expect("B was not served while A was held open (sequential-accept deadlock)")
        .unwrap();
    match wire::decode(&reply.payload, &keys).unwrap().1 {
        Msg::ActivationCommitted(t) => assert_eq!((t.epoch, t.attempt), (0, 0)),
        other => panic!("expected ActivationCommitted on B, got {other:?}"),
    }

    // A is still live: FINALIZE over A drives the SAME shared stage SM (committed by B) to ACTIVE_FINAL.
    conn_a.send(0, &wire::encode_finalize_activation(&keys, &tuple(), 1)).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(10), conn_a.recv())
        .await
        .expect("A's finalize was not served")
        .unwrap();
    assert!(
        matches!(wire::decode(&reply.payload, &keys).unwrap().1, Msg::ActivationFinalized),
        "expected ActivationFinalized on A — proves A and B share one Worker/stage SM"
    );
}

/// Many connections open at once are all served against the one shared `Worker`. Each connection
/// issues an idempotent `COMMIT_ACTIVATION` for the same tuple (the stage SM treats a repeat commit
/// of the already-committed tuple as a no-op-or-re-ack, never a second distinct activation) and must
/// get a reply — proving none is starved behind another.
#[tokio::test]
async fn many_concurrent_connections_are_each_served() {
    let ca = ClusterCa::new().unwrap();
    let worker_id = ca.issue(WORKER_NAME).unwrap();
    let coord_id = ca.issue(COORD_NAME).unwrap();
    let keys = SessionKeys::dev(12);

    let addr = spawn_multiconn_endpoint(control_plane_cfg(keys.clone()), ca.server_config(&worker_id).unwrap());
    let connector = TcpMtls::from_config(ca.client_config(&coord_id).unwrap()).unwrap();

    // Open several connections and hold them all open simultaneously.
    let mut conns = Vec::new();
    for _ in 0..4 {
        conns.push(connect(&connector, addr).await);
    }
    // Drive the LAST-opened connection first: if the accept loop were sequential it would be parked
    // behind the first, so serving the newest proves genuine concurrent accept.
    for conn in conns.iter_mut().rev() {
        conn.send(0, &wire::encode_commit_activation(&keys, &tuple(), 1)).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(10), conn.recv())
            .await
            .expect("a held-open connection was not served (starved behind another)")
            .unwrap();
        assert!(
            matches!(wire::decode(&reply.payload, &keys).unwrap().1, Msg::ActivationCommitted(_)),
            "each concurrent connection gets its own reply from the shared Worker"
        );
    }
}
