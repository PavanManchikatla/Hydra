//! The `--local-pair` harness (BLUEPRINT M2 sub-slice B).
//!
//! Two stage workers as **real TCP+mTLS endpoints on localhost** — the *same* handshake, framing,
//! and code path as multi-machine; only the wire is a loopback. A coordinator drives a
//! **teacher-forced NO_SAMPLE** prefill through them and checks the split pipeline reproduces the
//! unsplit model's final logits **bit-exactly** (the regression anchor before any sampler/API):
//! per position, S1 (layers `[0,k)`) extracts the boundary residual, it is serialized as a `FWD`
//! `Tensor`(f32), transmitted over mTLS, and injected into S2 (layers `[k,end)`), which produces
//! the unsampled logits. The witness is the BLAKE3 digest of those f32 logits.
//!
//! Two worker shapes are provided from day one:
//!   * [`spawn_endpoint`] — an **in-process** endpoint on its own thread (the deterministic,
//!     CI-gateable anchor; the engine context is non-`Send` so it stays on that thread);
//!   * [`SubprocessWorker`] — a **real `hydra-worker` OS process** with a literal `kill -9`
//!     (`child.kill()` → SIGKILL) + restart switch, so the later D1 recovery DoD runs against an
//!     existing kill-switch.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;

use hydra_engine_sys::{Model, ENGINE_AVAILABLE};
use hydra_transport::tcp_mtls::{TcpMtls, TcpMtlsListener};
use hydra_transport::{ClusterCa, DeviceIdentity};

use crate::bootstrap::Bootstrap;
use crate::wire::{self, Msg, SessionKeys};
use crate::worker::{serve_conn, Worker, WorkerConfig};

/// A cluster CA + issued identities for the coordinator and workers (dev/local-pair pairing).
pub struct Cluster {
    pub ca: ClusterCa,
    pub coordinator: DeviceIdentity,
}

impl Cluster {
    pub fn new() -> Result<Cluster, hydra_transport::TransportError> {
        let ca = ClusterCa::new()?;
        let coordinator = ca.issue("coordinator")?;
        Ok(Cluster { ca, coordinator })
    }

    pub fn issue(&self, name: &str) -> Result<DeviceIdentity, hydra_transport::TransportError> {
        self.ca.issue(name)
    }

    /// A connector presenting the coordinator identity, trusting this cluster's CA.
    pub fn coordinator_connector(&self) -> Result<TcpMtls, hydra_transport::TransportError> {
        TcpMtls::from_config(self.ca.client_config(&self.coordinator)?)
    }
}

/// Spawn an **in-process** worker endpoint on its own thread; return the bound loopback address.
/// The endpoint accepts connections in a loop (so a coordinator may reconnect) until the process
/// exits. The worker owns its (non-`Send`) engine context on this thread.
pub fn spawn_endpoint(
    cfg: WorkerConfig,
    server_cfg: rustls::ServerConfig,
) -> SocketAddr {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = TcpMtlsListener::bind_with_config("127.0.0.1:0".parse().unwrap(), server_cfg)
                .await
                .expect("bind endpoint");
            tx.send(listener.local_addr().unwrap()).unwrap();
            let mut worker = Worker::new(cfg).expect("worker");
            while let Ok(mut conn) = listener.accept().await {
                let _ = serve_conn(&mut worker, &mut conn).await;
            }
        });
    });
    rx.recv().expect("endpoint addr")
}

/// The unsplit reference: apply `tokens` **one at a time** (matching the pipeline's per-position
/// batching) to the full model and digest the final unsampled logits. Bit-exactness of the split
/// pipeline is defined against *this* — identical batching on both sides, so the only thing under
/// test is the split boundary (spike Check C, now over the wire).
pub fn golden_digest(model: &Model, tokens: &[u32]) -> Result<[u8; 32], hydra_engine_sys::EngineError> {
    let n_ctx = tokens.len() as i32 + 8;
    let mut ctx = model.context(0, -1, false, n_ctx, n_ctx)?;
    let mut last = Vec::new();
    for (pos, &tok) in tokens.iter().enumerate() {
        ctx.apply_tokens(&[tok as i32], pos as i32, None)?;
        last = ctx.logits(0)?; // batch-relative index 0: the single position just applied
    }
    Ok(*blake3::hash(&wire::f32_to_bytes_le(&last)).as_bytes())
}

/// Drive the teacher-forced NO_SAMPLE prefill through two connected workers and return the digest
/// of the final position's logits (as reported by S2's `APPLIED_ACK`). `keys` must match the
/// workers' session identity (F1).
pub async fn run_teacher_forced_pipeline(
    connector: &TcpMtls,
    s1_addr: SocketAddr,
    s1_name: &str,
    s2_addr: SocketAddr,
    s2_name: &str,
    keys: &SessionKeys,
    tokens: &[u32],
) -> Result<[u8; 32], String> {
    let mut c1 = connector.connect(s1_addr, s1_name).await.map_err(|e| format!("connect s1: {e}"))?;
    let mut c2 = connector.connect(s2_addr, s2_name).await.map_err(|e| format!("connect s2: {e}"))?;

    let mut last_digest = [0u8; 32];
    for (pos, &tok) in tokens.iter().enumerate() {
        // C -> S1: APPLY_TOKEN (NO_SAMPLE, teacher-forced).
        c1.send(0, &wire::encode_apply_token(keys, 0, pos as i64, tok, true))
            .await
            .map_err(|e| format!("send s1: {e}"))?;
        let f1 = c1.recv().await.map_err(|e| format!("recv s1: {e}"))?;
        let boundary = match wire::decode(&f1.payload, keys).map_err(|e| format!("decode s1: {e}"))?.1 {
            Msg::Fwd { activations, .. } => activations,
            other => return Err(format!("s1 pos {pos}: expected FWD, got {other:?}")),
        };

        // C relays S1's boundary to S2 as FWD; S2 injects it and produces logits.
        c2.send(0, &wire::encode_fwd(keys, 0, pos as i64, true, &boundary))
            .await
            .map_err(|e| format!("send s2: {e}"))?;
        let f2 = c2.recv().await.map_err(|e| format!("recv s2: {e}"))?;
        match wire::decode(&f2.payload, keys).map_err(|e| format!("decode s2: {e}"))?.1 {
            Msg::AppliedAck { cumulative_input_pos, output_checksum } => {
                if cumulative_input_pos != pos as i64 {
                    return Err(format!("s2 pos mismatch: {cumulative_input_pos} != {pos}"));
                }
                last_digest = output_checksum.try_into().map_err(|_| "s2 digest not 32 bytes".to_string())?;
            }
            other => return Err(format!("s2 pos {pos}: expected APPLIED_ACK, got {other:?}")),
        }
    }
    Ok(last_digest)
}

/// Load the small dev model (or return `None` if the engine/model is unavailable — both dev-env
/// artifacts). Honors `HYDRA_TEST_MODEL`.
pub fn dev_model_path() -> Option<String> {
    if !ENGINE_AVAILABLE {
        return None;
    }
    let default = concat!(env!("CARGO_MANIFEST_DIR"), "/../../models/qwen2.5-0.5b-instruct-fp16.gguf");
    std::env::var("HYDRA_TEST_MODEL")
        .ok()
        .filter(|p| std::path::Path::new(p).exists())
        .or_else(|| std::path::Path::new(default).exists().then(|| default.to_string()))
}

// ------------------------- real subprocess worker (literal kill -9) -------------------------

static BOOT_SEQ: AtomicU32 = AtomicU32::new(0);

/// A real `hydra-worker` **OS process**, provisioned via a bootstrap file, that can be `kill -9`'d
/// and restarted — the dev `--local-pair` kill-switch (the containerized-CI `docker kill` path is a
/// later M2 slice; see the amended M2 DoD).
pub struct SubprocessWorker {
    binary: String,
    boot_path: String,
    child: Child,
    /// The address the child is currently listening on (re-read on each (re)start).
    pub addr: SocketAddr,
}

impl SubprocessWorker {
    /// Spawn `binary` (path to the built `hydra-worker`) provisioned by `boot`, and wait until it
    /// prints its bound address.
    pub fn spawn(binary: &str, boot: &Bootstrap) -> std::io::Result<SubprocessWorker> {
        let seq = BOOT_SEQ.fetch_add(1, Ordering::Relaxed);
        let boot_path = std::env::temp_dir()
            .join(format!("hydra-worker-{}-{seq}.boot", std::process::id()))
            .to_string_lossy()
            .into_owned();
        boot.write_to(&boot_path)?;
        let (child, addr) = Self::launch(binary, &boot_path)?;
        Ok(SubprocessWorker { binary: binary.to_string(), boot_path, child, addr })
    }

    fn launch(binary: &str, boot_path: &str) -> std::io::Result<(Child, SocketAddr)> {
        let mut child = Command::new(binary).arg(boot_path).stdout(Stdio::piped()).spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "worker exited before listening"));
            }
            if let Some(rest) = line.trim().strip_prefix("HYDRA_WORKER_LISTENING ") {
                let addr: SocketAddr = rest.parse().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e}")))?;
                return Ok((child, addr));
            }
        }
    }

    /// `kill -9` the worker process (SIGKILL) and reap it.
    pub fn kill9(&mut self) -> std::io::Result<()> {
        self.child.kill()?;
        let _ = self.child.wait()?;
        Ok(())
    }

    /// Restart the (killed) worker from the same bootstrap; it binds a fresh port (advertised on
    /// stdout) and `self.addr` is updated.
    pub fn restart(&mut self) -> std::io::Result<()> {
        let (child, addr) = Self::launch(&self.binary, &self.boot_path)?;
        self.child = child;
        self.addr = addr;
        Ok(())
    }
}

impl Drop for SubprocessWorker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.boot_path);
    }
}
