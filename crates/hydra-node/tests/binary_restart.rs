//! **H10(d) — what the shipped binary does with an EXISTING session ledger (2026-09-02).**
//!
//! Coordinator-restart resume in the product process is ESCALATED, not implemented (PROJECT_STATE
//! §7.75): the model carries the coordinator's volatile variables across a crash, and the rule that
//! re-derives them from the WAL is a decision the spec does not state. Until it is ratified, the
//! honest behaviour is a refusal that says so — never a silent second session beside the old one,
//! and never a clobber. This test fails if the binary starts over an existing `commits.wal`.
//!
//! What it cannot see: a resume. There is none to see yet, and this file says so rather than
//! pretending a refusal is a recovery.

use std::process::{Command, Stdio};

#[test]
fn the_shipped_binary_refuses_to_start_over_an_existing_ledger_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let ca = hydra_transport::ClusterCa::new().expect("ca");
    ca.save_private(&dir.path().join("coordinator")).expect("save the paired CA");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("commits.wal"), b"a previous session's ledger").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_hydra-coordinator"))
        .args(["--pairing-dir", dir.path().to_str().unwrap(), "--api-addr", "127.0.0.1:0", "--data-dir", data.to_str().unwrap()])
        .env("HYDRA_API_TOKEN", "binary-restart-oracle-token-0123456789")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("run hydra-coordinator");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the binary must not start over an existing ledger; stderr: {stderr}");
    assert!(stderr.contains("existing session ledger"), "the refusal must name the cause; stderr: {stderr}");
    assert!(stderr.contains("does not resume"), "the refusal must say resume is not implemented; stderr: {stderr}");
    assert_eq!(std::fs::read(data.join("commits.wal")).unwrap(), b"a previous session's ledger", "the old ledger is untouched");
}
