//! P2·8 — **D0 mode: zero boundary overhead, Strategy-B recovery.**
//!
//! D0 is the explicit configuration where boundary durability is **off**. Spec §7's trade: D0 buys
//! **no boundary-copy traffic, no durability-target disk, and no `DURABILITY_ACK` round-trip on the
//! release path**, and pays for it in **recovery cost** — a replacement stage is rebuilt by **full
//! teacher-forced replay from the durable token ledger** (Strategy-B, the machinery C part 2 already
//! proved) instead of by replaying durable boundaries.
//!
//! The load-bearing test here is the **absence** one. "Zero boundary overhead" is not a claim that
//! the copies are cheaper — it is a claim that **there are none**, and a D0 run that emits a
//! `BOUNDARY_COPY` is a *defect*, not an inefficiency. So the durability endpoint counts every
//! frame it receives and the test asserts the count is exactly zero. Asserted, never assumed —
//! this is the same "prove it by absence" shape as the coordinator's emit-after-commit gate.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use hydra_transport::tcp_mtls::{TcpMtls, TcpMtlsListener};
use hydra_transport::ClusterCa;
use hydra_worker::durable::DurabilityMode;
use hydra_worker::wire::{self, Msg, SessionFence};
use hydra_worker::retain::R3Buffer;
use hydra_worker::DurableForwarder;

const DUR_NAME: &str = "durability-target";
const S1_NAME: &str = "s1";
static SEQ: AtomicU32 = AtomicU32::new(0);

/// A durability endpoint that persists nothing and simply **counts** what arrives. In D0 the
/// expected count is zero, and a counter is the only way to assert that without trusting the
/// forwarder's own account of itself.
fn spawn_counting_endpoint(server_cfg: rustls::ServerConfig, fence: SessionFence, counter: Arc<AtomicUsize>) -> SocketAddr {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = TcpMtlsListener::bind_with_config("127.0.0.1:0".parse().unwrap(), server_cfg, hydra_worker::pair::dev_role_table()).await.expect("bind dur");
            tx.send(listener.local_addr().unwrap()).unwrap();
            let mut conn = listener.accept().await.expect("accept").conn;
            while let Ok(frame) = conn.recv().await {
                match wire::decode(&frame.payload, &fence) {
                    Ok((view, Msg::BoundaryCopy { boundary_id, first_input_pos, chunk_id, activations, .. })) => {
                        counter.fetch_add(1, Ordering::SeqCst);
                        let _ = (first_input_pos, chunk_id, activations);
                        let ack = wire::encode_durability_ack(&fence, view.epoch, boundary_id, first_input_pos, 0);
                        if conn.send(0, &ack).await.is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
    });
    rx.recv().expect("dur addr")
}

fn boundary(v: f32) -> Vec<f32> {
    vec![v; 8]
}

/// THE D0 assertion: a D0 run emits **no** `BOUNDARY_COPY` at all.
#[tokio::test]
async fn a_d0_run_emits_no_boundary_copy_traffic_whatsoever() {
    let ca = ClusterCa::new().unwrap();
    let dur_id = ca.issue(DUR_NAME).unwrap();
    let s1_id = ca.issue(S1_NAME).unwrap();
    let fence = SessionFence::dev(SEQ.fetch_add(1, Ordering::Relaxed) as u8);
    let count = Arc::new(AtomicUsize::new(0));

    let dur_addr = spawn_counting_endpoint(ca.server_config(&dur_id).unwrap(), fence.clone(), count.clone());
    let connector = TcpMtls::from_config(ca.client_config(&s1_id).unwrap()).unwrap();
    let mut dur = connector.connect(dur_addr, DUR_NAME).await.expect("connect durability");

    // D0: require_durable = false.
    let mut fwd = DurableForwarder::new(fence.clone(), 0, false, 16);
    assert_eq!(fwd.mode(), DurabilityMode::D0);
    assert!(!fwd.mode().copies_boundaries());

    for pos in 0..12i64 {
        fwd.copy_and_retain(&mut dur, pos, &boundary(pos as f32)).await.expect("forward");
    }
    // Give anything in flight a chance to land before asserting absence — an absence test that
    // races is worthless.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "D0 must emit ZERO BOUNDARY_COPY frames — 'zero boundary overhead' means there are none, \
         and a D0 run that emits one is a defect, not an inefficiency"
    );
}

/// The control: the identical drive in D1 **does** copy, so the absence above is caused by the mode
/// and not by a broken harness that could never have counted anything.
#[tokio::test]
async fn the_same_drive_in_d1_does_emit_boundary_copies() {
    let ca = ClusterCa::new().unwrap();
    let dur_id = ca.issue(DUR_NAME).unwrap();
    let s1_id = ca.issue(S1_NAME).unwrap();
    let fence = SessionFence::dev(SEQ.fetch_add(1, Ordering::Relaxed) as u8);
    let count = Arc::new(AtomicUsize::new(0));

    let dur_addr = spawn_counting_endpoint(ca.server_config(&dur_id).unwrap(), fence.clone(), count.clone());
    let connector = TcpMtls::from_config(ca.client_config(&s1_id).unwrap()).unwrap();
    let mut dur = connector.connect(dur_addr, DUR_NAME).await.expect("connect durability");

    let mut fwd = DurableForwarder::new(fence.clone(), 0, true, 16);
    assert_eq!(fwd.mode(), DurabilityMode::D1);
    for pos in 0..12i64 {
        fwd.copy_and_retain(&mut dur, pos, &boundary(pos as f32)).await.expect("forward");
    }
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    assert_eq!(count.load(Ordering::SeqCst), 12, "D1 copies every boundary — the counter works");
}

/// D0 still retains under R3′ (the in-flight window), but releases on the **downstream ack alone**.
/// Recovery does not need those boundaries — Strategy-B replays tokens — so nothing waits on a
/// `DURABILITY_ACK` that will never come.
#[test]
fn d0_releases_on_the_downstream_ack_alone_and_never_waits_for_durability() {
    // Exercised on R3Buffer directly — it is the type that owns the release rule, and the
    // forwarder constructs it from the same flag.
    let mut d0 = R3Buffer::new(false);
    for pos in 0..5i64 {
        d0.retain(pos, boundary(pos as f32));
    }
    d0.on_applied_ack(4);
    assert_eq!(d0.release().len(), 5, "D0 releases the whole applied prefix without any DURABILITY_ACK");

    // D1, identical drive: nothing releases until durability also clears. This is the cost D0 is
    // choosing not to pay, made visible side by side.
    let mut d1 = R3Buffer::new(true);
    for pos in 0..5i64 {
        d1.retain(pos, boundary(pos as f32));
    }
    d1.on_applied_ack(4);
    assert_eq!(d1.release().len(), 0, "D1 holds until DURABILITY_ACK — the trade, made visible");
    d1.on_durability_ack(4);
    assert_eq!(d1.release().len(), 5);
}

#[test]
fn the_mode_is_explicit_not_a_bare_bool() {
    assert_eq!(DurabilityMode::from_require_durable(true), DurabilityMode::D1);
    assert_eq!(DurabilityMode::from_require_durable(false), DurabilityMode::D0);
    assert!(DurabilityMode::D1.copies_boundaries());
    assert!(!DurabilityMode::D0.copies_boundaries());
}
