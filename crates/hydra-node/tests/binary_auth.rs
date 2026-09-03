//! **M4 DoD row 3, clause (b) — "API auth enforced" — the oracle against the SHIPPED BINARY.**
//!
//! `session_http.rs` proves the router refuses an unauthenticated request in-process. This test
//! proves the **real `hydra-coordinator` process**, started the way the README starts it (a paired
//! cluster directory, `HYDRA_API_TOKEN` in the environment), serving over TLS with the paired CA,
//! refuses a request with no token and a request with the wrong token — with the structured
//! error the API documents — and does not refuse the right one. Rule 19: it fails if any of that
//! stops being true of the binary rather than of the library.
//!
//! What it cannot see: the token's provenance. The binary takes `HYDRA_API_TOKEN` from the
//! environment; nothing here (or in pairing) mints, stores, or rotates it — that is PROJECT_STATE
//! §8's "API token handling has no story" row, and it is why row 3(b) is recorded as NOT MET even
//! though enforcement is demonstrated here.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

struct Proc(Child);
impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// One HTTPS request over a fresh TLS connection; returns (status line, body).
fn https(port: u16, ca: &tokio_rustls::rustls::pki_types::CertificateDer<'static>, headers: &[(&str, &str)], body: &str) -> (String, String) {
    use tokio_rustls::rustls::{pki_types::ServerName, ClientConfig, RootCertStore};
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let mut roots = RootCertStore::empty();
    roots.add(ca.clone()).expect("trust the paired CA");
    let cfg = Arc::new(ClientConfig::builder().with_root_certificates(roots).with_no_client_auth());
    let name = ServerName::try_from("127.0.0.1").unwrap();
    let mut conn = tokio_rustls::rustls::ClientConnection::new(cfg, name).expect("client connection");
    let mut tcp = TcpStream::connect(("127.0.0.1", port)).expect("tcp connect");
    let mut tls = tokio_rustls::rustls::Stream::new(&mut conn, &mut tcp);
    let mut req = format!("POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    tls.write_all(req.as_bytes()).expect("write request");
    let mut raw = Vec::new();
    let _ = tls.read_to_end(&mut raw); // the server closes; a close_notify error is fine
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text.lines().next().unwrap_or("").to_string();
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

#[test]
fn the_shipped_binary_refuses_an_unauthenticated_request_over_tls_with_a_structured_error() {
    // ---- pair a throwaway cluster the way `hydra-cli pair` does: the CA lives under <dir>/coordinator ----
    let dir = tempfile::tempdir().unwrap();
    let ca = hydra_transport::ClusterCa::new().expect("ca");
    ca.save_private(&dir.path().join("coordinator")).expect("save the paired CA");
    let ca_der = ca.ca_cert_der();
    let port = free_port();
    let token = "binary-auth-oracle-token-0123456789"; // ≥ MIN_API_TOKEN_LEN

    let child = Command::new(env!("CARGO_BIN_EXE_hydra-coordinator"))
        .args(["--pairing-dir", dir.path().to_str().unwrap(), "--api-addr", &format!("127.0.0.1:{port}"), "--data-dir", dir.path().join("data").to_str().unwrap()])
        .env("HYDRA_API_TOKEN", token)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hydra-coordinator");
    let mut proc = Proc(child);

    // Wait for "API listening on https://…" on stderr — the binary's own statement, not a sleep.
    let stderr = proc.0.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut listening = false;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(l) if l.contains("API listening on https://") => { listening = true; break; }
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
    assert!(listening, "the binary never reported listening (did it exit? token/pairing problem?)");

    let body = r#"{"model":"m","messages":[{"role":"user","content":"hello"}]}"#;

    // (1) no Authorization header
    let (status, resp) = https(port, &ca_der, &[], body);
    assert!(status.contains(" 401 "), "no token must be 401, got: {status} / {resp}");
    assert!(resp.contains("missing_or_invalid_api_token"), "structured error code expected, got body: {resp}");

    // (2) the wrong token
    let (status, resp) = https(port, &ca_der, &[("Authorization", "Bearer definitely-not-the-token-0123456789")], body);
    assert!(status.contains(" 401 "), "wrong token must be 401, got: {status} / {resp}");
    assert!(resp.contains("missing_or_invalid_api_token"), "structured error code expected, got body: {resp}");

    // (3) control: the right token is NOT refused as unauthenticated (whatever else the endpoint
    // does with the request — in this seam the generation path is a stub — it is not a 401).
    let (status, resp) = https(port, &ca_der, &[("Authorization", &format!("Bearer {token}"))], body);
    assert!(!status.contains(" 401 "), "the right token was refused: {status} / {resp}");
}
