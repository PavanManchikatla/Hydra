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
//! Security (report Addendum 2 §E1, M4·1): the endpoint is **authenticated even on the LAN** with a
//! bearer token, and validates `Host` and `Origin` against an explicit allow-list — the two halves
//! of the DNS-rebinding / CSRF defence that has been used against Ollama-class local servers. See
//! [`ApiAuth`]. The listener's refusal to bind `0.0.0.0` by default is the third half, and lives in
//! `hydra_transport::check_bind_addr`.
//!
//! Deferred (named in §0(c)): the `DELETE` cancellation surface (I9's cutoff already lives in the
//! M1 ledger), full `DETACHED` TTL choreography (pausing works now; the timeout follows), and
//! tool-call/`PAUSED_TOOL` (M3).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
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

impl Registry {
    /// The id of a session that is still generating, if any.
    ///
    /// **This is the Option-A fence** (spec §1.4: *one active session per model instance*; Option B
    /// is `[RESERVED]`). Without it the reserved hook is not merely unimplemented — it is reachable
    /// by default, because every POST without a matching `Idempotency-Key` minted a fresh session
    /// and started a second generation against the same workers. The stage state machines, the
    /// sampler at S_P and the commit stream are all single-session by construction, so what looked
    /// like an unimplemented feature was really an admission gap.
    fn active_session(&self) -> Option<String> {
        self.sessions
            .iter()
            .find(|(_, st)| !st.lock().unwrap().done)
            .map(|(id, _)| id.clone())
    }
}

/// **Report Addendum 2 §E1 — the LAN API is an attack surface, not a trusted one.**
///
/// A browser on the same network is an unauthenticated remote attacker with the victim's network
/// position; local inference servers have been exploited exactly this way. Three defences, and the
/// type is built so none of them can be forgotten:
///
/// * **Bearer token, required.** There is no `ApiAuth::none()` and no `Option<token>` — an
///   `AppState` cannot be constructed without one, so "auth was not configured" is not a reachable
///   state. Comparison is over BLAKE3 digests of both sides, so it does not leak the token's length
///   or its matching prefix through timing.
/// * **`Host` allow-list.** DNS rebinding works by resolving an attacker-controlled name to the
///   victim's loopback/LAN address; the request then *arrives* with the attacker's name in `Host`.
///   Rejecting an unexpected `Host` breaks the rebind even when the network layer cooperates.
/// * **`Origin` allow-list.** A cross-site POST from a page the user is merely visiting carries an
///   `Origin`. Absent `Origin` (a normal API client) is allowed; a *foreign* one is refused. A
///   same-origin allow-listed one is allowed.
///
/// Every failure is a **refusal**, never a downgrade: there is deliberately no "warn and serve".
#[derive(Clone)]
pub struct ApiAuth {
    token_digest: [u8; 32],
    allowed_hosts: Arc<Vec<String>>,
    allowed_origins: Arc<Vec<String>>,
}

impl ApiAuth {
    /// Build from the API token and the host/origin allow-lists. Hosts are matched
    /// case-insensitively and compared **with** the port, because `localhost:8080` and
    /// `localhost:9999` are different endpoints.
    pub fn new(token: &str, allowed_hosts: Vec<String>, allowed_origins: Vec<String>) -> ApiAuth {
        ApiAuth {
            token_digest: *blake3::hash(token.as_bytes()).as_bytes(),
            allowed_hosts: Arc::new(allowed_hosts.into_iter().map(|h| h.to_ascii_lowercase()).collect()),
            allowed_origins: Arc::new(allowed_origins.into_iter().map(|o| o.to_ascii_lowercase()).collect()),
        }
    }

    /// The loopback default for a single-machine dev/desktop coordinator.
    pub fn loopback(token: &str, port: u16) -> ApiAuth {
        let hosts = vec![
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
        ];
        let origins = hosts.iter().flat_map(|h| [format!("http://{h}"), format!("https://{h}")]).collect();
        ApiAuth::new(token, hosts, origins)
    }

    /// `Ok(())`, or the refusal with the status and machine-readable code it should carry.
    fn check(&self, headers: &HeaderMap) -> Result<(), (StatusCode, &'static str, &'static str)> {
        let presented = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if *blake3::hash(presented.as_bytes()).as_bytes() != self.token_digest {
            return Err((
                StatusCode::UNAUTHORIZED,
                "missing_or_invalid_api_token",
                "this endpoint requires `Authorization: Bearer <token>` even on a trusted LAN",
            ));
        }
        // `Host` is mandatory in HTTP/1.1 and synthesised by axum for HTTP/2; a request without one
        // is refused rather than exempted, because "no Host" must not be the way around the check.
        let host = headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
        if !self.allowed_hosts.contains(&host) {
            return Err((
                StatusCode::FORBIDDEN,
                "host_not_allowed",
                "the request `Host` is not in this coordinator\'s allow-list (DNS-rebinding defence)",
            ));
        }
        if let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) {
            let origin = origin.to_ascii_lowercase();
            if !self.allowed_origins.contains(&origin) {
                return Err((
                    StatusCode::FORBIDDEN,
                    "origin_not_allowed",
                    "cross-site requests are refused (CSRF defence)",
                ));
            }
        }
        Ok(())
    }
}

/// axum application state.
#[derive(Clone)]
pub struct AppState {
    registry: Arc<Mutex<Registry>>,
    gen_fn: GenFn,
    /// Builds a fresh [`Session`] (fresh commit stream + piece source, with its own k / emit
    /// capacity baked in) per new session.
    make_session: Arc<dyn Fn() -> Session + Send + Sync>,
    /// Required — see [`ApiAuth`]. There is no unauthenticated construction path.
    auth: ApiAuth,
}

impl AppState {
    pub fn new(make_session: Arc<dyn Fn() -> Session + Send + Sync>, gen_fn: GenFn, auth: ApiAuth) -> AppState {
        AppState { registry: Arc::new(Mutex::new(Registry::default())), gen_fn, make_session, auth }
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
) -> axum::response::Response {
    // §E1 admission, before anything else touches the body or the registry.
    if let Err((status, code, message)) = state.auth.check(&headers) {
        return (
            status,
            [("content-type", "application/json")],
            format!("{{\"error\":{{\"type\":\"forbidden\",\"code\":\"{code}\",\"message\":\"{message}\"}}}}"),
        )
            .into_response();
    }

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
            // Option A (spec §1.4): one active session per model instance. A second concurrent
            // session is **refused**, never queued and never silently admitted — and the refusal
            // names the session that is holding the instance, because a refusal that does not say
            // what is in the way is not actionable (the same discipline as P2·4's admission
            // refusals). Reconnecting to the running session is what `Idempotency-Key` is for.
            if let Some(active) = reg.active_session() {
                return (
                    StatusCode::CONFLICT,
                    [("content-type", "application/json")],
                    format!(
                        "{{\"error\":{{\"type\":\"session_conflict\",\"code\":\"one_active_session_per_model_instance\",\
                         \"message\":\"this model instance is already serving session '{active}'; v1 supports one active session \
                         per model instance (spec §1.4 Option A; Option B is RESERVED). Reconnect with the running session's \
                         Idempotency-Key, or wait for it to finish.\",\"active_session\":\"{active}\"}}}}"
                    ),
                )
                    .into_response();
            }
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
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
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
