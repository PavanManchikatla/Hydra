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
use crate::sampler::SamplingConfig;
use crate::wire::{self, Msg, SessionKeys};
use crate::worker::{
    serve_conn, serve_conn_forwarding, serve_conn_forwarding_relink, serve_multi_conn, shared, DownTarget, Worker,
    WorkerConfig, INITIAL_CHECKPOINT_ID,
};

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

/// Spawn an **in-process** endpoint that serves **concurrent** inbound connections against one shared
/// `Worker` (the multi-connection serve loop, P1·1a). Unlike [`spawn_endpoint`] (one connection served
/// to completion before the next is accepted), this lets a stage serve its upstream `FWD` **and** the
/// coordinator's control connection at the same time (the seam-3 requirement for a direct-FWD
/// pipeline with a coordinator-controlled S_P). Returns the bound loopback address.
pub fn spawn_multiconn_endpoint(cfg: WorkerConfig, server_cfg: rustls::ServerConfig) -> SocketAddr {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            let listener = TcpMtlsListener::bind_with_config("127.0.0.1:0".parse().unwrap(), server_cfg)
                .await
                .expect("bind endpoint");
            tx.send(listener.local_addr().unwrap()).unwrap();
            let worker = shared(Worker::new(cfg).expect("worker"));
            let _ = serve_multi_conn(worker, listener).await;
        });
    });
    rx.recv().expect("endpoint addr")
}

/// Spawn a **forwarding** S1 endpoint: it connects **out** to the downstream S2 once at startup, then
/// serves the coordinator with [`serve_conn_forwarding`] so each `FWD` boundary travels **S1→S2
/// directly** (worker→worker), never relayed through the coordinator. Returns S1's bound address.
/// ([`DownTarget`] is the shared, updatable downstream address used by the re-linking variant below.)
pub fn spawn_forwarding_endpoint(
    cfg: WorkerConfig,
    server_cfg: rustls::ServerConfig,
    down_connector: TcpMtls,
    down: DownTarget,
) -> SocketAddr {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = TcpMtlsListener::bind_with_config("127.0.0.1:0".parse().unwrap(), server_cfg).await.expect("bind s1");
            tx.send(listener.local_addr().unwrap()).unwrap();
            let mut worker = Worker::new(cfg).expect("worker");
            while let Ok(mut up) = listener.accept().await {
                // (Re)connect the downstream S1→S2 link to the CURRENT target — so a coordinator
                // reconnect after a downstream replacement re-links the survivor to the new peer.
                let (addr, name) = down.lock().unwrap().clone();
                let mut dc = match down_connector.connect(addr, &name).await {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let _ = serve_conn_forwarding(&mut worker, &mut up, &mut dc).await;
            }
        });
    });
    rx.recv().expect("forwarding endpoint addr")
}

/// Spawn a **re-linking** forwarding endpoint (P1·1a): each upstream (coordinator) connection is served
/// with [`serve_conn_forwarding_relink`], whose downstream link is **reconnectable** from the shared
/// `down` target. If the downstream stage is killed and replaced mid-session, this survivor keeps
/// serving its upstream on the same connection and re-links to the replacement on its next forward —
/// the direct-FWD recovery re-link. `down_connector` presents this stage's own identity (S1 dials S2).
/// Returns S1's bound address.
pub fn spawn_forwarding_endpoint_relink(
    cfg: WorkerConfig,
    server_cfg: rustls::ServerConfig,
    down_connector: TcpMtls,
    down: DownTarget,
    relink_retries: usize,
) -> SocketAddr {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = TcpMtlsListener::bind_with_config("127.0.0.1:0".parse().unwrap(), server_cfg).await.expect("bind s1");
            tx.send(listener.local_addr().unwrap()).unwrap();
            let mut worker = Worker::new(cfg).expect("worker");
            while let Ok(mut up) = listener.accept().await {
                let _ = serve_conn_forwarding_relink(&mut worker, &mut up, &down_connector, &down, relink_retries).await;
            }
        });
    });
    rx.recv().expect("forwarding endpoint addr")
}

/// Drive the teacher-forced NO_SAMPLE prefill with **worker→worker direct FWD**: the coordinator
/// talks **only to S1**; S1 forwards each boundary straight to S2 and relays S2's `APPLIED_ACK` back.
/// Returns the final position's logits digest (bit-exact anchor, now without the coordinator relay).
pub async fn run_direct_fwd_pipeline(connector: &TcpMtls, s1_addr: SocketAddr, s1_name: &str, keys: &SessionKeys, tokens: &[u32]) -> Result<[u8; 32], String> {
    let mut c = connector.connect(s1_addr, s1_name).await.map_err(|e| format!("connect s1: {e}"))?;
    let mut last_digest = [0u8; 32];
    for (pos, &tok) in tokens.iter().enumerate() {
        c.send(0, &wire::encode_apply_token(keys, 0, pos as i64, tok, true)).await.map_err(|e| format!("send s1: {e}"))?;
        let r = c.recv().await.map_err(|e| format!("recv s1: {e}"))?;
        match wire::decode(&r.payload, keys).map_err(|e| format!("decode: {e}"))?.1 {
            Msg::AppliedAck { cumulative_input_pos, output_checksum } => {
                if cumulative_input_pos != pos as i64 {
                    return Err(format!("pos mismatch: {cumulative_input_pos} != {pos}"));
                }
                last_digest = output_checksum.try_into().map_err(|_| "digest not 32 bytes".to_string())?;
            }
            other => return Err(format!("pos {pos}: expected APPLIED_ACK (relayed from S2), got {other:?}")),
        }
    }
    Ok(last_digest)
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

/// The two pipeline endpoints (address + certificate identity) a coordinator connects to.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub s1_addr: SocketAddr,
    pub s1_name: String,
    pub s2_addr: SocketAddr,
    pub s2_name: String,
}

impl Endpoints {
    pub fn new(s1_addr: SocketAddr, s1_name: &str, s2_addr: SocketAddr, s2_name: &str) -> Self {
        Endpoints { s1_addr, s1_name: s1_name.to_string(), s2_addr, s2_name: s2_name.to_string() }
    }
}

/// Drive the teacher-forced NO_SAMPLE prefill through two connected workers and return the digest
/// of the final position's logits (as reported by S2's `APPLIED_ACK`). `keys` must match the
/// workers' session identity (F1).
pub async fn run_teacher_forced_pipeline(
    connector: &TcpMtls,
    ep: &Endpoints,
    keys: &SessionKeys,
    tokens: &[u32],
) -> Result<[u8; 32], String> {
    let mut c1 = connector.connect(ep.s1_addr, &ep.s1_name).await.map_err(|e| format!("connect s1: {e}"))?;
    let mut c2 = connector.connect(ep.s2_addr, &ep.s2_name).await.map_err(|e| format!("connect s2: {e}"))?;

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

/// The unsplit greedy next token: apply `tokens` one-at-a-time to the full model and take the
/// argmax of the final logits — the exact-tier reference for greedy sampling across the pipeline
/// (test (a): sampling at the end of a teacher-forced run stays bit-exact vs unsplit).
pub fn golden_next_token(model: &Model, tokens: &[u32]) -> Result<u32, hydra_engine_sys::EngineError> {
    let n_ctx = tokens.len() as i32 + 8;
    let mut ctx = model.context(0, -1, false, n_ctx, n_ctx)?;
    let mut last = Vec::new();
    for (pos, &tok) in tokens.iter().enumerate() {
        ctx.apply_tokens(&[tok as i32], pos as i32, None)?;
        last = ctx.logits(0)?;
    }
    let mut bi = 0usize;
    for i in 1..last.len() {
        if last[i] > last[bi] {
            bi = i;
        }
    }
    Ok(bi as u32)
}

async fn prefill<S>(c1: &mut hydra_transport::framed::Conn<S>, c2: &mut hydra_transport::framed::Conn<S>, keys: &SessionKeys, tokens: &[u32]) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    for (pos, &tok) in tokens.iter().enumerate() {
        c1.send(0, &wire::encode_apply_token(keys, 0, pos as i64, tok, true)).await.map_err(|e| format!("prefill s1 send: {e}"))?;
        let boundary = expect_fwd(c1, keys, pos as i64).await?;
        c2.send(0, &wire::encode_fwd(keys, 0, pos as i64, true, &boundary)).await.map_err(|e| format!("prefill s2 send: {e}"))?;
        expect_applied_ack(c2, keys).await?;
    }
    Ok(())
}

async fn expect_fwd<S>(c: &mut hydra_transport::framed::Conn<S>, keys: &SessionKeys, pos: i64) -> Result<Vec<f32>, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let f = c.recv().await.map_err(|e| format!("recv fwd: {e}"))?;
    match wire::decode(&f.payload, keys).map_err(|e| format!("decode fwd: {e}"))?.1 {
        Msg::Fwd { activations, .. } => Ok(activations),
        other => Err(format!("pos {pos}: expected FWD, got {other:?}")),
    }
}

async fn expect_applied_ack<S>(c: &mut hydra_transport::framed::Conn<S>, keys: &SessionKeys) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let a = c.recv().await.map_err(|e| format!("recv ack: {e}"))?;
    match wire::decode(&a.payload, keys).map_err(|e| format!("decode ack: {e}"))?.1 {
        Msg::AppliedAck { .. } => Ok(()),
        other => Err(format!("expected APPLIED_ACK, got {other:?}")),
    }
}

/// Drive prompt prefill (NO_SAMPLE) then `n_steps` autoregressive sample steps through the two-worker
/// pipeline, returning the generated token sequence. The coordinator owns **no** sampler state — it
/// only issues `SAMPLE_NEXT` and feeds sampled tokens back (spec §1.4 ownership boundary); every
/// snapshot is produced at S_P.
pub async fn run_generation(
    connector: &TcpMtls,
    ep: &Endpoints,
    keys: &SessionKeys,
    config: &SamplingConfig,
    prompt_tokens: &[u32],
    n_steps: usize,
) -> Result<Vec<u32>, String> {
    let mut c1 = connector.connect(ep.s1_addr, &ep.s1_name).await.map_err(|e| format!("connect s1: {e}"))?;
    let mut c2 = connector.connect(ep.s2_addr, &ep.s2_name).await.map_err(|e| format!("connect s2: {e}"))?;
    let cfg_hash = config.hash();

    prefill(&mut c1, &mut c2, keys, prompt_tokens).await?;

    let mut out = Vec::with_capacity(n_steps);
    let mut input_pos = prompt_tokens.len() as i64;
    for step in 0..n_steps {
        c2.send(0, &wire::encode_sample_next(keys, 0, step as i64, &cfg_hash, INITIAL_CHECKPOINT_ID))
            .await
            .map_err(|e| format!("send SAMPLE_NEXT: {e}"))?;
        let s = c2.recv().await.map_err(|e| format!("recv SAMPLED: {e}"))?;
        let token = match wire::decode(&s.payload, keys).map_err(|e| format!("decode SAMPLED: {e}"))?.1 {
            Msg::Sampled { token_id, .. } => token_id,
            Msg::Err { code } => return Err(format!("sampler error code {code} at step {step}")),
            other => return Err(format!("step {step}: expected SAMPLED, got {other:?}")),
        };
        out.push(token);

        // Feed the sampled token back autoregressively (except after the final step).
        if step + 1 < n_steps {
            c1.send(0, &wire::encode_apply_token(keys, 0, input_pos, token, false)).await.map_err(|e| format!("feedback s1: {e}"))?;
            let boundary = expect_fwd(&mut c1, keys, input_pos).await?;
            c2.send(0, &wire::encode_fwd(keys, 0, input_pos, false, &boundary)).await.map_err(|e| format!("feedback s2: {e}"))?;
            expect_applied_ack(&mut c2, keys).await?;
            input_pos += 1;
        }
    }
    Ok(out)
}

/// Prefill, then issue `SAMPLE_NEXT` for output position 0 **twice**, returning both decoded
/// `SAMPLED` replies — the directed idempotence probe (I14): the duplicate must be byte-identical
/// (served from the SAMPLED cache) and the RNG must not have advanced.
pub async fn sample_next_twice(
    connector: &TcpMtls,
    ep: &Endpoints,
    keys: &SessionKeys,
    config: &SamplingConfig,
    prompt_tokens: &[u32],
) -> Result<(Msg, Msg), String> {
    let mut c1 = connector.connect(ep.s1_addr, &ep.s1_name).await.map_err(|e| format!("connect s1: {e}"))?;
    let mut c2 = connector.connect(ep.s2_addr, &ep.s2_name).await.map_err(|e| format!("connect s2: {e}"))?;
    prefill(&mut c1, &mut c2, keys, prompt_tokens).await?;
    let cfg_hash = config.hash();
    let fire = || wire::encode_sample_next(keys, 0, 0, &cfg_hash, INITIAL_CHECKPOINT_ID);
    c2.send(0, &fire()).await.map_err(|e| format!("send SAMPLE_NEXT #1: {e}"))?;
    let first = wire::decode(&c2.recv().await.map_err(|e| format!("recv #1: {e}"))?.payload, keys).map_err(|e| format!("decode #1: {e}"))?.1;
    c2.send(0, &fire()).await.map_err(|e| format!("send SAMPLE_NEXT #2: {e}"))?;
    let second = wire::decode(&c2.recv().await.map_err(|e| format!("recv #2: {e}"))?.payload, keys).map_err(|e| format!("decode #2: {e}"))?.1;
    Ok((first, second))
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
