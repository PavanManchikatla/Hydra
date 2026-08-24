//! `hydra-cli` — the operator's tool: pair a cluster, issue device identities, read status.
//!
//! ```text
//! hydra-cli pair --out <dir>            # open a window on the coordinator; prints the PIN + QR payload
//! hydra-cli claim --name worker-s1 --pin 123456 --out <dir>
//! hydra-cli status --data-dir <dir>
//! ```
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

            // The coordinator provisions itself first: it is a peer, and the CA is persisted
            // coordinator-locally so a later re-pair is possible (M4·2).
            let coord_dir = std::path::Path::new(&out).join("coordinator");
            let coord = session.provision_coordinator(&coord_dir).map_err(|e| e.to_string())?;
            write_private(&coord_dir.join("identity.cert.der"), coord.identity.cert_chain[0].as_ref())?;
            println!("coordinator provisioned into {} (CA key persisted there, 0600, and nowhere else)", coord_dir.display());
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
        Some("status") => {
            let dir = flag("--data-dir").unwrap_or_else(|| "./hydra-data".into());
            for line in hydra_cli::status::status_for(std::path::Path::new(&dir)) {
                println!("{:<16} {:<24} ({})", line.what, line.value, line.source);
            }
            Ok(())
        }
        _ => Err("usage: hydra-cli <pair|status> ...".into()),
    }
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
