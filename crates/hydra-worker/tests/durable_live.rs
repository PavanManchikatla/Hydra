//! P1·1b seam A — **boundary durability in the LIVE serve path**.
//!
//! The seam-2 test exercised `R3Buffer` + `BoundaryStore` in isolation. This drives the real wiring:
//! [`DurableForwarder`] emits a `BOUNDARY_COPY` over **real mTLS** to a durability endpoint that
//! decodes it, persists it to a real `BoundaryStore` (`hydra-wal` file), and replies `DURABILITY_ACK`.
//! Release is gated on the **real** round-trip — the directed guarantee: a recovery-needed boundary
//! (downstream-applied but not yet durable) is **never released early** in the live path.
//!
//! Engine-free (synthetic boundary tensors) so it runs everywhere in CI; the engine-gated end-to-end
//! exercise is the 3-node pipeline (seam B/C).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;

use hydra_coordinator::BoundaryStore;
use hydra_transport::tcp_mtls::{TcpMtls, TcpMtlsListener};
use hydra_transport::ClusterCa;
use hydra_worker::wire::{self, Msg, SessionKeys};
use hydra_worker::DurableForwarder;

const DUR_NAME: &str = "durability-target";
const S1_NAME: &str = "s1";

static SEQ: AtomicU32 = AtomicU32::new(0);

fn store_path() -> std::path::PathBuf {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("hydra-durlive-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join("boundaries.wal")
}

/// A durability target: accept one connection and, for each inbound `BOUNDARY_COPY`, persist it to a
/// real `BoundaryStore` and reply `DURABILITY_ACK{durable_through}` (the fdatasync'd frontier). This
/// is the coordinator's durability role (spec §7), stood up as a real mTLS endpoint.
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
                match wire::decode(&frame.payload, &keys) {
                    Ok((view, Msg::BoundaryCopy { boundary_id, first_input_pos, chunk_id, activations })) => {
                        let durable_through = store.append_boundary(boundary_id, first_input_pos, chunk_id, &activations).expect("persist");
                        let ack = wire::encode_durability_ack(&keys, view.epoch, boundary_id, durable_through, 0);
                        if conn.send(0, &ack).await.is_err() {
                            break;
                        }
                    }
                    Ok(_) => {} // ignore non-BOUNDARY_COPY frames
                    Err(_) => break,
                }
            }
        });
    });
    rx.recv().expect("dur addr")
}

/// The directed guarantee, over the live durability path: with the downstream having applied every
/// boundary but only a **prefix** durable, R3′ releases exactly that durable prefix and **keeps** the
/// still-recovery-relevant tail. Every `BOUNDARY_COPY` and `DURABILITY_ACK` crosses real mTLS to a
/// real `BoundaryStore`.
#[tokio::test]
async fn a_recovery_needed_boundary_is_never_released_early_in_the_live_serve_path() {
    let ca = ClusterCa::new().unwrap();
    let dur_id = ca.issue(DUR_NAME).unwrap();
    let s1_id = ca.issue(S1_NAME).unwrap();
    let keys = SessionKeys::dev(0xB0);
    let path = store_path();

    let dur_addr = spawn_durability_endpoint(ca.server_config(&dur_id).unwrap(), path.clone(), keys.clone());
    let connector = TcpMtls::from_config(ca.client_config(&s1_id).unwrap()).unwrap();
    let mut dur = connector.connect(dur_addr, DUR_NAME).await.expect("connect durability");

    // D1 forwarding stage: require_durable = true. Capacity 8 (comfortably above the 3 boundaries
    // here — this test exercises the release gate, not the backpressure bound).
    let mut fwd = DurableForwarder::new(keys.clone(), 0, true, 8);

    // Forward + copy three boundaries (real BOUNDARY_COPY over mTLS; the endpoint persists each).
    let boundaries: Vec<Vec<f32>> = (0..3).map(|p| vec![p as f32, p as f32 + 0.5, -1.0]).collect();
    for (p, b) in boundaries.iter().enumerate() {
        let bid = fwd.copy_and_retain(&mut dur, p as i64, b).await.unwrap();
        assert_eq!(bid, p as u32, "boundary ids are dense/monotone");
    }

    // Downstream applied all three (compute path), but we only process the DURABILITY_ACKs for the
    // first TWO — the third is still "in flight" (unprocessed). The endpoint has acked all three, so
    // read exactly two acks and feed them; the third stays buffered/unread.
    fwd.on_applied_ack(2);
    for _ in 0..2 {
        let f = dur.recv().await.unwrap();
        match wire::decode(&f.payload, &keys).unwrap().1 {
            Msg::DurabilityAck { durable_through_input_pos, .. } => fwd.on_durability_ack(durable_through_input_pos),
            other => panic!("expected DURABILITY_ACK, got {other:?}"),
        }
    }

    // Applied through 2, durable through 1 → R3′ releases exactly {0,1}; position 2 is recovery-needed
    // (not durable) and MUST be kept. This is the whole point.
    assert_eq!(fwd.release_watermark(), 1, "min(applied=2, durable=1) = 1");
    assert_eq!(fwd.release(), vec![0, 1]);
    assert!(fwd.is_retained(2), "position 2 not yet durable → never released early in the live path");

    // Process the third DURABILITY_ACK → now position 2 may release.
    let f = dur.recv().await.unwrap();
    match wire::decode(&f.payload, &keys).unwrap().1 {
        Msg::DurabilityAck { durable_through_input_pos, .. } => fwd.on_durability_ack(durable_through_input_pos),
        other => panic!("expected DURABILITY_ACK, got {other:?}"),
    }
    assert_eq!(fwd.release(), vec![2]);
    assert!(fwd.retained().is_empty(), "all boundaries released once applied AND durable");

    // The durable boundaries read back ascending — what a replacement downstream rebuilds from (seam C).
    drop(dur);
    let durable = BoundaryStore::read(&path).unwrap();
    assert_eq!(durable.iter().map(|b| b.first_input_pos).collect::<Vec<_>>(), vec![0, 1, 2]);
    for (i, db) in durable.iter().enumerate() {
        assert_eq!(db.activations, boundaries[i], "durable boundary bytes round-trip exactly over the live path");
    }
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
