//! `hydra-modelsvc` — the GGUF splitter/verifier CLI (P2·10a).
//!
//! ```text
//! hydra-modelsvc info   <model.gguf>
//! hydra-modelsvc split  <model.gguf> <out_dir> --stages 0-14,14-21,21-24 [--key <pkcs8-file>]
//! hydra-modelsvc verify <manifest-file> <shard_dir>
//! ```
//! `split` writes one GGUF per stage + a `<arch>.manifest` (Ed25519-signed). Without `--key` it
//! generates a fresh keypair and writes the PKCS#8 next to the manifest (dev convenience; in the
//! field the signing key is the cluster identity — M4 pairing).

use std::path::Path;
use std::process::exit;

use hydra_modelsvc::gguf::{layers_present, Gguf};
use hydra_modelsvc::manifest::{generate_keypair, keypair_from_pkcs8, Manifest};
use hydra_modelsvc::split::split;

/// **Audit H11 / M19 — join a single, non-escaping component onto a directory.**
///
/// `Path::join` silently discards the directory when the component is absolute, and happily walks
/// upward on `..`. Requiring exactly one `Normal` component makes both impossible, and the failure
/// is an error rather than a sanitised path: a name the operator did not choose should not become
/// a file they did not expect, even a harmless-looking one.
fn safe_join(dir: &str, component: &str) -> Result<std::path::PathBuf, String> {
    let p = Path::new(component);
    let mut it = p.components();
    match (it.next(), it.next()) {
        (Some(std::path::Component::Normal(c)), None) => Ok(Path::new(dir).join(c)),
        _ => Err(format!("REFUSED: {component:?} is not a single safe path component (audit H11/M19)")),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let r = match args.get(1).map(String::as_str) {
        Some("info") => cmd_info(&args),
        Some("split") => cmd_split(&args),
        Some("verify") => cmd_verify(&args),
        _ => {
            eprintln!("usage: hydra-modelsvc <info|split|verify> ...");
            exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("error: {e}");
        exit(1);
    }
}

fn read(path: &str) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("read {path}: {e}"))
}

fn cmd_info(args: &[String]) -> Result<(), String> {
    let path = args.get(2).ok_or("usage: info <model.gguf>")?;
    let g = Gguf::parse(&read(path)?).map_err(|e| e.to_string())?;
    println!("architecture: {}", g.architecture().unwrap_or("?"));
    println!("gguf version: {}", g.version);
    println!("metadata KVs: {}", g.metadata.len());
    println!("tensors:      {}", g.tensors.len());
    let layers = layers_present(&g);
    println!("layers:       {} (blk.0 .. blk.{})", layers.len(), layers.keys().max().copied().unwrap_or(0));
    Ok(())
}

/// Parse `0-14,14-21,21-24` → `[(0,14),(14,21),(21,24)]`.
fn parse_ranges(s: &str) -> Result<Vec<(u32, u32)>, String> {
    s.split(',')
        .map(|part| {
            let (a, b) = part.split_once('-').ok_or_else(|| format!("bad range {part:?} (want first-last)"))?;
            Ok((a.trim().parse().map_err(|_| format!("bad {a:?}"))?, b.trim().parse().map_err(|_| format!("bad {b:?}"))?))
        })
        .collect()
}

fn cmd_split(args: &[String]) -> Result<(), String> {
    let model = args.get(2).ok_or("usage: split <model.gguf> <out_dir> --stages R --key K")?;
    let out_dir = args.get(3).ok_or("usage: split <model.gguf> <out_dir> --stages R")?;
    let stages = flag(args, "--stages").ok_or("--stages 0-14,14-21,21-24 required")?;
    let ranges = parse_ranges(&stages)?;

    let g = Gguf::parse(&read(model)?).map_err(|e| e.to_string())?;
    let (kp, pk8) = match flag(args, "--key") {
        Some(kf) => (keypair_from_pkcs8(&read(&kf)?).map_err(|e| e.to_string())?, None),
        None => {
            let (pk8, kp) = generate_keypair().map_err(|e| e.to_string())?;
            (kp, Some(pk8))
        }
    };

    let out = split(&g, &ranges, &kp).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(out_dir).map_err(|e| format!("mkdir {out_dir}: {e}"))?;
    for shard in &out.shards {
        // Audit H11 / M19: a file name from a manifest may never escape `out_dir`. `Path::join`
        // discards the left side on an absolute right side, so this is checked, not assumed.
        let p = safe_join(out_dir, &shard.file_name)?;
        std::fs::write(&p, &shard.bytes).map_err(|e| format!("write {}: {e}", p.display()))?;
        println!("wrote {} ({} bytes)", p.display(), shard.bytes.len());
    }
    // Audit H11: `architecture` is already validated at the source (`split::sanitize_arch`), but
    // the value written into a manifest by an OLDER build is untrusted input to this one — so the
    // joins below are guarded here too, and the guard is a re-validation rather than a comment
    // saying it happened earlier.
    let arch = hydra_modelsvc::split::sanitize_arch(&out.manifest.architecture).map_err(|e| e.to_string())?;
    let mp = safe_join(out_dir, &format!("{arch}.manifest"))?;
    std::fs::write(&mp, out.manifest.to_bytes().map_err(|e| e.to_string())?).map_err(|e| format!("write manifest: {e}"))?;
    println!("wrote {} (signed)", mp.display());
    if let Some(pk8) = pk8 {
        let kp_path = safe_join(out_dir, &format!("{arch}.signing.pkcs8"))?;
        std::fs::write(&kp_path, pk8).map_err(|e| format!("write key: {e}"))?;
        println!("wrote {} (dev signing key — keep private)", kp_path.display());
    }
    Ok(())
}

/// Parse a 64-hex-character Ed25519 public key, or read it from a file containing one.
fn trusted_key(arg: &str) -> Result<[u8; 32], String> {
    let text = if std::path::Path::new(arg).exists() {
        std::fs::read_to_string(arg).map_err(|e| format!("read trusted key {arg}: {e}"))?
    } else {
        arg.to_string()
    };
    let hex: String = text.trim().chars().filter(|c| !c.is_whitespace()).collect();
    if hex.len() != 64 {
        return Err(format!("trusted key must be 64 hex chars (32 bytes), got {} chars", hex.len()));
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|e| format!("bad hex: {e}"))?;
    }
    Ok(out)
}

fn cmd_verify(args: &[String]) -> Result<(), String> {
    let usage = "usage: verify <manifest> <shard_dir> <trusted-pubkey-hex|keyfile>";
    let mpath = args.get(2).ok_or(usage)?;
    let dir = args.get(3).ok_or(usage)?;
    // C1: the trusted key is REQUIRED, not optional. There is deliberately no "verify without a
    // key" mode — it would print "signature: OK" for a manifest an attacker signed themselves,
    // which is a stronger claim than no output at all and would be believed.
    let trusted = trusted_key(args.get(4).ok_or(usage)?)?;

    // H13: signature first, structure second. `verify_and_parse` needs the fence tuple's
    // manifest_hash for the H14 identity binding; the CLI is an operator tool inspecting a file it
    // already chose by path, so there is no session to bind to — it passes the file's own hash and
    // SAYS SO, rather than pretending to a binding it cannot make.
    let raw = read(mpath)?;
    let self_hash = *blake3::hash(&raw).as_bytes();
    let m = Manifest::verify_and_parse(&raw, &trusted, &self_hash)
        .map_err(|e| format!("MANIFEST REFUSED: {e}"))?;
    println!("signature: OK (signed by the pinned trusted key {})", hex8(&m.signer_pubkey));
    println!("manifest blake3: {} — bind this into the session fence tuple's manifest_hash (H14)", hex8(&self_hash));
    // 2. Each shard's bytes hash to the manifest's recorded BLAKE3.
    for s in &m.shards {
        // Audit M19: `file_name` is manifest-controlled, and the manifest is verified but not
        // therefore *safe* — a validly signed manifest can still name `../../etc/passwd`, and this
        // command would read and hash it. One Normal component, or refuse.
        let bytes = read(&safe_join(dir, &s.file_name)?.to_string_lossy())?;
        let got = *blake3::hash(&bytes).as_bytes();
        if got != s.shard_blake3 {
            return Err(format!("SHARD REFUSED: {} BLAKE3 mismatch", s.file_name));
        }
        println!("shard {} L[{},{}): OK ({} tensors)", s.file_name, s.layer_first, s.layer_last, s.tensors.len());
    }
    println!("ALL {} shards verify against the signed manifest", m.shards.len());
    Ok(())
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}
fn hex8(b: &[u8]) -> String {
    b.iter().take(4).map(|x| format!("{x:02x}")).collect()
}
