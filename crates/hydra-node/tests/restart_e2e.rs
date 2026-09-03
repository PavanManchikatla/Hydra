//! **THE ORACLE AT THE M4·0 BAR FOR COORDINATOR-RESTART RESUME (spec §6.5a; ruling 2026-09-02, item
//! 4d): kill −9 the REAL `hydra-coordinator` process mid-generation, restart it with the same
//! session and pairing directory, reconnect with `Last-Event-ID`, and hold the three assertions.**
//!
//! Windows (all adversarial; one lucky point is a demo):
//!   W1 — after the FIRST SSE event has been handed to the client;
//!   W2 — mid-stream, after several events;
//!   W3 — the window the spec now names: `ACTIVATION_COMMIT_INTENT` fsynced, the process killed
//!        BEFORE `COMMIT_ACTIVATION` is sent (`HYDRA_CRASH_AT=intent-durable`). The restart must
//!        fence forward and never resume that intent; and a stage that later hears the stale epoch
//!        must answer `ERR_FENCED`.
//!
//! Assertions, each window: (a) SSE id continuity across the reconnect (no gap, no repeat); (b)
//! the committed prefix ⧺ the resumed suffix is byte-identical to an uninterrupted run (the pair
//! driver on an identical second stage pair); (c) disk truth — the ledger holds each output
//! position exactly once, dense, in order.
//!
//! Engine-gated (CI status: unavailable, not green). **Cannot see:** the direct-FWD/D1 topology; a
//! restart that classifies as COMPLETE-but-unfinalized or UNSERVABLE (reported, not driven — §8);
//! more than one coordinator crash per session in this file.

mod common;
use common::*;

use std::io::{Read, Write};
use std::sync::Arc;

use hydra_wire::SessionFence;
use hydra_worker::pair::{dev_model_path, run_generation, Cluster, Endpoints};
use hydra_worker::sampler::SamplingConfig;
use hydra_worker::worker::WorkerConfig;

fn stage_cfg(fence: &SessionFence, path: &str, k: i32, n_ctx: i32, rank: u16) -> WorkerConfig {
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

/// Open a streaming POST and read SSE events until `stop_after` events have arrived (or the stream
/// ends); returns the events seen. The connection is dropped on return.
fn stream_until(port: u16, ca: &tokio_rustls::rustls::pki_types::CertificateDer<'static>, token: &str, last_event_id: Option<u64>, stop_after: Option<usize>) -> Vec<(u64, String)> {
    use tokio_rustls::rustls::{pki_types::ServerName, ClientConfig, RootCertStore};
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let mut roots = RootCertStore::empty();
    roots.add(ca.clone()).unwrap();
    let cfg = Arc::new(ClientConfig::builder().with_root_certificates(roots).with_no_client_auth());
    let mut conn = tokio_rustls::rustls::ClientConnection::new(cfg, ServerName::try_from("127.0.0.1").unwrap()).unwrap();
    let mut tcp = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_read_timeout(Some(std::time::Duration::from_secs(240))).unwrap();
    let mut tls = tokio_rustls::rustls::Stream::new(&mut conn, &mut tcp);
    let body = r#"{"model":"m","messages":[{"role":"user","content":"hello"}],"stream":true}"#;
    let mut req = format!("POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    // A reconnect presents its cursor even when it is 0: the client's stream may have died before
    // the first event (window 3), and a presented cursor is what distinguishes a reconnect from a
    // fresh request at the server (spec §6.5a attach rule).
    if let Some(k) = last_event_id {
        req.push_str(&format!("Last-Event-ID: {k}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    tls.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    // An OVERALL deadline, not only the per-read timeout above: the server's SSE keepalives reset a
    // per-read timeout, so a session that never ends read as a client that waits forever (the
    // third window sat 15 minutes that way on 2026-09-03). A stream still open after this many
    // seconds is a failure with text, never a hung suite.
    let overall = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        if std::time::Instant::now() > overall {
            eprintln!("[oracle] stream still open after 300 s — giving up on it");
            break;
        }
        match tls.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if let Some(limit) = stop_after {
                    let text = String::from_utf8_lossy(&raw);
                    let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
                    if parse_sse(body).len() >= limit {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let tail_from = text.len().saturating_sub(600);
    eprintln!("[oracle] stream tail (last {} bytes): {:?}", text.len() - tail_from, &text[tail_from..]);
    assert!(text.lines().next().unwrap_or("").contains(" 200 "), "expected 200: {text}");
    parse_sse(text.split("\r\n\r\n").nth(1).unwrap_or(""))
}

struct Fixture {
    dir: tempfile::TempDir,
    ca: hydra_transport::ClusterCa,
    token: String,
    fence: SessionFence,
    stages_arg: String,
    golden_text: String,
    golden: Vec<u32>,
    model: String,
    k: i32,
    n_ctx: i32,
    max_tokens: usize,
}

fn fixture() -> Option<Fixture> {
    let model = dev_model_path()?;
    let n_layer = hydra_engine_sys::Model::load(&model, 0).expect("model").n_layer();
    let k = (n_layer / 2).max(1);
    let n_ctx = 128;
    let max_tokens = 12usize;
    let dir = tempfile::tempdir().unwrap();
    let (ca, token, files) = pair_and_provision(dir.path(), &model, ["127.0.0.1:1".parse().unwrap(), "127.0.0.1:2".parse().unwrap()], k);
    let fence = files.fence.clone();
    // The stages the binary drives: MULTI-connection endpoints, so a restarted coordinator (and the
    // test's own stale-epoch probe) can connect while an older connection is still being torn down.
    let s1_id = ca.issue("worker-s1").unwrap();
    let s2_id = ca.issue("worker-s2").unwrap();
    let s1 = hydra_worker::pair::spawn_multiconn_endpoint(stage_cfg(&fence, &model, k, n_ctx, 0), ca.server_config(&s1_id).unwrap());
    let s2 = hydra_worker::pair::spawn_multiconn_endpoint(stage_cfg(&fence, &model, k, n_ctx, 1), ca.server_config(&s2_id).unwrap());
    // The uninterrupted reference on a second identical pair.
    let ref_cluster = Cluster::new().unwrap();
    let r1_id = ref_cluster.issue("worker-s1").unwrap();
    let r2_id = ref_cluster.issue("worker-s2").unwrap();
    let r1 = hydra_worker::pair::spawn_endpoint(stage_cfg(&fence, &model, k, n_ctx, 0), ref_cluster.ca.server_config(&r1_id).unwrap());
    let r2 = hydra_worker::pair::spawn_endpoint(stage_cfg(&fence, &model, k, n_ctx, 1), ref_cluster.ca.server_config(&r2_id).unwrap());
    let tokenizer = hydra_tokenizer::Tokenizer::load_vocab_only(&model).unwrap();
    let admission = hydra_tokenizer::admission::Admission::compute(&tokenizer, hydra_tokenizer::admission::ChatTemplate::ChatMl, &[hydra_tokenizer::admission::ChatMessage::new("user", "hello")]).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let golden: Vec<u32> = rt.block_on(async {
        let connector = ref_cluster.coordinator_connector().unwrap();
        run_generation(&connector, &Endpoints::new(r1, "worker-s1", r2, "worker-s2"), &fence, &SamplingConfig::greedy(), &admission.prompt_tokens, max_tokens).await.expect("reference")
    });
    let golden_text = String::from_utf8_lossy(&tokenizer.decode_bytes(&golden).unwrap()).into_owned();
    Some(Fixture { dir, ca, token, fence, stages_arg: format!("worker-s1={s1},worker-s2={s2}"), golden_text, golden, model, k, n_ctx, max_tokens })
}

fn coordinator_args(f: &Fixture, port: u16, data: &std::path::Path) -> Vec<String> {
    vec![
        "--pairing-dir".into(), f.dir.path().to_str().unwrap().into(),
        "--api-addr".into(), format!("127.0.0.1:{port}"),
        "--data-dir".into(), data.to_str().unwrap().into(),
        "--stages".into(), f.stages_arg.clone(),
        "--max-tokens".into(), f.max_tokens.to_string(),
    ]
}

/// Run one window: start, stream `kill_after` events (or crash at the injected point), kill −9,
/// restart, reconnect with Last-Event-ID, and hold the three assertions.
fn window(f: &Fixture, label: &str, kill_after: Option<usize>, crash_at: Option<&str>) {
    let port = free_port();
    let data = f.dir.path().join(format!("data-{label}"));
    let args = coordinator_args(f, port, &data);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let envs: Vec<(&str, &str)> = crash_at.map(|c| vec![("HYDRA_CRASH_AT", c)]).unwrap_or_default();
    let (mut proc, rx) = spawn_coordinator(&argv, &envs);
    assert!(wait_listening(&rx, 30), "[{label}] the binary never listened");
    let ca_der = f.ca.ca_cert_der();

    // ---- the pre-kill prefix ----
    let prefix: Vec<(u64, String)> = match (kill_after, crash_at) {
        (Some(n), None) => {
            let seen = stream_until(port, &ca_der, &f.token, None, Some(n));
            assert!(seen.len() >= n, "[{label}] wanted {n} events before the kill, got {}", seen.len());
            // ---- kill -9 the real coordinator process ----
            proc.0.kill().expect("kill -9");
            let _ = proc.0.wait();
            seen
        }
        (None, Some(_)) => {
            // The request makes the coordinator activate; the injected crash aborts it with the
            // INTENT durable and COMMIT unsent. The stream ends with nothing (the process died).
            let seen = stream_until(port, &ca_der, &f.token, None, None);
            let _ = proc.0.wait();
            assert!(seen.is_empty(), "[{label}] no token can have been produced before the injected crash: {seen:?}");
            seen
        }
        _ => unreachable!(),
    };
    let last_seen = prefix.last().map(|(id, _)| *id).unwrap_or(0);
    let control = data.join("control.wal");
    assert!(data.join("commits.wal").exists(), "[{label}] the ledger exists after the kill");

    // ---- restart with the same session + pairing dir (no crash injection this time) ----
    let port2 = free_port();
    let args2 = coordinator_args(f, port2, &data);
    let argv2: Vec<&str> = args2.iter().map(String::as_str).collect();
    let (_proc2, rx2) = spawn_coordinator(&argv2, &[]);
    assert!(wait_listening(&rx2, 30), "[{label}] the restarted binary never listened");
    let suffix = stream_until(port2, &ca_der, &f.token, Some(last_seen), None);

    // ---- (a) SSE id continuity across the reconnect ----
    let all: Vec<(u64, String)> = prefix.iter().cloned().chain(suffix.iter().cloned()).collect();
    let ids: Vec<u64> = all.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, (1..=ids.len() as u64).collect::<Vec<_>>(), "[{label}] ids must be dense across the kill: {ids:?}");
    assert!(suffix.first().map(|(id, _)| *id == last_seen + 1).unwrap_or(last_seen > 0 || !all.is_empty()), "[{label}] the resumed stream must start at Last-Event-ID + 1");

    // ---- (b) disk truth — checked FIRST so a byte divergence below names its token ----
    let ledger = hydra_coordinator::recovery::read(data.join("commits.wal")).expect("ledger reads back");
    let positions: Vec<i64> = ledger.generated_tokens.iter().map(|&(p, _)| p).collect();
    assert_eq!(positions, (0..f.golden.len() as i64).collect::<Vec<_>>(), "[{label}] each output position exactly once, dense");
    assert_eq!(ledger.generated_token_ids(), f.golden, "[{label}] the durable tokens are the reference's");
    // ---- (c) byte-identical to the uninterrupted run ----
    let text: String = all.iter().map(|(_, d)| d.as_str()).collect();
    assert_eq!(
        text, f.golden_text,
        "[{label}] prefix ⧺ suffix must equal the uninterrupted run byte for byte — events (id, text): {all:?}; durable ids {:?}; golden ids {:?}",
        ledger.generated_token_ids(), f.golden
    );

    let (_w, records) = hydra_coordinator::control_wal::ControlWal::open(&control, &f.fence.cluster_id, &f.fence.session_id).expect("control wal");
    let begins: Vec<_> = records.iter().filter(|r| matches!(r, hydra_state::WalRecord::BeginRecovery { .. })).collect();
    assert!(!begins.is_empty(), "[{label}] the restart fenced forward: a BEGIN_RECOVERY is durable: {records:?}");
    assert!(records.iter().any(|r| matches!(r, hydra_state::WalRecord::ActivationComplete { tuple, .. } if tuple.epoch >= 1)), "[{label}] the re-activation at the new epoch is durable; control records: {records:?}");
    let _ = (&f.model, f.k, f.n_ctx);
}

#[test]
fn w1_killed_after_the_first_event_resumes_gapless_and_byte_identical() {
    let Some(f) = fixture() else { eprintln!("SKIP: no engine/model (CI status: unavailable)"); return; };
    window(&f, "w1", Some(1), None);
}

#[test]
fn w2_killed_mid_stream_resumes_gapless_and_byte_identical() {
    let Some(f) = fixture() else { eprintln!("SKIP: no engine/model (CI status: unavailable)"); return; };
    window(&f, "w2", Some(4), None);
}

#[test]
fn w3_intent_durable_commit_unsent_fences_forward_and_a_stale_epoch_is_refused() {
    let Some(f) = fixture() else { eprintln!("SKIP: no engine/model (CI status: unavailable)"); return; };
    window(&f, "w3", None, Some("intent-durable"));

    // The stale-epoch half: after the fence-forward (epoch ≥ 1), a frame at epoch 0 reaching a stage
    // must be REFUSED as fenced, not applied. The test dials as `coordinator` with the paired CA.
    let id = f.ca.issue("coordinator").unwrap();
    let connector = hydra_transport::tcp_mtls::TcpMtls::from_config(f.ca.client_config(&id).unwrap()).unwrap();
    let s1_addr: std::net::SocketAddr = f.stages_arg.split(',').next().unwrap().split('=').nth(1).unwrap().parse().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let mut c = connector.connect(s1_addr, "worker-s1").await.expect("dial S1");
        c.send(0, &hydra_wire::encode_apply_token(&f.fence, 0, 0, 1, true)).await.unwrap();
        let reply = c.recv().await.expect("a reply, not silence (audit M10)");
        match hydra_wire::decode(&reply.payload, &f.fence).unwrap().1 {
            // `ErrCode::ERR_FENCED = 1` in hydra-proto's generated enum (F2 rejection; the worker's own const).
            hydra_wire::Msg::Err { code } => assert_eq!(code, 1, "a stale epoch is refused as ERR_FENCED (1), got code {code}"),
            other => panic!("a stale-epoch frame must be refused; the stage answered {other:?}"),
        }
    });
}
