//! **M3 gate row 8 — the calibration clause, under the ratified §7.24 amendment.**
//!
//! The gate is calibration: the deployed placement's **measured** TPOT must land within 15 % of the
//! **predicted** TPOT. The cost model against reality, not the search against itself.
//!
//! # Coefficient provenance (the binding §7.24 rule)
//!
//! Every coefficient is sourced **independently of the measurement it is asked to predict**:
//!
//! * **`fixed` and `per_layer`** come from the **windowed-context decomposition** — timing the full
//!   layer range and both halves through the *engine alone*, then solving `t(L) = fixed + L·per_layer`.
//!   That is an engine measurement with **no pipeline, no mTLS and no coordinator in it**, so it
//!   cannot be a residual of the thing it predicts. It is re-measured here in the same run so all
//!   coefficients share one machine state. Banked as the standard per-device-class method.
//! * **`protocol`** comes from the **zero-inference microbench**
//!   (`protocol_microbench.rs`): the real four-frame exchange against an echo peer that runs no
//!   model at all. **0.438 ms per coordinator↔stage exchange** on loopback.
//!
//! Nothing here is fitted to the pipeline run below. *Fitted-here-passes-here is worthless;
//! fitted-here-predicts-there is a cost model.*
//!
//! # Out-of-sample by construction
//!
//! The coefficients come from a configuration that is not a pipeline at all, and they are then used
//! to predict **two different deployed configurations** — the pipeline split at `k = n_layer/2` and
//! again at `k = n_layer/3`. A model that only works at the split it was derived from would fail
//! the second one.
//!
//! Honesty: local-pair (both stages on one Mac, loopback mTLS). This calibrates the cost model's
//! shape; the wired-LAN envelope is a separate, hardware-contingent owed item.

use std::time::Instant;

use hydra_worker::pair::{dev_model_path, time_teacher_forced_pipeline, Cluster};
use hydra_worker::wire::SessionKeys;
use hydra_worker::worker::WorkerConfig;

/// Measured independently by `hydra-worker/tests/protocol_microbench.rs` — zero inference in the
/// loop. Not fitted to anything measured below.
const PROTOCOL_MS_PER_EXCHANGE: f64 = 0.438;

/// **A HARNESS constant, not a model term.** `run_teacher_forced_pipeline` is the bit-exact ANCHOR
/// path: its final stage BLAKE3-hashes the entire `n_vocab` logits vector every token so the anchor
/// has a witness. Production sampling returns a token and does no such hash. Measured independently
/// at 9.607 ms/token by `digest_cost.rs`. Added to the PREDICTION of the harness path, never to the
/// production cost model — see PROJECT_STATE §7.25.
const ANCHOR_WITNESS_MS: f64 = 9.607;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.len() % 2 == 1 { v[v.len() / 2] } else { (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0 }
}

/// Time steady-state decode through one engine context windowed to `[l0, l1)`. No pipeline.
fn time_window(model: &hydra_engine_sys::Model, l0: i32, l1: i32, embeddings: bool, n_embd: usize, n_ctx: i32) -> f64 {
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
    for pos in 0..8 {
        run(&mut ctx, pos, &mut out);
    }
    let mut s = Vec::new();
    for i in 0..40 {
        let t = Instant::now();
        run(&mut ctx, 8 + i, &mut out);
        s.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    median(s)
}

#[tokio::test]
#[ignore = "calibration: needs the engine + dev model; run explicitly for the M3 gate"]
async fn deployed_tpot_is_within_15_percent_of_the_solver_prediction() {
    let Some(path) = dev_model_path() else {
        eprintln!("SKIP: no engine/model");
        return;
    };
    const N_TOKENS: usize = 64;
    const WARMUP: usize = 8;

    let model = hydra_engine_sys::Model::load(&path, 0).expect("load");
    let n_layer = model.n_layer();
    let n_embd = model.n_embd() as usize;
    let n_ctx = (N_TOKENS + 16) as i32;

    // ---------- coefficients: the windowed-context decomposition (engine only) ----------
    let k_half = n_layer / 2;
    let full = time_window(&model, 0, -1, false, n_embd, n_ctx);
    let lower = time_window(&model, 0, k_half, true, n_embd, n_ctx);
    let upper = time_window(&model, k_half, -1, false, n_embd, n_ctx);
    // t(24) = fixed + 24p ; mean of the two halves = fixed + 12p  =>  p and fixed follow.
    let half_mean = (lower + upper) / 2.0;
    let per_layer = (full - half_mean) / (n_layer - k_half) as f64;
    let fixed = half_mean - k_half as f64 * per_layer;
    drop(model);

    eprintln!(
        "COEFFICIENTS (windowed-context decomposition — engine only, no pipeline)\n\
         \x20 full {full:.2} · lower {lower:.2} · upper {upper:.2} ms/tok\n\
         \x20 fixed = {fixed:.3} ms/stage · per_layer = {per_layer:.4} ms/layer-tok\n\
         \x20 protocol = {PROTOCOL_MS_PER_EXCHANGE:.3} ms/exchange (independent microbench)"
    );

    // ---------- predict and measure TWO different deployed splits ----------
    let keys = SessionKeys::dev(0xCA);
    let cluster = Cluster::new().unwrap();
    let s1_id = cluster.issue("worker-s1").unwrap();
    let s2_id = cluster.issue("worker-s2").unwrap();
    let connector = cluster.coordinator_connector().unwrap();
    let tokens: Vec<u32> = vec![1u32; N_TOKENS];

    let mut worst_err: f64 = 0.0;
    for k in [n_layer / 2, n_layer / 3] {
        let cfg = |rank: u16, first: i32, last: i32, is_final: bool, recv: bool| WorkerConfig {
            keys: keys.clone(), rank, layer_first: first, layer_last: last, is_final,
            receives_tokens: recv, epoch: 0, recovery_id: 0, model_path: Some(path.clone()),
            n_gpu_layers: 0, n_ctx, sampler_config: None, recovery_start: false, shard_manifest: None,
        };
        let a = hydra_worker::pair::spawn_endpoint(cfg(0, 0, k, false, true), cluster.ca.server_config(&s1_id).unwrap());
        let b = hydra_worker::pair::spawn_endpoint(cfg(1, k, -1, true, false), cluster.ca.server_config(&s2_id).unwrap());
        let ep = hydra_worker::pair::Endpoints::new(a, "worker-s1", b, "worker-s2");

        let per_token = time_teacher_forced_pipeline(&connector, &ep, &keys, &tokens).await.expect("pipeline");
        let steady: Vec<f64> = per_token[WARMUP..].iter().map(|d| d.as_secs_f64() * 1000.0).collect();
        let measured = median(steady.clone());

        // §7.24: two stages, so `fixed` twice and `protocol` twice; the loopback rtt/transfer term
        // is below the noise floor and is not invented.
        let model_predicted = 2.0 * fixed + n_layer as f64 * per_layer + 2.0 * PROTOCOL_MS_PER_EXCHANGE;
        // HARNESS TERM — explicitly NOT part of the production cost model. The path being timed is
        // `run_teacher_forced_pipeline`, the bit-exact ANCHOR harness, and its final stage computes
        // a `logits_digest` witness over the whole n_vocab vector on every token so the anchor has
        // something to compare. Production sampling returns a token and does no such hash. Measured
        // independently at 9.607 ms/token (`digest_cost.rs`), and it is added to the PREDICTION of
        // the harness path rather than to the model, because the model must keep predicting
        // production.
        let predicted = model_predicted + ANCHOR_WITNESS_MS;
        let err = (measured - predicted).abs() / predicted;
        worst_err = worst_err.max(err);

        eprintln!(
            "SPLIT k={k}/{n_layer}: predicted {predicted:.2} ms/tok  measured {measured:.2} ms/tok  \
             residual {:+.2}  ERROR {:.1} %  [model {:.2} + harness witness {:.2}]",
            measured - predicted,
            err * 100.0,
            model_predicted,
            ANCHOR_WITNESS_MS
        );
    }

    assert!(
        worst_err <= 0.15,
        "M3 calibration clause under the §7.24 amendment: worst error across two out-of-sample \
         splits is {:.1} %, above the 15 % gate. Coefficients are independently sourced \
         (decomposition + zero-inference microbench), so this is a COST-MODEL finding to escalate \
         — a third term would need its own physical story — and NOT something to tune away.",
        worst_err * 100.0
    );
}
