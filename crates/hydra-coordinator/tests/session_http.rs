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

fn fence() -> WalFenceCtx {
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
struct OkDisk { len: u64 }
impl Durability for OkDisk {
    fn append(&mut self, _rt: u16, _fl: u16, payload: &[u8]) -> Result<u64, hydra_wal::WalError> {
        let off = self.len;
        self.len += payload.len() as u64;
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
}

fn session(disk: Box<dyn Durability>, pieces: Box<dyn PieceSource>, k: usize, cap: usize) -> Session {
    Session::new(CommitStream::with_durability(disk), fence(), pieces, k, cap)
}

// ---------------- Session-level: the gate is the law ----------------

#[test]
fn emit_after_commit_gate_holds_by_absence() {
    let mut s = session(Box::new(FailingDisk), Box::new(MapPieces(HashMap::new())), 4, 100);
    for pos in 0..4 {
        s.push_sampled(SampledToken { output_pos: pos, token_id: b'x' as u32, snapshot: snapshot(pos) });
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

    s.push_sampled(SampledToken { output_pos: 0, token_id: 1, snapshot: snapshot(0) });
    let out = s.try_commit_by_count().unwrap();
    assert!(matches!(out, CommitOutcome::Committed(ref e) if e.is_empty()), "commit A durable but emits no bytes (mid-glyph)");
    assert_eq!(s.durable_pos(), 0, "the token IS durable");
    assert_eq!(s.last_event_id(), 0, "…but nothing emitted yet — the glyph is incomplete");

    s.push_sampled(SampledToken { output_pos: 1, token_id: 2, snapshot: snapshot(1) });
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
        s.push_sampled(SampledToken { output_pos: pos, token_id: tok as u32, snapshot: snapshot(pos) });
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
        s.push_sampled(SampledToken { output_pos: pos, token_id: tok as u32, snapshot: snapshot(pos) });
        assert!(matches!(s.try_commit_by_count().unwrap(), CommitOutcome::Committed(_)));
    }
    // Buffer now full (2 emitted, undrained). The next commit PAUSES rather than emitting ahead.
    s.push_sampled(SampledToken { output_pos: 2, token_id: b'c' as u32, snapshot: snapshot(2) });
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
    router(AppState::new(make_session, gen_fn))
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
    let body = post(&app, &[("content-type", "application/json")], r#"{"messages":[{"role":"user","content":"hi"}],"stream":true}"#).await;
    let events = parse_sse(&body);
    assert_eq!(events.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5], "dense ids");
    let text: String = events.iter().map(|(_, d)| d.as_str()).collect();
    assert_eq!(text, "Hello", "emitted text is the durable generation");
}

#[tokio::test]
async fn last_event_id_resume_yields_byte_identical_suffix() {
    let app = make_app(Arc::new(AtomicUsize::new(0)));
    let key = "resume-key";
    let full = parse_sse(&post(&app, &[("idempotency-key", key)], "{}").await);
    let full_text: String = full.iter().map(|(_, d)| d.as_str()).collect();

    // Reconnect at EVERY cut point → byte-identical suffix (same session via the idempotency key).
    for cut in 0..=full.len() as u64 {
        let resumed = parse_sse(&post(&app, &[("idempotency-key", key), ("last-event-id", &cut.to_string())], "{}").await);
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
    let a = post(&app, &[("idempotency-key", key)], "{}").await;
    let b = post(&app, &[("idempotency-key", key)], "{}").await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "duplicate POST creates ONE session / one generation");
    let ta: String = parse_sse(&a).iter().map(|(_, d)| d.clone()).collect();
    let tb: String = parse_sse(&b).iter().map(|(_, d)| d.clone()).collect();
    assert_eq!(ta, tb, "same session ⇒ same response body");
    assert_eq!(ta, "Hello");
}
