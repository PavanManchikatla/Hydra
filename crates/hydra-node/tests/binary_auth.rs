//! **M4 DoD row 3, clause (b) — "API auth enforced" — the oracle against the SHIPPED BINARY.**
//!
//! The real `hydra-coordinator` process, started the way the README starts it (a paired and
//! provisioned directory; the API token pairing minted, read from the file), serving over TLS with
//! the paired CA, refuses a request with no token and with the wrong token — with the structured
//! error the API documents — and does not refuse the right one. Rule 19: it fails if any of that
//! stops being true of the binary rather than of the library.
//!
//! Since 2026-09-02 the token has a story (item 2): pairing mints it into `api-token` at 0600 and
//! the binary reads it from there; `HYDRA_API_TOKEN` is an override. Both paths are exercised.

mod common;
use common::*;

fn run_case(via_env: bool) {
    let dir = tempfile::tempdir().unwrap();
    let model = dummy_model(dir.path());
    let (ca, token, _files) = pair_and_provision(dir.path(), &model, ["127.0.0.1:1".parse().unwrap(), "127.0.0.1:2".parse().unwrap()], 1);
    let port = free_port();
    let data = dir.path().join("data");
    let args = ["--pairing-dir", dir.path().to_str().unwrap(), "--api-addr", &format!("127.0.0.1:{port}"), "--data-dir", data.to_str().unwrap()];
    let envs: Vec<(&str, &str)> = if via_env { vec![("HYDRA_API_TOKEN", token.as_str())] } else { vec![] };
    let (_proc, rx) = spawn_coordinator(&args, &envs);
    assert!(wait_listening(&rx, 20), "the binary never reported listening (token/pairing problem?)");
    let ca_der = ca.ca_cert_der();
    let body = r#"{"model":"m","messages":[{"role":"user","content":"hello"}]}"#;

    let (status, resp) = https(port, &ca_der, &[], body, 10);
    assert!(status.contains(" 401 "), "no token must be 401, got: {status} / {resp}");
    assert!(resp.contains("missing_or_invalid_api_token"), "structured error code expected, got body: {resp}");

    let (status, resp) = https(port, &ca_der, &[("Authorization", "Bearer definitely-not-the-token-0123456789abcdef0123456789abcdef")], body, 10);
    assert!(status.contains(" 401 "), "wrong token must be 401, got: {status} / {resp}");
    assert!(resp.contains("missing_or_invalid_api_token"), "structured error code expected, got body: {resp}");

    // Control: the right token is NOT refused as unauthenticated (the dummy model means the session
    // cannot generate — that is a different fact, logged by the binary, and not a 401).
    let (status, resp) = https(port, &ca_der, &[("Authorization", &format!("Bearer {token}"))], body, 10);
    assert!(!status.contains(" 401 "), "the right token was refused: {status} / {resp}");
}

#[test]
fn the_shipped_binary_refuses_an_unauthenticated_request_over_tls_with_a_structured_error() {
    run_case(false); // the token read from the pairing directory's api-token file
}

#[test]
fn the_environment_variable_overrides_the_minted_token_file() {
    run_case(true);
}
