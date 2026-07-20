//! P1·1b seam C — the **3-node kill-window** on the real **direct-FWD** topology (the flagship).
//!
//! Three stages S1 `[0,k1)` → S2 `[k1,k2)` → S_P `[k2,-1)`+sampler wired as a **chained direct FWD**
//! pipeline (seam B), each forwarding stage **durably copying** its boundary. Two adversarial kills,
//! each held to the **three-assertion gapless bar**:
//!   * **S_P kill** — the final stage dies; survivors S1+S2 freeze (Case A); the replacement S_P is
//!     rebuilt from **S2's durable boundaries** (D1, not token replay), sampler installed, activated;
//!     **S2 re-links** its direct down-link to the replacement (seam 2). Closes **gate-cond-(i)**
//!     (full recovery on the direct-FWD topology).
//!   * **middle-stage (S2) kill** — the genuinely-new case, with the **faithful survivor freeze**
//!     (§7.19 FIXED + CLOSED): freeze → **catch-up to FROZEN_READY** (the hang was a skipped catch-up,
//!     an orchestration omission — the SM correctly gates RecvCommit on FROZEN_READY) → reinstall (I17)
//!     → reactivate. The replacement S2 is rebuilt from **S1's durable boundaries** and **S1 re-links**
//!     to it. A second test covers the observably-load-bearing **KV-ahead** placement (S_P's KV runs one
//!     position past the durable frontier): the survivor sampler regenerates fresh head logits by
//!     **truncate-and-replay** (roll applied to goal-1, re-apply position goal teacher-forced with S2's
//!     durable boundary — Strategy-A/B's next_logits_ready; no new machinery). Qualifier removed.
//!
//! Assertions (both): (a) SSE id continuity (commit stream dense, every output position once);
//! (b) committed prefix ⊕ resumed suffix == an uninterrupted seeded run, **byte-for-byte**; (c) disk
//! truth — commit stream I19-valid, no output position twice. Timing honesty-annotated: **in-process
//! localhost, NOT the <15 s LAN/M3 D1 target**; the real 3-node `kill -9` over Tailscale is
//! `hydra-3node-wan` / the `container-3node` fallback.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hydra_coordinator::{BoundaryStore, CommitStream, GroupCommitter, WalFenceCtx};
use hydra_state::{ActivationKind, ActivationTuple};
use hydra_transport::framed::Conn;
use hydra_transport::tcp_mtls::TcpMtls;
use hydra_tokenizer::Admission;
use hydra_worker::pair::{dev_model_path, spawn_multiconn_endpoint, spawn_multiconn_forwarding_durable_endpoint, Cluster};
use hydra_worker::sampler::{initial_checkpoint_bytes, SamplingConfig};
use hydra_worker::wire::{self, Msg, SessionKeys};
use hydra_worker::worker::{DownTarget, WorkerConfig, INITIAL_CHECKPOINT_ID};
use tokio::io::{AsyncRead, AsyncWrite};

const CLUSTER_ID: [u8; 16] = [0x3C; 16];
const SESSION_ID: [u8; 16] = [0x53; 16];
static SEQ: AtomicU32 = AtomicU32::new(0);

fn greedy() -> SamplingConfig {
    SamplingConfig { temperature: 0.0, top_p: 1.0, repeat_penalty: 1.0, penalty_last_n: 0, seed: 5 }
}
fn fence() -> WalFenceCtx {
    WalFenceCtx { cluster_id: CLUSTER_ID, session_id: SESSION_ID, model_instance_id: [3; 16], manifest_hash: [4; 32], epoch: 0, recovery_id: 0, activation_attempt_id: 0 }
}
fn admission(prompt: &[u32]) -> Admission {
    Admission { tokenizer_hash: [1; 32], chat_template_hash: [2; 32], rendered_prompt_bytes_hash: [3; 32], rendered_prompt: String::new(), prompt_tokens: prompt.to_vec() }
}
fn temp_dir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("hydra-3nrec-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&d).unwrap();
    d
}
fn split3(n_layer: i32) -> (i32, i32) {
    let l = n_layer as f64;
    let k1 = ((0.563 * l).round() as i32).clamp(1, n_layer - 2);
    let k2 = (k1 + (0.296 * l).round() as i32).clamp(k1 + 1, n_layer - 1);
    (k1, k2)
}

fn s1_cfg(model: &str, keys: &SessionKeys, k1: i32, n_ctx: i32) -> WorkerConfig {
    WorkerConfig { keys: keys.clone(), rank: 0, layer_first: 0, layer_last: k1, is_final: false, receives_tokens: true, epoch: 0, recovery_id: 0, model_path: Some(model.into()), n_gpu_layers: 0, n_ctx, sampler_config: None, recovery_start: false }
}
fn s2_cfg(model: &str, keys: &SessionKeys, k1: i32, k2: i32, n_ctx: i32, recovery_start: bool) -> WorkerConfig {
    WorkerConfig { keys: keys.clone(), rank: 1, layer_first: k1, layer_last: k2, is_final: false, receives_tokens: false, epoch: 0, recovery_id: if recovery_start { 1 } else { 0 }, model_path: Some(model.into()), n_gpu_layers: 0, n_ctx, sampler_config: None, recovery_start }
}
fn sp_cfg(model: &str, keys: &SessionKeys, k2: i32, n_ctx: i32, recovery_start: bool) -> WorkerConfig {
    WorkerConfig { keys: keys.clone(), rank: 2, layer_first: k2, layer_last: -1, is_final: true, receives_tokens: false, epoch: 0, recovery_id: if recovery_start { 1 } else { 0 }, model_path: Some(model.into()), n_gpu_layers: 0, n_ctx, sampler_config: Some(greedy()), recovery_start }
}

/// A durability target: persist each BOUNDARY_COPY to a real BoundaryStore, ack the fdatasync'd frontier.
fn spawn_durability(cluster: &Cluster, name: &str, path: std::path::PathBuf, keys: SessionKeys) -> SocketAddr {
    use std::sync::mpsc;
    let id = cluster.issue(name).unwrap();
    let server_cfg = cluster.ca.server_config(&id).unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = hydra_transport::tcp_mtls::TcpMtlsListener::bind_with_config("127.0.0.1:0".parse().unwrap(), server_cfg).await.expect("bind dur");
            tx.send(listener.local_addr().unwrap()).unwrap();
            let mut store = BoundaryStore::create(&path, CLUSTER_ID, SESSION_ID).expect("store");
            let mut conn = listener.accept().await.expect("accept");
            while let Ok(frame) = conn.recv().await {
                if let Ok((view, Msg::BoundaryCopy { boundary_id, first_input_pos, chunk_id, activations })) = wire::decode(&frame.payload, &keys) {
                    let d = store.append_boundary(boundary_id, first_input_pos, chunk_id, &activations).unwrap_or(-1);
                    if conn.send(0, &wire::encode_durability_ack(&keys, view.epoch, boundary_id, d, 0)).await.is_err() {
                        break;
                    }
                }
            }
        });
    });
    rx.recv().expect("dur addr")
}

/// A discard sink: accept FWD, ack APPLIED_ACK{first_input_pos}, drop the boundary. Used as a
/// replacement middle stage's downstream during rebuild (the survivor downstream already holds those
/// positions, so its rebuild-time outputs are byte-identical and safely discarded).
fn spawn_sink(cluster: &Cluster, name: &str, keys: SessionKeys) -> SocketAddr {
    use std::sync::mpsc;
    let id = cluster.issue(name).unwrap();
    let server_cfg = cluster.ca.server_config(&id).unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = hydra_transport::tcp_mtls::TcpMtlsListener::bind_with_config("127.0.0.1:0".parse().unwrap(), server_cfg).await.expect("bind sink");
            tx.send(listener.local_addr().unwrap()).unwrap();
            let mut conn = listener.accept().await.expect("accept");
            while let Ok(frame) = conn.recv().await {
                if let Ok((view, Msg::Fwd { first_input_pos, .. })) = wire::decode(&frame.payload, &keys) {
                    if conn.send(0, &wire::encode_applied_ack(&keys, view.epoch, first_input_pos, &[0u8; 32])).await.is_err() {
                        break;
                    }
                }
            }
        });
    });
    rx.recv().expect("sink addr")
}

// --- coordinator-side drivers (direct-FWD chain: C talks only to S1 and S_P; chain is transparent) ---

async fn activate<S: AsyncRead + AsyncWrite + Unpin>(c: &mut Conn<S>, keys: &SessionKeys, kind: ActivationKind, epoch: u32, rid: u32) -> Result<(), String> {
    let t = ActivationTuple { kind, epoch, recovery_id: rid, attempt: 0, sampler_checkpoint_id: if matches!(kind, ActivationKind::Recovery) { INITIAL_CHECKPOINT_ID } else { 0 } };
    c.send(0, &wire::encode_commit_activation(keys, &t, 1)).await.map_err(|e| e.to_string())?;
    match wire::decode(&c.recv().await.map_err(|e| e.to_string())?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::ActivationCommitted(_) => {}
        o => return Err(format!("expected ACTIVATION_COMMITTED, got {o:?}")),
    }
    c.send(0, &wire::encode_finalize_activation(keys, &t, 1)).await.map_err(|e| e.to_string())?;
    match wire::decode(&c.recv().await.map_err(|e| e.to_string())?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::ActivationFinalized => Ok(()),
        o => Err(format!("expected ACTIVATION_FINALIZED, got {o:?}")),
    }
}

/// C→S1 APPLY_TOKEN → (S1→S2→S_P direct FWD, durable copies) → APPLIED_ACK relayed back up to C.
async fn chain_apply<S: AsyncRead + AsyncWrite + Unpin>(c1: &mut Conn<S>, keys: &SessionKeys, input_pos: i64, token: u32, no_sample: bool) -> Result<(), String> {
    c1.send(0, &wire::encode_apply_token(keys, 0, input_pos, token, no_sample)).await.map_err(|e| e.to_string())?;
    match wire::decode(&c1.recv().await.map_err(|e| format!("recv chain ack: {e}"))?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::AppliedAck { cumulative_input_pos, .. } if cumulative_input_pos == input_pos => Ok(()),
        o => Err(format!("chain @ {input_pos}: expected APPLIED_ACK, got {o:?}")),
    }
}

async fn sample<S: AsyncRead + AsyncWrite + Unpin>(c: &mut Conn<S>, keys: &SessionKeys, output_pos: i64, h: &[u8; 32]) -> Result<(u32, Vec<u8>), String> {
    c.send(0, &wire::encode_sample_next(keys, 0, output_pos, h, INITIAL_CHECKPOINT_ID)).await.map_err(|e| e.to_string())?;
    match wire::decode(&c.recv().await.map_err(|e| e.to_string())?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::Sampled { token_id, post_sample_snapshot, .. } => Ok((token_id, post_sample_snapshot)),
        Msg::Err { code } => Err(format!("SAMPLE_NEXT @ {output_pos} err {code}")),
        o => Err(format!("SAMPLE_NEXT @ {output_pos}: expected SAMPLED, got {o:?}")),
    }
}

/// FWD a durable boundary directly to a (replacement) stage — rebuilds its KV from boundaries. For a
/// final stage the reply is APPLIED_ACK; for a middle stage (down-linked to a sink) it is too (relayed).
async fn rebuild_apply<S: AsyncRead + AsyncWrite + Unpin>(c: &mut Conn<S>, keys: &SessionKeys, input_pos: i64, boundary: &[f32]) -> Result<(), String> {
    c.send(0, &wire::encode_fwd(keys, 0, input_pos, true, boundary)).await.map_err(|e| e.to_string())?;
    match wire::decode(&c.recv().await.map_err(|e| e.to_string())?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::AppliedAck { cumulative_input_pos, .. } if cumulative_input_pos == input_pos => Ok(()),
        o => Err(format!("rebuild @ {input_pos}: expected APPLIED_ACK, got {o:?}")),
    }
}

async fn begin_recovery<S: AsyncRead + AsyncWrite + Unpin>(c: &mut Conn<S>, keys: &SessionKeys, truncate_to: i64) -> Result<(), String> {
    c.send(0, &wire::encode_begin_recovery(keys, 0, 1, 1, truncate_to)).await.map_err(|e| e.to_string())?;
    match wire::decode(&c.recv().await.map_err(|e| e.to_string())?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::RecoveryAck { .. } => Ok(()),
        o => Err(format!("expected RECOVERY_ACK, got {o:?}")),
    }
}

struct Pipeline {
    cluster: Cluster,
    model: String,
    dir: std::path::PathBuf,
    store1: std::path::PathBuf,
    store2: std::path::PathBuf,
    s1_down: DownTarget,
    s2_down: DownTarget,
    s1_addr: SocketAddr,
    sp_addr: SocketAddr,
}

/// Stand up S1→S2→S_P (direct FWD, durable) and return the pipeline + the coordinator's S1/S_P conns.
fn build_pipeline(model: &str, keys: &SessionKeys, k1: i32, k2: i32, n_ctx: i32) -> Pipeline {
    let cluster = Cluster::new().unwrap();
    let dir = temp_dir();
    let store1 = dir.join("s1.wal");
    let store2 = dir.join("s2.wal");
    let dur1 = spawn_durability(&cluster, "dur1", store1.clone(), keys.clone());
    let dur2 = spawn_durability(&cluster, "dur2", store2.clone(), keys.clone());

    let sp_id = cluster.issue("sp").unwrap();
    let sp_addr = spawn_multiconn_endpoint(sp_cfg(model, keys, k2, n_ctx, false), cluster.ca.server_config(&sp_id).unwrap());

    let s2_id = cluster.issue("s2").unwrap();
    let s2_down: DownTarget = Arc::new(Mutex::new((sp_addr, "sp".to_string())));
    let s2_addr = spawn_multiconn_forwarding_durable_endpoint(
        s2_cfg(model, keys, k1, k2, n_ctx, false), cluster.ca.server_config(&s2_id).unwrap(),
        TcpMtls::from_config(cluster.ca.client_config(&s2_id).unwrap()).unwrap(), s2_down.clone(), 20,
        TcpMtls::from_config(cluster.ca.client_config(&s2_id).unwrap()).unwrap(), dur2, "dur2",
        true, 4,
    );

    let s1_id = cluster.issue("s1").unwrap();
    let s1_down: DownTarget = Arc::new(Mutex::new((s2_addr, "s2".to_string())));
    let s1_addr = spawn_multiconn_forwarding_durable_endpoint(
        s1_cfg(model, keys, k1, n_ctx), cluster.ca.server_config(&s1_id).unwrap(),
        TcpMtls::from_config(cluster.ca.client_config(&s1_id).unwrap()).unwrap(), s1_down.clone(), 20,
        TcpMtls::from_config(cluster.ca.client_config(&s1_id).unwrap()).unwrap(), dur1, "dur1",
        true, 4,
    );

    Pipeline { cluster, model: model.into(), dir, store1, store2, s1_down, s2_down, s1_addr, sp_addr }
}

fn unsplit_greedy(model: &str, prompt: &[u32], n: usize, n_ctx: i32) -> Vec<u32> {
    let m = hydra_engine_sys::Model::load(model, 0).expect("load");
    let mut ctx = m.context(0, -1, false, n_ctx, n_ctx).expect("ctx");
    for (pos, &t) in prompt.iter().enumerate() {
        ctx.apply_tokens(&[t as i32], pos as i32, None).expect("prefill");
    }
    let argmax = |l: &[f32]| (0..l.len()).max_by(|&a, &b| l[a].total_cmp(&l[b])).unwrap() as u32;
    let mut out = Vec::with_capacity(n);
    let mut pos = prompt.len();
    for _ in 0..n {
        out.push(argmax(&ctx.logits(0).expect("logits")));
        ctx.apply_tokens(&[*out.last().unwrap() as i32], pos as i32, None).expect("feedback");
        pos += 1;
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_node_kill_s_p_rebuilds_from_durable_boundaries_and_relinks_byte_identical() {
    let Some(model) = dev_model_path() else {
        eprintln!("SKIP: no engine/model (dev-environment artifacts)");
        return;
    };
    let (prompt, n_layer) = {
        let m = hydra_engine_sys::Model::load(&model, 0).expect("load");
        let p: Vec<u32> = m.tokenize("The capital of France is").expect("tok").into_iter().map(|t| t as u32).collect();
        (p, m.n_layer())
    };
    let (k1, k2) = split3(n_layer);
    let n = 8usize;
    let m_kill = n / 2;
    let n_ctx = prompt.len() as i32 + n as i32 + 8;
    let keys = SessionKeys::dev(0x5C);
    let cfg_hash = greedy().hash();

    let reference = unsplit_greedy(&model, &prompt, n, n_ctx);

    let pl = build_pipeline(&model, &keys, k1, k2, n_ctx);
    let cs_path = pl.dir.join("commit.wal");
    let mut cs = CommitStream::create(&cs_path, CLUSTER_ID, SESSION_ID).unwrap();
    cs.append_initial_commit(&fence(), &admission(&prompt), &initial_checkpoint_bytes(INITIAL_CHECKPOINT_ID, &greedy()), 1).unwrap();
    let mut group = GroupCommitter::new(2);

    let connector = pl.cluster.coordinator_connector().unwrap();
    let mut c1 = connector.connect(pl.s1_addr, "s1").await.unwrap();
    let mut cp = connector.connect(pl.sp_addr, "sp").await.unwrap();
    // Control connections to S2 (for freeze) — S2 is reachable directly (multi-conn).
    let s2_addr = pl.s1_down.lock().unwrap().0;
    let mut c2 = connector.connect(s2_addr, "s2").await.unwrap();

    activate(&mut c1, &keys, ActivationKind::Initial, 0, 0).await.unwrap();
    activate(&mut c2, &keys, ActivationKind::Initial, 0, 0).await.unwrap();
    activate(&mut cp, &keys, ActivationKind::Initial, 0, 0).await.unwrap();

    // Prefill + generate m tokens through the chain.
    for (i, &t) in prompt.iter().enumerate() {
        chain_apply(&mut c1, &keys, i as i64, t, true).await.unwrap();
    }
    let mut committed: Vec<u32> = Vec::new();
    let mut input_pos = prompt.len() as i64;
    for q in 0..m_kill as i64 {
        let (tok, snap) = sample(&mut cp, &keys, q, &cfg_hash).await.unwrap();
        committed.push(tok);
        group.push(q, tok, snap);
        if group.count_ready() {
            let b = group.take().unwrap();
            cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).unwrap();
        }
        chain_apply(&mut c1, &keys, input_pos, tok, false).await.unwrap();
        input_pos += 1;
    }
    if let Some(b) = group.take() {
        cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).unwrap();
    }
    assert_eq!(committed, reference[..m_kill].to_vec(), "pre-kill greedy matches the reference");

    // Let the background durability copies settle, then confirm S2's boundaries (S_P's rebuild source).
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let boundaries = BoundaryStore::read(&pl.store2).unwrap();
    assert!(boundaries.len() as i64 >= input_pos, "S2 durably copied every applied boundary ({} >= {input_pos})", boundaries.len());

    // ---- kill S_P ----
    let t_detect = Instant::now();
    drop(cp);
    // Survivors S1 + S2 take BEGIN_RECOVERY Case A (freeze).
    begin_recovery(&mut c1, &keys, input_pos).await.unwrap();
    begin_recovery(&mut c2, &keys, input_pos).await.unwrap();

    // Replacement S_P: rebuild from S2's DURABLE boundaries (not tokens — the D1 difference).
    let rsp_id = pl.cluster.issue("sp").unwrap();
    let rsp_addr = spawn_multiconn_endpoint(sp_cfg(&pl.model, &keys, k2, n_ctx, true), pl.cluster.ca.server_config(&rsp_id).unwrap());
    let mut rcp = connector.connect(rsp_addr, "sp").await.unwrap();
    begin_recovery(&mut rcp, &keys, 0).await.unwrap();
    for b in boundaries.iter().take(input_pos as usize) {
        rebuild_apply(&mut rcp, &keys, b.first_input_pos, &b.activations).await.unwrap();
    }
    rcp.send(0, &wire::encode_catch_up_context(&keys, 0, 1, input_pos)).await.unwrap();
    assert!(matches!(wire::decode(&rcp.recv().await.unwrap().payload, &keys).unwrap().1, Msg::CatchUpReady { .. }));
    rcp.send(0, &wire::encode_install_sampler_checkpoint(&keys, 0, INITIAL_CHECKPOINT_ID, &initial_checkpoint_bytes(INITIAL_CHECKPOINT_ID, &greedy()))).await.unwrap();
    assert!(matches!(wire::decode(&rcp.recv().await.unwrap().payload, &keys).unwrap().1, Msg::SamplerCheckpointInstalled { .. }));
    activate(&mut rcp, &keys, ActivationKind::Recovery, 1, 1).await.unwrap();

    // S2 re-links its direct down-link to the replacement S_P (seam 2).
    *pl.s2_down.lock().unwrap() = (rsp_addr, "sp".to_string());
    let detect_to_resumed = t_detect.elapsed();

    // ---- resume: sample from the replacement S_P; feedback through S1→S2→(re-linked)→new S_P ----
    let mut resumed = Vec::new();
    for q in (m_kill as i64)..n as i64 {
        let (tok, snap) = sample(&mut rcp, &keys, q, &cfg_hash).await.unwrap();
        resumed.push(tok);
        group.push(q, tok, snap);
        if group.count_ready() {
            let b = group.take().unwrap();
            cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).unwrap();
        }
        if (q as usize + 1) < n {
            chain_apply(&mut c1, &keys, input_pos, tok, false).await.unwrap();
            input_pos += 1;
        }
    }
    if let Some(b) = group.take() {
        cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).unwrap();
    }
    drop(cs);

    // (b) byte-identical.
    let mut full = committed.clone();
    full.extend(resumed);
    assert_eq!(full, reference, "recovered 3-node stream (S_P rebuilt from S2's durable boundaries, S2 re-linked) == uninterrupted greedy");
    // (a) SSE id continuity + (c) disk truth.
    let stats = hydra_coordinator::recovery::verify(&cs_path).unwrap();
    assert_eq!(stats.committed_positions, n);
    assert!(stats.positions_strictly_increasing);
    assert_eq!(stats.max_position, n as i64 - 1);

    eprintln!("seam C (kill S_P → closes gate-cond-(i)): detection→resumed {detect_to_resumed:?} (HONESTY: in-process local, NOT the <15s LAN/M3 D1 target; real 3-node kill -9 = hydra-3node-wan / container-3node)");
    let _ = std::fs::remove_dir_all(&pl.dir);
}

/// The genuinely-new case: kill the **MIDDLE** stage S2. The replacement S2 is rebuilt from **S1's**
/// durable boundaries (S2's inputs) — its rebuild-time outputs go to a discard **sink** (the survivor
/// downstream S_P already holds those positions, and the outputs are byte-identical). The downstream
/// survivor S_P is frozen (Case A) then re-activated; **S1 re-links** its direct down-link to the
/// replacement S2 (seam 2). Same three-assertion gapless bar.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_node_kill_middle_s2_rebuilds_from_upstream_durable_boundaries_byte_identical() {
    let Some(model) = dev_model_path() else {
        eprintln!("SKIP: no engine/model (dev-environment artifacts)");
        return;
    };
    let (prompt, n_layer) = {
        let m = hydra_engine_sys::Model::load(&model, 0).expect("load");
        let p: Vec<u32> = m.tokenize("The capital of France is").expect("tok").into_iter().map(|t| t as u32).collect();
        (p, m.n_layer())
    };
    let (k1, k2) = split3(n_layer);
    let n = 8usize;
    let m_kill = n / 2;
    let n_ctx = prompt.len() as i32 + n as i32 + 8;
    let keys = SessionKeys::dev(0x52);
    let cfg_hash = greedy().hash();

    let reference = unsplit_greedy(&model, &prompt, n, n_ctx);

    let pl = build_pipeline(&model, &keys, k1, k2, n_ctx);
    let cs_path = pl.dir.join("commit.wal");
    let mut cs = CommitStream::create(&cs_path, CLUSTER_ID, SESSION_ID).unwrap();
    cs.append_initial_commit(&fence(), &admission(&prompt), &initial_checkpoint_bytes(INITIAL_CHECKPOINT_ID, &greedy()), 1).unwrap();
    let mut group = GroupCommitter::new(2);

    let connector = pl.cluster.coordinator_connector().unwrap();
    let mut c1 = connector.connect(pl.s1_addr, "s1").await.unwrap();
    let mut cp = connector.connect(pl.sp_addr, "sp").await.unwrap();

    activate(&mut c1, &keys, ActivationKind::Initial, 0, 0).await.unwrap();
    activate(&mut cp, &keys, ActivationKind::Initial, 0, 0).await.unwrap();

    for (i, &t) in prompt.iter().enumerate() {
        chain_apply(&mut c1, &keys, i as i64, t, true).await.unwrap();
    }
    let mut committed: Vec<u32> = Vec::new();
    let mut input_pos = prompt.len() as i64;
    for q in 0..m_kill as i64 {
        let (tok, snap) = sample(&mut cp, &keys, q, &cfg_hash).await.unwrap();
        committed.push(tok);
        group.push(q, tok, snap);
        if group.count_ready() {
            let b = group.take().unwrap();
            cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).unwrap();
        }
        chain_apply(&mut c1, &keys, input_pos, tok, false).await.unwrap();
        input_pos += 1;
    }
    if let Some(b) = group.take() {
        cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).unwrap();
    }
    assert_eq!(committed, reference[..m_kill].to_vec(), "pre-kill greedy matches the reference");

    // S1's durable boundaries are the replacement S2's rebuild source.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let boundaries = BoundaryStore::read(&pl.store1).unwrap();
    assert!(boundaries.len() as i64 >= input_pos, "S1 durably copied every applied boundary ({} >= {input_pos})", boundaries.len());

    // ---- kill the MIDDLE stage S2 ----
    let t_detect = Instant::now();
    // Downstream survivor S_P freezes (Case A) — the ratified survivor path (§7.19). The §7.19 hang
    // was an ORCHESTRATION omission: a Case-A freeze leaves the stage FROZEN, and `RecvCommit` needs
    // FROZEN_READY, reached via **catch-up** (RebuildStep). The survivor's KV is intact, so the
    // catch-up re-applies nothing (applied ≥ goal → immediate FROZEN_READY). freeze → catch-up →
    // reinstall → reactivate — the fix proven by `survivor_reactivate.rs` (100×).
    begin_recovery(&mut cp, &keys, input_pos).await.unwrap();
    cp.send(0, &wire::encode_catch_up_context(&keys, 1, 1, input_pos - 1)).await.unwrap();
    match wire::decode(&cp.recv().await.unwrap().payload, &keys).unwrap().1 {
        Msg::CatchUpReady { .. } => {}
        o => panic!("survivor S_P catch-up to FROZEN_READY: expected CATCH_UP_READY, got {o:?}"),
    }

    // Replacement S2: rebuild from S1's DURABLE boundaries, its outputs going to a discard SINK
    // (S_P already holds those positions). During rebuild its down-link points at the sink.
    let sink = spawn_sink(&pl.cluster, "sink", keys.clone());
    let rs2_id = pl.cluster.issue("s2").unwrap();
    let rs2_down: DownTarget = Arc::new(Mutex::new((sink, "sink".to_string())));
    let rs2_addr = spawn_multiconn_forwarding_durable_endpoint(
        s2_cfg(&pl.model, &keys, k1, k2, n_ctx, true), pl.cluster.ca.server_config(&rs2_id).unwrap(),
        TcpMtls::from_config(pl.cluster.ca.client_config(&rs2_id).unwrap()).unwrap(), rs2_down.clone(), 20,
        TcpMtls::from_config(pl.cluster.ca.client_config(&rs2_id).unwrap()).unwrap(),
        spawn_durability(&pl.cluster, "dur2b", pl.dir.join("s2b.wal"), keys.clone()), "dur2b",
        true, 4,
    );
    let mut rc2 = connector.connect(rs2_addr, "s2").await.unwrap();
    begin_recovery(&mut rc2, &keys, 0).await.unwrap();
    for b in boundaries.iter().take(input_pos as usize) {
        rebuild_apply(&mut rc2, &keys, b.first_input_pos, &b.activations).await.unwrap();
    }
    rc2.send(0, &wire::encode_catch_up_context(&keys, 0, 1, input_pos)).await.unwrap();
    assert!(matches!(wire::decode(&rc2.recv().await.unwrap().payload, &keys).unwrap().1, Msg::CatchUpReady { .. }));
    activate(&mut rc2, &keys, ActivationKind::Recovery, 1, 1).await.unwrap();

    // Rebuild done → point the replacement S2's down-link at the real (survivor) S_P.
    *rs2_down.lock().unwrap() = (pl.sp_addr, "sp".to_string());
    // Re-activate the frozen survivor S_P (I17: reinstall the sampler checkpoint before activation),
    // then S1 re-links to the replacement S2.
    cp.send(0, &wire::encode_install_sampler_checkpoint(&keys, 0, INITIAL_CHECKPOINT_ID, &initial_checkpoint_bytes(INITIAL_CHECKPOINT_ID, &greedy()))).await.unwrap();
    assert!(matches!(wire::decode(&cp.recv().await.unwrap().payload, &keys).unwrap().1, Msg::SamplerCheckpointInstalled { .. }));
    activate(&mut cp, &keys, ActivationKind::Recovery, 1, 1).await.unwrap();
    // S1 re-links its direct down-link to the replacement S2 (the seam-2 re-link, middle-stage).
    *pl.s1_down.lock().unwrap() = (rs2_addr, "s2".to_string());
    let detect_to_resumed = t_detect.elapsed();

    // ---- resume: sample from the survivor S_P; feedback through S1→(re-linked)→new S2→S_P ----
    let mut resumed = Vec::new();
    for q in (m_kill as i64)..n as i64 {
        let (tok, snap) = sample(&mut cp, &keys, q, &cfg_hash).await.unwrap();
        resumed.push(tok);
        group.push(q, tok, snap);
        if group.count_ready() {
            let b = group.take().unwrap();
            cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).unwrap();
        }
        if (q as usize + 1) < n {
            chain_apply(&mut c1, &keys, input_pos, tok, false).await.unwrap();
            input_pos += 1;
        }
    }
    if let Some(b) = group.take() {
        cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).unwrap();
    }
    drop(cs);

    let mut full = committed.clone();
    full.extend(resumed);
    assert_eq!(full, reference, "recovered 3-node stream (MIDDLE S2 rebuilt from S1's durable boundaries, S1 re-linked) == uninterrupted greedy");
    let stats = hydra_coordinator::recovery::verify(&cs_path).unwrap();
    assert_eq!(stats.committed_positions, n);
    assert!(stats.positions_strictly_increasing);
    assert_eq!(stats.max_position, n as i64 - 1);

    eprintln!("seam C (kill MIDDLE S2): detection→resumed {detect_to_resumed:?} (HONESTY: in-process local, NOT the <15s LAN/M3 D1 target)");
    let _ = std::fs::remove_dir_all(&pl.dir);
}

/// §7.19 regression (b) — the observably-**load-bearing KV-ahead** middle-kill (the placement that
/// makes the survivor's Case-A freeze load-bearing, I7a/I7b/I15). S_P samples q=m AND its feedback is
/// applied — S1/S2/S_P KV advance one position **beyond generation_durable_pos**, none committed. Then
/// S2 dies. The replacement S2 rebuilds only to the **durable frontier**, so the survivors must
/// discard the provisional tail back to it. The survivor sampler S_P regenerates its stale head logits
/// by **truncate-and-replay** (owner-ruled, no new machinery): roll applied to goal-1, then re-apply
/// position goal teacher-forced with S2's durable boundary — the last application regenerates fresh
/// logits as a side effect (Strategy A/B's next_logits_ready). WITHOUT the §7.19 fix + this replay the
/// survivor samples the wrong position; WITH them the stream is byte-identical. **This is the
/// regression that removes the "durable-frontier kill placement" qualifier.**
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_node_kill_middle_with_sampled_ahead_survivor_truncates_byte_identical() {
    let Some(model) = dev_model_path() else {
        eprintln!("SKIP: no engine/model (dev-environment artifacts)");
        return;
    };
    let (prompt, n_layer) = {
        let m = hydra_engine_sys::Model::load(&model, 0).expect("load");
        let p: Vec<u32> = m.tokenize("The capital of France is").expect("tok").into_iter().map(|t| t as u32).collect();
        (p, m.n_layer())
    };
    let (k1, k2) = split3(n_layer);
    let n = 8usize;
    let m = 3usize; // commit m, then sample+feed ONE provisional position beyond the durable frontier
    let n_ctx = prompt.len() as i32 + n as i32 + 8;
    let keys = SessionKeys::dev(0x7B);
    let cfg_hash = greedy().hash();

    let reference = unsplit_greedy(&model, &prompt, n, n_ctx);

    let pl = build_pipeline(&model, &keys, k1, k2, n_ctx);
    let cs_path = pl.dir.join("commit.wal");
    let mut cs = CommitStream::create(&cs_path, CLUSTER_ID, SESSION_ID).unwrap();
    cs.append_initial_commit(&fence(), &admission(&prompt), &initial_checkpoint_bytes(INITIAL_CHECKPOINT_ID, &greedy()), 1).unwrap();
    let mut group = GroupCommitter::new(2);

    let connector = pl.cluster.coordinator_connector().unwrap();
    let mut c1 = connector.connect(pl.s1_addr, "s1").await.unwrap();
    let mut cp = connector.connect(pl.sp_addr, "sp").await.unwrap();

    activate(&mut c1, &keys, ActivationKind::Initial, 0, 0).await.unwrap();
    activate(&mut cp, &keys, ActivationKind::Initial, 0, 0).await.unwrap();

    for (i, &t) in prompt.iter().enumerate() {
        chain_apply(&mut c1, &keys, i as i64, t, true).await.unwrap();
    }
    // Commit m outputs (durable).
    let mut committed: Vec<u32> = Vec::new();
    let mut input_pos = prompt.len() as i64;
    for q in 0..m as i64 {
        let (tok, snap) = sample(&mut cp, &keys, q, &cfg_hash).await.unwrap();
        committed.push(tok);
        group.push(q, tok, snap);
        if group.count_ready() {
            let b = group.take().unwrap();
            cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).unwrap();
        }
        chain_apply(&mut c1, &keys, input_pos, tok, false).await.unwrap();
        input_pos += 1;
    }
    if let Some(b) = group.take() {
        cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).unwrap();
    }
    // The durable input frontier (last committed feedback) is input_pos-1.
    let durable_frontier = input_pos - 1;

    // PROVISIONAL **KV-AHEAD** (the observably-load-bearing placement): S_P samples q=m AND its
    // feedback is applied — S1/S2/S_P KV advance to `provisional_pos = durable_frontier + 1`, beyond
    // generation_durable_pos, and none of it is committed. This is the tail the survivors' Case-A
    // freeze must discard (I7a/I7b): the replacement S2 rebuilds only to the durable frontier, so a
    // survivor whose KV outruns it produces WRONG logits unless truncated back.
    let (prov_tok, _snap) = sample(&mut cp, &keys, m as i64, &cfg_hash).await.unwrap();
    chain_apply(&mut c1, &keys, input_pos, prov_tok, false).await.unwrap();
    let provisional_pos = input_pos; // = durable_frontier + 1

    // Durable boundaries. Settle the WAN-class copies first. S1's outputs rebuild the replacement S2;
    // S2's output boundary at the frontier is what the survivor S_P re-applies to regenerate logits.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let boundaries = BoundaryStore::read(&pl.store1).unwrap();
    let boundaries_s2 = BoundaryStore::read(&pl.store2).unwrap();
    let frontier_boundary = boundaries_s2.iter().find(|b| b.first_input_pos == durable_frontier)
        .expect("S2's durable output boundary at the frontier").activations.clone();

    // ---- kill the MIDDLE stage S2 ----
    let t_detect = Instant::now();
    // Freeze survivors. S1 (forwarder) truncates its KV to the durable frontier (drops the provisional
    // position); it re-forwards the re-applied position on resume, so it needs no logits regen.
    begin_recovery(&mut c1, &keys, durable_frontier).await.unwrap();
    // S_P (sampler) — TRUNCATE-AND-REPLAY (owner-ruled, no new machinery): roll applied to goal-1
    // (durable_frontier-1), then re-apply position `durable_frontier` teacher-forced with S2's durable
    // boundary. The last application regenerates FRESH head logits as a side effect (exactly how
    // Strategy A/B guarantee next_logits_ready) — its previously-retained logits came from the (now
    // truncated) provisional application and are stale.
    begin_recovery(&mut cp, &keys, durable_frontier - 1).await.unwrap();
    rebuild_apply(&mut cp, &keys, durable_frontier, &frontier_boundary).await.unwrap();
    cp.send(0, &wire::encode_catch_up_context(&keys, 1, 1, durable_frontier)).await.unwrap();
    assert!(matches!(wire::decode(&cp.recv().await.unwrap().payload, &keys).unwrap().1, Msg::CatchUpReady { .. }));

    // Replacement S2: rebuild from S1's durable boundaries ONLY to the durable frontier (drop the
    // provisional boundary at `provisional_pos`).
    let sink = spawn_sink(&pl.cluster, "sink", keys.clone());
    let rs2_id = pl.cluster.issue("s2").unwrap();
    let rs2_down: DownTarget = Arc::new(Mutex::new((sink, "sink".to_string())));
    let rs2_addr = spawn_multiconn_forwarding_durable_endpoint(
        s2_cfg(&pl.model, &keys, k1, k2, n_ctx, true), pl.cluster.ca.server_config(&rs2_id).unwrap(),
        TcpMtls::from_config(pl.cluster.ca.client_config(&rs2_id).unwrap()).unwrap(), rs2_down.clone(), 20,
        TcpMtls::from_config(pl.cluster.ca.client_config(&rs2_id).unwrap()).unwrap(),
        spawn_durability(&pl.cluster, "dur2b", pl.dir.join("s2b.wal"), keys.clone()), "dur2b",
        true, 4,
    );
    let mut rc2 = connector.connect(rs2_addr, "s2").await.unwrap();
    begin_recovery(&mut rc2, &keys, 0).await.unwrap();
    for b in boundaries.iter().filter(|b| b.first_input_pos <= durable_frontier).take((durable_frontier + 1) as usize) {
        rebuild_apply(&mut rc2, &keys, b.first_input_pos, &b.activations).await.unwrap();
    }
    rc2.send(0, &wire::encode_catch_up_context(&keys, 0, 1, durable_frontier)).await.unwrap();
    assert!(matches!(wire::decode(&rc2.recv().await.unwrap().payload, &keys).unwrap().1, Msg::CatchUpReady { .. }));
    activate(&mut rc2, &keys, ActivationKind::Recovery, 1, 1).await.unwrap();

    *rs2_down.lock().unwrap() = (pl.sp_addr, "sp".to_string());
    cp.send(0, &wire::encode_install_sampler_checkpoint(&keys, 0, INITIAL_CHECKPOINT_ID, &initial_checkpoint_bytes(INITIAL_CHECKPOINT_ID, &greedy()))).await.unwrap();
    assert!(matches!(wire::decode(&cp.recv().await.unwrap().payload, &keys).unwrap().1, Msg::SamplerCheckpointInstalled { .. }));
    activate(&mut cp, &keys, ActivationKind::Recovery, 1, 1).await.unwrap();
    *pl.s1_down.lock().unwrap() = (rs2_addr, "s2".to_string());
    let detect_to_resumed = t_detect.elapsed();

    // Resume from q=m, RE-APPLYING the provisional position that was truncated (S_P/S1 are back at the
    // durable frontier, replacement S2 rebuilt to it, so re-applying `provisional_pos` is consistent).
    input_pos = provisional_pos;
    let mut resumed = Vec::new();
    for q in (m as i64)..n as i64 {
        let (tok, snap) = sample(&mut cp, &keys, q, &cfg_hash).await.unwrap();
        resumed.push(tok);
        group.push(q, tok, snap);
        if group.count_ready() {
            let b = group.take().unwrap();
            cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).unwrap();
        }
        if (q as usize + 1) < n {
            chain_apply(&mut c1, &keys, input_pos, tok, false).await.unwrap();
            input_pos += 1;
        }
    }
    if let Some(b) = group.take() {
        cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).unwrap();
    }
    drop(cs);

    // The discarded provisional was re-sampled identically from the restored checkpoint (I15).
    assert_eq!(resumed[0], prov_tok, "the discarded provisional sampled-ahead token is re-sampled identically after truncation (I15)");
    let mut full = committed.clone();
    full.extend(resumed);
    assert_eq!(full, reference, "sampled-ahead middle-kill: survivor truncates the provisional tail (I7a/I7b) → byte-identical to uninterrupted greedy");
    let stats = hydra_coordinator::recovery::verify(&cs_path).unwrap();
    assert_eq!(stats.committed_positions, n);
    assert!(stats.positions_strictly_increasing);
    assert_eq!(stats.max_position, n as i64 - 1);

    eprintln!("seam C §7.19(b) (sampled-ahead middle-kill, survivor truncates): detection→resumed {detect_to_resumed:?} (HONESTY: in-process local)");
    let _ = std::fs::remove_dir_all(&pl.dir);
}
