//! `hydra-cli` — the operator's tool: pair a cluster, issue device identities, read status.
//!
//! ```text
//! hydra-cli pair --out <dir>            # open a window on the coordinator; prints the PIN + QR payload
//! hydra-cli claim --name worker-s1 --pin 123456 --out <dir>
//! hydra-cli provision --pairing-dir <dir> --model <gguf> --stages worker-s1=127.0.0.1:9001,worker-s2=127.0.0.1:9002 [--split K] [--n-ctx N]
//! hydra-cli status --data-dir <dir>
//! ```
//!
//! `pair` also mints the **API token** (`api-token`, 0600 — item 2 of the 2026-09-02 ruling), and
//! `provision` mints the **session fence** and writes one bootstrap per stage plus the stage table
//! (`hydra_cli::provision`). Rotation of the token is re-pair.
//!
//! **The CA private key never leaves the coordinator.** `pair` writes the CA *certificate* and each
//! device's own material; there is no flag that emits the CA key, and `hydra_cli::PairingSession`
//! has no accessor that could serve one.

use std::time::SystemTime;

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let flag = |n: &str| args.iter().position(|a| a == n).and_then(|i| args.get(i + 1)).cloned();

    match args.get(1).map(String::as_str) {
        Some("pair") => {
            let out = flag("--out").unwrap_or_else(|| "./hydra-pairing".into());
            let mut session = hydra_cli::PairingSession::open().map_err(|e| e.to_string())?;
            std::fs::create_dir_all(&out).map_err(|e| format!("mkdir {out}: {e}"))?;
            write_private(&std::path::Path::new(&out).join("cluster-ca.der"), &session.ca_cert_der())?;
            // The PEM is what a client passes to `--cacert`. Public material, so world-readable.
            write_public(&std::path::Path::new(&out).join("cluster-ca.pem"), session.ca_cert_pem().as_bytes())?;

            // The coordinator provisions itself first: it is a peer, and the CA is persisted
            // coordinator-locally so a later re-pair is possible (M4·2).
            let coord_dir = std::path::Path::new(&out).join("coordinator");
            let coord = session.provision_coordinator(&coord_dir).map_err(|e| e.to_string())?;
            write_private(&coord_dir.join("identity.cert.der"), coord.identity.cert_chain[0].as_ref())?;
            println!("coordinator provisioned into {} (CA key persisted there, 0600, and nowhere else)", coord_dir.display());
            // Item 2 (2026-09-02): pairing mints the API token, beside the CA material, 0600.
            let _token = hydra_cli::provision::mint_api_token(std::path::Path::new(&out)).map_err(|e| e.to_string())?;
            println!("API token minted into {}/api-token (0600). The coordinator reads it from there; HYDRA_API_TOKEN overrides it; rotation is re-pair.", out);
            println!();

            println!("Pairing window open for {} seconds.", hydra_cli::PAIRING_WINDOW.as_secs());
            println!();
            println!("    PIN: {}", session.pin());
            println!();
            println!("QR payload (encode this on the coordinator's screen):");
            println!("    hydra://pair?pin={}&ca={}", session.pin(), hex(&blake3::hash(&session.ca_cert_der()).as_bytes()[..8]));
            println!();
            println!("The PIN proves physical proximity for this window. It is not a password, and");
            println!("it stops working in {} seconds or after {} wrong attempts.", hydra_cli::PAIRING_WINDOW.as_secs(), hydra_cli::MAX_PIN_ATTEMPTS);

            // A single-process demo of the claim side, so `pair` is runnable end to end today.
            if let Some(name) = flag("--issue") {
                let pin = session.pin().to_string();
                let issued = session.claim(&name, &pin, SystemTime::now()).map_err(|e| e.to_string())?;
                let dir = std::path::Path::new(&out).join(&issued.device_name);
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                write_private(&dir.join("identity.cert.der"), issued.identity.cert_chain[0].as_ref())?;
                write_private(&dir.join("cluster-ca.der"), &issued.ca_cert_der)?;
                println!();
                println!("issued identity for {} into {}", issued.device_name, dir.display());
            }
            Ok(())
        }
        Some("provision") => {
            let dir = flag("--pairing-dir").ok_or("provision: --pairing-dir <dir> (what `pair --out` wrote) is required")?;
            let model = flag("--model").ok_or("provision: --model <gguf> is required")?;
            let stages_arg = flag("--stages").ok_or("provision: --stages worker-s1=127.0.0.1:9001,worker-s2=127.0.0.1:9002 is required (rank order)")?;
            let split = flag("--split").map(|v| v.parse::<i32>().map_err(|e| format!("--split: {e}"))).transpose()?;
            let n_ctx: i32 = flag("--n-ctx").unwrap_or_else(|| "512".into()).parse().map_err(|e| format!("--n-ctx: {e}"))?;
            let mut stages = Vec::new();
            for (rank, part) in stages_arg.split(',').enumerate() {
                let (name, addr) = part.split_once('=').ok_or_else(|| format!("--stages: expected name=addr, got {part:?}"))?;
                stages.push(hydra_cli::provision::StageSpec {
                    name: name.trim().to_string(),
                    rank: rank as u16,
                    addr: addr.trim().parse().map_err(|e| format!("--stages {name}: {e}"))?,
                });
            }
            let files = hydra_cli::provision::provision(std::path::Path::new(&dir), &model, &stages, split, n_ctx).map_err(|e| e.to_string())?;
            println!("provisioned {} stages into {dir}:", files.stages.len());
            for s in &files.stages {
                println!("    {}  rank {}  {}  -> {dir}/{}.boot  (start with: hydra-worker {dir}/{}.boot)", s.name, s.rank, s.addr, s.name, s.name);
            }
            println!("session fence written to {dir}/cluster.fence (session_id minted from the system CSPRNG; the coordinator and every stage share it)");
            println!("stage table written to {dir}/stages");
            Ok(())
        }
        Some("status") => {
            let dir = flag("--data-dir").unwrap_or_else(|| "./hydra-data".into());
            for line in hydra_cli::status::status_for(std::path::Path::new(&dir)) {
                println!("{:<16} {:<24} ({})", line.what, line.value, line.source);
            }
            Ok(())
        }
        _ => Err("usage: hydra-cli <pair|provision|status> ...".into()),
    }
}

/// Write a file anyone may read — for the CA **certificate**, which is a public trust anchor. A
/// trust anchor nobody can read is not a trust anchor.
fn write_public(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut f = std::fs::File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    f.write_all(bytes).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    Ok(())
}

/// Write a file only this user can read (audit H17's lesson, applied where it is being written).
fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(|e| e.to_string())?;
        }
    }
    let _ = std::fs::remove_file(path);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    f.write_all(bytes).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
