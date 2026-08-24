//! **`hydra-coordinator` — M4·0: the first binary that runs a coordinator.**
//!
//! # What did not exist before this file
//!
//! Every end-to-end demonstration this project has ever run was driven by a **worker-side demo
//! binary** (`hydra-wan`, `hydra-3node-*`, `hydra-2node-ci`, `hydra-local-pair`) that hand-rolled
//! the activation transaction: `send(COMMIT_ACTIVATION)` then `send(FINALIZE_ACTIVATION)`, with
//! `activation_attempt_id` hard-coded to `1`, no ack collection, no `ACTIVATION_COMMIT_INTENT`, no
//! `ACTIVATION_COMPLETE`, no abort path and no unservable path. `hydra_state::Coordinator` — the
//! machine TLC checks — was constructed in exactly one place in the workspace: the simulator.
//!
//! So the coordinator state machine was **verified but not deployed**. This binary is what makes
//! the verified machine the one that runs:
//!
//! * it mints a session identity from the **system CSPRNG** (audit M12), not from a seed byte;
//! * it creates a commit stream and a control WAL **bound to that session** (audit M8);
//! * it drives `hydra_state::Coordinator` through [`ActivationDriver`], which executes effects and
//!   nothing else — no protocol decision is taken outside the SM (BLUEPRINT §2);
//! * it serves the OpenAI-compatible API **over TLS** with the cluster's own material (audit H21).
//!
//! # Usage
//!
//! ```text
//! hydra-coordinator --bootstrap <path> --api-addr 127.0.0.1:8443 --data-dir <dir>
//! ```
//!
//! # Identity, and why it is not the worker's bootstrap blob
//!
//! A worker's `Bootstrap` embeds a `WorkerConfig` (layer range, model path, is_final) that a
//! coordinator has no use for, and it lives in `hydra-worker`, which the coordinator cannot depend
//! on. **Standing rule 21 says to check whether the graph permits the correct placement before
//! forcing one** — and here the honest answer is that a *coordinator* provisioning artifact is a
//! different artifact, and minting it is **M4·2's pairing slice**, not this one.
//!
//! So this seam takes identity explicitly: `--dev` mints a throwaway cluster CA and identity for a
//! single-machine run, and `--ca-cert/--cert/--key` take DER files for anything else. When M4·2
//! lands, it replaces the flags with the paired artifact and this comment with a pointer to it.

use std::net::SocketAddr;
use std::sync::Arc;

use hydra_coordinator::commit_stream::WalFenceCtx;
use hydra_coordinator::{ApiAuth, AppState, CommitStream};

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };

    let api_addr: SocketAddr = flag("--api-addr")
        .unwrap_or_else(|| "127.0.0.1:8443".to_string())
        .parse()
        .map_err(|e| format!("--api-addr: {e}"))?;
    let data_dir = flag("--data-dir").unwrap_or_else(|| "./hydra-data".to_string());
    // `--dev` is the single-machine path: a throwaway CA, minted here. Anything else must supply
    // real material, because a coordinator that silently invents its own trust anchor is exactly
    // the C1 shape (a self-attestation presented as verification).
    let dev = args.iter().any(|a| a == "--dev");
    // **The normal path: use the material `hydra-cli pair` wrote.**
    //
    // Found by RUNNING the documented quickstart: `--dev` mints its OWN throwaway CA, unrelated to
    // the one pairing just created — so a client trusting the paired CA could not connect and the
    // quickstart's last step failed. A coordinator started after pairing must use the paired
    // cluster's identity, or the pairing meant nothing.
    let boot_path = if let Some(dir) = flag("--pairing-dir") {
        dir
    } else if dev {
        String::from("--dev")
    } else {
        return Err("supply --pairing-dir <dir> (what `hydra-cli pair --out <dir>` wrote), or --dev \
                    for a throwaway single-machine identity. A coordinator does not invent its own \
                    trust anchor (audit C1's shape)"
            .into());
    };

    // The API token is REQUIRED and must be non-empty (audit H15). Reading it from the environment
    // rather than argv keeps it out of `ps`.
    let token = std::env::var("HYDRA_API_TOKEN")
        .map_err(|_| "HYDRA_API_TOKEN must be set: the API requires a bearer token even on a trusted LAN (audit H15)".to_string())?;

    let boot = identity_from_flags(&boot_path, api_addr)?;

    // **Audit M12 — the session identity is minted here, from the system CSPRNG.**
    let fence = hydra_wire::SessionFence::mint(boot.cluster_id, boot.manifest_hash, boot.model_instance_id);
    eprintln!("hydra-coordinator: session {} minted", hex8(&fence.session_id));

    std::fs::create_dir_all(&data_dir).map_err(|e| format!("mkdir {data_dir}: {e}"))?;
    let commits_path = std::path::Path::new(&data_dir).join("commits.wal");

    // **Audit M8 — the ledger is bound to this session at creation**, so a later open proves it
    // belongs here rather than replaying somebody else's generation.
    let commit_stream = CommitStream::create(&commits_path, fence.cluster_id, fence.session_id)
        .map_err(|e| format!("commit stream: {e}"))?;
    let _wal_fence = WalFenceCtx {
        cluster_id: fence.cluster_id,
        manifest_hash: fence.manifest_hash,
        model_instance_id: fence.model_instance_id,
        session_id: fence.session_id,
        epoch: 0,
        recovery_id: 0,
        activation_attempt_id: 0,
    };

    let auth = ApiAuth::loopback(&token, api_addr.port()).map_err(|e| e.to_string())?;
    let tls = hydra_transport::api_server_config(&boot.identity).map_err(|e| format!("api tls: {e}"))?;

    // The generation path is not wired to real stages in this seam — see the module note in
    // `driver.rs` and §8. What ships here is the coordinator process, its session identity, its
    // durable stream, and the TLS-served API; the stage links land with the next seam.
    // Spec §1.4 Option A: one active session per model instance. The commit stream is created
    // once, handed to the first session, and a second attempt is refused by the HTTP surface's
    // registry (audit M4·1's Option-B closure), not silently given a second stream.
    let make_session = {
        let cell = Arc::new(std::sync::Mutex::new(Some(commit_stream)));
        let fence_ctx = _wal_fence;
        Arc::new(move || {
            let cs = cell
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
                .expect("one active session per model instance (spec §1.4 Option A)");
            hydra_coordinator::Session::new(cs, fence_ctx.clone(), Box::new(NoPieces), 8, 1024)
        }) as Arc<dyn Fn() -> hydra_coordinator::Session + Send + Sync>
    };
    let gen_fn: hydra_coordinator::GenFn = Arc::new(|_prompt: String| {
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        rx // no stages linked yet in this seam; the channel closes and the session finishes
    });

    let router = hydra_coordinator::router(AppState::new(make_session, gen_fn, auth));
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(hydra_coordinator::serve_tls::serve_tls(api_addr, tls, router))
        .map_err(|e| format!("serve: {e}"))
}

/// A piece source with no tokenizer: the coordinator process starts before a model is attached.
struct NoPieces;
impl hydra_coordinator::PieceSource for NoPieces {
    fn piece(&self, _token: u32) -> Vec<u8> {
        Vec::new()
    }
    fn n_vocab(&self) -> u32 {
        u32::MAX
    }
}

/// What the coordinator needs out of a bootstrap blob.
struct Boot {
    cluster_id: [u8; 16],
    manifest_hash: [u8; 32],
    model_instance_id: [u8; 16],
    identity: hydra_transport::DeviceIdentity,
}

/// Build this coordinator's identity.
///
/// `--dev` mints a throwaway cluster CA in-process: right for a single machine, and **wrong for
/// anything else**, which is why it has to be asked for by name. Otherwise the DER files are read
/// as given. The cluster/manifest/model ids are dev constants in this seam; they become the paired
/// cluster's real values in M4·2.
fn identity_from_flags(mode: &str, api_addr: SocketAddr) -> Result<Boot, String> {
    // The API certificate must name the addresses clients dial, not just the device. Loopback is
    // always included because that is where a first run happens; the bind address is included
    // because that is where everyone else reaches it.
    let sans: Vec<String> = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
        api_addr.ip().to_string(),
    ];
    let identity = if mode == "--dev" {
        eprintln!("hydra-coordinator: --dev — minting a THROWAWAY cluster CA. Never use this off one machine.");
        eprintln!("hydra-coordinator: a client cannot pre-trust this CA; pair first and use --pairing-dir for anything real.");
        let ca = hydra_transport::ClusterCa::new().map_err(|e| format!("dev ca: {e}"))?;
        ca.issue_api("coordinator", &sans).map_err(|e| format!("dev identity: {e}"))?
    } else {
        // Load the CA this cluster was paired with and issue this run's leaf from it. The leaf is
        // short-lived by design (audit M3); the CA certificate is stable, so a client that trusted
        // it once keeps trusting the cluster across restarts.
        let dir = std::path::Path::new(mode).join("coordinator");
        let ca = hydra_transport::ClusterCa::load_private(&dir).map_err(|e| {
            format!("load the paired cluster CA from {}: {e}. Run `hydra-cli pair --out <dir>` first", dir.display())
        })?;
        eprintln!("hydra-coordinator: using the paired cluster CA from {}", dir.display());
        ca.issue_api("coordinator", &sans).map_err(|e| format!("issue coordinator identity: {e}"))?
    };
    Ok(Boot {
        cluster_id: [0x11; 16],
        manifest_hash: [0x22; 32],
        model_instance_id: [0x33; 16],
        identity,
    })
}

fn hex8(b: &[u8]) -> String {
    b.iter().take(8).map(|x| format!("{x:02x}")).collect()
}
