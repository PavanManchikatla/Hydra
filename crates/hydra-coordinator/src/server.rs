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
    /// **Audit M16 — the highest event id any client has actually been handed.**
    ///
    /// `Session::client_drained` **had no caller outside its own unit test**: the pump counted
    /// events *emitted* and nothing ever told it any had been *consumed*, so `pending_emit` only
    /// ever grew. After `emit_capacity` events `commit_group` returned `Paused` forever, the pump
    /// stopped committing, and the stream ended **with HTTP 200 and no error** — a truncated
    /// generation that looked like a completed one.
    ///
    /// The SSE task and the pump are different tasks on different threads, so the acknowledgement
    /// travels through this field. An **id**, not a count: with `Last-Event-ID` resume a client can
    /// reconnect and re-read, and the meaningful quantity is *how far anyone has got*, which is
    /// monotonic, rather than *how many were handed over*, which is not.
    client_acked_through: u64,
}

/// Record that a client has been handed every event up to `id` (audit M16).
fn ack_through(st: &Mutex<SessionState>, id: u64) {
    let mut s = lock_session(st);
    if id > s.client_acked_through {
        s.client_acked_through = id;
    }
}

/// **Audit H16 — marks a session finished however its thread exits.**
///
/// Held for the lifetime of the session thread. On a normal return, an early return, or an
/// unwinding panic, `Drop` runs and the session stops being "live" — so a failed generation frees
/// the instance instead of wedging it.
struct DoneOnDrop(Arc<Mutex<SessionState>>);

impl Drop for DoneOnDrop {
    fn drop(&mut self) {
        lock_session(&self.0).done = true;
    }
}

/// **Audit H16 — a poisoned mutex must not wedge the instance either.**
///
/// `lock_session(st)` panics if any holder ever panicked, which in a `Drop` would abort the
/// process and everywhere else would wedge the session exactly as the missing `done` did. The
/// state behind this lock is a log, a broadcast sender and a flag: a panic mid-update cannot leave
/// them in a shape that is unsafe to read, so recovering the inner value is the right call rather
/// than propagating a second failure on top of the first.
fn lock_session(st: &Mutex<SessionState>) -> std::sync::MutexGuard<'_, SessionState> {
    st.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How many finished sessions are retained for `Last-Event-ID` resume (audit M16).
const RETAINED_FINISHED_SESSIONS: usize = 8;
/// Longest accepted `Idempotency-Key` (audit M16: it had no bound at all, and it is a map key).
const MAX_IDEMPOTENCY_KEY_LEN: usize = 256;

/// Drop finished sessions beyond [`RETAINED_FINISHED_SESSIONS`], oldest first (audit M16).
fn prune_finished(reg: &mut Registry) {
    let mut finished: Vec<(String, u64)> = reg
        .sessions
        .iter()
        .filter(|(_, st)| lock_session(st).done)
        .map(|(id, _)| (id.clone(), id.rsplit('-').next().and_then(|n| n.parse().ok()).unwrap_or(0)))
        .collect();
    if finished.len() <= RETAINED_FINISHED_SESSIONS {
        return;
    }
    finished.sort_by_key(|(_, seq)| *seq);
    let drop_n = finished.len() - RETAINED_FINISHED_SESSIONS;
    for (id, _) in finished.into_iter().take(drop_n) {
        reg.sessions.remove(&id);
        reg.by_idempotency.retain(|_, v| v != &id);
    }
}

#[derive(Default)]
struct Registry {
    /// M4·4: the last telemetry sample from each stage, for the dashboard. Empty until a heartbeat
    /// arrives — and the page renders that emptiness as such.
    telemetry: Vec<hydra_sched::telemetry::TelemetrySample>,
    last_durable_pos: i64,
    last_prefill_pos: i64,
    last_checkpoint_id: u64,
    coordinator_state: String,
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
            .find(|(_, st)| !lock_session(st).done)
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
/// The shortest API token `ApiAuth::new` will accept (audit H15).
///
/// 16 bytes of a random alphabet is comfortably beyond guessing on a LAN, and short enough that a
/// human-pasted token still passes. The point is not the exact number, it is that **there is one**.
pub const MIN_API_TOKEN_LEN: usize = 16;

/// Why an [`ApiAuth`] could not be constructed (audit H15).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthConfigError {
    #[error("API token is {len} bytes; the minimum is {min}. An empty or near-empty token makes every unauthenticated request look authenticated (audit H15)")]
    TokenTooShort { len: usize, min: usize },
}

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
    /// **Audit H15 — an EMPTY token used to authenticate every request that sent no header.**
    ///
    /// `check` collapsed a missing `Authorization` header to `""` and then compared
    /// `hash("") == self.token_digest`. With an empty configured token — an unset environment
    /// variable, an `unwrap_or_default()`, a config field someone left blank — those are equal, so
    /// **the server accepted every unauthenticated request while reporting itself as
    /// authenticated.** The M4·1 checklist row said the token was *"required by the type"*, and
    /// that was true of the **argument** and not of a **non-empty secret**: `ApiAuth::new` could not
    /// be called without passing something, and `""` is something.
    ///
    /// Construction now fails instead. A minimum length is enforced because a one-character token
    /// is not meaningfully better than none, and the bound is stated rather than left to the
    /// operator's judgement at the moment they are least likely to exercise it.
    pub fn new(token: &str, allowed_hosts: Vec<String>, allowed_origins: Vec<String>) -> Result<ApiAuth, AuthConfigError> {
        if token.len() < MIN_API_TOKEN_LEN {
            return Err(AuthConfigError::TokenTooShort { len: token.len(), min: MIN_API_TOKEN_LEN });
        }
        Ok(ApiAuth {
            token_digest: *blake3::hash(token.as_bytes()).as_bytes(),
            allowed_hosts: Arc::new(allowed_hosts.into_iter().map(|h| h.to_ascii_lowercase()).collect()),
            allowed_origins: Arc::new(allowed_origins.into_iter().map(|o| o.to_ascii_lowercase()).collect()),
        })
    }

    /// The loopback default for a single-machine dev/desktop coordinator.
    pub fn loopback(token: &str, port: u16) -> Result<ApiAuth, AuthConfigError> {
        let hosts = vec![
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
        ];
        // **Audit H21 (the half that is fixable here).** This allow-listed `https://` origins that
        // the server **cannot serve**: the axum router has no TLS layer, so the one link carrying
        // user prompts and the API bearer token is plaintext. Allow-listing a scheme we do not
        // offer is a claim the code cannot honour, so the list now names only what is actually
        // served. Serving the API over the cluster's own rustls material is the real fix and is
        // owed (§8) — it lands with M4·0, which is the first binary that will serve this router.
        let origins = hosts.iter().map(|h| format!("http://{h}")).collect();
        ApiAuth::new(token, hosts, origins)
    }

    /// `Ok(())`, or the refusal with the status and machine-readable code it should carry.
    fn check(&self, headers: &HeaderMap) -> Result<(), (StatusCode, &'static str, &'static str)> {
        // **Audit H15 — an ABSENT header is refused before anything is hashed.**
        //
        // Collapsing "no header" to `""` and hashing it made the missing-header case
        // indistinguishable from the wrong-token case *in the code*, which is how an empty
        // configured token turned "no header" into "correct token". Refusing absence first means
        // the comparison below only ever runs on something a client actually sent.
        let Some(presented) = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|t| !t.is_empty())
        else {
            return Err((
                StatusCode::UNAUTHORIZED,
                "missing_or_invalid_api_token",
                "this endpoint requires `Authorization: Bearer <token>` even on a trusted LAN",
            ));
        };
        // **Audit M16 — constant-time comparison.** `!=` on `[u8; 32]` may exit at the first
        // differing byte. What leaks is a digest prefix rather than the token, so this is a small
        // hole and is fixed for a small reason: the checklist claims a digest comparison, and
        // `blake3::Hash`'s `PartialEq` is constant-time while `[u8; 32]`'s is not. A claim that is
        // *nearly* true is the §7.31 shape.
        if blake3::Hash::from(self.token_digest) != blake3::hash(presented.as_bytes()) {
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
    /// Build the dashboard's view of the world (M4·4).
    ///
    /// Reads only what the coordinator actually holds. Stage telemetry arrives on heartbeats; when
    /// none has been received the page says **"no stage telemetry received"** rather than showing
    /// an empty-but-healthy-looking table, because those are different facts.
    pub fn render_dashboard(&self) -> crate::dashboard::Dashboard {
        let reg = self.registry.lock().unwrap_or_else(|p| p.into_inner());
        let session = reg.active_session().map(|id| crate::dashboard::SessionView {
            session_id_prefix: id.chars().take(16).collect(),
            generation_durable_pos: reg.last_durable_pos,
            prefill_stable_pos: reg.last_prefill_pos,
            committed_sampler_checkpoint_id: reg.last_checkpoint_id,
            coordinator_state: reg.coordinator_state.clone(),
        });
        crate::dashboard::Dashboard {
            session,
            stages: reg.telemetry.iter().map(crate::dashboard::StageView::from_sample).collect(),
        }
    }

    pub fn new(make_session: Arc<dyn Fn() -> Session + Send + Sync>, gen_fn: GenFn, auth: ApiAuth) -> AppState {
        AppState { registry: Arc::new(Mutex::new(Registry::default())), gen_fn, make_session, auth }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        // **M4·4 — the dashboard is another CLIENT of this surface, not a privileged path.**
        // Same router, same `ApiAuth`, same TLS. A status page reachable without the token would
        // be a second, weaker front door to a user's session state.
        .route("/dashboard", axum::routing::get(dashboard))
        .with_state(state)
}

/// The read-only dashboard (M4·4). Behind the same bearer token and the same Host/Origin checks as
/// the generation endpoint — the auth check is the first thing it does, before it reads any state.
async fn dashboard(State(state): State<AppState>, headers: HeaderMap) -> axum::response::Response {
    if let Err((status, code, message)) = state.auth.check(&headers) {
        return (
            status,
            [("content-type", "application/json")],
            format!("{{\"error\":{{\"code\":\"{code}\",\"message\":\"{message}\"}}}}"),
        )
            .into_response();
    }
    let page = state.render_dashboard();
    (StatusCode::OK, [("content-type", "text/html; charset=utf-8")], page.to_html()).into_response()
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

    // Audit M16: an unbounded header value used as a map key. Bounded, and an over-long one is
    // refused rather than truncated (truncating would silently merge two distinct keys).
    let idempotency = match headers.get("idempotency-key").and_then(|v| v.to_str().ok()) {
        Some(k) if k.len() > MAX_IDEMPOTENCY_KEY_LEN => {
            return (
                StatusCode::BAD_REQUEST,
                [("content-type", "application/json")],
                format!(
                    "{{\"error\":{{\"code\":\"idempotency_key_too_long\",\"message\":\"Idempotency-Key must be at most {MAX_IDEMPOTENCY_KEY_LEN} bytes\"}}}}"
                ),
            )
                .into_response();
        }
        Some(k) => Some(k.to_string()),
        None => None,
    };
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
            // **Audit M16 — the registry and the idempotency map never pruned.**
            //
            // `sessions` and `by_idempotency` grew for the life of the process, each finished
            // session retaining its **full event log**. A long-lived coordinator therefore held
            // every token of every generation it had ever served. Finished sessions are dropped
            // here, oldest first, keeping a bounded tail so `Last-Event-ID` resume still works for
            // a client that reconnects shortly after a generation ends.
            prune_finished(&mut reg);
            reg.seq += 1;
            let id = format!("chatcmpl-{}", reg.seq);
            let (tx, _rx) = broadcast::channel(256);
            let st = Arc::new(Mutex::new(SessionState { log: Vec::new(), tx, done: false, client_acked_through: 0 }));
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
            // **Audit H16 — `done` is now set by a DROP GUARD, so it is set on EVERY exit.**
            //
            // It used to be set only where the generator channel closed cleanly. Every other way
            // out left it `false` **forever**: a panic in `make()` or `gen()`, a panic inside the
            // pump, a runtime that failed to build, or a thread killed for any reason. And because
            // `active_session()` treats any session with `done == false` as live, **one panic made
            // the coordinator answer 409 to every later client until the process restarted** —
            // a single-session instance wedged by a failure it never reported.
            //
            // A guard cannot be forgotten the way a `break` arm can: it fires on the normal path,
            // the early-return path, and the unwinding path alike.
            let _guard = DoneOnDrop(st.clone());
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("hydra-coordinator: session runtime failed to build: {e}");
                    return; // `_guard` still marks the session done on the way out
                }
            };
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
    // Audit M16: how far the pump has already credited the session for client consumption.
    let mut credited: u64 = 0;
    loop {
        // Relieve backpressure with whatever the streaming task has handed over since last time.
        // Without this the emit buffer only ever fills (see `client_acked_through`).
        {
            let acked = lock_session(&st).client_acked_through;
            if acked > credited {
                sess.client_drained((acked - credited) as usize);
                credited = acked;
            }
        }
        tokio::select! {
            maybe = rx.recv() => match maybe {
                Some(tok) => {
                    // Audit M5: an out-of-vocabulary token from S_P is an accident under the
                    // honest-worker assumption, and an accident must not become durable. The
                    // generation ends here — loudly, with nothing committed for this token.
                    if let Err(e) = sess.push_sampled(tok) {
                        eprintln!("hydra-coordinator: generation aborted: {e}");
                        lock_session(&st).done = true;
                        break;
                    }
                    if let Ok(CommitOutcome::Committed(evs)) = sess.try_commit_by_count() {
                        publish(&st, evs);
                    }
                }
                None => {
                    if let Ok(evs) = sess.finish() { publish(&st, evs); }
                    lock_session(&st).done = true;
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
    let mut s = lock_session(st);
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
        let s = lock_session(&st);
        let backlog: Vec<Event> = s.log.iter().filter(|e| e.id > last_event_id).cloned().collect();
        (s.tx.subscribe(), backlog, s.done)
    };
    async_stream(move |mut y| async move {
        let mut last = last_event_id;
        for ev in backlog {
            last = ev.id;
            y.yield_one(sse(&ev)).await;
            ack_through(&st, last);
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
                        ack_through(&st, last);
                    }
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                // Slow reader / timeout: recover any gap from the durable log, then stop if done.
                Ok(Err(broadcast::error::RecvError::Lagged(_))) | Err(_) => {
                    let (more, done) = {
                        let s = lock_session(&st);
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
