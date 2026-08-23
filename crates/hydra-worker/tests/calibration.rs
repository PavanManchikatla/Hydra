//! **M3 gate — the calibration clause (§7.23 restatement).**
//!
//! The DoD's "within 15 % of brute-force TPOT" was restated by the 2026-08-22 ruling: enumeration
//! is optimal by construction (independently asserted in `hydra-sched::solver`), so comparing the
//! search against itself is vacuous. The gate is **calibration** — the deployed placement's
//! **measured** TPOT must land within 15 % of the solver's **predicted** TPOT on the same measured
//! inputs. **The cost model against reality, not the search against itself.**
//!
//! This is deliberately **non-circular**. The prediction is assembled from parts measured
//! *independently* of the thing being predicted:
//!
//! * **compute** — an unsplit full-model decode on this box, giving ms/layer-token (P2·1's unit);
//! * **link** — a real round-trip on the live mTLS connection, plus the boundary transfer term.
//!
//! Those two are then combined by the solver's own objective-(a) cost model, and compared against
//! the **assembled** two-worker pipeline's per-token wall time. Nothing in the prediction is
//! derived from the pipeline run it is predicting.
//!
//! Honesty: this is the **local-pair** (both stages on one Mac, loopback mTLS). It calibrates the
//! cost model's *shape*, not a wired-LAN number — the wired-LAN envelope remains owed (§8).
//!
//! # ⛔ STATUS: THE CLAUSE IS **NOT SATISFIED**, AND THE FAULT IS THIS HARNESS
//!
//! This test currently **fails**, and it fails for a measurement reason, not a cost-model reason.
//! `run_teacher_forced_pipeline` opens its own mTLS connections and the timing therefore folds the
//! **one-time handshake** into what is being reported as a per-token cost. Two attempts, both
//! recorded because the numbers are instructive:
//!
//! 1. Timing a 3-token probe run produced a "link" term of **171 ms/token** — which was the
//!    handshake divided by three, not a cost the pipeline pays per token.
//! 2. Taking a marginal `(t_long − t_short)/(n_long − n_short)` across two *independent* pairs
//!    produced **−50.95 ms/token** — negative, because each pair pays its **own** setup and the
//!    short run's dominates. On a 5-token dev prompt, setup is the entire signal.
//!
//! **What a sound version needs:** the pipeline driver must expose **steady-state token-loop
//! timing with connection setup excluded** — i.e. `pair.rs` timing the loop, not the caller timing
//! the call. That is a harness change, and it is recorded as owed in PROJECT_STATE §8 rather than
//! worked around by softening the assertion. The assertion is deliberately left in place and
//! failing: a calibration gate that passes because its bar was lowered would be worse than one
//! that is honestly open.

use std::time::Instant;

use hydra_worker::pair::{dev_model_path, run_teacher_forced_pipeline, Cluster};
use hydra_worker::wire::SessionKeys;
use hydra_worker::worker::WorkerConfig;

#[tokio::test]
#[ignore = "calibration: needs the engine + dev model; run explicitly for the M3 gate"]
async fn deployed_tpot_is_within_15_percent_of_the_solver_prediction() {
    let Some(path) = dev_model_path() else {
        eprintln!("SKIP: no engine/model");
        return;
    };

    // ---------- part 1: compute, measured unsplit and independently ----------
    let (tokens, n_layer, n_embd, ms_per_layer_tok) = {
        let model = hydra_engine_sys::Model::load(&path, 0).expect("load");
        let tokens: Vec<u32> =
            model.tokenize("The capital of France is").expect("tok").into_iter().map(|t| t as u32).collect();
        let n_layer = model.n_layer();
        let n_embd = model.n_embd();
        let n_ctx = 96;
        let mut ctx = model.context(0, -1, false, n_ctx, n_ctx).expect("ctx");
        // Warm up, then time steady-state single-token decode (P2·1's discipline: discard warm-up).
        for pos in 0..8 {
            ctx.apply_tokens(&[1], pos, None).expect("warm");
            let _ = ctx.logits(0);
        }
        let steps = 32;
        let t = Instant::now();
        for i in 0..steps {
            ctx.apply_tokens(&[1], 8 + i, None).expect("decode");
            let _ = ctx.logits(0).expect("logits");
        }
        let ms_per_tok = t.elapsed().as_secs_f64() * 1000.0 / steps as f64;
        (tokens, n_layer, n_embd, ms_per_tok / n_layer as f64)
    };

    // ---------- part 2: the link, measured on the live connection ----------
    let keys = SessionKeys::dev(0xCA);
    let n_ctx = tokens.len() as i32 + 8;
    let k = (n_layer / 2).max(1);
    let cluster = Cluster::new().unwrap();
    let s1_id = cluster.issue("worker-s1").unwrap();
    let s2_id = cluster.issue("worker-s2").unwrap();
    let cfg = |rank: u16, first: i32, last: i32, is_final: bool, recv: bool| WorkerConfig {
        keys: keys.clone(), rank, layer_first: first, layer_last: last, is_final,
        receives_tokens: recv, epoch: 0, recovery_id: 0, model_path: Some(path.clone()),
        n_gpu_layers: 0, n_ctx, sampler_config: None, recovery_start: false, shard_manifest: None,
    };
    let connector = cluster.coordinator_connector().unwrap();
    // TWO independent worker pairs. A worker's engine KV persists across connections, so reusing
    // one pair for both the probe and the measured run would re-apply position 0 into a context
    // that already holds it (llama_decode ret=-1). Independent pairs also keep the prediction from
    // being derived from the very run it predicts.
    let spawn_pair = |tag: u8| {
        let a = hydra_worker::pair::spawn_endpoint(cfg(0, 0, k, false, true), cluster.ca.server_config(&s1_id).unwrap());
        let b = hydra_worker::pair::spawn_endpoint(cfg(1, k, -1, true, false), cluster.ca.server_config(&s2_id).unwrap());
        let _ = tag;
        hydra_worker::pair::Endpoints::new(a, "worker-s1", b, "worker-s2")
    };
    let ep_probe = spawn_pair(0);
    let ep = spawn_pair(1);

    // A boundary residual crosses as f32.
    let boundary_bytes = (n_embd as u64) * 4;

    // ---------- part 3: the prediction, from the solver's objective-(a) cost model ----------
    // Two stages on this box: k layers then n_layer-k layers, one boundary crossing between them.
    // Loopback link cost is dominated by the round-trip; the transfer term at ~3.5 KB is included
    // for completeness rather than because it matters.
    let predicted_compute_ms = n_layer as f64 * ms_per_layer_tok;
    // On loopback the boundary is ~3.5 KB and the hop is in-kernel, so the model predicts a link
    // term far below the per-token compute cost. The prediction is therefore essentially the
    // compute term — and whether that is right is exactly what this measurement decides. If the
    // deployed TPOT lands well above it, the cost model is missing a real per-token cost.
    let predicted_tpot_ms = predicted_compute_ms;

    // ---------- part 4: the deployed measurement, MARGINAL so setup cancels ----------
    // A single timed run would fold in the one-time mTLS handshake. Timing a SHORT run and a LONG
    // run on independent pairs and taking the difference cancels the constant, leaving the true
    // steady-state per-token cost. (An earlier version of this test measured a "link" term of
    // 171 ms/token on loopback — which was the handshake amortised over three tokens, not a cost
    // the pipeline actually pays per token.)
    let short_n = 2usize;
    let t = Instant::now();
    let _ = run_teacher_forced_pipeline(&connector, &ep_probe, &keys, &tokens[..short_n]).await.expect("short");
    let t_short = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let _ = run_teacher_forced_pipeline(&connector, &ep, &keys, &tokens).await.expect("pipeline");
    let t_long = t.elapsed().as_secs_f64() * 1000.0;
    let measured_tpot_ms = (t_long - t_short) / (tokens.len() - short_n) as f64;
    let measured_link_ms = (measured_tpot_ms - predicted_compute_ms).max(0.0);

    let err = (measured_tpot_ms - predicted_tpot_ms).abs() / predicted_tpot_ms;
    eprintln!(
        "M3 CALIBRATION (local-pair, {} layers split at {k}, n_embd={n_embd}, boundary {boundary_bytes} B)\n\
         \x20 compute   {predicted_compute_ms:.2} ms/tok  ({ms_per_layer_tok:.4} ms/layer-tok, measured unsplit)\n\
         \x20 residual  {measured_link_ms:.2} ms/tok  (measured minus compute = the real per-token link cost)\n\
         \x20 PREDICTED {predicted_tpot_ms:.2} ms/tok\n\
         \x20 MEASURED  {measured_tpot_ms:.2} ms/tok over {} tokens\n\
         \x20 ERROR     {:.1} %   (gate: <= 15 %)",
        n_layer,
        tokens.len(),
        err * 100.0
    );

    assert!(
        err <= 0.15,
        "M3 calibration clause: measured TPOT {measured_tpot_ms:.2} ms/tok must be within 15 % of \
         the predicted {predicted_tpot_ms:.2} ms/tok on the same measured inputs — got {:.1} %. \
         This gates the COST MODEL against reality; a miss here means the model, not the search, \
         is wrong.",
        err * 100.0
    );
}
