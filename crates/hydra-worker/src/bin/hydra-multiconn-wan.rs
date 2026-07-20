//! `hydra-multiconn-wan` — P1·1a: demonstrate the **multi-connection serve loop** on the real
//! 2-node pair over Tailscale (gate-condition-(i) substrate, on real heterogeneous hardware).
//!
//! Topology: **coordinator + S1 on this Mac (arm64)** ↔ **S_P on a cloud VM (x86-64) over
//! Tailscale**. Unlike `hydra-wan` (which relays every boundary through the coordinator), here S1 is
//! a **forwarding endpoint** that sends each boundary **straight to S_P** (worker→worker direct FWD),
//! while the coordinator drives `SAMPLE_NEXT` to S_P over a **separate, concurrent** connection. So
//! S_P must serve **two inbound connections at once** — S1's `FWD` and the coordinator's control —
//! which is exactly what the multi-connection serve loop (`serve_multi_conn`) provides. Under the old
//! sequential accept loop S_P would serve S1's connection and never accept the coordinator's,
//! deadlocking `SAMPLE_NEXT`; this run is the real-hardware proof that it no longer does.
//!
//! **Cross-arch honesty:** arm64 ↔ x86-64 boundary tensors are NOT bit-exact (spec I8); greedy argmax
//! agreement vs the Mac's unsplit reference is the mixed-backend tier probe, never a bit-exact claim.
//! Mac↔VM is **WAN/Tailscale**; timings are annotated as such and are NOT wired-LAN numbers.

use std::net::SocketAddr;
use std::process::Command;
use std::time::Instant;

use hydra_engine_sys::Model;
use hydra_worker::bootstrap::Bootstrap;
use hydra_worker::pair::{run_direct_fwd_generation, Cluster, Endpoints};
use hydra_worker::sampler::SamplingConfig;
use hydra_worker::wire::SessionKeys;
use hydra_worker::worker::{DownTarget, WorkerConfig};

// myVm-2 (B2als_v2, 4 GiB, x86-64) — Tailscale-only. Override via env for a different node.
const VM_SSH: &str = "hydra-vm2";
const VM_IP: &str = "100.73.205.31";
const VM_PORT: u16 = 41998;
const VM_WORKER: &str = "/home/azureuser/hydra/target/debug/hydra-worker";
const VM_MODEL: &str = "/home/azureuser/hydra/models/qwen2.5-0.5b-instruct-fp16.gguf";
const VM_LIBDIR: &str = "/home/azureuser/hydra/vendor/llama.cpp/build/bin";
const VM_BOOT: &str = "/home/azureuser/hydra/sp-mc.boot";

fn greedy() -> SamplingConfig {
    SamplingConfig { temperature: 0.0, top_p: 1.0, repeat_penalty: 1.0, penalty_last_n: 0, seed: 7 }
}

fn mac_model_path() -> String {
    std::env::var("HYDRA_TEST_MODEL")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../models/qwen2.5-0.5b-instruct-fp16.gguf").to_string())
}

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

fn kill_remote_sp() {
    let _ = sh(&["ssh", VM_SSH, "pkill -9 -f hydra-worker; exit 0"]);
}

/// scp the S_P bootstrap and start the (multi-conn) `hydra-worker` bound to the Tailscale IP, detached.
fn start_remote_sp(local_boot: &str) -> Result<(), String> {
    kill_remote_sp();
    std::thread::sleep(std::time::Duration::from_millis(800));
    sh(&["scp", "-q", local_boot, &format!("{VM_SSH}:{VM_BOOT}")])?;
    sh(&[
        "ssh", VM_SSH,
        &format!("rm -f ~/hydra/sp-mc.log; \
                  setsid env LD_LIBRARY_PATH={VM_LIBDIR} {VM_WORKER} {VM_BOOT} </dev/null >~/hydra/sp-mc.log 2>&1 & \
                  echo started; exit 0"),
    ])?;
    // The binary prints HYDRA_WORKER_LISTENING on bind but the `engine=` line only AFTER Worker::new
    // finishes loading the model (seconds on a small VM). Wait for `engine=`, not just LISTENING, else
    // we race the model load and see a false engine=false.
    for _ in 0..120 {
        let log = sh(&["ssh", VM_SSH, "cat ~/hydra/sp-mc.log 2>/dev/null || true"]).unwrap_or_default();
        if log.contains("engine=true") {
            eprintln!("[vm] S_P listening on {VM_IP}:{VM_PORT} (engine=true, model loaded)");
            return Ok(());
        }
        if log.contains("engine=false") {
            return Err("remote S_P came up with engine=false (model/libs not linked)".into());
        }
        if log.contains("panic") || log.contains("Error") {
            return Err(format!("remote S_P failed to start:\n{log}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err("remote S_P did not report engine= within 60s".into())
}

fn sp_bootstrap(cluster: &Cluster, keys: &SessionKeys, k: i32, n_ctx: i32) -> Bootstrap {
    let id = cluster.issue("sp").unwrap();
    Bootstrap {
        listen_addr: format!("{VM_IP}:{VM_PORT}"),
        device_name: "sp".to_string(),
        ca_cert_der: cluster.ca.ca_cert_der().as_ref().to_vec(),
        cert_chain_der: id.cert_chain.iter().map(|c| c.as_ref().to_vec()).collect(),
        key_pkcs8_der: id.key_pkcs8_der(),
        cfg: WorkerConfig {
            keys: keys.clone(), rank: 1, layer_first: k, layer_last: -1, is_final: true,
            receives_tokens: false, epoch: 0, recovery_id: 0, model_path: Some(VM_MODEL.to_string()),
            n_gpu_layers: 0, n_ctx, sampler_config: Some(greedy()), recovery_start: false,
        },
        forwarding: None,
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mac_model = mac_model_path();
    if !std::path::Path::new(&mac_model).exists() {
        eprintln!("SKIP: Mac model not found at {mac_model}");
        return Ok(());
    }

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
    println!("== hydra-multiconn-wan: direct-FWD generation, Mac arm64 S1 [0,{k}) → VM x86-64 S_P [{k},{n_layer}) over Tailscale ==");
    println!("   S_P serves S1's direct FWD AND the coordinator's SAMPLE_NEXT concurrently (multi-conn serve loop, P1·1a)");
    println!("   prompt {} tokens, greedy {n} steps; cross-arch → mixed-backend tier (NOT bit-exact, spec I8)", prompt.len());

    let keys = SessionKeys::dev(0x11);
    let cluster = Cluster::new()?;
    let local_boot = std::env::temp_dir().join("hydra-mc-sp.boot");
    sp_bootstrap(&cluster, &keys, k, n_ctx).write_to(local_boot.to_str().unwrap())?;
    let vm_addr: SocketAddr = format!("{VM_IP}:{VM_PORT}").parse()?;
    let connector = cluster.coordinator_connector()?;

    // A fresh S1 **forwarding** endpoint on the Mac whose downstream target is the VM S_P (direct FWD).
    let s1_client = cluster.issue("s1-client")?; // S1 dials S_P presenting this identity
    let fresh_s1 = |sp: SocketAddr| -> Result<SocketAddr, Box<dyn std::error::Error>> {
        let id = cluster.issue("s1")?;
        let cfg = WorkerConfig {
            keys: keys.clone(), rank: 0, layer_first: 0, layer_last: k, is_final: false,
            receives_tokens: true, epoch: 0, recovery_id: 0, model_path: Some(mac_model.clone()),
            n_gpu_layers: 0, n_ctx, sampler_config: None, recovery_start: false,
        };
        let down_connector = hydra_transport::tcp_mtls::TcpMtls::from_config(cluster.ca.client_config(&s1_client)?)?;
        let down: DownTarget = std::sync::Arc::new(std::sync::Mutex::new((sp, "sp".to_string())));
        Ok(hydra_worker::pair::spawn_forwarding_endpoint(cfg, cluster.ca.server_config(&id)?, down_connector, down))
    };

    println!("\n[run] provisioning multi-conn S_P on the VM + direct-FWD generation");
    start_remote_sp(local_boot.to_str().unwrap())?;
    let s1 = fresh_s1(vm_addr)?;
    let t = Instant::now();
    let ep = Endpoints::new(s1, "s1", vm_addr, "sp");
    let got = run_direct_fwd_generation(&connector, &ep, &keys, &greedy(), &prompt, n)
        .await
        .map_err(|e| format!("direct-fwd gen: {e}"))?;
    let wall = t.elapsed();
    let agree = got.iter().zip(&reference).filter(|(a, b)| a == b).count();
    println!("   tokens (cross-machine direct-FWD): {got:?}");
    println!("   tokens (Mac unsplit reference):    {reference:?}");
    println!("   argmax agreement: {agree}/{n} steps  (mixed-tier: cross-arch drift may flip an argmax — spec I8)");
    println!("   perf: {n} tokens in {:.2?} → {:.2} tok/s  [WAN/Tailscale, arm64↔x86-64 — NOT a wired-LAN number]", wall, n as f64 / wall.as_secs_f64());

    // Determinism: fresh workers, same placement → same tokens.
    start_remote_sp(local_boot.to_str().unwrap())?;
    let s1b = fresh_s1(vm_addr)?;
    let ep2 = Endpoints::new(s1b, "s1", vm_addr, "sp");
    let got2 = run_direct_fwd_generation(&connector, &ep2, &keys, &greedy(), &prompt, n)
        .await
        .map_err(|e| format!("direct-fwd gen2: {e}"))?;
    println!("   deterministic replay (fresh workers): {}", if got2 == got { "IDENTICAL ✓" } else { "DIVERGED ✗" });

    kill_remote_sp();
    let ok = agree == n && got2 == got;
    println!("\n[done] multi-conn serve loop demonstrated on the real 2-node pair over Tailscale.");
    println!(
        "   MULTICONN_WAN_{}: S_P served S1's direct FWD + coordinator SAMPLE_NEXT concurrently; {agree}/{n} argmax agreement",
        if ok { "OK" } else { "CHECK" }
    );
    if !ok {
        return Err(format!("multi-conn WAN run did not fully agree: {agree}/{n} argmax, replay {}", if got2 == got { "identical" } else { "diverged" }).into());
    }
    Ok(())
}
