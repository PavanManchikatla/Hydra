//! **Provisioning (2026-09-02, design-authority ruling items 1–2): everything a cluster needs to run
//! a generation, minted on the coordinator and written into the pairing directory.**
//!
//! Until this module existed the quickstart started no worker at all, and the shipped
//! `hydra-coordinator` answered every prompt with an empty stream (§7.76). A cluster needs, beyond
//! the CA and identities pairing already writes:
//!
//! * **the session fence** — `cluster_id`, `manifest_hash`, `model_instance_id`, `session_id` —
//!   the F1 identity every stage and the coordinator must share. It is minted ONCE here (the
//!   `session_id` from the system CSPRNG, audit M12) and written to `cluster.fence`; a worker
//!   bootstrap embeds it and the coordinator reads it, so a restart resumes the SAME session
//!   (spec §6.5) instead of minting a new one the ledger does not know.
//! * **one bootstrap per stage** (`<name>.boot`, 0600 — it carries the stage's private key), with
//!   its layer window, role table and model path.
//! * **the stage table** (`stages`): `name rank addr` per line, the coordinator's placement.
//! * **the API token** (`api-token`, 0600, ≥ 32 random bytes as hex) — item 2: pairing mints it;
//!   the coordinator reads it from here by default; `HYDRA_API_TOKEN` remains an override;
//!   **rotation is re-pair** (the same "re-pair issues, does not revoke" v1 posture).
//!
//! The pairing directory is already the secret store (the CA key lives in it at 0600), so no new
//! class of secret storage is introduced.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use hydra_transport::ClusterCa;
use hydra_wire::SessionFence;
use hydra_worker::bootstrap::{Bootstrap, ROLE_COORDINATOR, ROLE_STAGE_BASE};
use hydra_worker::sampler::SamplingConfig;
use hydra_worker::worker::WorkerConfig;

pub const FENCE_FILE: &str = "cluster.fence";
pub const STAGES_FILE: &str = "stages";
pub const API_TOKEN_FILE: &str = "api-token";
/// Minimum bytes of entropy in a minted API token (item 2: "≥32 random bytes").
pub const API_TOKEN_BYTES: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    #[error("io {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("transport: {0}")]
    Transport(String),
    #[error("{0}")]
    Invalid(String),
}

/// One stage of the placement, as the operator names it.
#[derive(Clone, Debug)]
pub struct StageSpec {
    pub name: String,
    pub rank: u16,
    pub addr: SocketAddr,
}

/// What a provisioned cluster looks like on disk — the coordinator reads exactly this.
#[derive(Clone, Debug)]
pub struct ClusterFiles {
    pub fence: SessionFence,
    pub model_path: String,
    pub stages: Vec<StageSpec>,
    pub n_ctx: i32,
}

/// Write `api-token` (0600) into `dir` with `API_TOKEN_BYTES` of CSPRNG entropy, hex-encoded.
/// Returns the token. Overwrites an existing one: a re-pair rotates the token, by design.
pub fn mint_api_token(dir: &Path) -> Result<String, ProvisionError> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0u8; API_TOKEN_BYTES];
    SystemRandom::new().fill(&mut bytes).map_err(|_| ProvisionError::Invalid("system CSPRNG unavailable".into()))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    write_private(&dir.join(API_TOKEN_FILE), token.as_bytes())?;
    Ok(token)
}

/// Read the API token the coordinator will enforce, refusing a file that is group- or
/// world-readable (rule 19's oracle for item 2 lives on this refusal).
pub fn read_api_token(dir: &Path) -> Result<String, ProvisionError> {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(API_TOKEN_FILE);
    let meta = std::fs::metadata(&path).map_err(|e| ProvisionError::Io(path.clone(), e))?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(ProvisionError::Invalid(format!(
            "api_token_file_permissions: {} is mode {mode:o}; the API token must not be group- or world-readable (expected 0600). \
             Fix with `chmod 600` or re-pair.",
            path.display()
        )));
    }
    let token = std::fs::read_to_string(&path).map_err(|e| ProvisionError::Io(path.clone(), e))?;
    let token = token.trim().to_string();
    if token.len() < 2 * API_TOKEN_BYTES {
        return Err(ProvisionError::Invalid(format!("{}: token shorter than the {API_TOKEN_BYTES}-byte minimum pairing mints", path.display())));
    }
    Ok(token)
}

/// Mint the session fence and write `cluster.fence`, `stages`, and one `<name>.boot` per stage.
///
/// `cluster_id` is derived from the CA certificate (so two clusters paired separately never share
/// one), `manifest_hash` is the BLAKE3 of the model file (the admission anchor C1 expects), and
/// `session_id` is CSPRNG-minted (M12). `split` is the first layer of the LAST stage for a two-stage
/// placement; `None` means half the model's layers, which needs a linked engine to read `n_layer`.
pub fn provision(dir: &Path, model_path: &str, stages: &[StageSpec], split: Option<i32>, n_ctx: i32) -> Result<ClusterFiles, ProvisionError> {
    if stages.len() != 2 {
        return Err(ProvisionError::Invalid(format!("v1 provisioning places exactly two stages (S1 → S_P); got {}", stages.len())));
    }
    let mut ranks: Vec<u16> = stages.iter().map(|s| s.rank).collect();
    ranks.sort_unstable();
    if ranks != vec![0, 1] {
        return Err(ProvisionError::Invalid("stage ranks must be exactly 0 and 1".into()));
    }
    let ca = ClusterCa::load_private(&dir.join("coordinator")).map_err(|e| ProvisionError::Transport(e.to_string()))?;
    let ca_der = ca.ca_cert_der();
    let cluster_id: [u8; 16] = blake3::hash(ca_der.as_ref()).as_bytes()[..16].try_into().unwrap();
    let model_bytes = std::fs::read(model_path).map_err(|e| ProvisionError::Io(PathBuf::from(model_path), e))?;
    let manifest_hash: [u8; 32] = *blake3::hash(&model_bytes).as_bytes();
    drop(model_bytes);
    let mut model_instance_id = [0u8; 16];
    {
        use ring::rand::{SecureRandom, SystemRandom};
        SystemRandom::new().fill(&mut model_instance_id).map_err(|_| ProvisionError::Invalid("system CSPRNG unavailable".into()))?;
    }
    let fence = SessionFence::mint(cluster_id, manifest_hash, model_instance_id);

    let split = match split {
        Some(k) => k,
        None => {
            if !hydra_engine_sys::ENGINE_AVAILABLE {
                return Err(ProvisionError::Invalid("no linked engine to read the model's layer count; pass --split <first layer of S_P>".into()));
            }
            let m = hydra_engine_sys::Model::load_vocab_only(model_path).map_err(|e| ProvisionError::Invalid(format!("read model metadata: {e}")))?;
            (m.n_layer() / 2).max(1)
        }
    };

    // The role table every stage accepts: the coordinator, and each stage by name (a stage dials
    // nothing in the relay topology, but naming both is the same table the harnesses use).
    let peers: Vec<(String, u8)> = std::iter::once(("coordinator".to_string(), ROLE_COORDINATOR))
        .chain(stages.iter().map(|s| (s.name.clone(), ROLE_STAGE_BASE + s.rank as u8)))
        .collect();

    for s in stages {
        let id = ca.issue(&s.name).map_err(|e| ProvisionError::Transport(e.to_string()))?;
        let is_final = s.rank == 1;
        let cfg = WorkerConfig {
            fence: fence.clone(),
            rank: s.rank as hydra_state::StageRank,
            layer_first: if is_final { split } else { 0 },
            layer_last: if is_final { -1 } else { split },
            is_final,
            receives_tokens: !is_final,
            epoch: 0,
            recovery_id: 0,
            model_path: Some(model_path.to_string()),
            n_gpu_layers: 0,
            n_ctx,
            sampler_config: if is_final { Some(SamplingConfig::greedy()) } else { None },
            recovery_start: false,
            shard_manifest: None,
        };
        let boot = Bootstrap {
            listen_addr: s.addr.to_string(),
            device_name: s.name.clone(),
            ca_cert_der: ca_der.as_ref().to_vec(),
            cert_chain_der: id.cert_chain.iter().map(|c| c.as_ref().to_vec()).collect(),
            key_pkcs8_der: id.key_pkcs8_der(),
            cfg,
            forwarding: None,
            expected_peers: peers.clone(),
        };
        let path = dir.join(format!("{}.boot", s.name));
        // `write_to_replacing` creates the file; the bootstrap carries a private key, so 0600.
        boot.write_to_replacing(&path.to_string_lossy()).map_err(|e| ProvisionError::Io(path.clone(), e))?;
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).map_err(|e| ProvisionError::Io(path.clone(), e))?;
    }

    let fence_txt = format!(
        "cluster_id={}\nmanifest_hash={}\nmodel_instance_id={}\nsession_id={}\nmodel={}\nn_ctx={}\n",
        hex(&fence.cluster_id), hex(&fence.manifest_hash), hex(&fence.model_instance_id), hex(&fence.session_id), model_path, n_ctx
    );
    write_private(&dir.join(FENCE_FILE), fence_txt.as_bytes())?;
    let stages_txt: String = stages.iter().map(|s| format!("{} {} {}\n", s.name, s.rank, s.addr)).collect();
    write_private(&dir.join(STAGES_FILE), stages_txt.as_bytes())?;
    Ok(ClusterFiles { fence, model_path: model_path.to_string(), stages: stages.to_vec(), n_ctx })
}

/// Read what [`provision`] wrote. The coordinator's startup calls this.
pub fn read_cluster(dir: &Path) -> Result<ClusterFiles, ProvisionError> {
    let fpath = dir.join(FENCE_FILE);
    let txt = std::fs::read_to_string(&fpath).map_err(|e| ProvisionError::Io(fpath.clone(), e))?;
    let mut kv = std::collections::HashMap::new();
    for line in txt.lines() {
        if let Some((k, v)) = line.split_once('=') {
            kv.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let get = |k: &str| kv.get(k).cloned().ok_or_else(|| ProvisionError::Invalid(format!("{}: missing {k}", fpath.display())));
    let fence = SessionFence {
        cluster_id: unhex(&get("cluster_id")?)?,
        manifest_hash: unhex(&get("manifest_hash")?)?,
        model_instance_id: unhex(&get("model_instance_id")?)?,
        session_id: unhex(&get("session_id")?)?,
    };
    let model_path = get("model")?;
    let n_ctx: i32 = get("n_ctx")?.parse().map_err(|_| ProvisionError::Invalid("n_ctx".into()))?;
    let spath = dir.join(STAGES_FILE);
    let stxt = std::fs::read_to_string(&spath).map_err(|e| ProvisionError::Io(spath.clone(), e))?;
    let mut stages = Vec::new();
    for line in stxt.lines().filter(|l| !l.trim().is_empty()) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 3 {
            return Err(ProvisionError::Invalid(format!("{}: bad line {line:?}", spath.display())));
        }
        stages.push(StageSpec {
            name: parts[0].to_string(),
            rank: parts[1].parse().map_err(|_| ProvisionError::Invalid(format!("rank {:?}", parts[1])))?,
            addr: parts[2].parse().map_err(|_| ProvisionError::Invalid(format!("addr {:?}", parts[2])))?,
        });
    }
    stages.sort_by_key(|s| s.rank);
    Ok(ClusterFiles { fence, model_path, stages, n_ctx })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn unhex<const N: usize>(s: &str) -> Result<[u8; N], ProvisionError> {
    if s.len() != 2 * N {
        return Err(ProvisionError::Invalid(format!("expected {} hex chars, got {}", 2 * N, s.len())));
    }
    let mut out = [0u8; N];
    for i in 0..N {
        out[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|_| ProvisionError::Invalid("hex".into()))?;
    }
    Ok(out)
}

/// 0600, create-new after removing any old copy (the same discipline as `write_private` in the CLI).
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), ProvisionError> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(|e| ProvisionError::Io(parent.to_path_buf(), e))?;
        }
    }
    let _ = std::fs::remove_file(path);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).map_err(|e| ProvisionError::Io(path.to_path_buf(), e))?;
    f.write_all(bytes).map_err(|e| ProvisionError::Io(path.to_path_buf(), e))?;
    f.sync_all().map_err(|e| ProvisionError::Io(path.to_path_buf(), e))?;
    Ok(())
}
