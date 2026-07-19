//! `hydra-wan` — the M2 DoD **real-second-machine + WAN data point** runner (see `docs/wan-run.md`).
//!
//! Topology: **coordinator + S1 on this Mac (arm64)** ↔ **S_P on a cloud VM (x86-64) over
//! Tailscale**. S1 hosts layers `[0,k)`, extracts the boundary residual, and forwards it as a `FWD`
//! tensor over the real WAN; the remote S_P hosts `[k,-1)` + the sampler and produces the token.
//!
//! It provisions the remote S_P over SSH (scp the bootstrap, start `hydra-worker` bound to the
//! **Tailscale IP only** — never 0.0.0.0), runs the correctness + perf phases, and (phase 2) kills
//! the remote worker mid-generation and drives recovery over the WAN.
//!
//! **Cross-arch honesty:** arm64 ↔ x86-64 boundary tensors are NOT bit-exact (spec I8). Greedy
//! decoding is the mixed-backend tier's "deterministic replay reproduces the same tokens" probe —
//! we report per-step argmax agreement vs the Mac's unsplit greedy reference, never a bit-exact
//! claim. All timings are annotated WAN/Tailscale and are NOT the <15s LAN/M3 targets.

use std::process::Command;
use std::time::{Duration, Instant};

use hydra_engine_sys::Model;
use hydra_state::{ActivationKind, ActivationTuple};
use hydra_transport::framed::Conn;
use hydra_worker::bootstrap::Bootstrap;
use hydra_worker::pair::{run_generation, Cluster, Endpoints};
use hydra_worker::sampler::SamplingConfig;
use hydra_worker::wire::{self, Msg, SessionKeys};
use hydra_worker::worker::{WorkerConfig, INITIAL_CHECKPOINT_ID};
use tokio::io::{AsyncRead, AsyncWrite};

const VM_SSH: &str = "hydra-vm";
const VM_IP: &str = "100.115.200.62"; // Tailscale IP — bind here ONLY, never 0.0.0.0
const VM_PORT: u16 = 41999;
const VM_WORKER: &str = "/home/azureuser/hydra/target/debug/hydra-worker";
const VM_MODEL: &str = "/home/azureuser/hydra/models/qwen2.5-0.5b-instruct-fp16.gguf";
const VM_LIBDIR: &str = "/home/azureuser/hydra/vendor/llama.cpp/build/bin";
const VM_BOOT: &str = "/home/azureuser/hydra/sp.boot";
const MAC_MODEL_ENV: &str = "HYDRA_TEST_MODEL";

fn mac_model_path() -> String {
    std::env::var(MAC_MODEL_ENV).unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../models/qwen2.5-0.5b-instruct-fp16.gguf").to_string()
    })
}

fn greedy() -> SamplingConfig {
    SamplingConfig { temperature: 0.0, top_p: 1.0, repeat_penalty: 1.0, penalty_last_n: 0, seed: 7 }
}

/// The Mac's unsplit greedy reference: apply the prompt to the full model (arm64), then argmax-decode
/// `n` steps, feeding each token back. Deterministic (greedy = argmax, no RNG).
fn mac_unsplit_greedy(model: &Model, prompt: &[u32], n: usize) -> Vec<u32> {
    let n_ctx = prompt.len() as i32 + n as i32 + 8;
    let mut ctx = model.context(0, -1, false, n_ctx, n_ctx).expect("ctx");
    for (pos, &t) in prompt.iter().enumerate() {
        ctx.apply_tokens(&[t as i32], pos as i32, None).expect("prefill");
    }
    let argmax = |l: &[f32]| (0..l.len()).max_by(|&a, &b| l[a].total_cmp(&l[b])).unwrap() as u32;
    let mut out = Vec::with_capacity(n);
    let mut input_pos = prompt.len() as i32;
    for step in 0..n {
        let tok = argmax(&ctx.logits(0).expect("logits"));
        out.push(tok);
        if step + 1 < n {
            ctx.apply_tokens(&[tok as i32], input_pos, None).expect("feedback");
            input_pos += 1;
        }
    }
    out
}

fn sh(args: &[&str]) -> Result<String, String> {
    let out = Command::new(args[0]).args(&args[1..]).output().map_err(|e| format!("{}: {e}", args[0]))?;
    if !out.status.success() {
        return Err(format!("{:?} failed: {}", args, String::from_utf8_lossy(&out.stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// scp the S_P bootstrap to the VM and start `hydra-worker` bound to the Tailscale IP:port, detached.
/// Waits until the worker advertises `HYDRA_WORKER_LISTENING`.
fn start_remote_sp(local_boot: &str) -> Result<(), String> {
    kill_remote_sp();
    std::thread::sleep(std::time::Duration::from_millis(800));
    sh(&["scp", "-q", local_boot, &format!("{VM_SSH}:{VM_BOOT}")])?;
    // Fully detach (`setsid` + `</dev/null`) so ssh returns; force exit 0. The worker prints
    // HYDRA_WORKER_LISTENING to sp.log. rpath is baked in; set LD_LIBRARY_PATH belt-and-suspenders.
    sh(&[
        "ssh", VM_SSH,
        &format!("rm -f ~/hydra/sp.log; \
                  setsid env LD_LIBRARY_PATH={VM_LIBDIR} {VM_WORKER} {VM_BOOT} </dev/null >~/hydra/sp.log 2>&1 & \
                  echo started; exit 0"),
    ])?;
    // Poll for the listening line (engine load + bind).
    for _ in 0..60 {
        let log = sh(&["ssh", VM_SSH, "cat ~/hydra/sp.log 2>/dev/null || true"]).unwrap_or_default();
        if log.contains("HYDRA_WORKER_LISTENING") {
            let engine = log.contains("engine=true");
            eprintln!("[vm] S_P listening on {VM_IP}:{VM_PORT} (engine={engine})");
            if !engine {
                return Err("remote S_P came up with engine=false (model/libs not linked)".into());
            }
            return Ok(());
        }
        if log.contains("panic") || log.contains("Error") {
            return Err(format!("remote S_P failed to start:\n{log}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err("remote S_P did not advertise HYDRA_WORKER_LISTENING within 30s".into())
}

fn kill_remote_sp() {
    let _ = sh(&["ssh", VM_SSH, "pkill -9 -f hydra-worker; exit 0"]);
}

fn sp_bootstrap(cluster: &Cluster, keys: &SessionKeys, k: i32, n_ctx: i32, recovery_start: bool) -> Bootstrap {
    let id = cluster.issue("sp").unwrap();
    Bootstrap {
        listen_addr: format!("{VM_IP}:{VM_PORT}"),
        device_name: "sp".to_string(),
        ca_cert_der: cluster.ca.ca_cert_der().as_ref().to_vec(),
        cert_chain_der: id.cert_chain.iter().map(|c| c.as_ref().to_vec()).collect(),
        key_pkcs8_der: id.key_pkcs8_der(),
        cfg: WorkerConfig {
            keys: keys.clone(), rank: 1, layer_first: k, layer_last: -1, is_final: true,
            receives_tokens: false, epoch: 0, recovery_id: if recovery_start { 1 } else { 0 },
            model_path: Some(VM_MODEL.to_string()), n_gpu_layers: 0, n_ctx,
            sampler_config: Some(greedy()), recovery_start,
        },
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mac_model = mac_model_path();
    if !std::path::Path::new(&mac_model).exists() {
        eprintln!("SKIP: Mac model not found at {mac_model}");
        return Ok(());
    }

    // Tokenize + reference + split point, on the Mac (model freed before S1 loads).
    let (prompt, reference, k, n_layer, n_ctx) = {
        let model = Model::load(&mac_model, 0)?;
        let prompt: Vec<u32> = model.tokenize("The capital of France is").expect("tokenize").into_iter().map(|t| t as u32).collect();
        let n = 12usize;
        let reference = mac_unsplit_greedy(&model, &prompt, n);
        let k = (model.n_layer() / 2).max(1);
        let n_ctx = prompt.len() as i32 + n as i32 + 8;
        (prompt, reference, k, model.n_layer(), n_ctx)
    };
    let n = reference.len();
    println!("== hydra-wan: WAN run (Mac arm64 S1 [0,{k}) ↔ VM x86-64 S_P [{k},{n_layer})) over Tailscale ==");
    println!("   prompt {} tokens, greedy {n} steps; cross-arch → mixed-backend tier (NOT bit-exact, spec I8)", prompt.len());

    let keys = SessionKeys::dev(0x7A);
    let cluster = Cluster::new()?;
    let local_boot = std::env::temp_dir().join("hydra-wan-sp.boot");
    sp_bootstrap(&cluster, &keys, k, n_ctx, false).write_to(local_boot.to_str().unwrap())?;
    let vm_addr: std::net::SocketAddr = format!("{VM_IP}:{VM_PORT}").parse()?;
    let connector = cluster.coordinator_connector()?;

    // A fresh S1 endpoint (in-process, Mac) — each logical run gets its own, since a worker keeps its
    // KV across reconnects (a re-prefill into a live KV would collide).
    let fresh_s1 = || -> Result<std::net::SocketAddr, Box<dyn std::error::Error>> {
        let id = cluster.issue("s1")?;
        let cfg = WorkerConfig {
            keys: keys.clone(), rank: 0, layer_first: 0, layer_last: k, is_final: false,
            receives_tokens: true, epoch: 0, recovery_id: 0, model_path: Some(mac_model.clone()),
            n_gpu_layers: 0, n_ctx, sampler_config: None, recovery_start: false,
        };
        Ok(hydra_worker::pair::spawn_endpoint(cfg, cluster.ca.server_config(&id)?))
    };

    // ---- Phase 1: cross-machine greedy correctness (mixed-tier) + perf ----
    println!("\n[phase 1] cross-machine greedy pipeline + perf");
    start_remote_sp(local_boot.to_str().unwrap())?;
    let ep = Endpoints::new(fresh_s1()?, "s1", vm_addr, "sp");
    let t = Instant::now();
    let got = run_generation(&connector, &ep, &keys, &greedy(), &prompt, n).await.map_err(|e| format!("gen: {e}"))?;
    let wall = t.elapsed();
    let agree = got.iter().zip(&reference).filter(|(a, b)| a == b).count();
    println!("   tokens (cross-machine): {got:?}");
    println!("   tokens (Mac unsplit ref): {reference:?}");
    println!("   argmax agreement: {agree}/{n} steps  (mixed-tier: cross-arch drift may flip an argmax — spec I8)");
    println!("   perf: {n} tokens in {:.2?} → {:.2} tok/s  [WAN/Tailscale, arm64↔x86-64 — NOT a wired-LAN number]", wall, n as f64 / wall.as_secs_f64());

    // Determinism: fresh workers, same placement → same tokens.
    start_remote_sp(local_boot.to_str().unwrap())?;
    let ep2 = Endpoints::new(fresh_s1()?, "s1", vm_addr, "sp");
    let got2 = run_generation(&connector, &ep2, &keys, &greedy(), &prompt, n).await.map_err(|e| format!("gen2: {e}"))?;
    println!("   deterministic replay (fresh workers): {}", if got2 == got { "IDENTICAL ✓" } else { "DIVERGED ✗" });

    // ---- Phase 2: real machine death over the WAN — kill a full-range S_P mid-generation, recover ----
    // A full-range D0-class S_P on the VM (the in-scope C-part-2 recovery: catch-up replays raw
    // tokens). The split-stage (S1↔S_P) recovery needs boundary replay — the multi-node flow welded
    // to the FWD slice, out of C-part-2 scope. Both S_P generations here are on the VM (x86-64), so
    // the recovered stream is byte-identical to the uninterrupted VM run (same-arch).
    println!("\n[phase 2] WAN kill-window: SIGKILL a full-range S_P mid-generation, recover through the real machinery");
    let timing = wan_kill_window(&cluster, &keys, n_ctx, &prompt, n).await?;
    println!("   detection→resumed (over WAN): {:.2?}  [WAN/Tailscale — NOT the <15s LAN/M3 D1 target]", timing);

    kill_remote_sp();
    println!("\n[done] WAN run complete. Remote S_P stopped.");
    println!("REMINDER: the Azure NSG still allows public port 22 as a fallback — now that the WAN run succeeded, close it.");
    Ok(())
}

// ------------------------- phase 2: WAN kill-window (full-range S_P) -------------------------

fn full_sp_bootstrap(cluster: &Cluster, keys: &SessionKeys, n_ctx: i32, recovery_start: bool) -> Bootstrap {
    let id = cluster.issue("sp").unwrap();
    Bootstrap {
        listen_addr: format!("{VM_IP}:{VM_PORT}"),
        device_name: "sp".to_string(),
        ca_cert_der: cluster.ca.ca_cert_der().as_ref().to_vec(),
        cert_chain_der: id.cert_chain.iter().map(|c| c.as_ref().to_vec()).collect(),
        key_pkcs8_der: id.key_pkcs8_der(),
        cfg: WorkerConfig {
            keys: keys.clone(), rank: 0, layer_first: 0, layer_last: -1, is_final: true,
            receives_tokens: true, epoch: 0, recovery_id: if recovery_start { 1 } else { 0 },
            model_path: Some(VM_MODEL.to_string()), n_gpu_layers: 0, n_ctx,
            sampler_config: Some(greedy()), recovery_start,
        },
    }
}

async fn apply<S: AsyncRead + AsyncWrite + Unpin>(c: &mut Conn<S>, keys: &SessionKeys, pos: i64, tok: u32, no_sample: bool) -> Result<(), String> {
    c.send(0, &wire::encode_apply_token(keys, 0, pos, tok, no_sample)).await.map_err(|e| format!("apply send: {e}"))?;
    match wire::decode(&c.recv().await.map_err(|e| format!("apply recv: {e}"))?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::AppliedAck { cumulative_input_pos, .. } if cumulative_input_pos == pos => Ok(()),
        other => Err(format!("apply @ {pos}: expected APPLIED_ACK, got {other:?}")),
    }
}

async fn sample<S: AsyncRead + AsyncWrite + Unpin>(c: &mut Conn<S>, keys: &SessionKeys, pos: i64, h: &[u8; 32]) -> Result<(u32, Vec<u8>), String> {
    c.send(0, &wire::encode_sample_next(keys, 0, pos, h, INITIAL_CHECKPOINT_ID)).await.map_err(|e| format!("sample send: {e}"))?;
    match wire::decode(&c.recv().await.map_err(|e| format!("sample recv: {e}"))?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::Sampled { token_id, post_sample_snapshot, .. } => Ok((token_id, post_sample_snapshot)),
        Msg::Err { code } => Err(format!("SAMPLE_NEXT @ {pos} errored: code {code}")),
        other => Err(format!("SAMPLE_NEXT @ {pos}: expected SAMPLED, got {other:?}")),
    }
}

/// Full-range greedy generation against a connected S_P: prompt prefill + `n` sample steps. Returns
/// `(tokens, last_snapshot, last_snapshot_pos)`.
async fn gen_full<S: AsyncRead + AsyncWrite + Unpin>(c: &mut Conn<S>, keys: &SessionKeys, prompt: &[u32], n: usize) -> Result<(Vec<u32>, Vec<u8>, i64), String> {
    let h = greedy().hash();
    for (pos, &t) in prompt.iter().enumerate() {
        apply(c, keys, pos as i64, t, true).await?;
    }
    let mut out = Vec::with_capacity(n);
    let (mut last_snap, mut last_pos) = (Vec::new(), -1i64);
    let mut input_pos = prompt.len() as i64;
    for pos in 0..n as i64 {
        let (tok, snap) = sample(c, keys, pos, &h).await?;
        out.push(tok);
        last_snap = snap;
        last_pos = pos;
        if (pos as usize + 1) < n {
            apply(c, keys, input_pos, tok, false).await?;
            input_pos += 1;
        }
    }
    Ok((out, last_snap, last_pos))
}

/// Drive the recovery flow on a connected fresh replacement S_P (through the real machinery), leaving
/// it ready to resume `SAMPLE_NEXT` at `durable_pos + 1`. `replay` = prompt ++ committed tokens.
async fn recover<S: AsyncRead + AsyncWrite + Unpin>(c: &mut Conn<S>, keys: &SessionKeys, replay: &[u32], input_frontier: i64, snapshot: &[u8]) -> Result<(), String> {
    c.send(0, &wire::encode_begin_recovery(keys, 0, 0, 1, 0)).await.map_err(|e| format!("begin: {e}"))?;
    match wire::decode(&c.recv().await.map_err(|e| e.to_string())?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::RecoveryAck { .. } => {}
        o => return Err(format!("expected RECOVERY_ACK, got {o:?}")),
    }
    for (i, &tok) in replay.iter().enumerate() {
        apply(c, keys, i as i64, tok, true).await?;
    }
    c.send(0, &wire::encode_catch_up_context(keys, 0, 1, input_frontier)).await.map_err(|e| format!("catchup: {e}"))?;
    match wire::decode(&c.recv().await.map_err(|e| e.to_string())?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::CatchUpReady { .. } => {}
        o => return Err(format!("expected CATCH_UP_READY, got {o:?}")),
    }
    c.send(0, &wire::encode_install_sampler_checkpoint(keys, 0, INITIAL_CHECKPOINT_ID, snapshot)).await.map_err(|e| format!("install: {e}"))?;
    match wire::decode(&c.recv().await.map_err(|e| e.to_string())?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::SamplerCheckpointInstalled { .. } => {}
        o => return Err(format!("expected SAMPLER_CHECKPOINT_INSTALLED, got {o:?}")),
    }
    let tuple = ActivationTuple { kind: ActivationKind::Recovery, epoch: 0, recovery_id: 1, attempt: 0, sampler_checkpoint_id: INITIAL_CHECKPOINT_ID };
    c.send(0, &wire::encode_commit_activation(keys, &tuple, 1)).await.map_err(|e| format!("commit: {e}"))?;
    match wire::decode(&c.recv().await.map_err(|e| e.to_string())?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::ActivationCommitted(_) => {}
        o => return Err(format!("expected ACTIVATION_COMMITTED, got {o:?}")),
    }
    c.send(0, &wire::encode_finalize_activation(keys, &tuple, 1)).await.map_err(|e| format!("finalize: {e}"))?;
    match wire::decode(&c.recv().await.map_err(|e| e.to_string())?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::ActivationFinalized => {}
        o => return Err(format!("expected ACTIVATION_FINALIZED, got {o:?}")),
    }
    Ok(())
}

/// Kill a full-range VM S_P mid-generation over the WAN and recover onto a replacement; assert the
/// recovered stream is byte-identical to an uninterrupted VM run (same-arch). Returns detection→resumed.
async fn wan_kill_window(cluster: &Cluster, keys: &SessionKeys, n_ctx: i32, prompt: &[u32], n: usize) -> Result<Duration, Box<dyn std::error::Error>> {
    let boot = full_sp_bootstrap(cluster, keys, n_ctx, false);
    let local = std::env::temp_dir().join("hydra-wan-full.boot");
    boot.write_to(local.to_str().unwrap())?;
    let connector = cluster.coordinator_connector()?;
    let vm_addr: std::net::SocketAddr = format!("{VM_IP}:{VM_PORT}").parse()?;
    let h = greedy().hash();

    // Uninterrupted VM reference (same-arch, deterministic).
    start_remote_sp(local.to_str().unwrap())?;
    let mut c = connector.connect(vm_addr, "sp").await?;
    let (vm_ref, _, _) = gen_full(&mut c, keys, prompt, n).await?;
    drop(c);
    println!("   uninterrupted VM S_P greedy (x86-64): {vm_ref:?}");

    // Kill-run: generate m tokens, SIGKILL, recover, resume.
    let m = n / 2;
    start_remote_sp(local.to_str().unwrap())?;
    let mut c = connector.connect(vm_addr, "sp").await?;
    let (pre, last_snap, last_pos) = gen_full(&mut c, keys, prompt, m).await?;
    assert_eq!(last_pos, m as i64 - 1);
    drop(c);

    // ---- real machine death over the WAN ----
    let t_detect = Instant::now();
    kill_remote_sp();
    println!("   SIGKILL'd the VM S_P after {m} committed tokens; bringing up a replacement...");

    // Replacement full-range S_P (FROZEN) + drive recovery.
    let rboot = full_sp_bootstrap(cluster, keys, n_ctx, true);
    rboot.write_to(local.to_str().unwrap())?;
    start_remote_sp(local.to_str().unwrap())?;
    let mut rc = connector.connect(vm_addr, "sp").await?;
    let replay: Vec<u32> = prompt.iter().copied().chain(pre.iter().copied()).collect();
    let input_frontier = replay.len() as i64;
    recover(&mut rc, keys, &replay, input_frontier, &last_snap).await?;

    // Resume: SAMPLE_NEXT at m, feed back, to n.
    let mut resumed = Vec::new();
    let mut first_resumed = None;
    let mut input_pos = input_frontier;
    for pos in (m as i64)..n as i64 {
        let (tok, _snap) = sample(&mut rc, keys, pos, &h).await?;
        if first_resumed.is_none() {
            first_resumed = Some(t_detect.elapsed());
        }
        resumed.push(tok);
        if (pos as usize + 1) < n {
            apply(&mut rc, keys, input_pos, tok, false).await?;
            input_pos += 1;
        }
    }
    drop(rc);

    let mut full = pre;
    full.extend(resumed);
    let byte_identical = full == vm_ref;
    println!("   recovered stream (pre-kill ⊕ resumed): {full:?}");
    println!("   byte-identical to uninterrupted VM run (same-arch VM→VM): {}", if byte_identical { "YES ✓" } else { "NO ✗" });
    if !byte_identical {
        return Err("WAN kill-window recovery diverged from the uninterrupted run".into());
    }
    Ok(first_resumed.unwrap_or_default())
}
