//! **M3 gate row 8 — the calibration clause.**
//!
//! The gate is calibration: the deployed placement's **measured** TPOT must land within 15 % of the
//! **predicted** TPOT. The cost model against reality, not the search against itself.
//!
//! # Which path is measured (PROJECT_STATE §7.25, ruled)
//!
//! The gate measures the **production-shaped** path: real sampling, no digest witness. §7.25 found
//! that the earlier measurement was timing `run_teacher_forced_pipeline` — the *correctness
//! harness* — whose final stage BLAKE3-hashes the whole `n_vocab` logits vector every token so the
//! rule-14 anchor has something to compare. That is 9.607 ms/token of work a deployed session never
//! does, and charging the cost model for it was a wrong measurement target, not a missing term.
//!
//! So there are two tests here, and their roles are not symmetric:
//!
//! * [`production_shaped_tpot_is_within_15_percent_of_the_solver_prediction`] — **the gate.**
//!   `SAMPLE_NEXT` → `SAMPLED` → feed the sampled token back; the decode ack carries no witness
//!   (`Worker::retain_and_ack` emits `output_checksum` only for a teacher-forced apply). Nothing is
//!   subtracted from the measurement and nothing extra is added to the prediction.
//! * [`anchor_path_tpot_is_within_15_percent_when_the_harness_witness_is_attributed`] —
//!   **corroboration only.** Same model, same coefficients, applied to the anchor path with the
//!   witness attributed as a harness constant. It exists because a cost model that predicts two
//!   differently-shaped paths from one set of coefficients is better evidence than one that
//!   predicts a single path.
//!
//! # Coefficient provenance (the binding §7.24 rule)
//!
//! Every coefficient is sourced **independently of the measurement it is asked to predict**:
//!
//! * **`fixed` and `per_layer`** come from the **windowed-context decomposition** — timing the full
//!   layer range and both halves through the *engine alone*, then solving `t(L) = fixed + L·per_layer`.
//!   That is an engine measurement with **no pipeline, no mTLS and no coordinator in it**, so it
//!   cannot be a residual of the thing it predicts. It is re-measured in the same run so all
//!   coefficients share one machine state. Banked as the standard per-device-class method.
//! * **`protocol`** comes from the **zero-inference microbench** (`protocol_microbench.rs`): the
//!   real frame exchanges against an echo peer that runs no model at all. Each crossing is measured
//!   **separately**, because they are not the same size — a `FWD` carries an `n_embd` boundary
//!   while a `SAMPLED` carries a small snapshot, and reusing one number for all three would price
//!   the cheapest crossing as if it were the most expensive.
//!
//! Nothing here is fitted to the pipeline runs below. *Fitted-here-passes-here is worthless;
//! fitted-here-predicts-there is a cost model.*
//!
//! # Out-of-sample by construction
//!
//! The coefficients come from a configuration that is not a pipeline at all, and they are then used
//! to predict **two different deployed configurations** — the split at `k = n_layer/2` and again at
//! `k = n_layer/3`. A model that only works at the split it was derived from would fail the second.
//!
//! # Position matching (why the decomposition runs where the pipeline runs)
//!
//! Per-token decode cost grows with context depth — attention reads a longer KV. So the
//! decomposition measures the **same position range** the pipeline will occupy (prompt length
//! onwards), not positions near zero. Otherwise the prediction would be systematically low for a
//! measurement reason and the model would be blamed for it.
//!
//! Honesty: local-pair (both stages on one Mac, loopback mTLS), debug profile throughout so every
//! coefficient and every measurement share one build. This calibrates the cost model's shape; the
//! wired-LAN envelope is a separate, hardware-contingent owed item.

use std::time::Instant;

use hydra_worker::pair::{dev_model_path, time_generation_pipeline, time_teacher_forced_pipeline, Cluster, Endpoints};
use hydra_worker::sampler::SamplingConfig;
use hydra_worker::wire::SessionFence;
use hydra_worker::worker::WorkerConfig;

/// Per-crossing protocol costs, measured by `protocol_microbench.rs` with **zero inference** in the
/// loop (2026-08-23, debug profile, loopback mTLS, `n_embd = 896`, 256 cycles after 16 warm-up).
/// Not fitted to anything measured below.
const XA_APPLY_FWD_MS: f64 = 0.463;
/// `FWD` out / `APPLIED_ACK` back — the coordinator↔S_P crossing.
const XB_FWD_ACK_MS: f64 = 0.431;
/// `SAMPLE_NEXT` out / `SAMPLED` back — the third crossing of a **production** decode step, and
/// measurably the cheapest of the three because it carries no boundary.
const XC_SAMPLE_MS: f64 = 0.154;

/// **A HARNESS constant, not a model term.** `run_teacher_forced_pipeline` is the bit-exact ANCHOR
/// path: its final stage BLAKE3-hashes the entire `n_vocab` logits vector every token so the anchor
/// has a witness. Production decode does no such hash — see `Worker::retain_and_ack`. Measured
/// independently at 9.607 ms/token by `digest_cost.rs`. Used **only** by the corroboration test,
/// and never added to the production model — see PROJECT_STATE §7.25.
const ANCHOR_WITNESS_MS: f64 = 9.607;

const PROMPT_TOKENS: usize = 64;
const STEPS: usize = 72;
const WARMUP: usize = 8;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.len() % 2 == 1 { v[v.len() / 2] } else { (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0 }
}

/// Time steady-state decode through one engine context windowed to `[l0, l1)`, over the position
/// range `[from, from + n)`. No pipeline, no mTLS, no coordinator. Positions below `from` are
/// applied first so the KV depth matches what the pipeline will see; the first [`WARMUP`] timed
/// samples are then discarded exactly as the pipeline's are.
fn time_window(
    model: &hydra_engine_sys::Model,
    layers: (i32, i32),
    embeddings: bool,
    n_embd: usize,
    n_ctx: i32,
    range: (i32, usize),
) -> f64 {
    let ((l0, l1), (from, n)) = (layers, range);
    let mut ctx = model.context(l0, l1, embeddings, n_ctx, n_ctx).expect("ctx");
    let mut out = vec![0f32; n_embd];
    let run = |ctx: &mut hydra_engine_sys::Context<'_>, pos: i32, out: &mut Vec<f32>| {
        if embeddings {
            ctx.apply_tokens(&[1], pos, Some(out)).expect("apply");
        } else {
            ctx.apply_tokens(&[1], pos, None).expect("apply");
            let _ = ctx.logits(0).expect("logits");
        }
    };
    for pos in 0..from {
        run(&mut ctx, pos, &mut out);
    }
    let mut s = Vec::new();
    for i in 0..n {
        let t = Instant::now();
        run(&mut ctx, from + i as i32, &mut out);
        s.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    median(s[WARMUP..].to_vec())
}

/// The engine-only coefficients of the §7.24 model, solved from the windowed-context decomposition.
struct Coefficients {
    fixed: f64,
    per_layer: f64,
    n_layer: usize,
}

fn decompose(path: &str, n_ctx: i32, from: i32, n: usize) -> Coefficients {
    let model = hydra_engine_sys::Model::load(path, 0).expect("load");
    let n_layer = model.n_layer();
    let n_embd = model.n_embd() as usize;
    let k_half = n_layer / 2;
    let full = time_window(&model, (0, -1), false, n_embd, n_ctx, (from, n));
    let lower = time_window(&model, (0, k_half), true, n_embd, n_ctx, (from, n));
    let upper = time_window(&model, (k_half, -1), false, n_embd, n_ctx, (from, n));
    // t(n_layer) = fixed + n_layer·p ; mean of the two halves = fixed + (n_layer/2)·p  =>  p, fixed.
    let half_mean = (lower + upper) / 2.0;
    let per_layer = (full - half_mean) / (n_layer - k_half) as f64;
    let fixed = half_mean - k_half as f64 * per_layer;

    eprintln!(
        "COEFFICIENTS (windowed-context decomposition — engine only, no pipeline)\n\
         \x20 positions [{from},{}) · full {full:.2} · lower {lower:.2} · upper {upper:.2} ms/tok\n\
         \x20 fixed = {fixed:.3} ms/stage · per_layer = {per_layer:.4} ms/layer-tok",
        from as usize + n
    );
    Coefficients { fixed, per_layer, n_layer: n_layer as usize }
}

/// Spawn a two-stage local pair split at `k` and hand back its endpoints.
struct Pair {
    _cluster: Cluster,
    connector: hydra_transport::tcp_mtls::TcpMtls,
    endpoints: Endpoints,
    fence: SessionFence,
}

fn spawn_pair(path: &str, k: i32, n_ctx: i32, seed: u8) -> Pair {
    let fence = SessionFence::dev(seed);
    let cluster = Cluster::new().unwrap();
    let s1_id = cluster.issue("worker-s1").unwrap();
    let s2_id = cluster.issue("worker-s2").unwrap();
    let connector = cluster.coordinator_connector().unwrap();
    let cfg = |rank: u16, first: i32, last: i32, is_final: bool, recv: bool| WorkerConfig {
        fence: fence.clone(),
        rank,
        layer_first: first,
        layer_last: last,
        is_final,
        receives_tokens: recv,
        epoch: 0,
        recovery_id: 0,
        model_path: Some(path.to_string()),
        n_gpu_layers: 0,
        n_ctx,
        sampler_config: if is_final { Some(SamplingConfig::greedy()) } else { None },
        recovery_start: false,
        shard_manifest: None,
    };
    let a = hydra_worker::pair::spawn_endpoint(cfg(0, 0, k, false, true), cluster.ca.server_config(&s1_id).unwrap());
    let b = hydra_worker::pair::spawn_endpoint(cfg(1, k, -1, true, false), cluster.ca.server_config(&s2_id).unwrap());
    let endpoints = Endpoints::new(a, "worker-s1", b, "worker-s2");
    Pair { _cluster: cluster, connector, endpoints, fence }
}

/// **THE M3 GATE (row 8).** Predicted vs measured TPOT on the production-shaped decode path.
#[tokio::test]
#[ignore = "calibration: needs the engine + dev model; run explicitly for the M3 gate"]
async fn production_shaped_tpot_is_within_15_percent_of_the_solver_prediction() {
    let Some(path) = dev_model_path() else {
        eprintln!("SKIP: no engine/model");
        return;
    };
    let n_ctx = (PROMPT_TOKENS + STEPS + 16) as i32;

    // Coefficients over exactly the positions the pipeline will decode at.
    let c = decompose(&path, n_ctx, PROMPT_TOKENS as i32, STEPS);
    // A production decode step crosses three times: APPLY_TOKEN→FWD, FWD→APPLIED_ACK,
    // SAMPLE_NEXT→SAMPLED. Each priced at its own independently-measured cost.
    let protocol = XA_APPLY_FWD_MS + XB_FWD_ACK_MS + XC_SAMPLE_MS;
    eprintln!(
        "\x20 protocol = {XA_APPLY_FWD_MS:.3} (A) + {XB_FWD_ACK_MS:.3} (B) + {XC_SAMPLE_MS:.3} (C) \
         = {protocol:.3} ms/token (independent microbench)"
    );

    let prompt: Vec<u32> = vec![1u32; PROMPT_TOKENS];
    let config = SamplingConfig::greedy();

    let mut worst_err: f64 = 0.0;
    for (i, k) in [c.n_layer / 2, c.n_layer / 3].into_iter().enumerate() {
        let p = spawn_pair(&path, k as i32, n_ctx, 0xC0 + i as u8);
        let per_token = time_generation_pipeline(&p.connector, &p.endpoints, &p.fence, &config, &prompt, STEPS)
            .await
            .expect("generation pipeline");
        let steady: Vec<f64> = per_token[WARMUP..].iter().map(|d| d.as_secs_f64() * 1000.0).collect();
        let measured = median(steady);

        // §7.24: two stages, so `fixed` twice; every layer once; the three crossings once each. The
        // loopback rtt/transfer term is below the noise floor and is not invented.
        let predicted = 2.0 * c.fixed + c.n_layer as f64 * c.per_layer + protocol;
        let err = (measured - predicted).abs() / predicted;
        worst_err = worst_err.max(err);

        eprintln!(
            "PRODUCTION SPLIT k={k}/{}: predicted {predicted:.2} ms/tok  measured {measured:.2} ms/tok  \
             residual {:+.2}  ERROR {:.1} %  [{} steady samples after {WARMUP} discarded, {PROMPT_TOKENS}-token prompt]",
            c.n_layer,
            measured - predicted,
            err * 100.0,
            STEPS - WARMUP,
        );
    }

    assert!(
        worst_err <= 0.15,
        "M3 calibration clause on the production-shaped path: worst error across two out-of-sample \
         splits is {:.1} %, above the 15 % gate. Coefficients are independently sourced \
         (decomposition + zero-inference microbench), so this is a COST-MODEL finding to escalate \
         — any further term would need its own physical story — and NOT something to tune away.",
        worst_err * 100.0
    );
}

/// **Corroboration, not the gate.** The same coefficients predicting the *anchor* path, whose final
/// stage pays the teacher-forced logits witness. The witness is added to the **prediction of the
/// harness path**, never to the production model.
#[tokio::test]
#[ignore = "calibration corroboration; run explicitly for the M3 gate"]
async fn anchor_path_tpot_is_within_15_percent_when_the_harness_witness_is_attributed() {
    let Some(path) = dev_model_path() else {
        eprintln!("SKIP: no engine/model");
        return;
    };
    // The anchor path teacher-forces from position 0, so its coefficients are measured there.
    let n_ctx = (PROMPT_TOKENS + STEPS + 16) as i32;
    let c = decompose(&path, n_ctx, 0, PROMPT_TOKENS + STEPS);
    // The anchor path crosses twice per token — there is no SAMPLE_NEXT in it.
    let protocol = XA_APPLY_FWD_MS + XB_FWD_ACK_MS;

    let tokens: Vec<u32> = vec![1u32; PROMPT_TOKENS + STEPS];

    let mut worst_err: f64 = 0.0;
    for (i, k) in [c.n_layer / 2, c.n_layer / 3].into_iter().enumerate() {
        let p = spawn_pair(&path, k as i32, n_ctx, 0xA0 + i as u8);
        let per_token = time_teacher_forced_pipeline(&p.connector, &p.endpoints, &p.fence, &tokens).await.expect("pipeline");
        let steady: Vec<f64> = per_token[WARMUP..].iter().map(|d| d.as_secs_f64() * 1000.0).collect();
        let measured = median(steady);

        let model_predicted = 2.0 * c.fixed + c.n_layer as f64 * c.per_layer + protocol;
        let predicted = model_predicted + ANCHOR_WITNESS_MS;
        let err = (measured - predicted).abs() / predicted;
        worst_err = worst_err.max(err);

        eprintln!(
            "ANCHOR SPLIT k={k}/{}: predicted {predicted:.2} ms/tok  measured {measured:.2} ms/tok  \
             residual {:+.2}  ERROR {:.1} %  [model {model_predicted:.2} + harness witness {ANCHOR_WITNESS_MS:.2}]",
            c.n_layer,
            measured - predicted,
            err * 100.0,
        );
    }

    assert!(
        worst_err <= 0.15,
        "anchor-path corroboration: worst error {:.1} % > 15 %. The production gate is the other \
         test; this one failing means the witness attribution or the coefficients moved.",
        worst_err * 100.0
    );
}
