//! Shared fixture for the `hydra-node` binary tests: a paired + provisioned cluster directory, and
//! an HTTPS client that trusts its CA.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

pub struct Proc(pub Child);
impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Pair a throwaway cluster the way `hydra-cli pair` does (CA under `<dir>/coordinator`, the API
/// token minted beside it) and provision two stages against `model` (any file: only its hash is
/// read at provisioning; `split` is explicit so no engine is needed to provision).
pub fn pair_and_provision(dir: &Path, model: &str, stage_addrs: [std::net::SocketAddr; 2], split: i32) -> (hydra_transport::ClusterCa, String, hydra_cli::provision::ClusterFiles) {
    let ca = hydra_transport::ClusterCa::new().expect("ca");
    ca.save_private(&dir.join("coordinator")).expect("save the paired CA");
    let token = hydra_cli::provision::mint_api_token(dir).expect("mint the api token");
    let stages = vec![
        hydra_cli::provision::StageSpec { name: "worker-s1".into(), rank: 0, addr: stage_addrs[0] },
        hydra_cli::provision::StageSpec { name: "worker-s2".into(), rank: 1, addr: stage_addrs[1] },
    ];
    let files = hydra_cli::provision::provision(dir, model, &stages, Some(split), 128).expect("provision");
    (ca, token, files)
}

/// A dummy "model" file for tests that never load one (auth, refusal): provisioning hashes it.
pub fn dummy_model(dir: &Path) -> String {
    let p = dir.join("dummy.gguf");
    std::fs::write(&p, b"not a model; only its hash is read at provisioning").unwrap();
    p.to_string_lossy().into_owned()
}

pub fn coordinator_binary() -> &'static str {
    env!("CARGO_BIN_EXE_hydra-coordinator")
}

/// Spawn the coordinator and wait for its own "API listening" line (its statement, not a sleep).
pub fn spawn_coordinator(args: &[&str], envs: &[(&str, &str)]) -> (Proc, std::sync::mpsc::Receiver<String>) {
    let mut cmd = Command::new(coordinator_binary());
    cmd.args(args).stdout(Stdio::null()).stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn hydra-coordinator");
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[coordinator] {line}");
            let _ = tx.send(line);
        }
    });
    (Proc(child), rx)
}

pub fn wait_listening(rx: &std::sync::mpsc::Receiver<String>, secs: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(l) if l.contains("API listening on https://") => return true,
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => return false,
        }
    }
    false
}

/// One HTTPS request over a fresh TLS connection trusting `ca`; returns (status line, body).
pub fn https(port: u16, ca: &tokio_rustls::rustls::pki_types::CertificateDer<'static>, headers: &[(&str, &str)], body: &str, read_timeout_secs: u64) -> (String, String) {
    use tokio_rustls::rustls::{pki_types::ServerName, ClientConfig, RootCertStore};
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let mut roots = RootCertStore::empty();
    roots.add(ca.clone()).expect("trust the paired CA");
    let cfg = Arc::new(ClientConfig::builder().with_root_certificates(roots).with_no_client_auth());
    let name = ServerName::try_from("127.0.0.1").unwrap();
    let mut conn = tokio_rustls::rustls::ClientConnection::new(cfg, name).expect("client connection");
    let mut tcp = TcpStream::connect(("127.0.0.1", port)).expect("tcp connect");
    tcp.set_read_timeout(Some(std::time::Duration::from_secs(read_timeout_secs))).unwrap();
    let mut tls = tokio_rustls::rustls::Stream::new(&mut conn, &mut tcp);
    let mut req = format!("POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    tls.write_all(req.as_bytes()).expect("write request");
    let mut raw = Vec::new();
    let _ = tls.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text.lines().next().unwrap_or("").to_string();
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

/// Parse an SSE body (possibly chunked-encoded) into (id, data) pairs.
pub fn parse_sse(body: &str) -> Vec<(u64, String)> {
    // Strip HTTP/1.1 chunk-size lines if present (hex line, then data).
    let mut text = String::new();
    let mut lines = body.lines().peekable();
    let chunked = lines.peek().map(|l| l.trim().chars().all(|c| c.is_ascii_hexdigit()) && !l.trim().is_empty()).unwrap_or(false);
    if chunked {
        let mut it = body.split("\r\n");
        while let Some(size) = it.next() {
            let Ok(n) = usize::from_str_radix(size.trim(), 16) else { continue };
            if n == 0 {
                break;
            }
            let mut got = 0;
            while got < n {
                let Some(piece) = it.next() else { break };
                text.push_str(piece);
                text.push_str("\r\n");
                got += piece.len() + 2;
            }
        }
    } else {
        text = body.to_string();
    }
    // SSE: an event's data is EVERY `data:` line of the block joined with "\n" (a text that is a
    // bare newline arrives as two empty `data:` lines). The first version of this parser kept only
    // the LAST `data:` line, so the product's final "\n" event read as "" and the restart oracle
    // blamed the binary for a byte the harness had dropped (2026-09-03, rule 12: the harness's own
    // parser gets no presumption of correctness either).
    let mut out = Vec::new();
    for block in text.replace("\r\n", "\n").split("\n\n") {
        let (mut id, mut data): (Option<u64>, Option<Vec<String>>) = (None, None);
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("id:") {
                id = v.trim().parse::<u64>().ok();
            } else if let Some(v) = line.strip_prefix("data:") {
                data.get_or_insert_with(Vec::new).push(v.strip_prefix(' ').unwrap_or(v).to_string());
            }
        }
        if let (Some(id), Some(lines)) = (id, data) {
            out.push((id, lines.join("\n")));
        }
    }
    out
}

pub fn dir_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().to_path_buf()
}
