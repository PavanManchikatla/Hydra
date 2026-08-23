//! P2·10b — **the signed manifest is the contract, and a worker refuses a shard that fails it.**
//!
//! The splitter (P2·10a, `hydra-modelsvc`) emits per-stage shard GGUFs plus one Ed25519-signed
//! manifest carrying per-tensor BLAKE3, the three admission hashes, and the **layer-range map**.
//! This module is the consuming half: before a worker loads any shard weights it
//!
//! 1. verifies the **manifest signature against the cluster's PINNED signing key**, before parsing
//!    a single byte of structure, and checks that `blake3(manifest)` is the one this session's
//!    fence tuple names — an unsigned, foreign-signed, tampered, or substituted manifest is refused
//!    (audit C1 / H13 / H14);
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

/// The cluster's manifest-signing identity, provisioned at pairing alongside the mTLS identity.
///
/// **C1 — this type exists so the trust anchor cannot be omitted.** The previous code called
/// `manifest.verify()`, which checked the signature against the key *embedded in the manifest*:
/// a self-attestation that any attacker can produce with `generate_keypair()`. Making the anchor a
/// required parameter means "verified" cannot be reached without answering *by whom*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedSigner(pub [u8; 32]);

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
    /// **Audit H6 — the shard path is a symlink, or not a regular file.**
    ///
    /// Opened with `O_NOFOLLOW`, so a symlinked shard path is refused outright rather than
    /// resolved: the whole point of hashing a file is to know *which* file, and a symlink is a
    /// name that can point somewhere else a microsecond later. A FIFO or device node is refused
    /// for a blunter reason — `llama.cpp` would `fopen` it and read whatever an attacker feeds,
    /// forever, and "the bytes I hashed" would have no meaning at all.
    #[error("shard REFUSED: {path} is not a regular file, or is a symlink (O_NOFOLLOW): {msg}")]
    NotARegularFile { path: String, msg: String },
    /// **Audit H6 — the file changed between the hash and the load (TOCTOU).**
    ///
    /// The bytes were hashed through one `open()` and the engine loads through a *second* one. If
    /// the file — or the name — is different between them, the verification proved nothing about
    /// what actually got mapped.
    #[error("shard REFUSED: {path} changed between verification and load (TOCTOU): {what}")]
    ChangedUnderneath { path: String, what: &'static str },
    /// **Audit H6 — the hardened parser refused the file, so the vendored one never sees it.**
    ///
    /// The finding's own title is *"the worker never runs the hardened GGUF parser"*. It hashed
    /// the bytes and handed the **path** to `llama.cpp`, whose parser this project has never
    /// fuzzed — while the 24-CPU-hour budget went to `hydra-modelsvc`'s reader, which only the
    /// offline splitter runs. Now the worker runs it too, on the metadata region, first.
    #[error("shard REFUSED: {path} failed the hardened GGUF parse before the engine saw it: {msg}")]
    HardenedParse { path: String, msg: String },
}

/// How much of a shard is read for the hardened pre-flight parse (audit H6).
///
/// The metadata region — header, KV block, tensor-info table — is where every GGUF parsing defect
/// lives, and it is small: a 70 B model's is a few hundred KiB. 64 MiB is a bound with several
/// orders of magnitude of headroom that still refuses to read a multi-gigabyte shard into memory.
/// A file whose metadata does not fit is refused rather than partially parsed, and the bound is
/// stated here so that refusal is legible instead of mysterious.
const HARDENED_PREFIX_BYTES: usize = 64 * 1024 * 1024;

/// **Audit H6 — the identity of the file we actually hashed.**
///
/// `(dev, ino, size, mtime)` is the "at minimum" the audit asks for when loading from the same
/// descriptor is not possible. It is not a cryptographic identity and is not claimed to be: an
/// attacker who can replace the file *and* reproduce all four fields defeats it. What it does
/// close is the ordinary window — the one that needs no timing skill at all, because today the
/// file is fully read, hashed, dropped, and then opened again by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime: (i64, i64),
}

impl FileIdentity {
    fn of(meta: &std::fs::Metadata) -> FileIdentity {
        use std::os::unix::fs::MetadataExt;
        FileIdentity { dev: meta.dev(), ino: meta.ino(), size: meta.size(), mtime: (meta.mtime(), meta.mtime_nsec()) }
    }
}

/// Open `path` **without following symlinks**, prove it is a regular file, and return the handle
/// together with its identity (audit H6).
///
/// The handle is deliberately returned and **held by the caller across the engine load**: an open
/// descriptor pins the inode, so even if the *name* is re-pointed, the object we hashed cannot be
/// recycled underneath us while we still hold it.
fn open_no_follow(path: &str) -> Result<(std::fs::File, FileIdentity), ShardRefused> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| ShardRefused::NotARegularFile { path: path.into(), msg: e.to_string() })?;
    let meta = file
        .metadata()
        .map_err(|e| ShardRefused::NotARegularFile { path: path.into(), msg: e.to_string() })?;
    if !meta.is_file() {
        return Err(ShardRefused::NotARegularFile { path: path.into(), msg: "not a regular file".into() });
    }
    Ok((file, FileIdentity::of(&meta)))
}

/// The identity `llama.cpp` will see when it opens `path` by name — i.e. **following** symlinks,
/// because that is what `fopen` does. Compared against the identity we hashed (audit H6).
fn identity_by_path(path: &str) -> Result<FileIdentity, ShardRefused> {
    let meta = std::fs::metadata(path)
        .map_err(|e| ShardRefused::NotARegularFile { path: path.into(), msg: e.to_string() })?;
    Ok(FileIdentity::of(&meta))
}

/// A shard whose manifest entry has been fully verified. Holding one of these is the proof that
/// steps 1–4 passed; it is the only way to reach [`load_verified_shard`].
#[derive(Debug, Clone)]
pub struct VerifiedShard {
    pub shard_path: String,
    pub layer_first: u32,
    pub layer_last: u32,
    pub n_layer_total: u32,
    /// Audit H6: the identity of the file whose bytes were actually hashed.
    pub identity: FileIdentity,
    /// Audit H6: the open descriptor, held so the hashed inode cannot be recycled.
    _held: std::sync::Arc<std::fs::File>,
}

/// Verify `shard_path` against the signed manifest at `manifest_path`, cross-checking the layer
/// range this worker was configured for. `want_last == -1` means "to the model's last layer".
pub fn verify_shard(
    manifest_path: &str,
    shard_path: &str,
    want_first: i32,
    want_last: i32,
    trusted: &TrustedSigner,
    expected_manifest_hash: &[u8; 32],
) -> Result<VerifiedShard, ShardRefused> {
    // 1. Signature against the PINNED key, checked BEFORE the structure is parsed, and identity
    //    bound to the session's fence tuple. The old comment here said "before anything else is
    //    trusted" while the code parsed the whole structure first and then checked a signature
    //    against the manifest's own embedded key — the comment described the intent, the code did
    //    neither. `verify_and_parse` is now the only way in (audit C1 / H13 / H14).
    let raw = std::fs::read(manifest_path)
        .map_err(|source| ShardRefused::ManifestUnreadable { path: manifest_path.into(), source })?;
    let manifest = Manifest::verify_and_parse(&raw, &trusted.0, expected_manifest_hash)
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

    // 3. The bytes on disk are the bytes the manifest signed — **hashed through a descriptor we
    //    open ourselves and keep** (audit H6). `std::fs::read(path)` used to do this, which opened
    //    the file, read it, hashed it, and closed it; the engine then opened the path *again*.
    //    Everything between those two opens was unverified, and re-pointing a symlink was enough.
    let (mut file, identity) = open_no_follow(shard_path)?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher)
        .map_err(|source| ShardRefused::ShardUnreadable { path: shard_path.into(), source })?;
    if *hasher.finalize().as_bytes() != entry.shard_blake3 {
        return Err(ShardRefused::Blake3Mismatch { file_name });
    }

    // 3b. **The hardened parser runs on the worker, before the engine (audit H6).**
    //
    //     `hydra-modelsvc`'s GGUF reader is clamped (`reserve_for`), checked-arithmetic, and the
    //     target of this project's entire parser-fuzzing budget. Until now **only the offline
    //     splitter ran it**: the worker hashed the file and handed the path to `llama.cpp`. So the
    //     fuzzing protected a program that never runs on the load path — and the vendored parser,
    //     which does, ABORTS on hostile input (observed: `hydra-fuzz --target vendored-gguf`,
    //     seed 1 iteration 350, SIGABRT inside `gguf_init_from_file_ptr`; an abort is uncatchable,
    //     so the shim's `catch (...)` cannot help).
    //
    //     Running the hardened parser first does not make the vendored one safe — it means the
    //     only files it is ever handed are files a fuzzed, clamped parser already accepted.
    let mut prefix = vec![0u8; HARDENED_PREFIX_BYTES.min(identity.size as usize)];
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = &file;
        f.seek(SeekFrom::Start(0)).map_err(|source| ShardRefused::ShardUnreadable { path: shard_path.into(), source })?;
        let n = f.read(&mut prefix).map_err(|source| ShardRefused::ShardUnreadable { path: shard_path.into(), source })?;
        prefix.truncate(n);
    }
    hydra_modelsvc::gguf::Gguf::parse_metadata(&prefix, identity.size)
        .map_err(|e| ShardRefused::HardenedParse { path: shard_path.into(), msg: e.to_string() })?;

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
        identity,
        // The descriptor is held for the lifetime of the `VerifiedShard`, so the inode we hashed
        // cannot be recycled while the proof is alive (audit H6).
        _held: std::sync::Arc::new(file),
    })
}

/// Load a [`VerifiedShard`]'s weights, windowed to **the manifest's** layer range.
pub fn load_verified_shard(v: &VerifiedShard, n_gpu_layers: i32) -> Result<Model, ShardRefused> {
    // **Audit H6 — the TOCTOU window, narrowed at both ends.**
    //
    // The engine opens by *path*, so the file it maps is not provably the file we hashed. Loading
    // from the held descriptor would settle it, but `llama_model_load_from_file` takes a path and
    // adding an fd-taking entry point means another patch to the vendored submodule — which L1
    // already names as a release-integrity problem, so compounding it is the wrong trade today.
    //
    // What is done instead is the audit's stated minimum, at both ends of the load: the path's
    // identity must match the descriptor's **before** the engine opens it, and again **after**.
    // The before-check catches a swap that already happened; the after-check catches one that
    // happened during the load, and is what makes this more than a formality — a mismatch there
    // means the mapping may be of bytes nobody verified, so the model is dropped rather than used.
    let before = identity_by_path(&v.shard_path)?;
    if before != v.identity {
        return Err(ShardRefused::ChangedUnderneath { path: v.shard_path.clone(), what: "identity differs before load" });
    }
    let model = Model::load_shard(&v.shard_path, v.layer_first as i32, v.layer_last as i32, n_gpu_layers)
        .map_err(|e| ShardRefused::Load { path: v.shard_path.clone(), msg: e.to_string() })?;
    let after = identity_by_path(&v.shard_path)?;
    if after != v.identity {
        // Dropping `model` here is the point: it may hold a mapping of unverified bytes.
        return Err(ShardRefused::ChangedUnderneath { path: v.shard_path.clone(), what: "identity differs after load" });
    }
    Ok(model)
}
