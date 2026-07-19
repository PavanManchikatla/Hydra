//! M2 FWD slice, seam 3 — **the two-stage D1 / Strategy-A recovery demo** (same three-assertion bar
//! as the C-part-2 flagship, now across a TWO-worker split: S1 `[0,k)` → S_P `[k,-1)` + sampler).
//!
//! The coordinator holds the durable **`BoundaryStore`** (the D1 substrate seam 2 built) and the
//! commit stream, and captures each boundary as it relays it S1→S_P (so it is durably copied). The
//! kill exercised here is **S_P**: the **survivor S1 takes `BEGIN_RECOVERY` Case A** (freeze — the
//! survivor path that until now only ran in the sim), and the **replacement S_P rebuilds its KV from
//! the durable BOUNDARIES** (`BoundaryStore` replay) — **NOT** from full-token replay; that is the D1
//! difference vs C-part-2's D0 token-replay, and it is what boundary durability exists for. Then the
//! sampler installs, activation commits, and generation resumes — byte-identical to an uninterrupted
//! greedy run.
//!
//! Assertions (C-part-2 bar): (a) SSE id continuity (commit stream covers every output position once,
//! dense); (b) byte-identical (committed prefix ⊕ resumed suffix == uninterrupted run); (c) disk
//! truth (commit stream I19 + no position twice). Timing honesty-annotated: **in-process local, NOT
//! the <15 s LAN/M3 D1 target**; the real `kill -9` of a two-node pipeline is seam 4 (containerized CI).
//!
//! **Transport note (honest):** the demo relays FWD through the coordinator (so each worker serves a
//! single connection and the coordinator captures boundaries for durability). **Worker→worker DIRECT
//! FWD is proven bit-exact in seam 1** (`local_pair::direct_worker_to_worker_fwd_is_bit_exact`); the
//! recovery machinery under test here is orthogonal to the FWD transport.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use hydra_coordinator::{BoundaryStore, CommitStream, GroupCommitter, WalFenceCtx};
use hydra_state::{ActivationKind, ActivationTuple};
use hydra_transport::framed::Conn;
use hydra_tokenizer::Admission;
use hydra_worker::pair::{dev_model_path, Cluster};
use hydra_worker::sampler::{initial_checkpoint_bytes, SamplingConfig};
use hydra_worker::wire::{self, Msg, SessionKeys};
use hydra_worker::worker::{WorkerConfig, INITIAL_CHECKPOINT_ID};
use tokio::io::{AsyncRead, AsyncWrite};

const CLUSTER_ID: [u8; 16] = [0xD1; 16];
const SESSION_ID: [u8; 16] = [0x25; 16];
static SEQ: AtomicU32 = AtomicU32::new(0);

fn greedy() -> SamplingConfig {
    SamplingConfig { temperature: 0.0, top_p: 1.0, repeat_penalty: 1.0, penalty_last_n: 0, seed: 3 }
}
fn fence() -> WalFenceCtx {
    WalFenceCtx { cluster_id: CLUSTER_ID, session_id: SESSION_ID, model_instance_id: [3; 16], manifest_hash: [4; 32], epoch: 0, recovery_id: 0, activation_attempt_id: 0 }
}
fn admission(prompt: &[u32]) -> Admission {
    Admission { tokenizer_hash: [1; 32], chat_template_hash: [2; 32], rendered_prompt_bytes_hash: [3; 32], rendered_prompt: String::new(), prompt_tokens: prompt.to_vec() }
}
fn temp_dir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("hydra-d1two-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn sp_cfg(model: &str, keys: &SessionKeys, k: i32, n_ctx: i32, recovery_start: bool) -> WorkerConfig {
    WorkerConfig { keys: keys.clone(), rank: 1, layer_first: k, layer_last: -1, is_final: true, receives_tokens: false, epoch: 0, recovery_id: if recovery_start { 1 } else { 0 }, model_path: Some(model.to_string()), n_gpu_layers: 0, n_ctx, sampler_config: Some(greedy()), recovery_start }
}
fn s1_cfg(model: &str, keys: &SessionKeys, k: i32, n_ctx: i32) -> WorkerConfig {
    WorkerConfig { keys: keys.clone(), rank: 0, layer_first: 0, layer_last: k, is_final: false, receives_tokens: true, epoch: 0, recovery_id: 0, model_path: Some(model.to_string()), n_gpu_layers: 0, n_ctx, sampler_config: None, recovery_start: false }
}
fn spawn(cluster: &Cluster, name: &str, cfg: WorkerConfig) -> std::net::SocketAddr {
    let id = cluster.issue(name).unwrap();
    hydra_worker::pair::spawn_endpoint(cfg, cluster.ca.server_config(&id).unwrap())
}

// --- coordinator-side drivers ---

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

/// C→S1 APPLY_TOKEN → S1 emits the boundary as `FWD` → C captures it (durably copies if `store`) and
/// relays it C→S_P → `APPLIED_ACK`. The boundary is thus made durable exactly when it is applied.
async fn apply_relay<A, B>(c1: &mut Conn<A>, cp: &mut Conn<B>, keys: &SessionKeys, input_pos: i64, token: u32, no_sample: bool, store: Option<&mut BoundaryStore>) -> Result<(), String>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    c1.send(0, &wire::encode_apply_token(keys, 0, input_pos, token, no_sample)).await.map_err(|e| e.to_string())?;
    let boundary = match wire::decode(&c1.recv().await.map_err(|e| format!("recv s1 fwd: {e}"))?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::Fwd { activations, .. } => activations,
        o => return Err(format!("S1 @ {input_pos}: expected FWD, got {o:?}")),
    };
    if let Some(store) = store {
        store.append_boundary(0, input_pos, 0, &boundary).map_err(|e| e.to_string())?;
    }
    cp.send(0, &wire::encode_fwd(keys, 0, input_pos, no_sample, &boundary)).await.map_err(|e| e.to_string())?;
    match wire::decode(&cp.recv().await.map_err(|e| format!("recv sp ack: {e}"))?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::AppliedAck { cumulative_input_pos, .. } if cumulative_input_pos == input_pos => Ok(()),
        o => Err(format!("S_P @ {input_pos}: expected APPLIED_ACK, got {o:?}")),
    }
}

/// FWD a durable boundary directly to a (replacement) S_P — rebuilds its KV from boundaries.
async fn rebuild_apply<S: AsyncRead + AsyncWrite + Unpin>(cp: &mut Conn<S>, keys: &SessionKeys, input_pos: i64, boundary: &[f32]) -> Result<(), String> {
    cp.send(0, &wire::encode_fwd(keys, 0, input_pos, true, boundary)).await.map_err(|e| e.to_string())?;
    match wire::decode(&cp.recv().await.map_err(|e| e.to_string())?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::AppliedAck { cumulative_input_pos, .. } if cumulative_input_pos == input_pos => Ok(()),
        o => Err(format!("rebuild @ {input_pos}: expected APPLIED_ACK, got {o:?}")),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn d1_two_stage_kill_s_p_rebuilds_from_durable_boundaries_byte_identical() {
    let Some(model) = dev_model_path() else {
        eprintln!("SKIP: no engine/model (dev-environment artifacts)");
        return;
    };
    let (prompt, k) = {
        let m = hydra_engine_sys::Model::load(&model, 0).expect("load");
        let prompt: Vec<u32> = m.tokenize("The capital of France is").expect("tok").into_iter().map(|t| t as u32).collect();
        (prompt, (m.n_layer() / 2).max(1))
    };
    let n = 8usize;
    let n_ctx = prompt.len() as i32 + n as i32 + 8;
    let keys = SessionKeys::dev(0xD1);
    let cfg_hash = greedy().hash();
    let cluster = Cluster::new().unwrap();
    let connector = cluster.coordinator_connector().unwrap();

    // ---- uninterrupted greedy reference through the two-worker pipeline ----
    let reference = {
        let s1 = spawn(&cluster, "s1", s1_cfg(&model, &keys, k, n_ctx));
        let sp = spawn(&cluster, "sp", sp_cfg(&model, &keys, k, n_ctx, false));
        let mut c1 = connector.connect(s1, "s1").await.unwrap();
        let mut cp = connector.connect(sp, "sp").await.unwrap();
        for (i, &t) in prompt.iter().enumerate() {
            apply_relay(&mut c1, &mut cp, &keys, i as i64, t, true, None).await.unwrap();
        }
        let mut out = Vec::new();
        let mut input_pos = prompt.len() as i64;
        for q in 0..n as i64 {
            let (tok, _) = sample(&mut cp, &keys, q, &cfg_hash).await.unwrap();
            out.push(tok);
            if (q as usize + 1) < n {
                apply_relay(&mut c1, &mut cp, &keys, input_pos, tok, false, None).await.unwrap();
                input_pos += 1;
            }
        }
        out
    };
    assert_eq!(reference.len(), n);

    // ---- kill-run: generate m, kill S_P, recover, resume ----
    let dir = temp_dir();
    let cs_path = dir.join("commit.wal");
    let bs_path = dir.join("boundaries.wal");
    let mut cs = CommitStream::create(&cs_path, CLUSTER_ID, SESSION_ID).unwrap();
    cs.append_initial_commit(&fence(), &admission(&prompt), &initial_checkpoint_bytes(INITIAL_CHECKPOINT_ID, &greedy()), 1).unwrap();
    let mut store = BoundaryStore::create(&bs_path, CLUSTER_ID, SESSION_ID).unwrap();
    let mut group = GroupCommitter::new(2);

    let s1 = spawn(&cluster, "s1", s1_cfg(&model, &keys, k, n_ctx));
    let sp = spawn(&cluster, "sp", sp_cfg(&model, &keys, k, n_ctx, false));
    let mut c1 = connector.connect(s1, "s1").await.unwrap();
    let mut cp = connector.connect(sp, "sp").await.unwrap();
    activate(&mut c1, &keys, ActivationKind::Initial, 0, 0).await.unwrap();
    activate(&mut cp, &keys, ActivationKind::Initial, 0, 0).await.unwrap();

    for (i, &t) in prompt.iter().enumerate() {
        apply_relay(&mut c1, &mut cp, &keys, i as i64, t, true, Some(&mut store)).await.unwrap();
    }
    let m = n / 2;
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
        apply_relay(&mut c1, &mut cp, &keys, input_pos, tok, false, Some(&mut store)).await.unwrap();
        input_pos += 1;
    }
    if let Some(b) = group.take() {
        cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).unwrap();
    }
    assert_eq!(committed, reference[..m].to_vec(), "pre-kill greedy matches the reference");

    // ---- kill S_P (drop the connection; the replacement is a fresh endpoint with an empty KV,
    // as a SIGKILL would abandon S_P's state) ----
    let t_detect = Instant::now();
    drop(cp);

    // Survivor S1 takes BEGIN_RECOVERY Case A (freeze) — the survivor path, for real.
    c1.send(0, &wire::encode_begin_recovery(&keys, 0, 1, 1, input_pos)).await.unwrap();
    match wire::decode(&c1.recv().await.unwrap().payload, &keys).unwrap().1 {
        Msg::RecoveryAck { .. } => {}
        o => panic!("survivor S1 must ack RECOVERY_ACK (Case A freeze), got {o:?}"),
    }

    // Replacement S_P: rebuild KV from the DURABLE BOUNDARIES (not tokens — the D1 difference).
    let boundaries = BoundaryStore::read(&bs_path).unwrap();
    assert_eq!(boundaries.len(), prompt.len() + m, "one durable boundary per applied input position");
    let rsp = spawn(&cluster, "sp", sp_cfg(&model, &keys, k, n_ctx, true));
    let mut rcp = connector.connect(rsp, "sp").await.unwrap();
    rcp.send(0, &wire::encode_begin_recovery(&keys, 0, 1, 1, 0)).await.unwrap();
    assert!(matches!(wire::decode(&rcp.recv().await.unwrap().payload, &keys).unwrap().1, Msg::RecoveryAck { .. }));
    for b in &boundaries {
        rebuild_apply(&mut rcp, &keys, b.first_input_pos, &b.activations).await.unwrap();
    }
    rcp.send(0, &wire::encode_catch_up_context(&keys, 0, 1, input_pos)).await.unwrap();
    assert!(matches!(wire::decode(&rcp.recv().await.unwrap().payload, &keys).unwrap().1, Msg::CatchUpReady { .. }));
    rcp.send(0, &wire::encode_install_sampler_checkpoint(&keys, 0, INITIAL_CHECKPOINT_ID, &initial_checkpoint_bytes(INITIAL_CHECKPOINT_ID, &greedy()))).await.unwrap();
    assert!(matches!(wire::decode(&rcp.recv().await.unwrap().payload, &keys).unwrap().1, Msg::SamplerCheckpointInstalled { .. }));
    activate(&mut rcp, &keys, ActivationKind::Recovery, 1, 1).await.unwrap();
    let detect_to_resumed = t_detect.elapsed();

    // Resume greedy generation from output m through the replacement S_P (survivor S1 relays as before).
    let mut resumed = Vec::new();
    for q in (m as i64)..n as i64 {
        let (tok, snap) = sample(&mut rcp, &keys, q, &cfg_hash).await.unwrap();
        resumed.push(tok);
        group.push(q, tok, snap);
        if group.count_ready() {
            let b = group.take().unwrap();
            cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).unwrap();
        }
        if (q as usize + 1) < n {
            apply_relay(&mut c1, &mut rcp, &keys, input_pos, tok, false, Some(&mut store)).await.unwrap();
            input_pos += 1;
        }
    }
    if let Some(b) = group.take() {
        cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).unwrap();
    }
    drop(cs);

    // (b) byte-identical: committed prefix ⊕ resumed suffix == uninterrupted run.
    let mut full = committed.clone();
    full.extend(resumed);
    assert_eq!(full, reference, "recovered two-stage stream (S_P rebuilt from durable boundaries) == uninterrupted greedy run");

    // (c) disk truth + (a) SSE id continuity: I19-valid, no output position twice, dense.
    let stats = hydra_coordinator::recovery::verify(&cs_path).unwrap();
    assert_eq!(stats.committed_positions, n);
    assert!(stats.positions_strictly_increasing);
    assert_eq!(stats.max_position, n as i64 - 1);

    eprintln!("d1 two-stage (kill S_P): detection→resumed {detect_to_resumed:?} (HONESTY: in-process local, NOT the <15s LAN/M3 D1 target; real kill -9 of a 2-node pipeline = seam 4 docker kill)");
    let _ = std::fs::remove_dir_all(&dir);
}

/// S1-local rebuild from raw tokens: APPLY_TOKEN each, dropping the produced boundary (the survivor
/// S_P already holds those positions — only S1's own KV needs rebuilding).
async fn rebuild_s1_from_tokens<S: AsyncRead + AsyncWrite + Unpin>(c1: &mut Conn<S>, keys: &SessionKeys, tokens: &[u32]) -> Result<(), String> {
    for (i, &t) in tokens.iter().enumerate() {
        c1.send(0, &wire::encode_apply_token(keys, 0, i as i64, t, true)).await.map_err(|e| e.to_string())?;
        match wire::decode(&c1.recv().await.map_err(|e| e.to_string())?.payload, keys).map_err(|e| e.to_string())?.1 {
            Msg::Fwd { .. } => {} // drop — the survivor S_P is not re-applied
            o => return Err(format!("rebuild S1 @ {i}: expected FWD, got {o:?}")),
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn d1_two_stage_kill_s1_survivor_sp_frozen_replacement_s1_from_tokens_byte_identical() {
    let Some(model) = dev_model_path() else {
        eprintln!("SKIP: no engine/model (dev-environment artifacts)");
        return;
    };
    let (prompt, k) = {
        let m = hydra_engine_sys::Model::load(&model, 0).expect("load");
        let prompt: Vec<u32> = m.tokenize("The capital of France is").expect("tok").into_iter().map(|t| t as u32).collect();
        (prompt, (m.n_layer() / 2).max(1))
    };
    let n = 8usize;
    let n_ctx = prompt.len() as i32 + n as i32 + 8;
    let keys = SessionKeys::dev(0x15);
    let cfg_hash = greedy().hash();
    let cluster = Cluster::new().unwrap();
    let connector = cluster.coordinator_connector().unwrap();

    // uninterrupted reference
    let reference = {
        let s1 = spawn(&cluster, "s1", s1_cfg(&model, &keys, k, n_ctx));
        let sp = spawn(&cluster, "sp", sp_cfg(&model, &keys, k, n_ctx, false));
        let mut c1 = connector.connect(s1, "s1").await.unwrap();
        let mut cp = connector.connect(sp, "sp").await.unwrap();
        for (i, &t) in prompt.iter().enumerate() {
            apply_relay(&mut c1, &mut cp, &keys, i as i64, t, true, None).await.unwrap();
        }
        let mut out = Vec::new();
        let mut input_pos = prompt.len() as i64;
        for q in 0..n as i64 {
            let (tok, _) = sample(&mut cp, &keys, q, &cfg_hash).await.unwrap();
            out.push(tok);
            if (q as usize + 1) < n {
                apply_relay(&mut c1, &mut cp, &keys, input_pos, tok, false, None).await.unwrap();
                input_pos += 1;
            }
        }
        out
    };

    let dir = temp_dir();
    let cs_path = dir.join("commit.wal");
    let mut cs = CommitStream::create(&cs_path, CLUSTER_ID, SESSION_ID).unwrap();
    cs.append_initial_commit(&fence(), &admission(&prompt), &initial_checkpoint_bytes(INITIAL_CHECKPOINT_ID, &greedy()), 1).unwrap();
    let mut group = GroupCommitter::new(2);

    let s1 = spawn(&cluster, "s1", s1_cfg(&model, &keys, k, n_ctx));
    let sp = spawn(&cluster, "sp", sp_cfg(&model, &keys, k, n_ctx, false));
    let mut c1 = connector.connect(s1, "s1").await.unwrap();
    let mut cp = connector.connect(sp, "sp").await.unwrap();
    activate(&mut c1, &keys, ActivationKind::Initial, 0, 0).await.unwrap();
    activate(&mut cp, &keys, ActivationKind::Initial, 0, 0).await.unwrap();

    for (i, &t) in prompt.iter().enumerate() {
        apply_relay(&mut c1, &mut cp, &keys, i as i64, t, true, None).await.unwrap();
    }
    let m = n / 2;
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
        apply_relay(&mut c1, &mut cp, &keys, input_pos, tok, false, None).await.unwrap();
        input_pos += 1;
    }
    if let Some(b) = group.take() {
        cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).unwrap();
    }
    assert_eq!(committed, reference[..m].to_vec());

    // ---- kill S1 (drop its connection; the replacement is a fresh S1 with empty KV) ----
    let t_detect = Instant::now();
    drop(c1);

    // Survivor S_P takes BEGIN_RECOVERY Case A (freeze) — the S_P survivor path.
    cp.send(0, &wire::encode_begin_recovery(&keys, 0, 1, 1, input_pos)).await.unwrap();
    match wire::decode(&cp.recv().await.unwrap().payload, &keys).unwrap().1 {
        Msg::RecoveryAck { .. } => {}
        o => panic!("survivor S_P must ack RECOVERY_ACK (Case A freeze), got {o:?}"),
    }

    // Replacement S1: rebuild its KV from the committed TOKENS (the S1-side rebuild — S1 ingests
    // tokens, so token replay is correct here; the boundaries it produces are dropped, since the
    // survivor S_P already holds those positions).
    let rs1 = spawn(&cluster, "s1r", { let mut c = s1_cfg(&model, &keys, k, n_ctx); c.recovery_id = 1; c.recovery_start = true; c });
    let mut rc1 = connector.connect(rs1, "s1r").await.unwrap();
    rc1.send(0, &wire::encode_begin_recovery(&keys, 0, 1, 1, 0)).await.unwrap();
    assert!(matches!(wire::decode(&rc1.recv().await.unwrap().payload, &keys).unwrap().1, Msg::RecoveryAck { .. }));
    let replay: Vec<u32> = prompt.iter().copied().chain(committed.iter().copied()).collect();
    rebuild_s1_from_tokens(&mut rc1, &keys, &replay).await.unwrap();
    rc1.send(0, &wire::encode_catch_up_context(&keys, 1, 1, input_pos)).await.unwrap();
    assert!(matches!(wire::decode(&rc1.recv().await.unwrap().payload, &keys).unwrap().1, Msg::CatchUpReady { .. }));
    activate(&mut rc1, &keys, ActivationKind::Recovery, 1, 1).await.unwrap();
    let detect_to_resumed = t_detect.elapsed();

    // Resume: sample from the SURVIVOR S_P; feed back through the replacement S1 (boundary replay downstream).
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
            apply_relay(&mut rc1, &mut cp, &keys, input_pos, tok, false, None).await.unwrap();
            input_pos += 1;
        }
    }
    if let Some(b) = group.take() {
        cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).unwrap();
    }
    drop(cs);

    let mut full = committed.clone();
    full.extend(resumed);
    assert_eq!(full, reference, "recovered two-stage stream (S1 rebuilt from tokens, S_P survivor) == uninterrupted greedy run");
    let stats = hydra_coordinator::recovery::verify(&cs_path).unwrap();
    assert_eq!(stats.committed_positions, n);
    assert!(stats.positions_strictly_increasing);
    eprintln!("d1 two-stage (kill S1): detection→resumed {detect_to_resumed:?} (HONESTY: in-process local, NOT the <15s LAN/M3 D1 target)");
    let _ = std::fs::remove_dir_all(&dir);
}
