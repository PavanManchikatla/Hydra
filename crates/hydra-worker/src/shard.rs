//! P2·10b — **the signed manifest is the contract, and a worker refuses a shard that fails it.**
//!
//! The splitter (P2·10a, `hydra-modelsvc`) emits per-stage shard GGUFs plus one Ed25519-signed
//! manifest carrying per-tensor BLAKE3, the three admission hashes, and the **layer-range map**.
//! This module is the consuming half: before a worker loads any shard weights it
//!
//! 1. verifies the **manifest signature** — an unsigned or tampered manifest is refused;
//! 2. finds **this shard's entry** by file name — an unlisted shard file is refused;
//! 3. verifies the **shard bytes' BLAKE3** against the entry — a modified shard is refused;
//! 4. cross-checks the entry's **layer range against the worker's configured range** — a shard
//!    that is not the stage this worker was told to be is refused;
//!
//! and only then loads, using **the manifest's** layer range as the load window.
//!
//! Every failure is a **structured error, never a warning** (binding point 1): the security
//! posture that governs the control plane extends to model distribution. A worker that cannot
//! prove what weights it is about to run does not run them.

use hydra_engine_sys::Model;
use hydra_modelsvc::manifest::{Manifest, ShardEntry};

/// Why a shard was refused. Every variant is fatal — there is no "load it anyway" path.
#[derive(Debug, thiserror::Error)]
pub enum ShardRefused {
    #[error("shard REFUSED: cannot read manifest {path}: {source}")]
    ManifestUnreadable { path: String, source: std::io::Error },
    #[error("shard REFUSED: malformed manifest {path}: {msg}")]
    ManifestMalformed { path: String, msg: String },
    #[error("shard REFUSED: manifest signature does not verify ({path}): {msg}")]
    Signature { path: String, msg: String },
    #[error("shard REFUSED: cannot read shard file {path}: {source}")]
    ShardUnreadable { path: String, source: std::io::Error },
    #[error("shard REFUSED: {file_name} is not listed in the manifest")]
    NotInManifest { file_name: String },
    #[error("shard REFUSED: {file_name} BLAKE3 mismatch — the shard is not the bytes the manifest signed")]
    Blake3Mismatch { file_name: String },
    #[error(
        "shard REFUSED: {file_name} covers layers [{have_first},{have_last}) but this worker is \
         configured for [{want_first},{want_last}) — wrong stage"
    )]
    RangeMismatch {
        file_name: String,
        have_first: u32,
        have_last: u32,
        want_first: i32,
        want_last: i32,
    },
    #[error("shard REFUSED: engine could not load verified shard {path}: {msg}")]
    Load { path: String, msg: String },
}

/// A shard whose manifest entry has been fully verified. Holding one of these is the proof that
/// steps 1–4 passed; it is the only way to reach [`load_verified_shard`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedShard {
    pub shard_path: String,
    pub layer_first: u32,
    pub layer_last: u32,
    pub n_layer_total: u32,
}

/// Verify `shard_path` against the signed manifest at `manifest_path`, cross-checking the layer
/// range this worker was configured for. `want_last == -1` means "to the model's last layer".
pub fn verify_shard(
    manifest_path: &str,
    shard_path: &str,
    want_first: i32,
    want_last: i32,
) -> Result<VerifiedShard, ShardRefused> {
    // 1. Signature — before anything else is trusted, including the file names in the manifest.
    let raw = std::fs::read(manifest_path)
        .map_err(|source| ShardRefused::ManifestUnreadable { path: manifest_path.into(), source })?;
    let manifest = Manifest::from_bytes(&raw)
        .map_err(|e| ShardRefused::ManifestMalformed { path: manifest_path.into(), msg: e.to_string() })?;
    manifest
        .verify()
        .map_err(|e| ShardRefused::Signature { path: manifest_path.into(), msg: e.to_string() })?;

    // 2. This shard's entry, by file name.
    let file_name = std::path::Path::new(shard_path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| shard_path.to_string());
    let entry: &ShardEntry = manifest
        .shards
        .iter()
        .find(|s| s.file_name == file_name)
        .ok_or_else(|| ShardRefused::NotInManifest { file_name: file_name.clone() })?;

    // 3. The bytes on disk are the bytes the manifest signed.
    let bytes = std::fs::read(shard_path)
        .map_err(|source| ShardRefused::ShardUnreadable { path: shard_path.into(), source })?;
    if *blake3::hash(&bytes).as_bytes() != entry.shard_blake3 {
        return Err(ShardRefused::Blake3Mismatch { file_name });
    }

    // 4. The manifest's layer range is the stage this worker was told to be. A mismatch means the
    //    cluster's placement and this worker's configuration disagree — never silently follow one.
    let want_last_abs = if want_last < 0 { manifest.n_layer_total as i32 } else { want_last };
    if entry.layer_first as i32 != want_first || entry.layer_last as i32 != want_last_abs {
        return Err(ShardRefused::RangeMismatch {
            file_name,
            have_first: entry.layer_first,
            have_last: entry.layer_last,
            want_first,
            want_last: want_last_abs,
        });
    }

    Ok(VerifiedShard {
        shard_path: shard_path.to_string(),
        layer_first: entry.layer_first,
        layer_last: entry.layer_last,
        n_layer_total: manifest.n_layer_total,
    })
}

/// Load a [`VerifiedShard`]'s weights, windowed to **the manifest's** layer range.
pub fn load_verified_shard(v: &VerifiedShard, n_gpu_layers: i32) -> Result<Model, ShardRefused> {
    Model::load_shard(&v.shard_path, v.layer_first as i32, v.layer_last as i32, n_gpu_layers)
        .map_err(|e| ShardRefused::Load { path: v.shard_path.clone(), msg: e.to_string() })
}
