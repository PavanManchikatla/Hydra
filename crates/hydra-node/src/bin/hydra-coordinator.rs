//! **`hydra-coordinator` — the product process (moved to `hydra-node`, 2026-09-02; §7.76, rule 27).**
//!
//! # What this binary does now that it did not before
//!
//! From M4·0 (2026-08-24) until 2026-09-02 this binary served the API over TLS, minted a session,
//! created the commit stream — and answered every prompt with an **empty stream**: its generation
//! function returned a closed channel (*"no stages linked yet in this seam"*), because linking a
//! stage needed `hydra-worker`'s drivers and the crate graph forbade that where the binary sat. The
//! quickstart's step 5 returned `200` with no tokens, and nothing in the record said so until the
//! design authority asked (§7.76). Now:
//!
//! * the **session fence**, **stage table**, **model path** and **API token** come from the
//!   pairing directory that `hydra-cli pair` + `hydra-cli provision` wrote (`HYDRA_API_TOKEN` and
//!   `--stages` override; the token FILE must be 0600 or the process refuses to start);
//! * a prompt is rendered (ChatML), tokenized with the model's own tokenizer, and its
//!   `INITIAL_COMMIT` is durable before the first `SAMPLE_NEXT` (spec §2.6a);
//! * the two stages are activated **through the state machine** (`ActivationDriver`), then the
//!   coordinator-relayed pipeline prefills and samples, and every sampled token goes through the
//!   session's commit-then-emit path to SSE (`hydra_node::Pipeline`).
//!
//! # Usage
//!
//! ```text
//! hydra-cli pair --out ~/.hydra
//! hydra-cli provision --pairing-dir ~/.hydra --model <gguf> --stages worker-s1=127.0.0.1:9001,worker-s2=127.0.0.1:9002
//! hydra-worker ~/.hydra/worker-s1.boot &   hydra-worker ~/.hydra/worker-s2.boot &
//! hydra-coordinator --pairing-dir ~/.hydra --api-addr 127.0.0.1:8443 --data-dir ~/.hydra/data
//! ```
//!
//! # What it still does not do (stated, not implied)
//!
//! It drives the coordinator-relayed two-stage topology only (no direct-FWD / durability target).
//! One generation per process life (spec §1.4 Option A; a second request is refused with 409). A
//! restart (an existing `commits.wal`) resumes THAT generation per spec §6.5a — the coordinator
//! fences forward through the state machine and recovers both stages from the ledger — and only
//! the fence-forward classification is wired; a restart that classifies as a decided-but-unfinalized
//! activation or a recorded UNSERVABLE is reported and not driven (§8).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hydra_coordinator::{ApiAuth, AppState, CommitStream, Session, TokenizerPieces, WalFenceCtx};
use hydra_node::{parse_stages, read_api_token, read_cluster, ClusterFiles, Pipeline, StageConnector};
use hydra_tokenizer::admission::{Admission, ChatMessage, ChatTemplate};
use hydra_tokenizer::Tokenizer;
use hydra_worker::sampler::{initial_checkpoint_bytes, SamplingConfig};
use hydra_worker::worker::INITIAL_CHECKPOINT_ID;

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| -> Option<String> { args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned() };

    let api_addr: SocketAddr = flag("--api-addr").unwrap_or_else(|| "127.0.0.1:8443".to_string()).parse().map_err(|e| format!("--api-addr: {e}"))?;
    let data_dir = flag("--data-dir").unwrap_or_else(|| "./hydra-data".to_string());
    let max_tokens: usize = flag("--max-tokens").unwrap_or_else(|| "64".to_string()).parse().map_err(|e| format!("--max-tokens: {e}"))?;
    let pairing_dir = PathBuf::from(flag("--pairing-dir").ok_or(
        "supply --pairing-dir <dir> (what `hydra-cli pair --out <dir>` and `hydra-cli provision` wrote). A coordinator does not invent its own trust anchor (audit C1's shape), and since 2026-09-02 it does not invent its session fence, stage table or API token either.",
    )?);

    // **The API token (item 2, 2026-09-02): pairing mints it; the file is read by default and MUST be
    // 0600; the environment variable is an override for deployments that manage secrets elsewhere.**
    let token = match std::env::var("HYDRA_API_TOKEN") {
        Ok(t) => {
            eprintln!("hydra-coordinator: API token taken from HYDRA_API_TOKEN (overrides {}/api-token)", pairing_dir.display());
            t
        }
        Err(_) => read_api_token(&pairing_dir).map_err(|e| format!("api token: {e}"))?,
    };

    // The cluster as provisioned: fence, stages, model, context length.
    let cluster: ClusterFiles = read_cluster(&pairing_dir).map_err(|e| format!("cluster files in {}: {e}. Run `hydra-cli provision` first", pairing_dir.display()))?;
    let stages = match flag("--stages") {
        Some(s) => parse_stages(&s).map_err(|e| e.to_string())?,
        None => cluster.stages.clone(),
    };
    let fence = cluster.fence.clone();
    eprintln!("hydra-coordinator: session {} (from {}/cluster.fence); stages: {}", hex8(&fence.session_id), pairing_dir.display(),
        stages.iter().map(|s| format!("{}@{}", s.name, s.addr)).collect::<Vec<_>>().join(", "));

    let boot = identity_from_pairing(&pairing_dir, api_addr)?;

    std::fs::create_dir_all(&data_dir).map_err(|e| format!("mkdir {data_dir}: {e}"))?;
    let commits_path = Path::new(&data_dir).join("commits.wal");
    let control_path = Path::new(&data_dir).join("control.wal");

    let wal_fence = hydra_node::wal_fence(&fence);

    // **Spec §6.5a (2026-09-02): an existing ledger means a RESTART.** The ledger is reopened bound
    // to the provisioned session (audit M8 — a ledger of another session is refused), the event
    // log is rebuilt from the durable prefix with the same ids, and the generation continues from
    // `generation_durable_pos + 1` after the coordinator has fenced forward through the state
    // machine. Until 2026-09-02 this path was a refusal by name (§7.75).
    let resumed: Option<(Vec<hydra_coordinator::Event>, hydra_coordinator::RecoveryState)> = if commits_path.exists() {
        // Bound to the provisioned session (audit M8): a ledger of another session is refused here.
        drop(CommitStream::open(&commits_path, &fence.cluster_id, &fence.session_id)
            .map_err(|e| format!("reopen the session ledger at {}: {e} (a ledger of another session or cluster is refused, audit M8)", commits_path.display()))?);
        let ledger = hydra_coordinator::recovery::read(&commits_path).map_err(|e| format!("read the ledger: {e}"))?;
        let tok = Tokenizer::load_vocab_only(&cluster.model_path).map_err(|e| format!("tokenizer for resume: {e}"))?;
        let backlog = Session::replay_events(&commits_path, &TokenizerPieces(tok), &mut hydra_tokenizer::utf8::Utf8Streamer::new()).map_err(|e| format!("rebuild the event log: {e}"))?;
        eprintln!(
            "hydra-coordinator: RESTART — ledger reopened: {} prompt tokens, generation_durable_pos={}, prefill_stable_pos={}, {} events rebuilt",
            ledger.prompt_tokens.len(), ledger.generation_durable_pos, ledger.prefill_stable_pos, backlog.len()
        );
        Some((backlog, ledger))
    } else {
        None
    };
    // A fresh start creates the ledger bound to this session (audit M8); a restart already holds it.
    let commit_stream = if resumed.is_none() {
        Some(CommitStream::create(&commits_path, fence.cluster_id, fence.session_id).map_err(|e| format!("commit stream: {e}"))?)
    } else {
        None
    };

    let auth = ApiAuth::loopback(&token, api_addr.port()).map_err(|e| e.to_string())?;
    let tls = hydra_transport::api_server_config(&boot).map_err(|e| format!("api tls: {e}"))?;

    // The prompt's tokens cross from the session factory (which appends INITIAL_COMMIT) to the
    // generation function on the same session thread, in that order — never re-tokenized twice.
    let prompt_tokens: Arc<Mutex<Option<Vec<u32>>>> = Arc::new(Mutex::new(None));

    // Spec §1.4 Option A: one active session per model instance. The commit stream is created once
    // and handed to the first session; a second is refused by the HTTP registry.
    let make_session = {
        let cell = Arc::new(Mutex::new(commit_stream));
        let model = cluster.model_path.clone();
        let wf: WalFenceCtx = wal_fence.clone();
        let slot = prompt_tokens.clone();
        Arc::new(move |prompt: &str| -> Session {
            let cs = cell.lock().unwrap_or_else(|p| p.into_inner()).take().expect("one active session per model instance (spec §1.4 Option A)");
            match Tokenizer::load_vocab_only(&model) {
                Ok(tok) => {
                    let admission = Admission::compute(&tok, ChatTemplate::ChatMl, &[ChatMessage::new("user", prompt)]);
                    match admission {
                        Ok(adm) => {
                            let mut cs = cs;
                            let ckpt = initial_checkpoint_bytes(INITIAL_CHECKPOINT_ID, &SamplingConfig::greedy());
                            // Durability mode D0 (spec §7): this topology keeps only the commit stream and the
                            // prompt, so recovery is Strategy B (token replay) — stated in the record.
                            if let Err(e) = cs.append_initial_commit(&wf, &adm, &ckpt, 0) {
                                eprintln!("hydra-coordinator: INITIAL_COMMIT failed: {e} — the session cannot be recovered and will not generate");
                            } else {
                                *slot.lock().unwrap() = Some(adm.prompt_tokens.clone());
                            }
                            Session::new(cs, wf.clone(), Box::new(TokenizerPieces(tok)), 8, 1024)
                        }
                        Err(e) => {
                            eprintln!("hydra-coordinator: admission failed: {e}");
                            Session::new(cs, wf.clone(), Box::new(TokenizerPieces(tok)), 8, 1024)
                        }
                    }
                }
                Err(e) => {
                    eprintln!("hydra-coordinator: tokenizer unavailable ({e}) — no engine linked or no model at {model}; the session will not generate");
                    Session::new(cs, wf.clone(), Box::new(NoPieces), 8, 1024)
                }
            }
        }) as Arc<dyn Fn(&str) -> Session + Send + Sync>
    };

    let gen_fn: hydra_coordinator::GenFn = {
        let stages = stages.clone();
        let fence = fence.clone();
        let pairing = pairing_dir.clone();
        let control_path = control_path.clone();
        let slot = prompt_tokens.clone();
        Arc::new(move |_prompt: String| {
            let (tx, rx) = tokio::sync::mpsc::channel::<hydra_coordinator::SampledToken>(64);
            let Some(tokens) = slot.lock().unwrap().take() else {
                eprintln!("hydra-coordinator: no prompt tokens (admission failed) — generation not started");
                return rx; // closes: the session finishes empty, and said why above
            };
            let (stages, fence, pairing, control) = (stages.clone(), fence.clone(), pairing.clone(), control_path.clone());
            std::thread::spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => rt,
                    Err(e) => { eprintln!("hydra-coordinator: generation runtime: {e}"); return; }
                };
                rt.block_on(async move {
                    let connector = match StageConnector::from_pairing_dir(&pairing) {
                        Ok(c) => c,
                        Err(e) => { eprintln!("hydra-coordinator: stage connector: {e}"); return; }
                    };
                    let mut pipe = match Pipeline::activate(&connector, &stages, &fence, &control).await {
                        Ok(p) => p,
                        Err(e) => { eprintln!("hydra-coordinator: activation failed: {e}"); return; }
                    };
                    eprintln!("hydra-coordinator: both stages ACTIVE_FINAL through the state machine; generating up to {max_tokens} tokens");
                    let r = pipe
                        .generate(&tokens, &SamplingConfig::greedy(), max_tokens, |s| push_token(&tx, s.clone()))
                        .await;
                    match r {
                        Ok(out) => eprintln!("hydra-coordinator: generation complete: {} tokens", out.len()),
                        Err(e) => eprintln!("hydra-coordinator: generation ended with error: {e}"),
                    }
                });
            });
            rx
        })
    };

    let state = AppState::with_prompt_aware_session(make_session, gen_fn, auth);

    // The resumed session, if any: fence forward, recover both stages from the ledger, continue.
    if let Some((backlog, ledger)) = resumed {
        let (tx, rx) = tokio::sync::mpsc::channel::<hydra_coordinator::SampledToken>(64);
        let (cp, model, wf) = (commits_path.clone(), cluster.model_path.clone(), wal_fence.clone());
        let (cid, sid) = (fence.cluster_id, fence.session_id);
        let make: Box<dyn FnOnce() -> Session + Send> = Box::new(move || {
            let cs = CommitStream::open(&cp, &cid, &sid).expect("the ledger reopened once already on this path");
            let tok = Tokenizer::load_vocab_only(&model).expect("the tokenizer loaded once already on this path");
            Session::reopen(&cp, cs, wf, Box::new(TokenizerPieces(tok)), 8, 1024).expect("the event log rebuilt once already on this path")
        });
        let id = state.adopt_resumed(backlog, make, rx);
        eprintln!("hydra-coordinator: resumed session {id}; a client reconnects with Last-Event-ID");
        let (stages, fence, pairing, control) = (stages.clone(), fence.clone(), pairing_dir.clone(), control_path.clone());
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => { eprintln!("hydra-coordinator: resume runtime: {e}"); return; }
            };
            rt.block_on(async move {
                let connector = match StageConnector::from_pairing_dir(&pairing) {
                    Ok(c) => c,
                    Err(e) => { eprintln!("hydra-coordinator: stage connector: {e}"); return; }
                };
                let mut pipe = match Pipeline::recover(&connector, &stages, &fence, &control, &ledger).await {
                    Ok(p) => p,
                    Err(e) => { eprintln!("hydra-coordinator: RECOVERY FAILED: {e}"); return; }
                };
                let generated = ledger.generated_tokens.len();
                let remaining = max_tokens.saturating_sub(generated);
                eprintln!("hydra-coordinator: recovered at epoch {:?}; resuming at output_pos {} for up to {remaining} more tokens", pipe.state(), ledger.generation_durable_pos + 1);
                let r = pipe
                    .resume_generate(ledger.generation_durable_pos + 1, ledger.input_frontier(), ledger.checkpoint_id, &SamplingConfig::greedy(), remaining, |s| push_token(&tx, s.clone()))
                    .await;
                match r {
                    Ok(out) => eprintln!("hydra-coordinator: resumed generation complete: {} more tokens", out.len()),
                    Err(e) => eprintln!("hydra-coordinator: resumed generation ended with error: {e}"),
                }
            });
        });
    }

    let router = hydra_coordinator::router(state);
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(hydra_coordinator::serve_tls::serve_tls(api_addr, tls, router)).map_err(|e| format!("serve: {e}"))
}

/// Hand a sampled token to the session's stream from INSIDE the generation thread's runtime.
///
/// The generation callback is synchronous and runs under that thread's `block_on`, so a blocking
/// send is forbidden there — `tokio` panics with *"Cannot block the current thread from within a
/// runtime"* — and the first product oracle (`generation_e2e.rs`, 2026-09-03) found exactly that:
/// the log said "generating up to N tokens", the thread died, and the client got an empty stream —
/// the same shape as the stub the ruling's item 0 named. `try_send` with a short retry on a full
/// channel is the sanctioned shape: the receiver drains on the server's runtime, so waiting here
/// stalls only this thread. Returns `false` when the client is gone, which stops generation.
fn push_token(tx: &tokio::sync::mpsc::Sender<hydra_coordinator::SampledToken>, s: hydra_coordinator::SampledToken) -> bool {
    let mut s = s;
    loop {
        match tx.try_send(s) {
            Ok(()) => return true,
            Err(tokio::sync::mpsc::error::TrySendError::Full(v)) => {
                s = v;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return false,
        }
    }
}

/// A piece source with no tokenizer (engine absent): the session can still refuse and log.
struct NoPieces;
impl hydra_coordinator::PieceSource for NoPieces {
    fn piece(&self, _token: u32) -> Vec<u8> {
        Vec::new()
    }
    fn n_vocab(&self) -> u32 {
        u32::MAX
    }
}

/// The coordinator's API identity, issued from the paired cluster CA with SANs for the addresses
/// clients dial (loopback always; the bind address too).
fn identity_from_pairing(pairing_dir: &Path, api_addr: SocketAddr) -> Result<hydra_transport::DeviceIdentity, String> {
    let sans: Vec<String> = vec!["localhost".to_string(), "127.0.0.1".to_string(), "::1".to_string(), api_addr.ip().to_string()];
    let dir = pairing_dir.join("coordinator");
    let ca = hydra_transport::ClusterCa::load_private(&dir)
        .map_err(|e| format!("load the paired cluster CA from {}: {e}. Run `hydra-cli pair --out <dir>` first", dir.display()))?;
    eprintln!("hydra-coordinator: using the paired cluster CA from {}", dir.display());
    ca.issue_api("coordinator", &sans).map_err(|e| format!("issue coordinator identity: {e}"))
}

fn hex8(b: &[u8]) -> String {
    b.iter().take(8).map(|x| format!("{x:02x}")).collect()
}
