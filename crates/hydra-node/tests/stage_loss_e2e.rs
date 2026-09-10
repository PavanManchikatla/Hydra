//! **THE ORACLE AT THE M4·0 BAR FOR STAGE LOSS IN THE PRODUCT (ruling 2026-09-09, item 1; rule 27's
//! fourth instance): kill −9 one REAL `hydra-worker` process under the SHIPPED `hydra-coordinator`
//! mid-generation, start its replacement from the SAME bootstrap, reconnect with `Last-Event-ID`,
//! and hold the three assertions plus the stale-epoch probe.**
//!
//! Two lost stages × two windows. **FIRST stage (S1):** W1 after the first event; W2 mid-stream —
//! the SM takes `StageLost` → `ProceedBeginRecovery{truncate_to: 0}` (a new epoch, durable before
//! anything is sent), the replacement is dialled at the same address, `BEGIN_RECOVERY` to both (the
//! survivor S_P freezes at base and keeps position 0; the replacement is empty), the ledger is
//! replayed through the gate (each stage gets only what it lacks). **FINAL stage (S_P) — spec
//! v0.10.5, design authority 2026-09-10, ruling item 1:** the lost stage is DOWNSTREAM of the
//! survivor, which cannot re-emit the activations of the positions it holds (§2.3d), so the BEGIN
//! carries `truncate_to = EMPTY`: the survivor S1 discards its whole KV, both acknowledge `-1`, the
//! ledger is replayed from position 0 as fresh emissions at the new epoch. Then, for both: catch-up,
//! sampler checkpoint, the activation transaction at the new epoch, and the session is re-adopted so
//! a client's `Last-Event-ID` lands on the continuation — exactly the shape a coordinator restart has.
//!
//! Assertions per window: (a) SSE id continuity across the reconnect; (b) prefix ⧺ suffix
//! byte-identical to an uninterrupted run (the pair driver on a second identical pair, stopping at
//! the model's EOS like the product); (c) disk truth — each output position exactly once, dense,
//! the durable ids the reference's; a durable BEGIN and a durable COMPLETE at the new epoch (and,
//! for the final stage, the durable BEGIN carries `EMPTY`). Plus: a frame at epoch 0 to the
//! survivor after the fence is answered `ERR_FENCED` (`SAMPLE_NEXT` to S_P; `APPLY_TOKEN` to S1).
//!
//! Engine-gated (CI status: unavailable, not green). **Cannot see:** a second loss in the same
//! session; a replacement whose bootstrap epoch is not the base (a later recovery — the bootstrap
//! is static at epoch 0); a coordinator restart over disagreeing frontiers (PROJECT_STATE §8).

mod common;
use common::*;

use std::io::{Read, Write};
use std::sync::Arc;

use hydra_wire::SessionFence;
use hydra_worker::pair::{dev_model_path, run_generation, Cluster, Endpoints};
use hydra_worker::sampler::SamplingConfig;
use hydra_worker::worker::WorkerConfig;

fn ref_stage_cfg(fence: &SessionFence, path: &str, k: i32, n_ctx: i32, rank: u16) -> WorkerConfig {
    let is_final = rank == 1;
    WorkerConfig {
        fence: fence.clone(),
        rank: rank as hydra_state::StageRank,
        layer_first: if is_final { k } else { 0 },
        layer_last: if is_final { -1 } else { k },
        is_final,
        receives_tokens: !is_final,
        epoch: 0,
        recovery_id: 0,
        model_path: Some(path.to_string()),
        n_gpu_layers: 0,
        n_ctx,
        sampler_config: if is_final { Some(SamplingConfig::greedy()) } else { None },
        recovery_start: false,
        shard_manifest: None,
    }
}

/// One streaming POST; returns (text events, finish reason). Reads until `stop_after` text events
/// or the stream's end; an overall deadline turns a hang into a failure with text.
fn stream(port: u16, ca: &tokio_rustls::rustls::pki_types::CertificateDer<'static>, token: &str, last_event_id: Option<u64>, stop_after: Option<usize>) -> (Vec<(u64, String)>, Option<String>) {
    use tokio_rustls::rustls::{pki_types::ServerName, ClientConfig, RootCertStore};
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let mut roots = RootCertStore::empty();
    roots.add(ca.clone()).unwrap();
    let cfg = Arc::new(ClientConfig::builder().with_root_certificates(roots).with_no_client_auth());
    let mut conn = tokio_rustls::rustls::ClientConnection::new(cfg, ServerName::try_from("127.0.0.1").unwrap()).unwrap();
    let mut tcp = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_read_timeout(Some(std::time::Duration::from_secs(240))).unwrap();
    let mut tls = tokio_rustls::rustls::Stream::new(&mut conn, &mut tcp);
    let body = r#"{"messages":[{"role":"user","content":"hello"}],"stream":true}"#;
    let mut req = format!("POST /v1/chat/completions HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\nauthorization: Bearer {token}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n", body.len());
    if let Some(k) = last_event_id {
        req.push_str(&format!("Last-Event-ID: {k}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    tls.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let overall = std::time::Instant::now() + std::time::Duration::from_secs(600);
    loop {
        if std::time::Instant::now() > overall {
            eprintln!("[oracle] stream still open after 600 s — giving up on it");
            break;
        }
        match tls.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if let Some(limit) = stop_after {
                    let text = String::from_utf8_lossy(&raw);
                    if parse_sse(text.split("\r\n\r\n").nth(1).unwrap_or("")).len() >= limit {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text.lines().next().unwrap_or("").split_whitespace().nth(1).unwrap_or("").to_string();
    if status == "503" {
        // The coordinator is recovering the session (a stage was lost); it says so with
        // `session_recovering` and a Retry-After — the client comes back.
        eprintln!("[oracle] 503 during recovery — retrying");
        return (Vec::new(), Some("__retry__".to_string()));
    }
    assert!(status == "200", "expected 200: {text}");
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
    (parse_sse(body), parse_sse_finish(body))
}

struct Fixture {
    dir: tempfile::TempDir,
    ca: hydra_transport::ClusterCa,
    token: String,
    fence: SessionFence,
    s1_addr: std::net::SocketAddr,
    s2_addr: std::net::SocketAddr,
    golden_text: String,
    golden: Vec<u32>,
    golden_finish: &'static str,
    max_tokens: usize,
}

fn fixture() -> Option<Fixture> {
    let model = dev_model_path()?;
    let n_layer = hydra_engine_sys::Model::load(&model, 0).expect("model").n_layer();
    let k = (n_layer / 2).max(1);
    let n_ctx = 128;
    let max_tokens = 12usize;
    let dir = tempfile::tempdir().unwrap();
    let s1_addr: std::net::SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let s2_addr: std::net::SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    // The REAL bootstraps the shipped workers boot from — the same files a replacement reuses.
    let (ca, token, files) = pair_and_provision(dir.path(), &model, [s1_addr, s2_addr], k);
    let fence = files.fence.clone();
    // The uninterrupted reference on a second identical in-process pair, stopping at EOS.
    let ref_cluster = Cluster::new().unwrap();
    let r1_id = ref_cluster.issue("worker-s1").unwrap();
    let r2_id = ref_cluster.issue("worker-s2").unwrap();
    let r1 = hydra_worker::pair::spawn_endpoint(ref_stage_cfg(&fence, &model, k, n_ctx, 0), ref_cluster.ca.server_config(&r1_id).unwrap());
    let r2 = hydra_worker::pair::spawn_endpoint(ref_stage_cfg(&fence, &model, k, n_ctx, 1), ref_cluster.ca.server_config(&r2_id).unwrap());
    let tokenizer = hydra_tokenizer::Tokenizer::load_vocab_only(&model).unwrap();
    let admission = hydra_tokenizer::admission::Admission::compute(&tokenizer, hydra_tokenizer::admission::ChatTemplate::ChatMl, &[hydra_tokenizer::admission::ChatMessage::new("user", "hello")]).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let golden: Vec<u32> = rt.block_on(async {
        let connector = ref_cluster.coordinator_connector().unwrap();
        run_generation(&connector, &Endpoints::new(r1, "worker-s1", r2, "worker-s2").with_model(&model), &fence, &SamplingConfig::greedy(), &admission.prompt_tokens, max_tokens).await.expect("reference")
    });
    let golden_text = String::from_utf8_lossy(&tokenizer.decode_bytes(&golden).unwrap()).into_owned();
    let golden_finish = if golden.last().map(|&t| tokenizer.is_eog(t)).unwrap_or(false) { "stop" } else { "length" };
    Some(Fixture { dir, ca, token, fence, s1_addr, s2_addr, golden_text, golden, golden_finish, max_tokens })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lost { First, Final }

/// One window: real workers up, the coordinator up, `kill_after` events read, the `lost` stage's
/// process killed −9 and replaced from the same bootstrap, the client reconnecting until it reaches
/// the continuation.
fn window(f: &Fixture, label: &str, kill_after: usize, lost: Lost) {
    let boot1 = f.dir.path().join("worker-s1.boot");
    let boot2 = f.dir.path().join("worker-s2.boot");
    let mut w1 = spawn_worker(&boot1);
    let mut w2 = spawn_worker(&boot2);
    let port = free_port();
    let data = f.dir.path().join(format!("data-{label}"));
    let args = [
        "--pairing-dir".to_string(), f.dir.path().to_str().unwrap().to_string(),
        "--api-addr".to_string(), format!("127.0.0.1:{port}"),
        "--data-dir".to_string(), data.to_str().unwrap().to_string(),
        "--max-tokens".to_string(), f.max_tokens.to_string(),
    ];
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let (_coord, rx) = spawn_coordinator(&argv, &[]);
    assert!(wait_listening(&rx, 60), "[{label}] the binary never listened");
    let ca_der = f.ca.ca_cert_der();

    // ---- the pre-kill prefix, then kill −9 the lost stage's real process ----
    let (prefix, _) = stream(port, &ca_der, &f.token, None, Some(kill_after));
    assert!(prefix.len() >= kill_after, "[{label}] wanted {kill_after} events before the kill, got {}", prefix.len());
    let (victim, boot) = match lost { Lost::First => (&mut w1, &boot1), Lost::Final => (&mut w2, &boot2) };
    victim.0.kill().expect("kill -9 the lost stage");
    let _ = victim.0.wait();
    // ---- the replacement, from the SAME bootstrap ----
    let _replacement = spawn_worker(boot);

    // ---- reconnect with Last-Event-ID until the continuation is reached ----
    // The coordinator notices the dead link at its next data-plane frame, ends the old stream with
    // `stage_lost`, recovers, and re-adopts; a client reconnecting earlier lands on the old session
    // (finish = stage_lost) and reconnects again. Bounded.
    let last_seen = prefix.last().map(|(id, _)| *id).unwrap_or(0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    let (mut suffix, mut finish) = (Vec::new(), None);
    let mut attempts = 0;
    while std::time::Instant::now() < deadline {
        attempts += 1;
        let (s, fin) = stream(port, &ca_der, &f.token, Some(last_seen), None);
        if fin.as_deref() == Some("stage_lost") || fin.as_deref() == Some("__retry__") || fin.is_none() {
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        }
        suffix = s;
        finish = fin;
        break;
    }
    eprintln!("[{label}] reached the continuation after {attempts} reconnect(s); finish={finish:?}");

    // ---- (a) SSE id continuity across the reconnect ----
    let all: Vec<(u64, String)> = prefix.iter().cloned().chain(suffix.iter().cloned()).collect();
    let ids: Vec<u64> = all.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, (1..=ids.len() as u64).collect::<Vec<_>>(), "[{label}] ids must be dense across the kill: {ids:?}");
    // ---- (b) disk truth, then bytes ----
    let ledger = hydra_coordinator::recovery::read(data.join("commits.wal")).expect("ledger reads back");
    let positions: Vec<i64> = ledger.generated_tokens.iter().map(|&(p, _)| p).collect();
    assert_eq!(positions, (0..f.golden.len() as i64).collect::<Vec<_>>(), "[{label}] each output position exactly once, dense");
    assert_eq!(ledger.generated_token_ids(), f.golden, "[{label}] the durable tokens are the reference's");
    let text: String = all.iter().map(|(_, d)| d.as_str()).collect();
    assert_eq!(text, f.golden_text, "[{label}] prefix ⧺ suffix must equal the uninterrupted run byte for byte — events {all:?}");
    assert_eq!(finish.as_deref(), Some(f.golden_finish), "[{label}] the continuation ends where the reference ended");
    // ---- (c) the control log: fenced forward, re-activated at the new epoch ----
    let control = data.join("control.wal");
    let (_w, records) = hydra_coordinator::control_wal::ControlWal::open(&control, &f.fence.cluster_id, &f.fence.session_id).expect("control wal");
    assert!(records.iter().any(|r| matches!(r, hydra_state::WalRecord::BeginRecovery { target: 1, .. })), "[{label}] BEGIN_RECOVERY at epoch 1 is durable: {records:?}");
    assert!(records.iter().any(|r| matches!(r, hydra_state::WalRecord::ActivationComplete { tuple, .. } if tuple.epoch == 1)), "[{label}] the re-activation at epoch 1 is durable: {records:?}");
    // The spec's choice is on disk (v0.10.5 §7): EMPTY iff the lost stage is downstream of the survivor.
    let want = match lost { Lost::First => 0, Lost::Final => hydra_state::TRUNCATE_TO_EMPTY };
    assert!(records.iter().any(|r| matches!(r, hydra_state::WalRecord::BeginRecovery { target: 1, truncate_to, .. } if *truncate_to == want)),
        "[{label}] the durable BEGIN at epoch 1 carries truncate_to = {want} ({lost:?} stage lost): {records:?}");

    // ---- the stale-epoch probe: the SURVIVOR (now at epoch 1) refuses a frame at epoch 0 ----
    let id = f.ca.issue("coordinator").unwrap();
    let connector = hydra_transport::tcp_mtls::TcpMtls::from_config(f.ca.client_config(&id).unwrap()).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let (addr, name, frame) = match lost {
            Lost::First => (f.s2_addr, "worker-s2", hydra_wire::encode_sample_next(&f.fence, 0, 0, &SamplingConfig::greedy().hash(), hydra_worker::worker::INITIAL_CHECKPOINT_ID)),
            Lost::Final => (f.s1_addr, "worker-s1", hydra_wire::encode_apply_token(&f.fence, 0, 0, f.golden[0], true)),
        };
        let mut c = connector.connect(addr, name).await.expect("dial the survivor");
        c.send(0, &frame).await.unwrap();
        let reply = c.recv().await.expect("a reply, not silence (audit M10)");
        match hydra_wire::decode(&reply.payload, &f.fence).unwrap().1 {
            hydra_wire::Msg::Err { code } => assert_eq!(code, 1, "[{label}] a stale epoch is refused as ERR_FENCED (1), got code {code}"),
            other => panic!("[{label}] a stale-epoch frame must be refused; the survivor answered {other:?}"),
        }
    });
}

#[test]
fn w1_first_stage_killed_after_the_first_event_is_replaced_and_the_stream_resumes_byte_identical() {
    let Some(f) = fixture() else { eprintln!("SKIP: no engine/model (CI status: unavailable)"); return; };
    window(&f, "sl-w1", 1, Lost::First);
}

#[test]
fn w2_first_stage_killed_mid_stream_is_replaced_and_the_stream_resumes_byte_identical() {
    let Some(f) = fixture() else { eprintln!("SKIP: no engine/model (CI status: unavailable)"); return; };
    window(&f, "sl-w2", 4, Lost::First);
}

#[test]
fn w1_final_stage_killed_after_the_first_event_is_replaced_under_empty_and_the_stream_resumes_byte_identical() {
    let Some(f) = fixture() else { eprintln!("SKIP: no engine/model (CI status: unavailable)"); return; };
    window(&f, "sl-final-w1", 1, Lost::Final);
}

#[test]
fn w2_final_stage_killed_mid_stream_is_replaced_under_empty_and_the_stream_resumes_byte_identical() {
    let Some(f) = fixture() else { eprintln!("SKIP: no engine/model (CI status: unavailable)"); return; };
    window(&f, "sl-final-w2", 4, Lost::Final);
}
