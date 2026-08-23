//! **§7.24 coefficient provenance, route (a) — the independent protocol microbench.**
//!
//! The amendment adds a per-crossing `protocol` term to the TPOT model. The binding provenance rule
//! is that this coefficient **may not be a residual fitted to the configuration the gate measures**:
//! fitted-here-passes-here is worthless. So it is measured **here**, on its own, with **zero model
//! compute** in the loop.
//!
//! What one *exchange* costs is exactly what the pipeline pays per stage per token: the coordinator
//! encodes a frame, BLAKE3-frames it, mTLS-encrypts it, the peer decrypts/decodes it, encodes a
//! reply carrying a boundary-sized payload, and the coordinator decodes that. This bench performs
//! precisely that cycle against an **echo peer that does no inference at all**, so whatever it
//! measures is protocol processing and nothing else.
//!
//! The number this produces is consumed by `calibration.rs` as `protocol_ms`. Nothing in the
//! calibration run feeds back into it.

use std::net::SocketAddr;
use std::sync::mpsc;
use std::time::Instant;

use hydra_transport::tcp_mtls::{TcpMtls, TcpMtlsListener};
use hydra_transport::ClusterCa;
use hydra_worker::wire::{self, Msg, SessionKeys};

const PEER: &str = "echo-peer";
const CLIENT: &str = "coordinator";

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// An mTLS peer that decodes each `APPLY_TOKEN` and replies with a real `FWD` carrying an
/// `n_embd`-float boundary — the same encode/decode/BLAKE3/mTLS work a stage does, **without the
/// inference**. That is the whole point: everything it spends is protocol.
fn spawn_echo_peer(server_cfg: rustls::ServerConfig, keys: SessionKeys, n_embd: usize) -> SocketAddr {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = TcpMtlsListener::bind_with_config("127.0.0.1:0".parse().unwrap(), server_cfg)
                .await
                .expect("bind");
            tx.send(listener.local_addr().unwrap()).unwrap();
            let mut conn = listener.accept().await.expect("accept");
            let boundary = vec![0.5f32; n_embd];
            while let Ok(frame) = conn.recv().await {
                match wire::decode(&frame.payload, &keys) {
                    Ok((view, Msg::ApplyToken { input_pos, .. })) => {
                        let reply = wire::encode_fwd(&keys, view.epoch, input_pos, true, &boundary);
                        if conn.send(0, &reply).await.is_err() {
                            break;
                        }
                    }
                    Ok((view, Msg::Fwd { first_input_pos, .. })) => {
                        let reply = wire::encode_applied_ack(&keys, view.epoch, first_input_pos, &[0u8; 32]);
                        if conn.send(0, &reply).await.is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        });
    });
    rx.recv().expect("addr")
}

/// Measure the per-exchange protocol cost and print it for the calibration model to consume.
#[tokio::test]
#[ignore = "§7.24 coefficient provenance (route a); run explicitly for the M3 gate"]
async fn one_coordinator_stage_exchange_costs() {
    const N_EMBD: usize = 896; // the dev model's boundary width
    const WARMUP: usize = 16;
    const CYCLES: usize = 256;

    let ca = ClusterCa::new().unwrap();
    let peer_id = ca.issue(PEER).unwrap();
    let client_id = ca.issue(CLIENT).unwrap();
    let keys = SessionKeys::dev(0x7A);

    let addr = spawn_echo_peer(ca.server_config(&peer_id).unwrap(), keys.clone(), N_EMBD);
    let connector = TcpMtls::from_config(ca.client_config(&client_id).unwrap()).unwrap();
    // Connected BEFORE any timing — the same discipline the calibration harness now uses.
    let mut conn = connector.connect(addr, PEER).await.expect("connect");

    let boundary = vec![0.25f32; N_EMBD];

    // Exchange A: APPLY_TOKEN out, FWD (boundary payload) back — what the coordinator↔S1 hop costs.
    let mut a_samples = Vec::new();
    for i in 0..(WARMUP + CYCLES) {
        let t = Instant::now();
        conn.send(0, &wire::encode_apply_token(&keys, 0, i as i64, 1, true)).await.expect("send");
        let f = conn.recv().await.expect("recv");
        let _ = wire::decode(&f.payload, &keys).expect("decode");
        if i >= WARMUP {
            a_samples.push(t.elapsed().as_secs_f64() * 1000.0);
        }
    }

    // Exchange B: FWD (boundary payload) out, APPLIED_ACK back — the coordinator↔S2 hop.
    let mut b_samples = Vec::new();
    for i in 0..(WARMUP + CYCLES) {
        let t = Instant::now();
        conn.send(0, &wire::encode_fwd(&keys, 0, i as i64, true, &boundary)).await.expect("send");
        let f = conn.recv().await.expect("recv");
        let _ = wire::decode(&f.payload, &keys).expect("decode");
        if i >= WARMUP {
            b_samples.push(t.elapsed().as_secs_f64() * 1000.0);
        }
    }

    let a = median(a_samples);
    let b = median(b_samples);
    let per_exchange = (a + b) / 2.0;

    eprintln!(
        "PROTOCOL MICROBENCH (loopback mTLS, n_embd={N_EMBD}, {CYCLES} cycles after {WARMUP} warm-up, NO inference)\n\
         \x20 exchange A (APPLY_TOKEN out / FWD back)   {a:.3} ms\n\
         \x20 exchange B (FWD out / APPLIED_ACK back)   {b:.3} ms\n\
         \x20 PROTOCOL_MS_PER_CROSSING                  {per_exchange:.3} ms\n\
         \x20 (a 2-stage pipeline pays this twice per token)"
    );

    // Sanity, not a tuning knob: the term must be a positive, finite, sub-second quantity. If it
    // were ~0 the amendment would have nothing to explain; if it were huge the bench is broken.
    assert!(per_exchange > 0.0 && per_exchange < 1000.0, "implausible protocol cost {per_exchange}");
}
