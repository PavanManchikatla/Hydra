//! M2 slice 5 sub-slice C (seam 2b) — **the D1 recovery flagship, end-to-end, engine-gated.**
//!
//! A single full-range **D0-class** S_P (one worker hosts layers `[0,-1)` + the sampler) generates a
//! seeded stream. At three adversarial `kill -9` windows the real `hydra-worker` OS process is
//! SIGKILL'd and the coordinator drives recovery **through the real machinery only** — detection →
//! `BEGIN_RECOVERY` Case A → catch-up (`REBUILD_APPLY` of the durable committed tokens) →
//! `INSTALL_SAMPLER_CHECKPOINT` from the last durable `GENERATION_COMMIT` → the activation
//! transaction → `SAMPLE_NEXT` at `goal+1` — reconstructing every input from its **own durable
//! commit stream** (I3). Three assertions, all required:
//!   (a) **SSE id continuity** — the commit stream (hence the derived event log) covers every output
//!       position exactly once, dense, across the recovery boundary;
//!   (b) **byte-identical** — the committed pre-kill prefix ⊕ the post-recovery suffix equals an
//!       uninterrupted seeded run of the same session, token-for-token (seeded sampling makes the
//!       comparison exact; provisional sampled-ahead outputs above `generation_durable_pos` are
//!       discarded and re-sampled — I7b/I15);
//!   (c) **disk truth** — the commit stream reads back I19-valid with no output position twice.
//!
//! Skips cleanly without the engine/model (both dev-environment artifacts), like the bit-exact anchor.
//! Timing is honesty-annotated: local-pair dev machine, NOT the <15 s LAN/M3 D1 target.

use std::time::Instant;

use hydra_coordinator::recovery::{self, RecoveryState};
use hydra_coordinator::{CommitStream, GroupCommitter, WalFenceCtx};
use hydra_state::{ActivationKind, ActivationTuple};
use hydra_tokenizer::Admission;
use hydra_transport::framed::Conn;
use hydra_transport::tcp_mtls::TcpMtls;
use hydra_worker::pair::{dev_model_path, Cluster, SubprocessWorker};
use hydra_worker::sampler::{initial_checkpoint_bytes, SamplingConfig};
use hydra_worker::wire::{self, Msg, SessionKeys};
use hydra_worker::worker::{WorkerConfig, INITIAL_CHECKPOINT_ID};
use hydra_worker::Bootstrap;
use tokio::io::{AsyncRead, AsyncWrite};

const CLUSTER_ID: [u8; 16] = [0xC1; 16];
const SESSION_ID: [u8; 16] = [0x5E; 16];
const K: usize = 3; // group commit count threshold (small so kills land between/within groups)

/// Which adversarial kill window a run exercises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Window {
    /// Killed at a clean k-group boundary, worker idle between groups (nothing provisional).
    Steady,
    /// Killed after S_P sampled PAST `generation_durable_pos` (provisional outputs buffered but not
    /// committed) — the I7b/I15 truncation case: recovery discards them and re-samples.
    SampledAhead,
    /// Killed in the window after a group commit's `fdatasync` returned (durable) but before the
    /// client event was emitted — emit-after-commit must make the durable commit visible on resume.
    BetweenFsyncAndEmit,
}

fn fence() -> WalFenceCtx {
    WalFenceCtx {
        cluster_id: CLUSTER_ID,
        session_id: SESSION_ID,
        model_instance_id: [3; 16],
        manifest_hash: [4; 32],
        epoch: 0,
        recovery_id: 0,
        activation_attempt_id: 0,
    }
}

fn sampling_config() -> SamplingConfig {
    // Stochastic + seeded: temperature/top-p make the RNG state load-bearing, so the byte-identical
    // assertion genuinely exercises INSTALL_SAMPLER_CHECKPOINT's RNG restore (greedy would pass even
    // if the restore were broken).
    SamplingConfig { temperature: 0.8, top_p: 0.95, repeat_penalty: 1.1, penalty_last_n: 32, seed: 0xD1_D1_D1 }
}

fn sp_config(model_path: &str, keys: &SessionKeys, n_ctx: i32, recovery_id: u32, recovery_start: bool) -> WorkerConfig {
    WorkerConfig {
        keys: keys.clone(),
        rank: 0,
        layer_first: 0,
        layer_last: -1,
        is_final: true,
        receives_tokens: true,
        epoch: 0,
        recovery_id,
        model_path: Some(model_path.to_string()),
        n_gpu_layers: 0,
        n_ctx,
        sampler_config: Some(sampling_config()),
        recovery_start,
    }
}

fn boot(cluster: &Cluster, name: &str, cfg: WorkerConfig) -> Bootstrap {
    let id = cluster.issue(name).unwrap();
    Bootstrap {
        listen_addr: "127.0.0.1:0".to_string(),
        device_name: name.to_string(),
        ca_cert_der: cluster.ca.ca_cert_der().as_ref().to_vec(),
        cert_chain_der: id.cert_chain.iter().map(|c| c.as_ref().to_vec()).collect(),
        key_pkcs8_der: id.key_pkcs8_der(),
        cfg,
        forwarding: None,
    }
}

fn admission(prompt: &[u32]) -> Admission {
    Admission {
        tokenizer_hash: [0xA1; 32],
        chat_template_hash: [0xB2; 32],
        rendered_prompt_bytes_hash: [0xC3; 32],
        rendered_prompt: "<|im_start|>user\nseed<|im_end|>\n<|im_start|>assistant\n".to_string(),
        prompt_tokens: prompt.to_vec(),
    }
}

// ------------------------- wire drivers (coordinator side) -------------------------

async fn apply<S>(c: &mut Conn<S>, keys: &SessionKeys, input_pos: i64, token: u32, no_sample: bool) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    c.send(0, &wire::encode_apply_token(keys, 0, input_pos, token, no_sample)).await.map_err(|e| format!("apply send: {e}"))?;
    match wire::decode(&c.recv().await.map_err(|e| format!("apply recv: {e}"))?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::AppliedAck { cumulative_input_pos, .. } if cumulative_input_pos == input_pos => Ok(()),
        other => Err(format!("apply @ {input_pos}: expected APPLIED_ACK, got {other:?}")),
    }
}

/// `SAMPLE_NEXT` → `(token_id, snapshot bytes)`.
async fn sample<S>(c: &mut Conn<S>, keys: &SessionKeys, output_pos: i64, cfg_hash: &[u8; 32]) -> Result<(u32, Vec<u8>), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    c.send(0, &wire::encode_sample_next(keys, 0, output_pos, cfg_hash, INITIAL_CHECKPOINT_ID)).await.map_err(|e| format!("sample send: {e}"))?;
    match wire::decode(&c.recv().await.map_err(|e| format!("sample recv: {e}"))?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::Sampled { output_pos: p, token_id, post_sample_snapshot, .. } if p == output_pos => Ok((token_id, post_sample_snapshot)),
        Msg::Err { code } => Err(format!("SAMPLE_NEXT @ {output_pos} errored: code {code}")),
        other => Err(format!("SAMPLE_NEXT @ {output_pos}: expected SAMPLED, got {other:?}")),
    }
}

/// Prefill the prompt (NO_SAMPLE) at input positions `0..prompt.len()`.
async fn prefill<S>(c: &mut Conn<S>, keys: &SessionKeys, prompt: &[u32]) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    for (pos, &tok) in prompt.iter().enumerate() {
        apply(c, keys, pos as i64, tok, true).await?;
    }
    Ok(())
}

/// The uninterrupted seeded reference: prompt prefill then `n` autoregressive sample steps. Returns
/// the generated token sequence.
async fn reference_run(connector: &TcpMtls, addr: std::net::SocketAddr, name: &str, keys: &SessionKeys, prompt: &[u32], n: usize) -> Result<Vec<u32>, String> {
    let cfg_hash = sampling_config().hash();
    let mut c = connector.connect(addr, name).await.map_err(|e| format!("ref connect: {e}"))?;
    prefill(&mut c, keys, prompt).await?;
    let mut out = Vec::with_capacity(n);
    let mut input_pos = prompt.len() as i64;
    for pos in 0..n as i64 {
        let (tok, _snap) = sample(&mut c, keys, pos, &cfg_hash).await?;
        out.push(tok);
        if (pos as usize + 1) < n {
            apply(&mut c, keys, input_pos, tok, false).await?;
            input_pos += 1;
        }
    }
    Ok(out)
}

/// Drive the recovery flow on an already-connected fresh replacement S_P through the REAL machinery,
/// leaving it ACTIVE_FINAL with KV rebuilt and the sampler restored to `state`'s last durable
/// checkpoint — ready to resume `SAMPLE_NEXT` at `generation_durable_pos + 1`.
async fn drive_recovery<S>(c: &mut Conn<S>, keys: &SessionKeys, state: &RecoveryState) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // 1. BEGIN_RECOVERY Case A — fresh replica (empty KV) → truncate_to = 0, new recovery_id = 1.
    c.send(0, &wire::encode_begin_recovery(keys, 0, 0, 1, 0)).await.map_err(|e| format!("begin send: {e}"))?;
    match wire::decode(&c.recv().await.map_err(|e| format!("begin recv: {e}"))?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::RecoveryAck { .. } => {}
        other => return Err(format!("BEGIN_RECOVERY: expected RECOVERY_ACK, got {other:?}")),
    }

    // 2. Catch-up: REBUILD_APPLY (APPLY_TOKEN NO_SAMPLE) the durable prompt+generated tokens to
    //    rebuild the engine KV, then CATCH_UP_CONTEXT advances the stage SM to FROZEN_READY.
    for (i, tok) in state.replay_tokens().into_iter().enumerate() {
        apply(c, keys, i as i64, tok, true).await?;
    }
    c.send(0, &wire::encode_catch_up_context(keys, 0, 1, state.input_frontier())).await.map_err(|e| format!("catchup send: {e}"))?;
    match wire::decode(&c.recv().await.map_err(|e| format!("catchup recv: {e}"))?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::CatchUpReady { applied_input_pos } if applied_input_pos == state.input_frontier() => {}
        other => return Err(format!("CATCH_UP: expected CATCH_UP_READY @ {}, got {other:?}", state.input_frontier())),
    }

    // 3. INSTALL_SAMPLER_CHECKPOINT from the last durable GENERATION_COMMIT's snapshot (I7b/I15
    //    restore point) — restores the exact RNG state at generation_durable_pos.
    c.send(0, &wire::encode_install_sampler_checkpoint(keys, 0, state.checkpoint_id, &state.last_checkpoint)).await.map_err(|e| format!("install send: {e}"))?;
    match wire::decode(&c.recv().await.map_err(|e| format!("install recv: {e}"))?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::SamplerCheckpointInstalled { .. } => {}
        Msg::Err { code } => return Err(format!("INSTALL_SAMPLER_CHECKPOINT errored: code {code}")),
        other => return Err(format!("INSTALL: expected SAMPLER_CHECKPOINT_INSTALLED, got {other:?}")),
    }

    // 4. Activation transaction (RECOVERY): COMMIT → COMMITTED, FINALIZE → FINALIZED (ACTIVE_FINAL).
    let tuple = ActivationTuple { kind: ActivationKind::Recovery, epoch: 0, recovery_id: 1, attempt: 0, sampler_checkpoint_id: state.checkpoint_id };
    c.send(0, &wire::encode_commit_activation(keys, &tuple, 1)).await.map_err(|e| format!("commit send: {e}"))?;
    match wire::decode(&c.recv().await.map_err(|e| format!("commit recv: {e}"))?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::ActivationCommitted(_) => {}
        other => return Err(format!("COMMIT_ACTIVATION: expected ACTIVATION_COMMITTED, got {other:?}")),
    }
    c.send(0, &wire::encode_finalize_activation(keys, &tuple, 1)).await.map_err(|e| format!("finalize send: {e}"))?;
    match wire::decode(&c.recv().await.map_err(|e| format!("finalize recv: {e}"))?.payload, keys).map_err(|e| e.to_string())?.1 {
        Msg::ActivationFinalized => {}
        other => return Err(format!("FINALIZE: expected ACTIVATION_FINALIZED, got {other:?}")),
    }

    Ok(())
}

/// The shared, per-test-run context every kill-window is driven against.
struct Harness<'a> {
    cluster: &'a Cluster,
    connector: &'a TcpMtls,
    binary: &'a str,
    keys: &'a SessionKeys,
    model_path: &'a str,
    n_ctx: i32,
    prompt: &'a [u32],
    n: usize,
}

/// Run one kill-window: generate against a real subprocess S_P, kill -9 at the window, recover onto
/// a replacement, resume to `n`. Returns `(committed_tokens ⊕ resumed_tokens, commit_stream_path,
/// detection_to_resumed_wall)`.
async fn run_window(h: &Harness<'_>, window: Window) -> Result<(Vec<u32>, std::path::PathBuf, std::time::Duration), String> {
    let (cluster, connector, binary, keys, model_path, n_ctx, prompt, n) =
        (h.cluster, h.connector, h.binary, h.keys, h.model_path, h.n_ctx, h.prompt, h.n);
    let cfg_hash = sampling_config().hash();
    let dir = std::env::temp_dir().join(format!("hydra-d1-2b-{}-{:?}", std::process::id(), window));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let cs_path = dir.join("commit-stream.wal");

    // The coordinator's durable commit stream (survives the worker kill — only S_P dies).
    let mut cs = CommitStream::create(&cs_path, CLUSTER_ID, SESSION_ID).map_err(|e| e.to_string())?;
    cs.append_initial_commit(&fence(), &admission(prompt), &initial_checkpoint_bytes(INITIAL_CHECKPOINT_ID, &sampling_config()), 1).map_err(|e| e.to_string())?;
    let mut group = GroupCommitter::new(K);

    // Initial S_P subprocess.
    let mut sp = SubprocessWorker::spawn(binary, &boot(cluster, "sp-init", sp_config(model_path, keys, n_ctx, 0, false))).map_err(|e| e.to_string())?;
    let mut c = connector.connect(sp.addr, "sp-init").await.map_err(|e| format!("init connect: {e}"))?;
    prefill(&mut c, keys, prompt).await?;

    // The kill boundary (which durable group to stop at). Positions 0-indexed; durable at K-1, 2K-1…
    // Steady/BetweenFsyncAndEmit kill right after a commit; SampledAhead kills after sampling past it.
    let durable_target: i64 = K as i64 - 1; // first group boundary (position 2 at K=3)
    let mut input_pos = prompt.len() as i64;
    let mut generated_before_kill: Vec<u32> = Vec::new();
    let mut killed = false;

    for pos in 0..n as i64 {
        let (tok, snap) = sample(&mut c, keys, pos, &cfg_hash).await?;
        generated_before_kill.push(tok);
        group.push(pos, tok, snap);

        // Commit on the count threshold.
        if group.count_ready() {
            let b = group.take().unwrap();
            cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).map_err(|e| e.to_string())?;

            // Window 1 & 3 kill AT a commit boundary (durable just advanced to b.last_pos).
            if !killed && b.last_pos == durable_target && matches!(window, Window::Steady | Window::BetweenFsyncAndEmit) {
                killed = true;
                break;
            }
        }

        // Window 2 kills AFTER sampling two positions past the last durable commit (provisional ahead).
        if !killed && window == Window::SampledAhead && pos == durable_target + 2 {
            killed = true;
            break;
        }

        if (pos as usize + 1) < n {
            apply(&mut c, keys, input_pos, tok, false).await?;
            input_pos += 1;
        }
    }
    assert!(killed, "the window must have triggered a kill");

    // ---- kill -9 (detection starts here) ----
    let t_detect = Instant::now();
    drop(c);
    sp.kill9().map_err(|e| format!("kill9: {e}"))?;

    // I7b/I15: any provisional sampled-ahead outputs above generation_durable_pos are discarded —
    // they were buffered in the group committer but never made durable, so recovery drops them and
    // re-samples from the installed checkpoint. (Steady: the buffer is already empty.)
    let _ = group.take();

    // Reconstruct recovery inputs from the durable ledger alone (I3).
    let state = recovery::read(&cs_path).map_err(|e| format!("recovery read: {e}"))?;
    let durable_pos = state.generation_durable_pos;
    let committed = state.generated_token_ids();

    // Spawn the replacement S_P (FROZEN) and drive recovery through the real machinery.
    let mut rp = SubprocessWorker::spawn(binary, &boot(cluster, "sp-recover", sp_config(model_path, keys, n_ctx, 1, true))).map_err(|e| e.to_string())?;
    let mut rc = connector.connect(rp.addr, "sp-recover").await.map_err(|e| format!("recover connect: {e}"))?;
    drive_recovery(&mut rc, keys, &state).await?;
    let detect_to_resumed = t_detect.elapsed();

    // Resume: SAMPLE_NEXT at durable+1, feeding back autoregressively, committing in k-groups.
    let mut resumed: Vec<u32> = Vec::new();
    input_pos = state.input_frontier();
    for pos in (durable_pos + 1)..n as i64 {
        let (tok, snap) = sample(&mut rc, keys, pos, &cfg_hash).await?;
        resumed.push(tok);
        group.push(pos, tok, snap);
        if group.count_ready() {
            let b = group.take().unwrap();
            cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).map_err(|e| e.to_string())?;
        }
        if (pos as usize + 1) < n {
            apply(&mut rc, keys, input_pos, tok, false).await?;
            input_pos += 1;
        }
    }
    // Flush any trailing partial group durably (finish-of-generation).
    if let Some(b) = group.take() {
        cs.append_generation_commit(&fence(), b.first_pos, b.last_pos, &b.tokens, &b.snapshot).map_err(|e| e.to_string())?;
    }
    drop(rc);
    rp.kill9().ok();

    // The client-visible stream = committed pre-kill prefix ⊕ post-recovery suffix.
    let mut visible = committed;
    visible.extend(resumed);
    let _ = generated_before_kill; // provisional sampled-ahead outputs are intentionally discarded.

    drop(cs);
    Ok((visible, cs_path, detect_to_resumed))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d1_recovery_three_kill_windows_are_byte_identical_to_an_uninterrupted_seeded_run() {
    let Some(model_path) = dev_model_path() else {
        eprintln!("SKIP: no engine/model (dev-environment artifacts)");
        return;
    };

    // Tokenize a fixed prompt (model freed before workers load — 8 GB dev-box discipline).
    let prompt: Vec<u32> = {
        let model = hydra_engine_sys::Model::load(&model_path, 0).expect("load");
        model.tokenize("The capital of France is").expect("tokenize").into_iter().map(|t| t as u32).collect()
    };
    assert!(prompt.len() >= 2, "need a non-trivial prompt");
    let n = 9usize; // generated tokens (spans 3 group commits at k=3)
    let n_ctx = prompt.len() as i32 + n as i32 + 8;

    let keys = SessionKeys::dev(0xD1);
    let cluster = Cluster::new().unwrap();
    let connector = cluster.coordinator_connector().unwrap();
    let binary = env!("CARGO_BIN_EXE_hydra-worker");

    // The uninterrupted seeded reference (in-process endpoint; deterministic).
    let ref_id = cluster.issue("sp-ref").unwrap();
    let ref_addr = hydra_worker::pair::spawn_endpoint(sp_config(&model_path, &keys, n_ctx, 0, false), cluster.ca.server_config(&ref_id).unwrap());
    let reference = reference_run(&connector, ref_addr, "sp-ref", &keys, &prompt, n).await.expect("reference run");
    assert_eq!(reference.len(), n);

    let harness = Harness { cluster: &cluster, connector: &connector, binary, keys: &keys, model_path: &model_path, n_ctx, prompt: &prompt, n };
    for window in [Window::Steady, Window::SampledAhead, Window::BetweenFsyncAndEmit] {
        let (visible, cs_path, timing) = run_window(&harness, window)
            .await
            .unwrap_or_else(|e| panic!("window {window:?}: {e}"));

        // (b) byte-identical: committed prefix ⊕ post-recovery suffix == uninterrupted seeded run.
        assert_eq!(visible, reference, "window {window:?}: recovered stream must equal the uninterrupted seeded run");

        // (c) disk truth: the commit stream reads back I19-valid, no output position twice.
        let stats = recovery::verify(&cs_path).unwrap_or_else(|e| panic!("window {window:?} verify: {e}"));
        assert_eq!(stats.committed_positions, n, "window {window:?}: every output position committed exactly once");
        assert!(stats.positions_strictly_increasing, "window {window:?}: no position committed twice / out of order");
        assert_eq!(stats.max_position, n as i64 - 1);

        // (a) SSE id continuity: the event log is a pure function of this commit stream; dense,
        // gap-free, non-repeating ids follow from (c)'s strictly-increasing single-cover.
        let state = recovery::read(&cs_path).unwrap();
        assert_eq!(state.generated_token_ids(), reference, "window {window:?}: durable ledger == reference (event-log continuity)");

        eprintln!(
            "window {window:?}: detection->resumed-stream {:?} (HONESTY: local-pair dev machine, NOT the <15s LAN/M3 D1 target)",
            timing
        );
        let _ = std::fs::remove_dir_all(cs_path.parent().unwrap());
    }
}
