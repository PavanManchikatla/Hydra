//! **What the shipped binary does with an EXISTING ledger that is NOT this session's (audit M8).**
//!
//! Since 2026-09-03 the product coordinator RESUMES its own session over an existing `commits.wal`
//! (spec §6.5a; the three-window oracle is `restart_e2e.rs`). Resume is bound to the provisioned
//! session: a ledger the coordinator cannot read as this session's — another cluster's, another
//! session's, or bytes that are not a ledger — is REFUSED by name, never adopted, never clobbered,
//! never silently replaced by a second session beside it. This test fails if the binary starts over
//! such a file or touches it.
//!
//! What it cannot see: a resume (that is `restart_e2e.rs`), and a ledger of the same session that
//! was truncated mid-record (the WAL layer's own recovery tests cover the record boundary).

mod common;
use common::*;
use std::process::{Command, Stdio};

#[test]
fn the_shipped_binary_refuses_a_ledger_that_is_not_this_sessions_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let model = dummy_model(dir.path());
    let (_ca, token, _files) = pair_and_provision(dir.path(), &model, ["127.0.0.1:1".parse().unwrap(), "127.0.0.1:2".parse().unwrap()], 1);
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("commits.wal"), b"a previous session's ledger").unwrap();

    let out = Command::new(coordinator_binary())
        .args(["--pairing-dir", dir.path().to_str().unwrap(), "--api-addr", "127.0.0.1:0", "--data-dir", data.to_str().unwrap()])
        .env("HYDRA_API_TOKEN", &token)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("run hydra-coordinator");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the binary must not start over a ledger it cannot read as this session's; stderr: {stderr}");
    assert!(stderr.contains("reopen the session ledger"), "the refusal must name what it was doing; stderr: {stderr}");
    assert!(stderr.contains("audit M8"), "the refusal must name the rule that binds a ledger to its session; stderr: {stderr}");
    assert!(!stderr.contains("API listening"), "and it must refuse BEFORE serving; stderr: {stderr}");
    assert_eq!(std::fs::read(data.join("commits.wal")).unwrap(), b"a previous session's ledger", "the old ledger is untouched");
}
