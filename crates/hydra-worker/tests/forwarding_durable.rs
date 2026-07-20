//! P1·1b seam B — the **multi-conn + forwarding + durable serve loop** and its concurrency contract.
//!
//! Three directed tests, in ascending faithfulness:
//!   1. `full_queue_holds_every_copy_and_frees_only_on_durability` — CI-safe (real mTLS durability
//!      target, no engine): the R3′ retention **bound** and the no-drop backpressure guarantee.
//!   2. `concurrent_control_connections_are_each_served` — CI-safe (control-plane worker): the durable
//!      forwarding loop still accepts+serves **concurrent** inbound connections (the multi-conn base).
//!   3. `panic_vector_concurrent_sample_next_during_an_inflight_fwd` — **engine-gated**, looped **100×**:
//!      a real in-flight `FWD` (S1 forwarding to a deliberately-slow downstream) with a **concurrent
//!      `SAMPLE_NEXT`** borrowing the same shared `Worker` — the exact interleaving that would detonate
//!      a borrow-across-await. A single pass proves little; the loop is the point.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use hydra_coordinator::BoundaryStore;
use hydra_transport::tcp_mtls::{TcpMtls, TcpMtlsListener};
use hydra_transport::ClusterCa;
use hydra_worker::pair::{dev_model_path, spawn_multiconn_forwarding_durable_endpoint};
use hydra_worker::sampler::SamplingConfig;
use hydra_worker::wire::{self, Msg, SessionKeys};
use hydra_worker::worker::{WorkerConfig, INITIAL_CHECKPOINT_ID};
use hydra_worker::DurableForwarder;

static SEQ: AtomicU32 = AtomicU32::new(0);

fn store_path() -> std::path::PathBuf {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("hydra-fwddur-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join("boundaries.wal")
}

// --------------------------- fake endpoints (wire-level, engine-free) ---------------------------

/// A durability target that persists each `BOUNDARY_COPY` to a real `BoundaryStore` and replies
/// `DURABILITY_ACK{durable_through = fdatasync'd frontier}`. Accepts exactly one connection.
fn spawn_durability_endpoint(server_cfg: rustls::ServerConfig, path: std::path::PathBuf, keys: SessionKeys) -> SocketAddr {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = TcpMtlsListener::bind_with_config("127.0.0.1:0".parse().unwrap(), server_cfg).await.expect("bind dur");
            tx.send(listener.local_addr().unwrap()).unwrap();
            let mut store = BoundaryStore::create(&path, [1; 16], [2; 16]).expect("store");
            let mut conn = listener.accept().await.expect("accept");
            while let Ok(frame) = conn.recv().await {
                if let Ok((view, Msg::BoundaryCopy { boundary_id, first_input_pos, chunk_id, activations })) = wire::decode(&frame.payload, &keys) {
                    let durable_through = store.append_boundary(boundary_id, first_input_pos, chunk_id, &activations).expect("persist");
                    let ack = wire::encode_durability_ack(&keys, view.epoch, boundary_id, durable_through, 0);
                    if conn.send(0, &ack).await.is_err() {
                        break;
                    }
                }
            }
        });
    });
    rx.recv().expect("dur addr")
}

/// A downstream stage stand-in: for each inbound `FWD{first_input_pos}` it **sleeps** `delay` (so the
/// survivor's forward is genuinely *in flight*) and replies `APPLIED_ACK{cumulative = first_input_pos}`.
/// Engine-free — the panic vector is about the serve loop's locks, not the downstream's compute.
fn spawn_slow_downstream(server_cfg: rustls::ServerConfig, keys: SessionKeys, delay: Duration) -> SocketAddr {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = TcpMtlsListener::bind_with_config("127.0.0.1:0".parse().unwrap(), server_cfg).await.expect("bind down");
            tx.send(listener.local_addr().unwrap()).unwrap();
            let mut conn = listener.accept().await.expect("accept");
            while let Ok(frame) = conn.recv().await {
                if let Ok((view, Msg::Fwd { first_input_pos, .. })) = wire::decode(&frame.payload, &keys) {
                    tokio::time::sleep(delay).await;
                    let ack = wire::encode_applied_ack(&keys, view.epoch, first_input_pos, &[0u8; 32]);
                    if conn.send(0, &ack).await.is_err() {
                        break;
                    }
                }
            }
        });
    });
    rx.recv().expect("down addr")
}

/// A durability target that quietly **absorbs** `BOUNDARY_COPY`s and acks each (`durable_through =
/// first_input_pos`). Used where durability must not gate the forward path (the panic vector).
fn spawn_absorbing_durability(server_cfg: rustls::ServerConfig, keys: SessionKeys) -> SocketAddr {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = TcpMtlsListener::bind_with_config("127.0.0.1:0".parse().unwrap(), server_cfg).await.expect("bind dur");
            tx.send(listener.local_addr().unwrap()).unwrap();
            let mut conn = listener.accept().await.expect("accept");
            while let Ok(frame) = conn.recv().await {
                if let Ok((view, Msg::BoundaryCopy { boundary_id, first_input_pos, .. })) = wire::decode(&frame.payload, &keys) {
                    let ack = wire::encode_durability_ack(&keys, view.epoch, boundary_id, first_input_pos, 0);
                    if conn.send(0, &ack).await.is_err() {
                        break;
                    }
                }
            }
        });
    });
    rx.recv().expect("dur addr")
}

// --------------------------- 1. the R3′ bound + no-drop backpressure ---------------------------

/// The full-queue guarantee (spec §5, R3′ bound): with retention at the bound, every copy is **held,
/// never dropped**, and a slot frees **only** when a `DURABILITY_ACK` advances the durable frontier —
/// in order. This is exactly the policy the serve loop leans on when it backpressures (`while
/// is_at_capacity { block on DURABILITY_ACK }`): the forward path slows rather than dropping a copy,
/// because a dropped boundary is a future recovery hole.
#[tokio::test]
async fn full_queue_holds_every_copy_and_frees_only_on_durability() {
    let ca = ClusterCa::new().unwrap();
    let dur_id = ca.issue("dur").unwrap();
    let s1_id = ca.issue("s1").unwrap();
    let keys = SessionKeys::dev(0xB1);
    let path = store_path();

    let dur_addr = spawn_durability_endpoint(ca.server_config(&dur_id).unwrap(), path.clone(), keys.clone());
    let connector = TcpMtls::from_config(ca.client_config(&s1_id).unwrap()).unwrap();
    let mut dur = connector.connect(dur_addr, "dur").await.expect("connect durability");

    // D1 forwarding stage, R3′ bound = 2.
    let mut fwd = DurableForwarder::new(keys.clone(), 0, true, 2);
    assert_eq!(fwd.capacity(), 2);

    // Copy+retain two boundaries (real BOUNDARY_COPY over mTLS). Downstream has applied both, but we
    // do NOT drain the DURABILITY_ACKs yet — as the serve loop would not, until it needs a slot.
    for p in 0..2i64 {
        fwd.copy_and_retain(&mut dur, p, &[p as f32, 0.5, -1.0]).await.unwrap();
    }
    fwd.on_applied_ack(1);

    // At the bound: both boundaries HELD (no drop), and nothing releasable (durability has not been
    // observed). This is precisely where the serve loop would backpressure the next forward.
    assert!(fwd.is_at_capacity(), "retention is at the R3′ bound");
    assert_eq!(fwd.retained(), vec![0, 1], "every copy is retained — none dropped to make room");
    assert!(fwd.release().is_empty(), "durability gates release: applied but not durable → held, not dropped");

    // Drain ONE durability ack → durable advances to 0 → slot 0 frees, in order; below the bound again.
    let f = dur.recv().await.unwrap();
    match wire::decode(&f.payload, &keys).unwrap().1 {
        Msg::DurabilityAck { durable_through_input_pos, .. } => fwd.on_durability_ack(durable_through_input_pos),
        o => panic!("expected DURABILITY_ACK, got {o:?}"),
    }
    assert_eq!(fwd.release(), vec![0], "exactly the now-durable prefix frees, in order");
    assert!(!fwd.is_at_capacity(), "a slot freed — the forward path may proceed");
    assert!(fwd.is_retained(1), "position 1 not yet durable → still held for recovery");

    // The second ack frees position 1.
    let f = dur.recv().await.unwrap();
    match wire::decode(&f.payload, &keys).unwrap().1 {
        Msg::DurabilityAck { durable_through_input_pos, .. } => fwd.on_durability_ack(durable_through_input_pos),
        o => panic!("expected DURABILITY_ACK, got {o:?}"),
    }
    assert_eq!(fwd.release(), vec![1]);
    assert!(fwd.retained().is_empty(), "all released once applied AND durable — never dropped, only released");

    // The durable side holds both boundaries (nothing was lost to the bound).
    drop(dur);
    let durable = BoundaryStore::read(&path).unwrap();
    assert_eq!(durable.iter().map(|b| b.first_input_pos).collect::<Vec<_>>(), vec![0, 1]);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

// --------------------------- 2. the multi-conn base of the durable loop ---------------------------

fn control_fwd_cfg(keys: SessionKeys) -> WorkerConfig {
    WorkerConfig {
        keys,
        rank: 0,
        layer_first: 0,
        layer_last: 4, // a forwarding (non-final) stage shape; control-plane only (no engine)
        is_final: false,
        receives_tokens: true,
        epoch: 0,
        recovery_id: 0,
        model_path: None,
        n_gpu_layers: 0,
        n_ctx: 64,
        sampler_config: None,
        recovery_start: false,
    }
}

fn tuple() -> hydra_state::ActivationTuple {
    hydra_state::ActivationTuple {
        kind: hydra_state::ActivationKind::Initial,
        epoch: 0,
        recovery_id: 0,
        attempt: 0,
        sampler_checkpoint_id: 0,
    }
}

/// The durable forwarding loop is still a **multi-connection** accept loop: several inbound
/// connections are each served against the one shared `Worker` (a mid stage must accept its upstream
/// data connection **and** the coordinator's control connection at once). Control-plane only, so it
/// runs everywhere; the FWD path is exercised engine-gated in test 3.
#[tokio::test]
async fn concurrent_control_connections_are_each_served() {
    let ca = ClusterCa::new().unwrap();
    let s2_id = ca.issue("s2").unwrap();
    let down_id = ca.issue("down").unwrap();
    let dur_id = ca.issue("dur").unwrap();
    let coord_id = ca.issue("coordinator").unwrap();
    let keys = SessionKeys::dev(0xB2);

    // Downstream + durability stand-ins (dialed at startup though this test sends only control frames).
    let down_keys = keys.clone();
    let down_addr = spawn_slow_downstream(ca.server_config(&down_id).unwrap(), down_keys, Duration::from_millis(0));
    let dur_addr = spawn_absorbing_durability(ca.server_config(&dur_id).unwrap(), keys.clone());
    // Two dialers (TcpMtls is not Clone): one for the down-link, one for the durability link.
    let s2_down_dialer = TcpMtls::from_config(ca.client_config(&s2_id).unwrap()).unwrap();
    let s2_dur_dialer = TcpMtls::from_config(ca.client_config(&s2_id).unwrap()).unwrap();
    let s2_down = std::sync::Arc::new(std::sync::Mutex::new((down_addr, "down".to_string())));

    let addr = spawn_multiconn_forwarding_durable_endpoint(
        control_fwd_cfg(keys.clone()),
        ca.server_config(&s2_id).unwrap(),
        s2_down_dialer,
        s2_down,
        4,
        s2_dur_dialer,
        dur_addr,
        "dur",
        true,
        8,
    );

    let connector = TcpMtls::from_config(ca.client_config(&coord_id).unwrap()).unwrap();
    // Open several connections and hold them all open at once, then drive the newest first: under a
    // sequential accept loop the newest would be parked behind the first (deadlock).
    let mut conns = Vec::new();
    for _ in 0..4 {
        conns.push(
            tokio::time::timeout(Duration::from_secs(10), connector.connect(addr, "s2"))
                .await
                .expect("connect timed out — a second connection was not accepted")
                .expect("connect"),
        );
    }
    for conn in conns.iter_mut().rev() {
        conn.send(0, &wire::encode_commit_activation(&keys, &tuple(), 1)).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(10), conn.recv())
            .await
            .expect("a held-open connection was not served (starved behind another)")
            .unwrap();
        assert!(
            matches!(wire::decode(&reply.payload, &keys).unwrap().1, Msg::ActivationCommitted(_)),
            "each concurrent connection is served by the shared Worker via the durable forwarding loop"
        );
    }
}

// --------------------------- 3. the panic vector (engine-gated, 100×) ---------------------------

fn s1_fwd_cfg(model: &str, keys: &SessionKeys, k: i32, n_ctx: i32) -> WorkerConfig {
    WorkerConfig {
        keys: keys.clone(),
        rank: 0,
        layer_first: 0,
        layer_last: k,
        is_final: false,
        receives_tokens: true,
        epoch: 0,
        recovery_id: 0,
        model_path: Some(model.to_string()),
        n_gpu_layers: 0,
        n_ctx,
        sampler_config: None, // S1 forwards; it has no sampler — a SAMPLE_NEXT here is rejected (ERR)
        recovery_start: false,
    }
}

/// The panic vector, looped **100×**: while S1's `FWD` forward to a deliberately-slow downstream is in
/// flight (its serve task parked in `down.recv().await`, holding the async downstream lock), a
/// **concurrent `SAMPLE_NEXT`** arrives on S1's second connection and steps through
/// `worker.borrow_mut().on_frame(..)`. If the forward held the `RefCell<Worker>` borrow across its
/// await, this second borrow would panic (double borrow) on the current-thread runtime. It must not —
/// and the `SAMPLE_NEXT` must be answered (with `ERR`, since S1 has no sampler) **while the forward is
/// still in flight**, proving genuine concurrency, not serialization. A single pass would prove almost
/// nothing; the repetition is the test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panic_vector_concurrent_sample_next_during_an_inflight_fwd() {
    let Some(model) = dev_model_path() else {
        eprintln!("SKIP: no engine/model (dev-environment artifacts)");
        return;
    };
    let (prompt, k) = {
        let m = hydra_engine_sys::Model::load(&model, 0).expect("load");
        let prompt: Vec<u32> = m.tokenize("The capital of France is").expect("tok").into_iter().map(|t| t as u32).collect();
        (prompt, (m.n_layer() / 2).max(1))
    };
    let iters = 100usize;
    let n_ctx = prompt.len() as i32 + iters as i32 + 8;
    let keys = SessionKeys::dev(0xB3);
    let cfg_hash = SamplingConfig { temperature: 0.0, top_p: 1.0, repeat_penalty: 1.0, penalty_last_n: 0, seed: 1 }.hash();

    let ca = ClusterCa::new().unwrap();
    let s1_id = ca.issue("s1").unwrap();
    let down_id = ca.issue("down").unwrap();
    let dur_id = ca.issue("dur").unwrap();
    let coord_id = ca.issue("coordinator").unwrap();
    let s1_down_dialer = TcpMtls::from_config(ca.client_config(&s1_id).unwrap()).unwrap();
    let s1_dur_dialer = TcpMtls::from_config(ca.client_config(&s1_id).unwrap()).unwrap();

    // Slow downstream (15 ms per FWD → the forward is genuinely in flight) + absorbing durability.
    let down_addr = spawn_slow_downstream(ca.server_config(&down_id).unwrap(), keys.clone(), Duration::from_millis(15));
    let dur_addr = spawn_absorbing_durability(ca.server_config(&dur_id).unwrap(), keys.clone());
    let s1_down = std::sync::Arc::new(std::sync::Mutex::new((down_addr, "down".to_string())));

    let s1_addr = spawn_multiconn_forwarding_durable_endpoint(
        s1_fwd_cfg(&model, &keys, k, n_ctx),
        ca.server_config(&s1_id).unwrap(),
        s1_down_dialer,
        s1_down,
        4,
        s1_dur_dialer,
        dur_addr,
        "dur",
        true,
        256, // capacity high → durability never gates; we are stressing the LOCKS, not backpressure
    );

    let connector = TcpMtls::from_config(ca.client_config(&coord_id).unwrap()).unwrap();
    // Connection A: the data path (APPLY_TOKEN → FWD forwarded to the slow downstream).
    let mut ca_conn = connector.connect(s1_addr, "s1").await.expect("connect A");
    // Connection B: the control path — concurrent SAMPLE_NEXTs that borrow the SAME shared Worker.
    let mut cb_conn = connector.connect(s1_addr, "s1").await.expect("connect B");

    for i in 0..iters {
        let pos = (prompt.len() + i) as i64;
        let tok = prompt[i % prompt.len()];
        // Fire A's forward and B's SAMPLE_NEXT so both are outstanding at S1 at once. A parks in the
        // downstream recv (15 ms); B's SAMPLE_NEXT must be served during that window.
        ca_conn.send(0, &wire::encode_apply_token(&keys, 0, pos, tok, true)).await.unwrap();
        cb_conn.send(0, &wire::encode_sample_next(&keys, 0, i as i64, &cfg_hash, INITIAL_CHECKPOINT_ID)).await.unwrap();

        // B is answered first (A is still awaiting the slow downstream) — but assert only correctness,
        // not ordering (ordering is timing-dependent; a borrow-across-await panic is deterministic).
        let b = tokio::time::timeout(Duration::from_secs(10), cb_conn.recv())
            .await
            .expect("SAMPLE_NEXT was not served during an in-flight forward (deadlock or panic)")
            .unwrap();
        assert!(
            matches!(wire::decode(&b.payload, &keys).unwrap().1, Msg::Err { .. }),
            "S1 has no sampler → SAMPLE_NEXT is answered with ERR (the point is that on_frame ran, borrowing the shared Worker, with no double-borrow panic)"
        );

        let a = tokio::time::timeout(Duration::from_secs(10), ca_conn.recv())
            .await
            .expect("the in-flight forward never completed")
            .unwrap();
        match wire::decode(&a.payload, &keys).unwrap().1 {
            Msg::AppliedAck { cumulative_input_pos, .. } => assert_eq!(cumulative_input_pos, pos),
            o => panic!("iter {i}: expected APPLIED_ACK relayed from downstream, got {o:?}"),
        }
    }
    eprintln!("panic vector: {iters}× concurrent SAMPLE_NEXT during an in-flight FWD — no double-borrow, both served");
}
