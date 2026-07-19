//! `hydra-2node-ci` — the **containerized two-node recovery runner** (M2 FWD slice, seam 4).
//!
//! Orchestrates two `hydra-worker` **containers** over a real docker network with real TCP+mTLS
//! between the coordinator and each worker, and uses **`docker kill` as the kill −9 mechanism**. It
//! drives the real control-plane two-node recovery through the DST-tested `hydra-state` stage SM:
//!
//!   1. `docker run` S1 + S_P (control-plane workers, ports published to 127.0.0.1);
//!   2. activate both (COMMIT → FINALIZE → ACTIVE_FINAL);
//!   3. **`docker kill` S_P** (SIGKILL);
//!   4. survivor **S1 takes `BEGIN_RECOVERY` Case A** (freeze, epoch 0→1);
//!   5. `docker run` a **replacement S_P** (recovery-start) → Case A → catch-up → recovery activation.
//!
//! On full success it prints the single semantic line `CONTAINER_2NODE_RECOVERY_OK`; the workflow is
//! GREEN only if that line is present (rule-16 spirit — never map a container exit to pass). Any
//! failed step panics with the trace. Engine-gated byte-identical recovery is proven locally (seam
//! 3); this proves the two-node kill/recover machinery + real mTLS across containers, in CI.
//!
//! Requires the docker CLI + a built image (`HYDRA_WORKER_IMAGE`, default `hydra-worker:ci`).

use std::process::Command;
use std::time::Duration;

use hydra_state::{ActivationKind, ActivationTuple};
use hydra_transport::framed::Conn;
use hydra_worker::pair::Cluster;
use hydra_worker::wire::{self, Msg, SessionKeys};
use hydra_worker::worker::WorkerConfig;
use hydra_worker::Bootstrap;
use tokio::io::{AsyncRead, AsyncWrite};

const CONTAINER_PORT: u16 = 9000;

fn image() -> String {
    std::env::var("HYDRA_WORKER_IMAGE").unwrap_or_else(|_| "hydra-worker:ci".to_string())
}

fn docker(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker").args(args).output().map_err(|e| format!("docker {args:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!("docker {args:?} failed: {}", String::from_utf8_lossy(&out.stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A `hydra-worker` OS **container** with a `docker kill` (SIGKILL) switch.
struct ContainerWorker {
    name: String,
    host_port: u16,
}

impl ContainerWorker {
    /// `docker run -d` the worker with `boot` mounted; publish its port to 127.0.0.1; wait for it to
    /// accept a connection.
    fn spawn(cluster: &Cluster, name: &str, boot_path: &str) -> Result<ContainerWorker, String> {
        let _ = docker(&["rm", "-f", name]); // idempotent
        // Mount the bootstrap to a root-level FILE path (not `/boot` — that is an existing directory
        // in the Debian base image, and a file→directory bind mount fails).
        docker(&[
            "run", "-d", "--name", name,
            "-v", &format!("{boot_path}:/hydra.boot:ro"),
            "-p", &format!("127.0.0.1:0:{CONTAINER_PORT}"),
            &image(), "/hydra.boot",
        ])?;
        // Read the published host port (`docker port` may print several lines — take the first).
        let mapping = docker(&["port", name, &CONTAINER_PORT.to_string()])?; // e.g. "127.0.0.1:49153"
        let host_port: u16 = mapping.lines().next().unwrap_or("").rsplit(':').next().and_then(|p| p.trim().parse().ok())
            .ok_or_else(|| format!("could not parse published port from {mapping:?}"))?;
        let _ = cluster;
        Ok(ContainerWorker { name: name.to_string(), host_port })
    }

    fn addr(&self) -> std::net::SocketAddr {
        format!("127.0.0.1:{}", self.host_port).parse().unwrap()
    }

    /// `docker kill` (SIGKILL) — the kill −9 mechanism.
    fn kill9(&self) -> Result<(), String> {
        docker(&["kill", "--signal", "KILL", &self.name]).map(|_| ())
    }
}

impl Drop for ContainerWorker {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", &self.name]);
    }
}

fn write_boot(cluster: &Cluster, name: &str, dir: &std::path::Path, recovery_start: bool) -> String {
    let id = cluster.issue(name).unwrap();
    let boot = Bootstrap {
        // Container-internal bind; the runner reaches it only via the 127.0.0.1-published port
        // (docker's network namespace is the isolation boundary; never exposed on a host interface).
        listen_addr: format!("0.0.0.0:{CONTAINER_PORT}"),
        device_name: name.to_string(),
        ca_cert_der: cluster.ca.ca_cert_der().as_ref().to_vec(),
        cert_chain_der: id.cert_chain.iter().map(|c| c.as_ref().to_vec()).collect(),
        key_pkcs8_der: id.key_pkcs8_der(),
        cfg: WorkerConfig {
            keys: SessionKeys::dev(0xC1), rank: 0, layer_first: 0, layer_last: -1, is_final: true,
            receives_tokens: true, epoch: 0, recovery_id: if recovery_start { 1 } else { 0 },
            model_path: None, n_gpu_layers: 0, n_ctx: 64, sampler_config: None, recovery_start,
        },
    };
    let path = dir.join(format!("{name}.boot")).to_string_lossy().into_owned();
    boot.write_to(&path).unwrap();
    path
}

async fn connect_retry(connector: &hydra_transport::tcp_mtls::TcpMtls, addr: std::net::SocketAddr, name: &str) -> hydra_transport::tcp_mtls::ClientConn {
    for _ in 0..100 {
        if let Ok(c) = connector.connect(addr, name).await {
            return c;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("could not connect to {name} at {addr} within 20s");
}

async fn activate<S: AsyncRead + AsyncWrite + Unpin>(c: &mut Conn<S>, keys: &SessionKeys, kind: ActivationKind, epoch: u32, rid: u32) {
    let t = ActivationTuple { kind, epoch, recovery_id: rid, attempt: 0, sampler_checkpoint_id: 0 };
    c.send(0, &wire::encode_commit_activation(keys, &t, 1)).await.unwrap();
    assert!(matches!(wire::decode(&c.recv().await.unwrap().payload, keys).unwrap().1, Msg::ActivationCommitted(_)), "COMMIT_ACTIVATION");
    c.send(0, &wire::encode_finalize_activation(keys, &t, 1)).await.unwrap();
    assert!(matches!(wire::decode(&c.recv().await.unwrap().payload, keys).unwrap().1, Msg::ActivationFinalized), "FINALIZE_ACTIVATION");
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Preflight: docker present.
    docker(&["version", "--format", "{{.Server.Version}}"]).expect("docker daemon must be available");
    let keys = SessionKeys::dev(0xC1);
    let cluster = Cluster::new().unwrap();
    let connector = cluster.coordinator_connector().unwrap();
    let dir = std::env::temp_dir().join(format!("hydra-2node-ci-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let s1_boot = write_boot(&cluster, "s1", &dir, false);
    let sp_boot = write_boot(&cluster, "sp", &dir, false);
    let spr_boot = write_boot(&cluster, "sp-recover", &dir, true);

    // 1. two worker containers.
    let s1 = ContainerWorker::spawn(&cluster, "hydra-ci-s1", &s1_boot).expect("run s1 container");
    let sp = ContainerWorker::spawn(&cluster, "hydra-ci-sp", &sp_boot).expect("run sp container");
    let mut c1 = connect_retry(&connector, s1.addr(), "s1").await;
    let mut cp = connect_retry(&connector, sp.addr(), "sp").await;

    // 2. activate both across the real container mTLS.
    activate(&mut c1, &keys, ActivationKind::Initial, 0, 0).await;
    activate(&mut cp, &keys, ActivationKind::Initial, 0, 0).await;
    eprintln!("[ci] both workers ACTIVE_FINAL over real container mTLS");

    // 3. docker kill S_P (kill -9).
    drop(cp);
    sp.kill9().expect("docker kill sp");
    eprintln!("[ci] docker kill sp (SIGKILL)");

    // 4. survivor S1 takes BEGIN_RECOVERY Case A (freeze, epoch 0->1).
    c1.send(0, &wire::encode_begin_recovery(&keys, 0, 1, 1, 0)).await.unwrap();
    assert!(matches!(wire::decode(&c1.recv().await.unwrap().payload, &keys).unwrap().1, Msg::RecoveryAck { .. }), "survivor S1 Case A freeze");
    eprintln!("[ci] survivor S1 froze (Case A)");

    // 5. replacement S_P container: Case A -> catch-up -> recovery activation.
    let rsp = ContainerWorker::spawn(&cluster, "hydra-ci-sp2", &spr_boot).expect("run replacement sp container");
    let mut rcp = connect_retry(&connector, rsp.addr(), "sp-recover").await;
    rcp.send(0, &wire::encode_begin_recovery(&keys, 0, 1, 1, 0)).await.unwrap();
    assert!(matches!(wire::decode(&rcp.recv().await.unwrap().payload, &keys).unwrap().1, Msg::RecoveryAck { .. }), "replacement S_P Case A");
    rcp.send(0, &wire::encode_catch_up_context(&keys, 1, 1, 3)).await.unwrap();
    assert!(matches!(wire::decode(&rcp.recv().await.unwrap().payload, &keys).unwrap().1, Msg::CatchUpReady { .. }), "replacement S_P catch-up");
    activate(&mut rcp, &keys, ActivationKind::Recovery, 1, 1).await;
    eprintln!("[ci] replacement S_P recovered + ACTIVE_FINAL (Case A -> catch-up -> activation)");

    // Success — the workflow gates on this exact line (rule-16 spirit).
    println!("CONTAINER_2NODE_RECOVERY_OK");
    let _ = std::fs::remove_dir_all(&dir);
    drop((s1, rsp));
}
