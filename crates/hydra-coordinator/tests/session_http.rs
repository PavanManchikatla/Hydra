//! M2 slice 5 sub-slice B — the HTTP surface + the emit-after-commit law.
//!
//! Session-level (the gate is the law — proven by ABSENCE):
//!   * `emit_after_commit_gate_holds_by_absence` — with `fdatasync` stubbed to fail, nothing is
//!     emitted past the last durable position.
//!   * `multibyte_glyph_straddling_a_commit_boundary_emits_whole` — an emoji split across a group
//!     commit buffers and emits whole.
//!   * `deadline_path_commits_a_sub_k_group` — the 50 ms trigger commits below k.
//!   * `backpressure_pauses_at_the_commit_stage` — a full emit buffer pauses committing.
//!
//! HTTP (axum): dense SSE ids + emit-after-commit text; `Last-Event-ID` byte-identical resume;
//! `Idempotency-Key` dedups session creation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use flatbuffers::FlatBufferBuilder;
use hydra_coordinator::{
    router, AppState, CommitOutcome, CommitStream, Durability, PieceSource, SampledToken, Session, WalFenceCtx,
};
use hydra_proto::wal;
use http_body_util::BodyExt;
use tokio::sync::mpsc;
use tower::ServiceExt;

// ---------------- stubs ----------------

fn wal_fence() -> WalFenceCtx {
    WalFenceCtx { cluster_id: [1; 16], session_id: [2; 16], model_instance_id: [3; 16], manifest_hash: [4; 32], epoch: 0, recovery_id: 0, activation_attempt_id: 0 }
}

/// A valid `SamplerCheckpointRec` for output position `pos` (I19-satisfying at `last=pos`).
fn snapshot(pos: i64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let rng_key = Some(fbb.create_vector(&[0u8; 8]));
    let empty = Some(fbb.create_vector::<u8>(&[]));
    let cfg = Some(fbb.create_vector(&[7u8; 32]));
    let sum = Some(fbb.create_vector(&[9u8; 32]));
    let rec = wal::SamplerCheckpointRec::create(&mut fbb, &wal::SamplerCheckpointRecArgs {
        checkpoint_id: 1, rng_key, rng_counter: 0, generated_through_output_pos: pos,
        serialized_grammar_state: empty, serialized_penalty_state: empty, sampled_output_pos: pos,
        sampling_config_hash: cfg, state_checksum: sum,
    });
    fbb.finish(rec, None);
    fbb.finished_data().to_vec()
}

/// A durability sink that succeeds (tracks length) — a working `fdatasync`.
#[derive(Default)]
struct OkDisk { len: u64, appends: Arc<AtomicUsize> }
impl Durability for OkDisk {
    fn append(&mut self, _rt: u16, _fl: u16, payload: &[u8]) -> Result<u64, hydra_wal::WalError> {
        let off = self.len;
        self.len += payload.len() as u64;
        self.appends.fetch_add(1, Ordering::SeqCst);
        Ok(off)
    }
    fn durable_len(&self) -> u64 { self.len }
}

/// A durability sink whose `fdatasync` never succeeds (stall/failure) — nothing is ever durable.
struct FailingDisk;
impl Durability for FailingDisk {
    fn append(&mut self, _rt: u16, _fl: u16, _p: &[u8]) -> Result<u64, hydra_wal::WalError> {
        Err(hydra_wal::WalError::Io(std::io::Error::other("stubbed fdatasync stall")))
    }
    fn durable_len(&self) -> u64 { 0 }
}

/// Piece source from an explicit map (defaults to one byte = the token id, so token 72 → "H").
struct MapPieces(HashMap<u32, Vec<u8>>);
impl PieceSource for MapPieces {
    fn piece(&self, token: u32) -> Vec<u8> {
        self.0.get(&token).cloned().unwrap_or_else(|| vec![token as u8])
    }
    fn n_vocab(&self) -> u32 {
        1 << 20
    }
}

fn session(disk: Box<dyn Durability>, pieces: Box<dyn PieceSource>, k: usize, cap: usize) -> Session {
    Session::new(CommitStream::with_durability(disk), wal_fence(), pieces, k, cap)
}

// ---------------- Session-level: the gate is the law ----------------

#[test]
fn emit_after_commit_gate_holds_by_absence() {
    let mut s = session(Box::new(FailingDisk), Box::new(MapPieces(HashMap::new())), 4, 100);
    for pos in 0..4 {
        s.push_sampled(SampledToken { output_pos: pos, token_id: b'x' as u32, snapshot: snapshot(pos) }).unwrap();
    }
    // The durable append fails → the commit errors → NOTHING is emitted.
    let r = s.try_commit_by_count();
    assert!(r.is_err(), "a failed fdatasync must surface as an error, not a silent emit");
    assert_eq!(s.durable_pos(), -1, "durable pos never advanced");
    assert_eq!(s.last_event_id(), 0, "no event exists");
    assert_eq!(s.log().full_text(), "", "no bytes left the process past durability");
}

#[test]
fn multibyte_glyph_straddling_a_commit_boundary_emits_whole() {
    // "😀" = F0 9F 98 80, split across two tokens that fall in two separate group commits (k=1).
    let mut map = HashMap::new();
    map.insert(1u32, vec![0xF0, 0x9F]);
    map.insert(2u32, vec![0x98, 0x80]);
    let mut s = session(Box::new(OkDisk::default()), Box::new(MapPieces(map)), 1, 100);

    s.push_sampled(SampledToken { output_pos: 0, token_id: 1, snapshot: snapshot(0) }).unwrap();
    let out = s.try_commit_by_count().unwrap();
    assert!(matches!(out, CommitOutcome::Committed(ref e) if e.is_empty()), "commit A durable but emits no bytes (mid-glyph)");
    assert_eq!(s.durable_pos(), 0, "the token IS durable");
    assert_eq!(s.last_event_id(), 0, "…but nothing emitted yet — the glyph is incomplete");

    s.push_sampled(SampledToken { output_pos: 1, token_id: 2, snapshot: snapshot(1) }).unwrap();
    let out = s.try_commit_by_count().unwrap();
    match out {
        CommitOutcome::Committed(evs) => {
            assert_eq!(evs.len(), 1);
            assert_eq!(evs[0].data, "😀", "the whole glyph emits once complete");
        }
        other => panic!("expected Committed, got {other:?}"),
    }
    assert_eq!(s.log().full_text(), "😀");
}

#[test]
fn deadline_path_commits_a_sub_k_group() {
    let mut s = session(Box::new(OkDisk::default()), Box::new(MapPieces(HashMap::new())), 8, 100);
    for (pos, tok) in [(0i64, b'h'), (1, b'i')] {
        s.push_sampled(SampledToken { output_pos: pos, token_id: tok as u32, snapshot: snapshot(pos) }).unwrap();
    }
    assert!(matches!(s.try_commit_by_count().unwrap(), CommitOutcome::Nothing), "below k, count trigger does not fire");
    match s.commit_on_deadline().unwrap() {
        CommitOutcome::Committed(evs) => assert_eq!(evs[0].data, "hi", "the 50ms deadline commits the sub-k group"),
        other => panic!("expected Committed, got {other:?}"),
    }
    assert_eq!(s.durable_pos(), 1);
}

#[test]
fn backpressure_pauses_at_the_commit_stage() {
    let mut s = session(Box::new(OkDisk::default()), Box::new(MapPieces(HashMap::new())), 1, 2); // cap = 2
    for (pos, tok) in [(0i64, b'a'), (1, b'b')] {
        s.push_sampled(SampledToken { output_pos: pos, token_id: tok as u32, snapshot: snapshot(pos) }).unwrap();
        assert!(matches!(s.try_commit_by_count().unwrap(), CommitOutcome::Committed(_)));
    }
    // Buffer now full (2 emitted, undrained). The next commit PAUSES rather than emitting ahead.
    s.push_sampled(SampledToken { output_pos: 2, token_id: b'c' as u32, snapshot: snapshot(2) }).unwrap();
    assert!(matches!(s.try_commit_by_count().unwrap(), CommitOutcome::Paused), "full buffer pauses the commit stage");
    assert_eq!(s.durable_pos(), 1, "the paused token is not committed");

    // Client reads → backpressure relieved → committing resumes.
    s.client_drained(2);
    assert!(matches!(s.try_commit_by_count().unwrap(), CommitOutcome::Committed(_)));
    assert_eq!(s.durable_pos(), 2);
}

// ---------------- HTTP: axum surface ----------------

fn make_app(gen_calls: Arc<AtomicUsize>) -> axum::Router {
    // Canned generation: "Hello" as five 1-byte ascii tokens, each its own commit (k=1).
    let tokens: Vec<(i64, u32)> = "Hello".bytes().enumerate().map(|(i, b)| (i as i64, b as u32)).collect();
    let gen_fn: hydra_coordinator::GenFn = Arc::new(move |_prompt: String| {
        gen_calls.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel(16);
        let toks = tokens.clone();
        tokio::spawn(async move {
            for (pos, tok) in toks {
                let _ = tx.send(SampledToken { output_pos: pos, token_id: tok, snapshot: snapshot(pos) }).await;
            }
        });
        rx
    });
    let make_session: Arc<dyn Fn() -> Session + Send + Sync> =
        Arc::new(|| session(Box::new(OkDisk::default()), Box::new(MapPieces(HashMap::new())), 1, 1000));
    router(AppState::new(make_session, gen_fn, test_auth()))
}

/// The API auth every test request must satisfy (report Addendum 2 §E1). Fixed token + the
/// loopback host/origin allow-list, so the happy path exercises the real check rather than a
/// bypass.
fn test_auth() -> hydra_coordinator::ApiAuth {
    hydra_coordinator::ApiAuth::loopback(API_TOKEN, 0)
}

const API_TOKEN: &str = "test-api-token";
const AUTH_HEADERS: [(&str, &str); 2] = [("authorization", "Bearer test-api-token"), ("host", "127.0.0.1:0")];

/// Prepend the auth headers every request needs, so each test states only what it is about.
fn with_auth<'a>(extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    AUTH_HEADERS.iter().copied().chain(extra.iter().copied()).collect()
}

/// Parse an SSE body into (id, data) pairs.
fn parse_sse(body: &str) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    for block in body.split("\n\n") {
        let (mut id, mut data) = (None, None);
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("id:") {
                id = v.trim().parse::<u64>().ok();
            } else if let Some(v) = line.strip_prefix("data:") {
                data = Some(v.strip_prefix(' ').unwrap_or(v).to_string());
            }
        }
        if let (Some(id), Some(data)) = (id, data) {
            out.push((id, data));
        }
    }
    out
}

async fn post(app: &axum::Router, headers: &[(&str, &str)], body: &str) -> String {
    let mut req = Request::builder().method("POST").uri("/v1/chat/completions");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn sse_stream_has_dense_ids_and_emit_after_commit_text() {
    let app = make_app(Arc::new(AtomicUsize::new(0)));
    let body = post(&app, &with_auth(&[("content-type", "application/json")]), r#"{"messages":[{"role":"user","content":"hi"}],"stream":true}"#).await;
    let events = parse_sse(&body);
    assert_eq!(events.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5], "dense ids");
    let text: String = events.iter().map(|(_, d)| d.as_str()).collect();
    assert_eq!(text, "Hello", "emitted text is the durable generation");
}

#[tokio::test]
async fn last_event_id_resume_yields_byte_identical_suffix() {
    let app = make_app(Arc::new(AtomicUsize::new(0)));
    let key = "resume-key";
    let full = parse_sse(&post(&app, &with_auth(&[("idempotency-key", key)]), "{}").await);
    let full_text: String = full.iter().map(|(_, d)| d.as_str()).collect();

    // Reconnect at EVERY cut point → byte-identical suffix (same session via the idempotency key).
    for cut in 0..=full.len() as u64 {
        let resumed = parse_sse(&post(&app, &with_auth(&[("idempotency-key", key), ("last-event-id", &cut.to_string())]), "{}").await);
        let prefix: String = full.iter().take(cut as usize).map(|(_, d)| d.as_str()).collect();
        let suffix: String = resumed.iter().map(|(_, d)| d.as_str()).collect();
        assert_eq!(format!("{prefix}{suffix}"), full_text, "resume at {cut} is byte-identical");
        // Resumed ids are exactly those > cut.
        assert!(resumed.iter().all(|(id, _)| *id > cut));
    }
}

#[tokio::test]
async fn idempotency_key_dedups_session_creation() {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = make_app(calls.clone());
    let key = "idem-key";
    let a = post(&app, &with_auth(&[("idempotency-key", key)]), "{}").await;
    let b = post(&app, &with_auth(&[("idempotency-key", key)]), "{}").await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "duplicate POST creates ONE session / one generation");
    let ta: String = parse_sse(&a).iter().map(|(_, d)| d.clone()).collect();
    let tb: String = parse_sse(&b).iter().map(|(_, d)| d.clone()).collect();
    assert_eq!(ta, tb, "same session ⇒ same response body");
    assert_eq!(ta, "Hello");
}

// ---------------- Option A: one active session per model instance (M4·1 reserved-hook audit) ----

/// A generation source that produces nothing until `release` fires, so a session can be held
/// *active* while a second request arrives. Without this the canned generator finishes in
/// microseconds and the concurrency window never exists to test.
fn make_blocking_app(started: Arc<AtomicUsize>, release: Arc<tokio::sync::Notify>) -> axum::Router {
    let gen_fn: hydra_coordinator::GenFn = Arc::new(move |_prompt: String| {
        started.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel(16);
        let release = release.clone();
        tokio::spawn(async move {
            release.notified().await;
            for (pos, b) in "Hello".bytes().enumerate() {
                let _ = tx.send(SampledToken { output_pos: pos as i64, token_id: b as u32, snapshot: snapshot(pos as i64) }).await;
            }
        });
        rx
    });
    let make_session: Arc<dyn Fn() -> Session + Send + Sync> =
        Arc::new(|| session(Box::new(OkDisk::default()), Box::new(MapPieces(HashMap::new())), 1, 1000));
    router(AppState::new(make_session, gen_fn, test_auth()))
}

async fn post_raw(app: &axum::Router, headers: &[(&str, &str)], body: &str) -> (StatusCode, String) {
    let mut req = Request::builder().method("POST").uri("/v1/chat/completions");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// **Spec §1.4 Option A is enforced, and Option B stays RESERVED.** *"One active session per model
/// instance (Option A; Option B [RESERVED])."*
///
/// This is the reserved-hook audit's coordinator half, and it is the one that found a real gap: the
/// HTTP surface minted a **fresh session on every POST** that did not match a known
/// `Idempotency-Key`, so a second client simply started a second generation against the same
/// workers. Option B was not absent — it was the default. The stage state machines, the sampler at
/// S_P and the commit stream are all single-session by construction, so what read as an
/// unimplemented feature was really an admission gap.
///
/// The refusal is **structured and names the holder**, per the same discipline as P2·4's admission
/// refusals: a refusal that does not say what is in the way is not actionable.
#[tokio::test]
async fn a_second_concurrent_session_is_refused_option_b_stays_reserved() {
    let started = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let app = make_blocking_app(started.clone(), release.clone());

    // Client A opens a session and holds it open (its generator is blocked).
    let a_app = app.clone();
    let a = tokio::spawn(async move { post_raw(&a_app, &with_auth(&[("idempotency-key", "A")]), "{}").await });
    while started.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }

    // Client B asks for a *different* session while A is still generating.
    let (status, body) = post_raw(&app, &with_auth(&[("idempotency-key", "B")]), "{}").await;
    assert_eq!(status, StatusCode::CONFLICT, "a second concurrent session must be refused, not admitted");
    assert!(body.contains("one_active_session_per_model_instance"), "structured code, got: {body}");
    assert!(body.contains("active_session"), "the refusal names the session holding the instance, got: {body}");
    assert_eq!(started.load(Ordering::SeqCst), 1, "and NO second generation was started");

    // Reconnecting to the *running* session is still fine — that is what Idempotency-Key is for.
    // (Proven by the fence not firing for key "A"; the body is the live stream, released below.)
    release.notify_waiters();
    release.notify_one();
    let (a_status, a_body) = a.await.unwrap();
    assert_eq!(a_status, StatusCode::OK);
    assert_eq!(parse_sse(&a_body).iter().map(|(_, d)| d.as_str()).collect::<String>(), "Hello");

    // The fence is a *concurrency* fence, not a one-shot: once A is done, a new session is admitted.
    let (c_status, _) = post_raw(&app, &with_auth(&[("idempotency-key", "C")]), "{}").await;
    assert_eq!(c_status, StatusCode::OK, "the instance is free again once the active session finishes");
    assert_eq!(started.load(Ordering::SeqCst), 2, "exactly one further generation was started");
}

// ---------------- report Addendum 2 §E1: the LAN API is an attack surface (M4·1 b) ----------------

/// **Auth is enforced even on the LAN**, and its absence is a refusal rather than a warning.
///
/// The threat is not a malicious user — it is a *browser* on the same network, driven by a page the
/// user is merely visiting, holding the victim's network position. Ollama-class servers have been
/// exploited exactly this way. So "unauthenticated but only on localhost" is not a safe state, and
/// `AppState` has no constructor that produces one.
#[tokio::test]
async fn the_api_refuses_an_unauthenticated_request() {
    let app = make_app(Arc::new(AtomicUsize::new(0)));

    let (no_token, body) = post_raw(&app, &[("host", "127.0.0.1:0")], "{}").await;
    assert_eq!(no_token, StatusCode::UNAUTHORIZED, "no Authorization header must be refused");
    assert!(body.contains("missing_or_invalid_api_token"), "structured code, got: {body}");

    let (bad_token, _) =
        post_raw(&app, &[("authorization", "Bearer wrong-token"), ("host", "127.0.0.1:0")], "{}").await;
    assert_eq!(bad_token, StatusCode::UNAUTHORIZED, "a wrong token must be refused");

    // A prefix of the real token must fail exactly like an unrelated one — the comparison is over
    // BLAKE3 digests precisely so a matching prefix is not observable.
    let (prefix, _) = post_raw(&app, &[("authorization", "Bearer test-api"), ("host", "127.0.0.1:0")], "{}").await;
    assert_eq!(prefix, StatusCode::UNAUTHORIZED);

    // Control: the correct token on the same app is admitted, so the refusals above are caused by
    // the token and not by a harness that could never have succeeded.
    let (ok, _) = post_raw(&app, &with_auth(&[]), "{}").await;
    assert_eq!(ok, StatusCode::OK);
}

/// **DNS-rebinding defence:** a rebind resolves an attacker-controlled name to the victim's
/// address, so the request arrives carrying the attacker's name in `Host`. Refusing an unexpected
/// `Host` breaks the attack at the application layer even when DNS and the network cooperate.
#[tokio::test]
async fn the_api_refuses_a_foreign_host_header() {
    let app = make_app(Arc::new(AtomicUsize::new(0)));

    for host in ["evil.example.com", "hydra.attacker.test:80", ""] {
        let (status, body) =
            post_raw(&app, &[("authorization", "Bearer test-api-token"), ("host", host)], "{}").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "Host {host:?} must be refused");
        assert!(body.contains("host_not_allowed"), "structured code, got: {body}");
    }
}

/// **CSRF defence:** a cross-site POST carries an `Origin`. A foreign one is refused; a plain API
/// client that sends none is served (absence is normal, and must not be the way around the check —
/// which is why `Host` is checked unconditionally above).
#[tokio::test]
async fn the_api_refuses_a_cross_site_origin_but_allows_a_plain_client() {
    let app = make_app(Arc::new(AtomicUsize::new(0)));

    let (status, body) = post_raw(
        &app,
        &[("authorization", "Bearer test-api-token"), ("host", "127.0.0.1:0"), ("origin", "https://evil.example.com")],
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a cross-site Origin must be refused");
    assert!(body.contains("origin_not_allowed"), "structured code, got: {body}");

    let (same_site, _) = post_raw(
        &app,
        &[("authorization", "Bearer test-api-token"), ("host", "127.0.0.1:0"), ("origin", "http://127.0.0.1:0")],
        "{}",
    )
    .await;
    assert_eq!(same_site, StatusCode::OK, "an allow-listed same-origin request is served");

    let (no_origin, _) = post_raw(&app, &with_auth(&[]), "{}").await;
    assert_eq!(no_origin, StatusCode::OK, "an API client that sends no Origin is served");
}

/// A refused request must not have started anything. An auth check that rejects the *response*
/// while the side effect already happened is not an auth check.
#[tokio::test]
async fn a_refused_request_starts_no_session_and_no_generation() {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = make_app(calls.clone());

    let _ = post_raw(&app, &[("host", "127.0.0.1:0")], "{}").await;
    let _ = post_raw(&app, &[("authorization", "Bearer nope"), ("host", "127.0.0.1:0")], "{}").await;
    let _ = post_raw(&app, &[("authorization", "Bearer test-api-token"), ("host", "evil.example.com")], "{}").await;
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no generation may start behind a refused request");

    let (ok, _) = post_raw(&app, &with_auth(&[]), "{}").await;
    assert_eq!(ok, StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "control: an admitted request does start one");
}

/// **Audit M5 — `token_id < n_vocab` is validated BEFORE the token becomes durable.**
///
/// A `SAMPLED` frame is network input. Before M5 an out-of-vocabulary id went straight into the
/// group buffer, then into a `GENERATION_COMMIT` record, then through `fdatasync` — and the first
/// component to notice was whichever later read it. The assertion here is about *absence*, the
/// same way the emit-after-commit gate is proven: after the refusal, the group buffer is empty,
/// the disk has seen **zero** appends, `durable_pos` has not moved, and no event exists. And the
/// control afterwards shows the session is still usable — a refusal is not a poisoning.
#[test]
fn an_out_of_vocabulary_token_is_refused_before_anything_is_written() {
    let disk = OkDisk::default();
    let appends = disk.appends.clone();
    let n_vocab = MapPieces(HashMap::new()).n_vocab();
    let mut s = session(Box::new(disk), Box::new(MapPieces(HashMap::new())), 1, 100);

    let err = s.push_sampled(SampledToken { output_pos: 0, token_id: n_vocab, snapshot: snapshot(0) }).unwrap_err();
    assert!(
        matches!(err, hydra_coordinator::CommitError::TokenOutOfVocab { output_pos: 0, token_id, n_vocab: nv } if token_id == n_vocab && nv == n_vocab),
        "got {err:?}"
    );
    assert_eq!(s.buffered(), 0, "a refused token leaves no trace in the group buffer");
    assert_eq!(appends.load(Ordering::SeqCst), 0, "nothing reached the disk");
    assert_eq!(s.durable_pos(), -1, "durability did not advance");
    assert_eq!(s.last_event_id(), 0, "no event exists");
    // `u32::MAX` — the value a `SAMPLED` forger (or a bit-flip) most plausibly produces.
    assert!(s.push_sampled(SampledToken { output_pos: 0, token_id: u32::MAX, snapshot: snapshot(0) }).is_err());

    // Control: the last legal id commits normally, so the bound is `< n_vocab` and not `< n_vocab - 1`.
    s.push_sampled(SampledToken { output_pos: 0, token_id: n_vocab - 1, snapshot: snapshot(0) }).unwrap();
    assert!(matches!(s.try_commit_by_count().unwrap(), CommitOutcome::Committed(_)));
    assert_eq!(s.durable_pos(), 0);
    assert_eq!(appends.load(Ordering::SeqCst), 1);
}
