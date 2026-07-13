//! The OpenAI-compatible HTTP surface (axum): `POST /v1/chat/completions`, streaming over SSE.
//!
//! Minimalism is fine (one model, chat completions, streaming) — compat breadth is not correctness.
//! What *is* correctness, and is enforced here:
//!   * **emit-after-commit** — the SSE stream forwards only events the [`Session`] has appended to
//!     its log, and the session appends only after a durable commit (the gate lives in
//!     [`Session::commit_group`], not here);
//!   * **dense stable SSE ids** — `id:` is the event id; a reconnect with `Last-Event-ID: k` replays
//!     `events_since(k)` (a pure function of the durable log) then tails live, so the client sees a
//!     byte-identical suffix (at-least-once; the client's dedup is the exactly-once half, spec §8);
//!   * **`Idempotency-Key`** — a duplicate session-creation POST returns the *same* session.
//!
//! Deferred (named in §0(c)): the `DELETE` cancellation surface (I9's cutoff already lives in the
//! M1 ledger), full `DETACHED` TTL choreography (pausing works now; the timeout follows), and
//! tool-call/`PAUSED_TOOL` (M3).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use futures_core::Stream;
use tokio::sync::{broadcast, mpsc};

use crate::event_log::Event;
use crate::session::{CommitOutcome, Session, SampledToken};

/// A generation source: given a rendered prompt, start producing sampled tokens on the returned
/// channel (the real two-worker pipeline in sub-slice C; a canned list in tests).
pub type GenFn = Arc<dyn Fn(String) -> mpsc::Receiver<SampledToken> + Send + Sync>;

/// Per-session shared state the HTTP layer needs for live-tail + resume.
struct SessionState {
    /// All events so far (the durable-derived log), for `Last-Event-ID` replay.
    log: Vec<Event>,
    /// Live fan-out to any currently-streaming client.
    tx: broadcast::Sender<Event>,
    done: bool,
}

#[derive(Default)]
struct Registry {
    by_idempotency: HashMap<String, String>, // Idempotency-Key -> session_id
    sessions: HashMap<String, Arc<Mutex<SessionState>>>,
    seq: u64,
}

/// axum application state.
#[derive(Clone)]
pub struct AppState {
    registry: Arc<Mutex<Registry>>,
    gen_fn: GenFn,
    /// Builds a fresh [`Session`] (fresh commit stream + piece source, with its own k / emit
    /// capacity baked in) per new session.
    make_session: Arc<dyn Fn() -> Session + Send + Sync>,
}

impl AppState {
    pub fn new(make_session: Arc<dyn Fn() -> Session + Send + Sync>, gen_fn: GenFn) -> AppState {
        AppState { registry: Arc::new(Mutex::new(Registry::default())), gen_fn, make_session }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new().route("/v1/chat/completions", post(chat_completions)).with_state(state)
}

/// Extract a minimal prompt from the request body — the string after the last `"content":"` up to
/// the next `"`. Deliberately minimal (no serde dep); the generator may ignore it in tests.
fn extract_prompt(body: &str) -> String {
    if let Some(i) = body.rfind("\"content\"") {
        let rest = &body[i + "\"content\"".len()..];
        if let Some(colon) = rest.find(':') {
            let after = rest[colon + 1..].trim_start();
            if let Some(open) = after.find('"') {
                let s = &after[open + 1..];
                if let Some(close) = s.find('"') {
                    return s[..close].to_string();
                }
            }
        }
    }
    body.trim().to_string()
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    let idempotency = headers.get("idempotency-key").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);

    // Resolve (or create) the session under the idempotency key.
    let (session_id, session, created) = {
        let mut reg = state.registry.lock().unwrap();
        if let Some(id) = idempotency.as_ref().and_then(|k| reg.by_idempotency.get(k).cloned()) {
            let s = reg.sessions.get(&id).unwrap().clone();
            (id, s, false)
        } else {
            reg.seq += 1;
            let id = format!("chatcmpl-{}", reg.seq);
            let (tx, _rx) = broadcast::channel(256);
            let st = Arc::new(Mutex::new(SessionState { log: Vec::new(), tx, done: false }));
            reg.sessions.insert(id.clone(), st.clone());
            if let Some(k) = idempotency.clone() {
                reg.by_idempotency.insert(k, id.clone());
            }
            (id, st, true)
        }
    };

    // On first creation, run the generation → session → log/broadcast pump on a DEDICATED thread:
    // the `Session` (non-`Send`, it owns the engine tokenizer) is created *on* that thread and never
    // crosses a boundary — only `Send` handles (the make-fn, gen-fn, prompt, shared state) do.
    if created {
        let prompt = extract_prompt(&body);
        let make = state.make_session.clone();
        let gen = state.gen_fn.clone();
        let st = session.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let sess = make();
                let rx = gen(prompt);
                pump(sess, rx, st).await;
            });
        });
    }

    let stream = resume_and_tail(session, last_event_id, session_id);
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// The generation pump (runs on the dedicated session thread): consume sampled tokens, commit under
/// the count-or-50ms-deadline policy (spec §3), and publish only durable events (emit-after-commit).
async fn pump(mut sess: Session, mut rx: mpsc::Receiver<SampledToken>, st: Arc<Mutex<SessionState>>) {
    let mut deadline = tokio::time::interval(std::time::Duration::from_millis(50));
    deadline.tick().await; // consume the immediate first tick
    loop {
        tokio::select! {
            maybe = rx.recv() => match maybe {
                Some(tok) => {
                    sess.push_sampled(tok);
                    if let Ok(CommitOutcome::Committed(evs)) = sess.try_commit_by_count() {
                        publish(&st, evs);
                    }
                }
                None => {
                    if let Ok(evs) = sess.finish() { publish(&st, evs); }
                    st.lock().unwrap().done = true;
                    break;
                }
            },
            _ = deadline.tick() => {
                if let Ok(CommitOutcome::Committed(evs)) = sess.commit_on_deadline() {
                    publish(&st, evs);
                }
            }
        }
    }
}

fn publish(st: &Arc<Mutex<SessionState>>, events: Vec<Event>) {
    if events.is_empty() {
        return;
    }
    let mut s = st.lock().unwrap();
    for ev in events {
        s.log.push(ev.clone());
        let _ = s.tx.send(ev); // best-effort live fan-out; the log is the source of truth
    }
}

/// The SSE body: replay `events_since(last_event_id)` from the durable log, then tail live events
/// until generation completes.
fn resume_and_tail(
    st: Arc<Mutex<SessionState>>,
    last_event_id: u64,
    _session_id: String,
) -> impl Stream<Item = Result<SseEvent, std::convert::Infallible>> {
    async_stream_impl(st, last_event_id)
}

fn async_stream_impl(
    st: Arc<Mutex<SessionState>>,
    last_event_id: u64,
) -> impl Stream<Item = Result<SseEvent, std::convert::Infallible>> {
    // Snapshot the backlog after `last_event_id` and subscribe for live events **under one lock**,
    // so no event slips between the two (an event is appended to the log then broadcast, both under
    // the lock — so it is either in the backlog or in the subscription, never lost).
    let (mut rx, backlog, already_done) = {
        let s = st.lock().unwrap();
        let backlog: Vec<Event> = s.log.iter().filter(|e| e.id > last_event_id).cloned().collect();
        (s.tx.subscribe(), backlog, s.done)
    };
    async_stream(move |mut y| async move {
        let mut last = last_event_id;
        for ev in backlog {
            last = ev.id;
            y.yield_one(sse(&ev)).await;
        }
        if already_done {
            return;
        }
        // Tail live events. `done` is set-once and monotonic; poll it with a short timeout so the
        // stream terminates race-free even if generation finished between events (no lost-wake).
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(5), rx.recv()).await {
                Ok(Ok(ev)) => {
                    if ev.id > last {
                        last = ev.id;
                        y.yield_one(sse(&ev)).await;
                    }
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                // Slow reader / timeout: recover any gap from the durable log, then stop if done.
                Ok(Err(broadcast::error::RecvError::Lagged(_))) | Err(_) => {
                    let (more, done) = {
                        let s = st.lock().unwrap();
                        (s.log.iter().filter(|e| e.id > last).cloned().collect::<Vec<_>>(), s.done)
                    };
                    for ev in &more {
                        last = ev.id;
                        y.yield_one(sse(ev)).await;
                    }
                    if done {
                        break;
                    }
                }
            }
        }
    })
}

fn sse(ev: &Event) -> Result<SseEvent, std::convert::Infallible> {
    Ok(SseEvent::default().id(ev.id.to_string()).data(ev.data.clone()))
}

// ---- a tiny local async-stream generator (no async-stream crate) ----

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

struct Yielder<T> {
    tx: mpsc::Sender<T>,
}
impl<T> Yielder<T> {
    async fn yield_one(&mut self, v: T) {
        let _ = self.tx.send(v).await;
    }
}

struct GenStream<T> {
    rx: mpsc::Receiver<T>,
    _task: tokio::task::JoinHandle<()>,
}
impl<T> Stream for GenStream<T> {
    type Item = T;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.rx.poll_recv(cx)
    }
}

fn async_stream<T, F, Fut>(f: F) -> impl Stream<Item = T>
where
    T: Send + 'static,
    F: FnOnce(Yielder<T>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = mpsc::channel(64);
    let task = tokio::spawn(async move { f(Yielder { tx }).await });
    GenStream { rx, _task: task }
}
