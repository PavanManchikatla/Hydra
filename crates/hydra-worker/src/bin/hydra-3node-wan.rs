//! `hydra-3node-wan` — P1·1b seam B, part 2: the **3-node chained direct-FWD** pipeline on real
//! heterogeneous hardware over Tailscale (the in-process byte-identical `three_node` test gates this).
//!
//! Topology (capability-weighted split from P1·2, 4.0 : 2.1 : 1.0 → the most layers on the fastest
//! node): **coordinator + S1 on this Mac (arm64, 4.0)** → **S2 on myVm-2 (x86-64, 2.1)** → **S_P on
//! myVm-1 (x86-64, 1.0)**. Each boundary travels worker→worker (S1→S2→S_P), never via the
//! coordinator, and S1 and S2 each **durably copy** their boundary to a `BoundaryStore` the
//! coordinator hosts on the Mac (seam B: `serve_multi_conn_forwarding_durable`). S_P serves S2's
//! `FWD` **and** the coordinator's `SAMPLE_NEXT` concurrently (the multi-conn substrate).
//!
//! **Link classes (honesty rule, §9):** every leg here is **WAN/Tailscale** — Mac↔VM inherently, and
//! the VM↔VM leg (S2→S_P) is also routed over Tailscale this run to keep every listener bound to its
//! Tailscale IP (never 0.0.0.0; NSG `DenyAllInbound`). The cloud-VNet fast path (10.0.0.x, sub-ms) is
//! *available* between the VMs but is not exercised here — a future optimization, annotated as such.
//! No number here is a wired-LAN number.
//!
//! **Cross-arch honesty:** arm64 ↔ x86-64 boundaries are NOT bit-exact (spec I8); greedy **argmax
//! agreement** vs the Mac's unsplit reference is the mixed-backend-tier probe, never a bit-exact claim.
//!
//! Env overrides: `HYDRA_TEST_MODEL` (Mac model), `HYDRA_VM1_SSH`/`HYDRA_VM2_SSH` (ssh hosts).

use std::net::SocketAddr;
use std::process::Command;
use std::sync::mpsc;
use std::time::Instant;

use hydra_coordinator::BoundaryStore;
use hydra_engine_sys::Model;
use hydra_transport::tcp_mtls::{TcpMtls, TcpMtlsListener};
use hydra_worker::bootstrap::{Bootstrap, ForwardingBootstrap};
use hydra_worker::pair::{run_direct_fwd_generation, spawn_multiconn_forwarding_durable_endpoint, Cluster, Endpoints};
use hydra_worker::sampler::SamplingConfig;
use hydra_worker::wire::{self, Msg, SessionKeys};
use hydra_worker::worker::WorkerConfig;

const MAC_TS_IP: &str = "100.93.110.78";
// myVm-1 (B2ms, 8 GB) hosts S_P; myVm-2 (B2als_v2, 4 GiB) hosts S2. Tailscale IPs (§9).
const VM1_TS_IP: &str = "100.115.200.62";
const VM2_TS_IP: &str = "100.73.205.31";
const SP_PORT: u16 = 41997;
const S2_PORT: u16 = 41996;
const DUR1_PORT: u16 = 42001; // S1's boundary copies (S1 is local, dials over loopback/Tailscale)
const DUR2_PORT: u16 = 42002; // S2's boundary copies (myVm-2 dials the Mac over Tailscale)
const VM_WORKER: &str = "/home/azureuser/hydra/target/debug/hydra-worker";
const VM_MODEL: &str = "/home/azureuser/hydra/models/qwen2.5-0.5b-instruct-fp16.gguf";
const VM_LIBDIR: &str = "/home/azureuser/hydra/vendor/llama.cpp/build/bin";

fn vm1_ssh() -> String {
    std::env::var("HYDRA_VM1_SSH").unwrap_or_else(|_| "hydra-vm".into())
}
fn vm2_ssh() -> String {
    std::env::var("HYDRA_VM2_SSH").unwrap_or_else(|_| "hydra-vm2".into())
}
fn mac_model_path() -> String {
    std::env::var("HYDRA_TEST_MODEL")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../models/qwen2.5-0.5b-instruct-fp16.gguf").to_string())
}
fn greedy() -> SamplingConfig {
    SamplingConfig { temperature: 0.0, top_p: 1.0, repeat_penalty: 1.0, penalty_last_n: 0, seed: 7 }
}

/// Capability-weighted 3-way split (P1·2 ratios 4.0 : 2.1 : 1.0 → fractions 0.563/0.296/0.141).
fn split3(n_layer: i32) -> (i32, i32) {
    let l = n_layer as f64;
    let k1 = ((0.563 * l).round() as i32).clamp(1, n_layer - 2);
    let k2 = (k1 + (0.296 * l).round() as i32).clamp(k1 + 1, n_layer - 1);
    (k1, k2)
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

fn kill_remote(ssh: &str) {
    let _ = sh(&["ssh", ssh, "pkill -9 -f hydra-worker; exit 0"]);
}

/// scp a bootstrap to a VM and start `hydra-worker` (Tailscale-bound, detached); wait for `engine=`.
fn start_remote(ssh: &str, local_boot: &str, remote_boot: &str, log: &str) -> Result<(), String> {
    sh(&["scp", "-q", local_boot, &format!("{ssh}:{remote_boot}")])?;
    sh(&[
        "ssh", ssh,
        &format!("rm -f {log}; setsid env LD_LIBRARY_PATH={VM_LIBDIR} {VM_WORKER} {remote_boot} </dev/null >{log} 2>&1 & echo started; exit 0"),
    ])?;
    for _ in 0..160 {
        let out = sh(&["ssh", ssh, &format!("cat {log} 2>/dev/null || true")]).unwrap_or_default();
        if out.contains("engine=true") {
            return Ok(());
        }
        if out.contains("engine=false") {
            return Err(format!("{ssh}: worker came up engine=false (model/libs not linked)"));
        }
        if out.contains("panic") || out.contains("serve loop ended with error") {
            return Err(format!("{ssh}: worker failed:\n{out}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(format!("{ssh}: worker did not report engine= within 80s"))
}

/// A durability target on the Mac: persist each `BOUNDARY_COPY` to a real `BoundaryStore`, ack the
/// fdatasync'd frontier. Bound to the Mac's Tailscale IP so a VM stage can reach it.
fn spawn_mac_durability(cluster: &Cluster, name: &str, port: u16, path: std::path::PathBuf, keys: SessionKeys) -> Result<SocketAddr, String> {
    let id = cluster.issue(name).map_err(|e| e.to_string())?;
    let server_cfg = cluster.ca.server_config(&id).map_err(|e| e.to_string())?;
    let bind: SocketAddr = format!("{MAC_TS_IP}:{port}").parse().map_err(|e| format!("{e}"))?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = match TcpMtlsListener::bind_with_config(bind, server_cfg).await {
                Ok(l) => l,
                Err(e) => {
                    let _ = tx.send(Err(format!("bind durability {bind}: {e}")));
                    return;
                }
            };
            let addr = listener.local_addr().unwrap();
            let _ = tx.send(Ok(addr));
            let mut store = BoundaryStore::create(&path, [0x3D; 16], [0x33; 16]).expect("store");
            // This edge is served by exactly one forwarding stage (dur1←S1 local, dur2←S2 remote):
            // accept its one connection and persist every BOUNDARY_COPY it sends.
            let Ok(mut conn) = listener.accept().await else { return };
            while let Ok(frame) = conn.recv().await {
                if let Ok((view, Msg::BoundaryCopy { boundary_id, first_input_pos, chunk_id, activations })) = wire::decode(&frame.payload, &keys) {
                    let durable_through = store.append_boundary(boundary_id, first_input_pos, chunk_id, &activations).unwrap_or(-1);
                    let ack = wire::encode_durability_ack(&keys, view.epoch, boundary_id, durable_through, 0);
                    if conn.send(0, &ack).await.is_err() {
                        break;
                    }
                }
            }
        });
    });
    rx.recv().map_err(|e| e.to_string())?
}

fn sp_bootstrap(cluster: &Cluster, keys: &SessionKeys, k2: i32, n_ctx: i32) -> Bootstrap {
    let id = cluster.issue("sp").unwrap();
    Bootstrap {
        listen_addr: format!("{VM1_TS_IP}:{SP_PORT}"),
        device_name: "sp".into(),
        ca_cert_der: cluster.ca.ca_cert_der().as_ref().to_vec(),
        cert_chain_der: id.cert_chain.iter().map(|c| c.as_ref().to_vec()).collect(),
        key_pkcs8_der: id.key_pkcs8_der(),
        cfg: WorkerConfig {
            keys: keys.clone(), rank: 2, layer_first: k2, layer_last: -1, is_final: true,
            receives_tokens: false, epoch: 0, recovery_id: 0, model_path: Some(VM_MODEL.into()),
            n_gpu_layers: 0, n_ctx, sampler_config: Some(greedy()), recovery_start: false,
        },
        forwarding: None,
    }
}

fn s2_bootstrap(cluster: &Cluster, keys: &SessionKeys, k1: i32, k2: i32, n_ctx: i32, dur2: SocketAddr) -> Bootstrap {
    let id = cluster.issue("s2").unwrap();
    Bootstrap {
        listen_addr: format!("{VM2_TS_IP}:{S2_PORT}"),
        device_name: "s2".into(),
        ca_cert_der: cluster.ca.ca_cert_der().as_ref().to_vec(),
        cert_chain_der: id.cert_chain.iter().map(|c| c.as_ref().to_vec()).collect(),
        key_pkcs8_der: id.key_pkcs8_der(),
        cfg: WorkerConfig {
            keys: keys.clone(), rank: 1, layer_first: k1, layer_last: k2, is_final: false,
            receives_tokens: false, epoch: 0, recovery_id: 0, model_path: Some(VM_MODEL.into()),
            n_gpu_layers: 0, n_ctx, sampler_config: None, recovery_start: false,
        },
        forwarding: Some(ForwardingBootstrap {
            down_addr: format!("{VM1_TS_IP}:{SP_PORT}"),
            down_name: "sp".into(),
            dur_addr: format!("{MAC_TS_IP}:{}", dur2.port()),
            dur_name: "dur2".into(),
            require_durable: true,
            capacity: 64,
        }),
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mac_model = mac_model_path();
    if !std::path::Path::new(&mac_model).exists() {
        eprintln!("SKIP: Mac model not found at {mac_model}");
        return Ok(());
    }
    let (prompt, reference, k1, k2, n_layer, n_ctx) = {
        let model = Model::load(&mac_model, 0)?;
        let prompt: Vec<u32> = model.tokenize("The capital of France is").expect("tok").into_iter().map(|t| t as u32).collect();
        let n = 12usize;
        let reference = mac_unsplit_greedy(&model, &prompt, n);
        let (k1, k2) = split3(model.n_layer());
        let n_ctx = prompt.len() as i32 + n as i32 + 8;
        (prompt, reference, k1, k2, model.n_layer(), n_ctx)
    };
    let n = reference.len();
    println!("== hydra-3node-wan: chained direct FWD, Mac S1 [0,{k1}) → myVm-2 S2 [{k1},{k2}) → myVm-1 S_P [{k2},{n_layer}) ==");
    println!("   cap-weighted split 4.0/2.1/1.0; every leg WAN/Tailscale (VNet fast-path available, not exercised); cross-arch → mixed tier (spec I8)");

    let keys = SessionKeys::dev(0x3D);
    let cluster = Cluster::new()?;
    let (vm1, vm2) = (vm1_ssh(), vm2_ssh());

    // Mac-hosted durability endpoints (S1 edge local, S2 edge over WAN).
    let d1 = std::env::temp_dir().join("hydra-3node-s1.wal");
    let d2 = std::env::temp_dir().join("hydra-3node-s2.wal");
    let _ = std::fs::remove_file(&d1);
    let _ = std::fs::remove_file(&d2);
    let dur1 = spawn_mac_durability(&cluster, "dur1", DUR1_PORT, d1.clone(), keys.clone())?;
    let dur2 = spawn_mac_durability(&cluster, "dur2", DUR2_PORT, d2.clone(), keys.clone())?;
    println!("[mac] durability endpoints up: dur1={dur1} dur2={dur2}");

    // Start S_P (myVm-1) then S2 (myVm-2): S2 dials S_P + the Mac dur2 at startup, so both must be up.
    kill_remote(&vm1);
    kill_remote(&vm2);
    std::thread::sleep(std::time::Duration::from_millis(600));
    let sp_boot = std::env::temp_dir().join("hydra-3node-sp.boot");
    sp_bootstrap(&cluster, &keys, k2, n_ctx).write_to(sp_boot.to_str().unwrap())?;
    start_remote(&vm1, sp_boot.to_str().unwrap(), "/home/azureuser/hydra/sp-3n.boot", "/home/azureuser/hydra/sp-3n.log")?;
    println!("[vm1] S_P up on {VM1_TS_IP}:{SP_PORT} (engine=true)");
    let s2_boot = std::env::temp_dir().join("hydra-3node-s2.boot");
    s2_bootstrap(&cluster, &keys, k1, k2, n_ctx, dur2).write_to(s2_boot.to_str().unwrap())?;
    start_remote(&vm2, s2_boot.to_str().unwrap(), "/home/azureuser/hydra/s2-3n.boot", "/home/azureuser/hydra/s2-3n.log")?;
    println!("[vm2] S2 up on {VM2_TS_IP}:{S2_PORT} (engine=true, forwarding→S_P, durability→Mac)");

    // S1 local on the Mac: forwarding-durable, down = S2 (vm2), dur = dur1 (local).
    let s2_addr: SocketAddr = format!("{VM2_TS_IP}:{S2_PORT}").parse()?;
    let s1_id = cluster.issue("s1")?;
    let s1_cfg = WorkerConfig {
        keys: keys.clone(), rank: 0, layer_first: 0, layer_last: k1, is_final: false,
        receives_tokens: true, epoch: 0, recovery_id: 0, model_path: Some(mac_model.clone()),
        n_gpu_layers: 0, n_ctx, sampler_config: None, recovery_start: false,
    };
    let s1_down = std::sync::Arc::new(std::sync::Mutex::new((s2_addr, "s2".to_string())));
    let s1_addr = spawn_multiconn_forwarding_durable_endpoint(
        s1_cfg, cluster.ca.server_config(&s1_id)?,
        TcpMtls::from_config(cluster.ca.client_config(&cluster.issue("s1-down")?)?)?, s1_down, 8,
        TcpMtls::from_config(cluster.ca.client_config(&cluster.issue("s1-dur")?)?)?, dur1, "dur1",
        true, 64,
    );
    println!("[mac] S1 up on {s1_addr} (forwarding→S2, durability→dur1)");

    // Drive: coordinator → S1 (data) + S_P (control). S1→S2→S_P chaining is transparent.
    let connector = cluster.coordinator_connector()?;
    let sp_addr: SocketAddr = format!("{VM1_TS_IP}:{SP_PORT}").parse()?;
    let ep = Endpoints::new(s1_addr, "s1", sp_addr, "sp");
    let t = Instant::now();
    let got = run_direct_fwd_generation(&connector, &ep, &keys, &greedy(), &prompt, n).await.map_err(|e| format!("3-node gen: {e}"))?;
    let wall = t.elapsed();

    let agree = got.iter().zip(&reference).filter(|(a, b)| a == b).count();
    println!("\n   tokens (3-node chained direct FWD): {got:?}");
    println!("   tokens (Mac unsplit reference):     {reference:?}");
    println!("   argmax agreement: {agree}/{n} steps  (mixed tier: cross-arch drift may flip an argmax — spec I8)");
    println!("   perf: {n} tokens in {:.2?} → {:.2} tok/s  [WAN/Tailscale, arm64→x86-64→x86-64 — NOT a wired-LAN number]", wall, n as f64 / wall.as_secs_f64());

    // Durability wired across the chain: both Mac stores captured a strictly-increasing prefix.
    std::thread::sleep(std::time::Duration::from_millis(400));
    for (label, path) in [("S1→S2 (dur1)", &d1), ("S2→S_P (dur2)", &d2)] {
        match BoundaryStore::read(path) {
            Ok(b) => {
                let pos: Vec<i64> = b.iter().map(|x| x.first_input_pos).collect();
                let mono = pos.windows(2).all(|w| w[0] < w[1]);
                println!("   durability {label}: {} boundaries, strictly-increasing={mono}", pos.len());
            }
            Err(e) => println!("   durability {label}: read failed: {e}"),
        }
    }

    kill_remote(&vm1);
    kill_remote(&vm2);
    let ok = agree == n;
    println!("\n[done] 3-node chained direct-FWD pipeline over Tailscale (Mac→myVm-2→myVm-1).");
    println!("   THREENODE_WAN_{}: {agree}/{n} argmax agreement vs Mac unsplit; durable copies on both edges", if ok { "OK" } else { "CHECK" });
    if !ok {
        return Err(format!("3-node WAN run argmax agreement {agree}/{n} < {n}").into());
    }
    Ok(())
}
