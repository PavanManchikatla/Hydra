//! **Item 2 (2026-09-02) — the API token's story, with its oracles (rule 19).**
//!
//! Pairing mints `api-token` at 0600 beside the CA material; the binary reads it from there and
//! REFUSES to start if the file is group- or world-readable, with a structured error. Rotation is
//! re-pair (the v1 "re-pair issues, does not revoke" posture; stated, not hidden).

mod common;
use common::*;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
fn pairing_mints_the_token_at_0600_with_at_least_32_bytes_of_entropy() {
    let dir = tempfile::tempdir().unwrap();
    let token = hydra_cli::provision::mint_api_token(dir.path()).expect("mint");
    let path = dir.path().join("api-token");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the token file must be 0600, got {mode:o}");
    assert!(token.len() >= 64 && token.chars().all(|c| c.is_ascii_hexdigit()), "≥ 32 random bytes, hex: {token}");
    assert_eq!(hydra_cli::provision::read_api_token(dir.path()).unwrap(), token);
    // Re-pair rotates: a second mint replaces the token.
    let again = hydra_cli::provision::mint_api_token(dir.path()).expect("re-mint");
    assert_ne!(again, token, "re-pair must rotate the token");
}

#[test]
fn the_binary_refuses_to_start_on_a_group_or_world_readable_token_file() {
    let dir = tempfile::tempdir().unwrap();
    let model = dummy_model(dir.path());
    let (_ca, _token, _files) = pair_and_provision(dir.path(), &model, ["127.0.0.1:1".parse().unwrap(), "127.0.0.1:2".parse().unwrap()], 1);
    std::fs::set_permissions(dir.path().join("api-token"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let out = Command::new(coordinator_binary())
        .args(["--pairing-dir", dir.path().to_str().unwrap(), "--api-addr", "127.0.0.1:0", "--data-dir", dir.path().join("data").to_str().unwrap()])
        .env_remove("HYDRA_API_TOKEN")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("run hydra-coordinator");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a 0644 token file must refuse startup; stderr: {stderr}");
    assert!(stderr.contains("api_token_file_permissions"), "structured error code expected; stderr: {stderr}");
    // Control: at 0600 the same directory starts (and the binary tells us it is listening).
    std::fs::set_permissions(dir.path().join("api-token"), std::fs::Permissions::from_mode(0o600)).unwrap();
    let port = free_port();
    let (_proc, rx) = spawn_coordinator(&["--pairing-dir", dir.path().to_str().unwrap(), "--api-addr", &format!("127.0.0.1:{port}"), "--data-dir", dir.path().join("data2").to_str().unwrap()], &[]);
    assert!(wait_listening(&rx, 20), "with the token at 0600 the binary must start");
}
