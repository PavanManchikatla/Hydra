//! `hydra-3node-kill` — P1·1b: the **real-node 3-node kill-window** (owner-owed VM-window run).
//!
//! The seam-C S_P kill on **real hardware** over Tailscale (the clean, §7.19-independent path), which
//! re-confirms **gate-cond-(i)** on real nodes. Topology (cap-weighted 4.0/2.1/1.0): coordinator +
//! **S1 on the Mac (arm64)** `[0,14)` → **S2 on myVm-2 (x86)** `[14,21)` → **S_P on myVm-1 (x86)**
//! `[21,24)` + sampler; direct FWD, S1 and S2 durably copy to `BoundaryStore`s the coordinator hosts
//! on the Mac.
//!
//! Kill sequence: generate `m` tokens; **real `kill -9`** of myVm-1's S_P (`pkill -9`); survivors S1
//! (local) + S2 (myVm-2) freeze via `BEGIN_RECOVERY` Case A; a **replacement S_P is started on
//! myVm-1 at the SAME address**, rebuilt from **S2's durable boundaries** (D1, not token replay),
//! sampler installed, activated; **S2 re-links** to the replacement on its next forward (its dead
//! socket to the killed S_P fails → `forward_with_relink` reconnects to the same address → the
//! replacement); generation resumes.
//!
//! Three assertions (real-node, cross-arch → mixed tier, spec I8 — argmax agreement not bit-exact):
//! (a) SSE id continuity (commit stream dense, every output position once); (b) committed prefix ⊕
//! resumed suffix == the Mac unsplit greedy reference (the oracle, 12/12 in the pipeline-only run);
//! (c) disk truth — commit stream I19-valid, no output position twice. Timing WAN-annotated.

use std::net::SocketAddr;
use std::process::Command;
use std::sync::mpsc;
use std::time::Instant;

use hydra_coordinator::{recovery, BoundaryStore, CommitStream, GroupCommitter, WalFenceCtx};
use hydra_engine_sys::Model;
use hydra_state::{ActivationKind, ActivationTuple};
use hydra_tokenizer::Admission;
use hydra_transport::framed::Conn;
use hydra_transport::tcp_mtls::{TcpMtls, TcpMtlsListener};
use hydra_worker::bootstrap::{Bootstrap, ForwardingBootstrap};
use hydra_worker::pair::{spawn_multiconn_forwarding_durable_endpoint, Cluster};
use hydra_worker::sampler::{initial_checkpoint_bytes, SamplingConfig};
use hydra_worker::wire::{self, Msg, SessionKeys};
use hydra_worker::worker::{DownTarget, WorkerConfig, INITIAL_CHECKPOINT_ID};
use tokio::io::{AsyncRead, AsyncWrite};

const MAC_TS_IP: &str = "100.93.110.78";
const VM1_TS_IP: &str = "100.115.200.62"; // myVm-1 hosts S_P
const VM2_TS_IP: &str = "100.73.205.31"; // myVm-2 hosts S2
const SP_PORT: u16 = 41997;
const S2_PORT: u16 = 41996;
const DUR1_PORT: u16 = 42011;
const DUR2_PORT: u16 = 42012;
const VM_WORKER: &str = "/home/azureuser/hydra/target/debug/hydra-worker";
const VM_MODEL: &str = "/home/azureuser/hydra/models/qwen2.5-0.5b-instruct-fp16.gguf";
const VM_LIBDIR: &str = "/home/azureuser/hydra/vendor/llama.cpp/build/bin";
const CLUSTER_ID: [u8; 16] = [0x3C; 16];
const SESSION_ID: [u8; 16] = [0x53; 16];

fn vm1_ssh() -> String { std::env::var("HYDRA_VM1_SSH").unwrap_or_else(|_| "hydra-vm".into()) }
fn vm2_ssh() -> String { std::env::var("HYDRA_VM2_SSH").unwrap_or_else(|_| "hydra-vm2".into()) }
fn mac_model_path() -> String {
    std::env::var("HYDRA_TEST_MODEL").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../models/qwen2.5-0.5b-instruct-fp16.gguf").to_string())
}
fn greedy() -> SamplingConfig { SamplingConfig { temperature: 0.0, top_p: 1.0, repeat_penalty: 1.0, penalty_last_n: 0, seed: 5 } }
fn fence() -> WalFenceCtx { WalFenceCtx { cluster_id: CLUSTER_ID, session_id: SESSION_ID, model_instance_id: [3; 16], manifest_hash: [4; 32], epoch: 0, recovery_id: 0, activation_attempt_id: 0 } }
fn admission(p: &[u32]) -> Admission { Admission { tokenizer_hash: [1; 32], chat_template_hash: [2; 32], rendered_prompt_bytes_hash: [3; 32], rendered_prompt: String::new(), prompt_tokens: p.to_vec() } }

fn split3(n_layer: i32) -> (i32, i32) {
    let l = n_layer as f64;
    let k1 = ((0.563 * l).round() as i32).clamp(1, n_layer - 2);
    let k2 = (k1 + (0.296 * l).round() as i32).clamp(k1 + 1, n_layer - 1);
    (k1, k2)
}
fn mac_unsplit_greedy(model: &Model, prompt: &[u32], n: usize) -> Vec<u32> {
    let n_ctx = prompt.len() as i32 + n as i32 + 8;
    let mut ctx = model.context(0, -1, false, n_ctx, n_ctx).expect("ctx");
    for (pos, &t) in prompt.iter().enumerate() { ctx.apply_tokens(&[t as i32], pos as i32, None).expect("prefill"); }
    let argmax = |l: &[f32]| (0..l.len()).max_by(|&a, &b| l[a].total_cmp(&l[b])).unwrap() as u32;
    let mut out = Vec::with_capacity(n);
    let mut pos = prompt.len() as i32;
    for step in 0..n {
        out.push(argmax(&ctx.logits(0).expect("logits")));
        if step + 1 < n { ctx.apply_tokens(&[*out.last().unwrap() as i32], pos, None).expect("feedback"); pos += 1; }
    }
    out
}

fn sh(args: &[&str]) -> Result<String, String> {
    let out = Command::new(args[0]).args(&args[1..]).output().map_err(|e| format!("{}: {e}", args[0]))?;
    if !out.status.success() { return Err(format!("{:?} failed: {}", args, String::from_utf8_lossy(&out.stderr))); }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
fn kill_remote(ssh: &str) { let _ = sh(&["ssh", ssh, "pkill -9 -f hydra-worker; exit 0"]); }
fn start_remote(ssh: &str, local_boot: &str, remote_boot: &str, log: &str) -> Result<(), String> {
    sh(&["scp", "-q", local_boot, &format!("{ssh}:{remote_boot}")])?;
    sh(&["ssh", ssh, &format!("rm -f {log}; setsid env LD_LIBRARY_PATH={VM_LIBDIR} {VM_WORKER} {remote_boot} </dev/null >{log} 2>&1 & echo started; exit 0")])?;
    for _ in 0..160 {
        let out = sh(&["ssh", ssh, &format!("cat {log} 2>/dev/null || true")]).unwrap_or_default();
        if out.contains("engine=true") { return Ok(()); }
        if out.contains("engine=false") { return Err(format!("{ssh}: engine=false")); }
        if out.contains("panic") || out.contains("serve loop ended with error") { return Err(format!("{ssh}: worker failed:\n{out}")); }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(format!("{ssh}: no engine= within 80s"))
}

fn spawn_mac_durability(cluster: &Cluster, name: &str, port: u16, path: std::path::PathBuf, keys: SessionKeys) -> Result<SocketAddr, String> {
    let id = cluster.issue(name).map_err(|e| e.to_string())?;
    let server_cfg = cluster.ca.server_config(&id).map_err(|e| e.to_string())?;
    let bind: SocketAddr = format!("{MAC_TS_IP}:{port}").parse().map_err(|e| format!("{e}"))?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = match TcpMtlsListener::bind_with_config(bind, server_cfg).await { Ok(l) => l, Err(e) => { let _ = tx.send(Err(format!("bind {bind}: {e}"))); return; } };
            let _ = tx.send(Ok(listener.local_addr().unwrap()));
            let mut store = BoundaryStore::create(&path, CLUSTER_ID, SESSION_ID).expect("store");
            let Ok(mut conn) = listener.accept().await else { return };
            while let Ok(frame) = conn.recv().await {
                if let Ok((view, Msg::BoundaryCopy { boundary_id, first_input_pos, chunk_id, activations })) = wire::decode(&frame.payload, &keys) {
                    let d = store.append_boundary(boundary_id, first_input_pos, chunk_id, &activations).unwrap_or(-1);
                    if conn.send(0, &wire::encode_durability_ack(&keys, view.epoch, boundary_id, d, 0)).await.is_err() { break; }
                }
            }
        });
    });
    rx.recv().map_err(|e| e.to_string())?
}

fn sp_bootstrap(cluster: &Cluster, keys: &SessionKeys, k2: i32, n_ctx: i32, recovery_start: bool) -> Bootstrap {
    let id = cluster.issue("sp").unwrap();
    Bootstrap {
        listen_addr: format!("{VM1_TS_IP}:{SP_PORT}"), device_name: "sp".into(),
        ca_cert_der: cluster.ca.ca_cert_der().as_ref().to_vec(),
        cert_chain_der: id.cert_chain.iter().map(|c| c.as_ref().to_vec()).collect(), key_pkcs8_der: id.key_pkcs8_der(),
        cfg: WorkerConfig { keys: keys.clone(), rank: 2, layer_first: k2, layer_last: -1, is_final: true, receives_tokens: false, epoch: 0, recovery_id: if recovery_start { 1 } else { 0 }, model_path: Some(VM_MODEL.into()), n_gpu_layers: 0, n_ctx, sampler_config: Some(greedy()), recovery_start },
        forwarding: None,
    }
}
fn s2_bootstrap(cluster: &Cluster, keys: &SessionKeys, k1: i32, k2: i32, n_ctx: i32, dur2: SocketAddr) -> Bootstrap {
    let id = cluster.issue("s2").unwrap();
    Bootstrap {
        listen_addr: format!("{VM2_TS_IP}:{S2_PORT}"), device_name: "s2".into(),
        ca_cert_der: cluster.ca.ca_cert_der().as_ref().to_vec(),
        cert_chain_der: id.cert_chain.iter().map(|c| c.as_ref().to_vec()).collect(), key_pkcs8_der: id.key_pkcs8_der(),
        cfg: WorkerConfig { keys: keys.clone(), rank: 1, layer_first: k1, layer_last: k2, is_final: false, receives_tokens: false, epoch: 0, recovery_id: 0, model_path: Some(VM_MODEL.into()), n_gpu_layers: 0, n_ctx, sampler_config: None, recovery_start: false },
        forwarding: Some(ForwardingBootstrap { down_addr: format!("{VM1_TS_IP}:{SP_PORT}"), down_name: "sp".into(), dur_addr: format!("{MAC_TS_IP}:{}", dur2.port()), dur_name: "dur2".into(), require_durable: true, capacity: 8 }),
    }
}

// coordinator drivers (generic over the connection)
async fn activate<S: AsyncRead + AsyncWrite + Unpin>(c: &mut Conn<S>, keys: &SessionKeys, kind: ActivationKind, epoch: u32, rid: u32) -> Result<(), String> {
    let t = ActivationTuple { kind, epoch, recovery_id: rid, attempt: 0, sampler_checkpoint_id: if matches!(kind, ActivationKind::Recovery) { INITIAL_CHECKPOINT_ID } else { 0 } };
    c.send(0, &wire::encode_commit_activation(keys, &t, 1)).await.map_err(|e| e.to_string())?;
    match wire::decode(&c.recv().await.map_err(|e| e.to_string())?.payload, keys).map_err(|e| e.to_string())?.1 { Msg::ActivationCommitted(_) => {} o => return Err(format!("expected COMMITTED, got {o:?}")) }
    c.send(0, &wire::encode_finalize_activation(keys, &t, 1)).await.map_err(|e| e.to_string())?;
    match wire::decode(&c.recv().await.map_err(|e| e.to_string())?.payload, keys).map_err(|e| e.to_string())?.1 { Msg::ActivationFinalized => Ok(()), o => Err(format!("expected FINALIZED, got {o:?}")) }
}
async fn chain_apply<S: AsyncRead + AsyncWrite + Unpin>(c1: &mut Conn<S>, keys: &SessionKeys, input_pos: i64, token: u32, no_sample: bool) -> Result<(), String> {
    c1.send(0, &wire::encode_apply_token(keys, 0, input_pos, token, no_sample)).await.map_err(|e| e.to_string())?;
    match wire::decode(&c1.recv().await.map_err(|e| format!("recv chain ack: {e}"))?.payload, keys).map_err(|e| e.to_string())?.1 { Msg::AppliedAck { cumulative_input_pos, .. } if cumulative_input_pos == input_pos => Ok(()), o => Err(format!("chain @ {input_pos}: expected APPLIED_ACK, got {o:?}")) }
}
async fn sample<S: AsyncRead + AsyncWrite + Unpin>(c: &mut Conn<S>, keys: &SessionKeys, output_pos: i64, h: &[u8; 32]) -> Result<(u32, Vec<u8>), String> {
    c.send(0, &wire::encode_sample_next(keys, 0, output_pos, h, INITIAL_CHECKPOINT_ID)).await.map_err(|e| e.to_string())?;
    match wire::decode(&c.recv().await.map_err(|e| e.to_string())?.payload, keys).map_err(|e| e.to_string())?.1 { Msg::Sampled { token_id, post_sample_snapshot, .. } => Ok((token_id, post_sample_snapshot)), Msg::Err { code } => Err(format!("SAMPLE_NEXT @ {output_pos} err {code}")), o => Err(format!("SAMPLE_NEXT @ {output_pos}: expected SAMPLED, got {o:?}")) }
}
async fn rebuild_apply<S: AsyncRead + AsyncWrite + Unpin>(c: &mut Conn<S>, keys: &SessionKeys, input_pos: i64, boundary: &[f32]) -> Result<(), String> {
    c.send(0, &wire::encode_fwd(keys, 0, input_pos, true, boundary)).await.map_err(|e| e.to_string())?;
    match wire::decode(&c.recv().await.map_err(|e| e.to_string())?.payload, keys).map_err(|e| e.to_string())?.1 { Msg::AppliedAck { cumulative_input_pos, .. } if cumulative_input_pos == input_pos => Ok(()), o => Err(format!("rebuild @ {input_pos}: expected APPLIED_ACK, got {o:?}")) }
}
async fn begin_recovery<S: AsyncRead + AsyncWrite + Unpin>(c: &mut Conn<S>, keys: &SessionKeys, truncate_to: i64) -> Result<(), String> {
    c.send(0, &wire::encode_begin_recovery(keys, 0, 1, 1, truncate_to)).await.map_err(|e| e.to_string())?;
    match wire::decode(&c.recv().await.map_err(|e| e.to_string())?.payload, keys).map_err(|e| e.to_string())?.1 { Msg::RecoveryAck { .. } => Ok(()), o => Err(format!("expected RECOVERY_ACK, got {o:?}")) }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 3)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mac_model = mac_model_path();
    if !std::path::Path::new(&mac_model).exists() { eprintln!("SKIP: Mac model not found at {mac_model}"); return Ok(()); }
    let (prompt, reference, k1, k2, n_layer, n_ctx, n, m) = {
        let model = Model::load(&mac_model, 0)?;
        let prompt: Vec<u32> = model.tokenize("The capital of France is").expect("tok").into_iter().map(|t| t as u32).collect();
        let n = 8usize;
        let reference = mac_unsplit_greedy(&model, &prompt, n);
        let (k1, k2) = split3(model.n_layer());
        let n_ctx = prompt.len() as i32 + n as i32 + 8;
        (prompt, reference, k1, k2, model.n_layer(), n_ctx, n, 4usize)
    };
    println!("== hydra-3node-kill: real S_P kill-window, Mac S1 [0,{k1}) → myVm-2 S2 [{k1},{k2}) → myVm-1 S_P [{k2},{n_layer}) ==");
    println!("   cap-weighted 4.0/2.1/1.0; WAN/Tailscale; cross-arch → mixed tier (argmax agreement, spec I8); n={n}, kill after m={m}");

    let keys = SessionKeys::dev(0x5C);
    let cfg_hash = greedy().hash();
    let cluster = Cluster::new()?;
    let (vm1, vm2) = (vm1_ssh(), vm2_ssh());
    let dir = std::env::temp_dir();
    let d1 = dir.join("hydra-3nk-s1.wal");
    let d2 = dir.join("hydra-3nk-s2.wal");
    let _ = std::fs::remove_file(&d1);
    let _ = std::fs::remove_file(&d2);
    let dur1 = spawn_mac_durability(&cluster, "dur1", DUR1_PORT, d1.clone(), keys.clone())?;
    let dur2 = spawn_mac_durability(&cluster, "dur2", DUR2_PORT, d2.clone(), keys.clone())?;
    println!("[mac] durability up: dur1={dur1} dur2={dur2}");

    kill_remote(&vm1);
    kill_remote(&vm2);
    std::thread::sleep(std::time::Duration::from_millis(600));
    let sp_boot = dir.join("hydra-3nk-sp.boot");
    sp_bootstrap(&cluster, &keys, k2, n_ctx, false).write_to(sp_boot.to_str().unwrap())?;
    start_remote(&vm1, sp_boot.to_str().unwrap(), "/home/azureuser/hydra/sp-3nk.boot", "/home/azureuser/hydra/sp-3nk.log")?;
    println!("[vm1] S_P up on {VM1_TS_IP}:{SP_PORT}");
    let s2_boot = dir.join("hydra-3nk-s2.boot");
    s2_bootstrap(&cluster, &keys, k1, k2, n_ctx, dur2).write_to(s2_boot.to_str().unwrap())?;
    start_remote(&vm2, s2_boot.to_str().unwrap(), "/home/azureuser/hydra/s2-3nk.boot", "/home/azureuser/hydra/s2-3nk.log")?;
    println!("[vm2] S2 up on {VM2_TS_IP}:{S2_PORT} (forwarding→S_P, durability→Mac)");

    // S1 local (Mac), forwarding-durable, down = S2 (vm2), dur = dur1 (local). Re-linkable target.
    let s2_addr: SocketAddr = format!("{VM2_TS_IP}:{S2_PORT}").parse()?;
    let s1_id = cluster.issue("s1")?;
    let s1_cfg = WorkerConfig { keys: keys.clone(), rank: 0, layer_first: 0, layer_last: k1, is_final: false, receives_tokens: true, epoch: 0, recovery_id: 0, model_path: Some(mac_model.clone()), n_gpu_layers: 0, n_ctx, sampler_config: None, recovery_start: false };
    let s1_down: DownTarget = std::sync::Arc::new(std::sync::Mutex::new((s2_addr, "s2".to_string())));
    let s1_addr = spawn_multiconn_forwarding_durable_endpoint(
        s1_cfg, cluster.ca.server_config(&s1_id)?,
        TcpMtls::from_config(cluster.ca.client_config(&cluster.issue("s1-down")?)?)?, s1_down, 20,
        TcpMtls::from_config(cluster.ca.client_config(&cluster.issue("s1-dur")?)?)?, dur1, "dur1", true, 8,
    );
    println!("[mac] S1 up on {s1_addr}");

    // commit stream + coordinator connections
    let connector = cluster.coordinator_connector()?;
    let sp_addr: SocketAddr = format!("{VM1_TS_IP}:{SP_PORT}").parse()?;
    let cs_path = dir.join("hydra-3nk-commit.wal");
    let _ = std::fs::remove_file(&cs_path);
    let mut cs = CommitStream::create(&cs_path, CLUSTER_ID, SESSION_ID)?;
    cs.append_initial_commit(&fence(), &admission(&prompt), &initial_checkpoint_bytes(INITIAL_CHECKPOINT_ID, &greedy()), 1)?;
    let mut group = GroupCommitter::new(2);

    let mut c1 = connector.connect(s1_addr, "s1").await?;
    let mut cp = connector.connect(sp_addr, "sp").await?;
    let mut c2 = connector.connect(s2_addr, "s2").await?;
    activate(&mut c1, &keys, ActivationKind::Initial, 0, 0).await?;
    activate(&mut c2, &keys, ActivationKind::Initial, 0, 0).await?;
    activate(&mut cp, &keys, ActivationKind::Initial, 0, 0).await?;

    for (i, &t) in prompt.iter().enumerate() { chain_apply(&mut c1, &keys, i as i64, t, true).await?; }
    let mut committed: Vec<u32> = Vec::new();
    let mut input_pos = prompt.len() as i64;
    for q in 0..m as i64 {
        let (tok, snap) = sample(&mut cp, &keys, q, &cfg_hash).await?;
        committed.push(tok);
        group.push(q, tok, snap);
        if group.count_ready() { let b = group.take().unwrap(); cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot)?; }
        chain_apply(&mut c1, &keys, input_pos, tok, false).await?;
        input_pos += 1;
    }
    if let Some(b) = group.take() { cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot)?; }
    println!("[gen] pre-kill committed {committed:?} (vs reference {:?})", &reference[..m]);

    // Let S2's durable copies (over WAN) settle, then confirm the rebuild source.
    std::thread::sleep(std::time::Duration::from_millis(700));
    let boundaries = BoundaryStore::read(&d2)?;
    if (boundaries.len() as i64) < input_pos { return Err(format!("S2 durable boundaries {} < {input_pos}", boundaries.len()).into()); }

    // ---- REAL kill -9 of S_P on myVm-1 ----
    let t_detect = Instant::now();
    kill_remote(&vm1);
    drop(cp);
    println!("[kill] myVm-1 S_P killed (pkill -9)");
    // Survivors S1 (local) + S2 (myVm-2) freeze (Case A).
    begin_recovery(&mut c1, &keys, input_pos).await?;
    begin_recovery(&mut c2, &keys, input_pos).await?;

    // Replacement S_P on myVm-1 at the SAME address (so S2 re-links via connection-failure).
    std::thread::sleep(std::time::Duration::from_millis(800)); // let the listen port free
    sp_bootstrap(&cluster, &keys, k2, n_ctx, true).write_to(sp_boot.to_str().unwrap())?;
    start_remote(&vm1, sp_boot.to_str().unwrap(), "/home/azureuser/hydra/sp-3nk.boot", "/home/azureuser/hydra/sp-3nk.log")?;
    println!("[vm1] replacement S_P up on {VM1_TS_IP}:{SP_PORT} (recovery_start)");
    let mut rcp = connector.connect(sp_addr, "sp").await?;
    begin_recovery(&mut rcp, &keys, 0).await?;
    for b in boundaries.iter().take(input_pos as usize) { rebuild_apply(&mut rcp, &keys, b.first_input_pos, &b.activations).await?; }
    rcp.send(0, &wire::encode_catch_up_context(&keys, 0, 1, input_pos)).await?;
    match wire::decode(&rcp.recv().await?.payload, &keys)?.1 { Msg::CatchUpReady { .. } => {} o => return Err(format!("expected CATCH_UP_READY, got {o:?}").into()) }
    rcp.send(0, &wire::encode_install_sampler_checkpoint(&keys, 0, INITIAL_CHECKPOINT_ID, &initial_checkpoint_bytes(INITIAL_CHECKPOINT_ID, &greedy()))).await?;
    match wire::decode(&rcp.recv().await?.payload, &keys)?.1 { Msg::SamplerCheckpointInstalled { .. } => {} o => return Err(format!("expected INSTALLED, got {o:?}").into()) }
    activate(&mut rcp, &keys, ActivationKind::Recovery, 1, 1).await?;
    let detect_to_resumed = t_detect.elapsed();
    println!("[recover] replacement S_P rebuilt from S2's durable boundaries + activated; S2 re-links on next forward");

    // ---- resume: sample from the replacement S_P; feedback through S1 → S2 → (re-link) → replacement ----
    let mut resumed = Vec::new();
    for q in (m as i64)..n as i64 {
        let (tok, snap) = sample(&mut rcp, &keys, q, &cfg_hash).await?;
        resumed.push(tok);
        group.push(q, tok, snap);
        if group.count_ready() { let b = group.take().unwrap(); cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot)?; }
        if (q as usize + 1) < n { chain_apply(&mut c1, &keys, input_pos, tok, false).await?; input_pos += 1; }
    }
    if let Some(b) = group.take() { cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot)?; }
    drop(cs);

    // ---- three assertions ----
    let mut full = committed.clone();
    full.extend(resumed.clone());
    let agree = full.iter().zip(&reference).filter(|(a, b)| a == b).count();
    let stats = recovery::verify(&cs_path)?;
    println!("\n   recovered stream: {full:?}");
    println!("   reference (Mac):  {reference:?}");
    println!("   (b) argmax agreement: {agree}/{n}  (mixed tier, cross-arch — spec I8)");
    println!("   (a) SSE continuity: committed_positions={} strictly_increasing={} max_position={}", stats.committed_positions, stats.positions_strictly_increasing, stats.max_position);
    println!("   (c) disk truth: commit stream I19-valid, no position twice (via recovery::verify)");
    println!("   timing: detection→resumed {detect_to_resumed:?}  [WAN/Tailscale, real kill -9 — NOT the <15s LAN/M3 target]");

    kill_remote(&vm1);
    kill_remote(&vm2);
    let ok = agree == n && stats.committed_positions == n && stats.positions_strictly_increasing && stats.max_position == n as i64 - 1;
    println!("\n[done] real 3-node S_P kill-window over Tailscale (Mac→myVm-2→myVm-1).");
    println!("   THREENODE_KILL_{}: rebuild from S2 durable boundaries + S2 re-link; 3 assertions {}", if ok { "OK" } else { "CHECK" }, if ok { "held" } else { "FAILED" });
    if !ok { return Err(format!("real S_P kill assertions failed: agree {agree}/{n}, positions {} max {}", stats.committed_positions, stats.max_position).into()); }
    Ok(())
}
