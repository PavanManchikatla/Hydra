//! P1·1b seam B, part 1 — the **in-process byte-identical 3-node chained-FWD** demonstration.
//!
//! Three stages S1 `[0,k1)` → S2 `[k1,k2)` → S_P `[k2,-1)` + sampler, wired as a **chained direct
//! FWD** pipeline: each boundary travels worker→worker (S1→S2→S_P), never via the coordinator, and
//! S1 and S2 each **durably copy** their boundary (seam A/B: `serve_multi_conn_forwarding_durable`).
//! The coordinator talks only to S1 (`APPLY_TOKEN`) and S_P (`SAMPLE_NEXT`) — chain depth is
//! transparent to it — while S_P serves S2's `FWD` **and** the coordinator's `SAMPLE_NEXT` at once
//! (the multi-conn substrate). The generated tokens must be **byte-identical** to an unsplit greedy
//! run of the same model. This in-process test **gates the real-node `hydra-3node-wan` run** (part 2).
//!
//! The split is the **capability-weighted** one from P1·2 (Mac 4.0 : myVm-2 2.1 : myVm-1 1.0 → layer
//! fractions 0.563 / 0.296 / 0.141; for the 24-layer 0.5B dev model that is exactly `[0,14)` /
//! `[14,21)` / `[21,24)`), so the in-process split mirrors the placement the WAN run uses.
//!
//! Engine-gated (skips without the dev model). Honesty: in-process localhost, not a WAN/heterogeneity
//! number — that is part 2.

use std::sync::atomic::{AtomicU32, Ordering};

use hydra_coordinator::BoundaryStore;
use hydra_transport::tcp_mtls::{TcpMtls, TcpMtlsListener};
use hydra_transport::ClusterCa;
use hydra_worker::pair::{dev_model_path, run_direct_fwd_generation, spawn_multiconn_endpoint, spawn_multiconn_forwarding_durable_endpoint, Endpoints};
use hydra_worker::sampler::SamplingConfig;
use hydra_worker::wire::SessionKeys;
use hydra_worker::worker::WorkerConfig;

const CLUSTER_ID: [u8; 16] = [0x3D; 16];
const SESSION_ID: [u8; 16] = [0x33; 16];
static SEQ: AtomicU32 = AtomicU32::new(0);

fn greedy() -> SamplingConfig {
    SamplingConfig { temperature: 0.0, top_p: 1.0, repeat_penalty: 1.0, penalty_last_n: 0, seed: 7 }
}

fn temp_dir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("hydra-3node-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Capability-weighted 3-way layer split (P1·2 ratios 4.0 : 2.1 : 1.0), each stage ≥ 1 layer.
fn split3(n_layer: i32) -> (i32, i32) {
    let l = n_layer as f64;
    let k1 = ((0.563 * l).round() as i32).clamp(1, n_layer - 2);
    let k2 = (k1 + (0.296 * l).round() as i32).clamp(k1 + 1, n_layer - 1);
    (k1, k2)
}

fn s1_cfg(model: &str, keys: &SessionKeys, k1: i32, n_ctx: i32) -> WorkerConfig {
    WorkerConfig { keys: keys.clone(), rank: 0, layer_first: 0, layer_last: k1, is_final: false, receives_tokens: true, epoch: 0, recovery_id: 0, model_path: Some(model.into()), n_gpu_layers: 0, n_ctx, sampler_config: None, recovery_start: false }
}
fn s2_cfg(model: &str, keys: &SessionKeys, k1: i32, k2: i32, n_ctx: i32) -> WorkerConfig {
    WorkerConfig { keys: keys.clone(), rank: 1, layer_first: k1, layer_last: k2, is_final: false, receives_tokens: false, epoch: 0, recovery_id: 0, model_path: Some(model.into()), n_gpu_layers: 0, n_ctx, sampler_config: None, recovery_start: false }
}
fn sp_cfg(model: &str, keys: &SessionKeys, k2: i32, n_ctx: i32) -> WorkerConfig {
    WorkerConfig { keys: keys.clone(), rank: 2, layer_first: k2, layer_last: -1, is_final: true, receives_tokens: false, epoch: 0, recovery_id: 0, model_path: Some(model.into()), n_gpu_layers: 0, n_ctx, sampler_config: Some(greedy()), recovery_start: false }
}

/// The unsplit greedy reference: prefill the prompt, then argmax-decode `n` tokens feeding each back
/// — the exact-tier witness the chained pipeline must reproduce byte-for-byte (greedy over f32
/// boundaries is bit-exact; spec I8 / M2(b) exact tier).
fn unsplit_greedy(model: &str, prompt: &[u32], n: usize, n_ctx: i32) -> Vec<u32> {
    let m = hydra_engine_sys::Model::load(model, 0).expect("load");
    let mut ctx = m.context(0, -1, false, n_ctx, n_ctx).expect("ctx");
    for (pos, &t) in prompt.iter().enumerate() {
        ctx.apply_tokens(&[t as i32], pos as i32, None).expect("prefill");
    }
    let mut out = Vec::with_capacity(n);
    let mut pos = prompt.len();
    for _ in 0..n {
        let logits = ctx.logits(0).expect("logits");
        let tok = argmax(&logits);
        out.push(tok);
        ctx.apply_tokens(&[tok as i32], pos as i32, None).expect("feedback");
        pos += 1;
    }
    out
}

fn argmax(logits: &[f32]) -> u32 {
    let mut best = 0usize;
    for i in 1..logits.len() {
        if logits[i] > logits[best] {
            best = i;
        }
    }
    best as u32
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_node_chained_direct_fwd_is_byte_identical_to_unsplit_greedy() {
    let Some(model) = dev_model_path() else {
        eprintln!("SKIP: no engine/model (dev-environment artifacts)");
        return;
    };
    let (prompt, n_layer) = {
        let m = hydra_engine_sys::Model::load(&model, 0).expect("load");
        let prompt: Vec<u32> = m.tokenize("The capital of France is").expect("tok").into_iter().map(|t| t as u32).collect();
        (prompt, m.n_layer())
    };
    let (k1, k2) = split3(n_layer);
    assert!(0 < k1 && k1 < k2 && k2 < n_layer, "valid 3-way split [0,{k1}) [{k1},{k2}) [{k2},{n_layer})");
    let n = 8usize;
    let n_ctx = prompt.len() as i32 + n as i32 + 8;
    let keys = SessionKeys::dev(0x3D);
    let cfg = greedy();

    // ---- reference: unsplit greedy ----
    let reference = unsplit_greedy(&model, &prompt, n, n_ctx);
    assert_eq!(reference.len(), n);

    // ---- the 3-node chained-FWD pipeline ----
    let ca = ClusterCa::new().unwrap();
    let dir = temp_dir();
    let store1 = dir.join("s1_boundaries.wal");
    let store2 = dir.join("s2_boundaries.wal");

    // Durability targets (one per forwarding edge): a mid-stage's copies rebuild the DOWNSTREAM stage
    // on recovery (seam C). Each persists BOUNDARY_COPY → BoundaryStore and acks DURABILITY_ACK.
    let dur1 = spawn_durability_endpoint(&ca, "dur1", store1.clone(), keys.clone());
    let dur2 = spawn_durability_endpoint(&ca, "dur2", store2.clone(), keys.clone());

    // S_P: final sampler stage, multi-conn (serves S2's FWD + the coordinator's SAMPLE_NEXT).
    let sp_id = ca.issue("sp").unwrap();
    let sp_addr = spawn_multiconn_endpoint(sp_cfg(&model, &keys, k2, n_ctx), ca.server_config(&sp_id).unwrap());

    // S2: middle forwarding+durable stage, down = S_P, dur = dur2.
    let s2_id = ca.issue("s2").unwrap();
    let s2_addr = spawn_multiconn_forwarding_durable_endpoint(
        s2_cfg(&model, &keys, k1, k2, n_ctx), ca.server_config(&s2_id).unwrap(),
        TcpMtls::from_config(ca.client_config(&s2_id).unwrap()).unwrap(), sp_addr, "sp",
        TcpMtls::from_config(ca.client_config(&s2_id).unwrap()).unwrap(), dur2, "dur2",
        true, 64,
    );

    // S1: first forwarding+durable stage, down = S2, dur = dur1.
    let s1_id = ca.issue("s1").unwrap();
    let s1_addr = spawn_multiconn_forwarding_durable_endpoint(
        s1_cfg(&model, &keys, k1, n_ctx), ca.server_config(&s1_id).unwrap(),
        TcpMtls::from_config(ca.client_config(&s1_id).unwrap()).unwrap(), s2_addr, "s2",
        TcpMtls::from_config(ca.client_config(&s1_id).unwrap()).unwrap(), dur1, "dur1",
        true, 64,
    );

    // Drive prefill + greedy generation. The coordinator connects only to S1 (data) and S_P (control);
    // S1→S2→S_P chaining is transparent to it. `Endpoints{s1, s2=S_P}` reuses the direct-FWD driver.
    let connector = TcpMtls::from_config(ca.client_config(&ca.issue("coordinator").unwrap()).unwrap()).unwrap();
    let ep = Endpoints::new(s1_addr, "s1", sp_addr, "sp");
    let tokens = run_direct_fwd_generation(&connector, &ep, &keys, &cfg, &prompt, n).await.expect("3-node generation");

    // (1) BYTE-IDENTICAL: the chained 3-node stream == the unsplit greedy run.
    assert_eq!(tokens, reference, "3-node chained direct FWD reproduces unsplit greedy byte-for-byte");

    // (2) durability wired into the chain: both edges captured a strictly-increasing boundary prefix
    // (the prompt boundaries are long-since persisted; the last one or two feedback copies may still
    // be in flight, so assert the durable prefix, not an exact tail count).
    for (label, path) in [("S1→S2", &store1), ("S2→S_P", &store2)] {
        let durable = BoundaryStore::read(path).unwrap();
        let positions: Vec<i64> = durable.iter().map(|b| b.first_input_pos).collect();
        assert!(positions.len() >= prompt.len(), "{label}: at least the prompt boundaries are durable ({} < {})", positions.len(), prompt.len());
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{label}: durable boundaries strictly increasing (no dup, no gap-reorder)");
        assert_eq!(positions[0], 0, "{label}: durable prefix starts at input position 0");
    }

    eprintln!("3-node chained direct FWD byte-identical (split [0,{k1}) [{k1},{k2}) [{k2},{n_layer}), cap-weighted 4.0/2.1/1.0). HONESTY: in-process localhost, NOT a WAN/heterogeneity number (part 2).");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A durability target: persist each `BOUNDARY_COPY` to a real `BoundaryStore`, ack the fdatasync'd
/// frontier. (Same shape as `durable_live`/`forwarding_durable`; inlined so this test is standalone.)
fn spawn_durability_endpoint(ca: &ClusterCa, name: &str, path: std::path::PathBuf, keys: SessionKeys) -> std::net::SocketAddr {
    use hydra_worker::wire::{self, Msg};
    use std::sync::mpsc;
    let id = ca.issue(name).unwrap();
    let server_cfg = ca.server_config(&id).unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = TcpMtlsListener::bind_with_config("127.0.0.1:0".parse().unwrap(), server_cfg).await.expect("bind dur");
            tx.send(listener.local_addr().unwrap()).unwrap();
            let mut store = BoundaryStore::create(&path, CLUSTER_ID, SESSION_ID).expect("store");
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
