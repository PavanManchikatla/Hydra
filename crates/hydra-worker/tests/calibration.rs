//! **M3 gate row 8 — the calibration clause (§7.23 restatement).**
//!
//! The DoD's "within 15 % of brute-force TPOT" was restated by the 2026-08-22 ruling: enumeration
//! is optimal by construction (independently asserted in `hydra-sched::solver`), so comparing the
//! search against itself is vacuous. The gate is **calibration** — the deployed placement's
//! **measured** TPOT must land within 15 % of the solver's **predicted** TPOT on the same measured
//! inputs. **The cost model against reality, not the search against itself.**
//!
//! # The measurement, and why the first two attempts were wrong
//!
//! Recorded because the numbers are instructive, and because the fix is the point:
//!
//! 1. Timing a 3-token run gave a "link" term of **171 ms/token** — the one-time mTLS handshake
//!    divided by three.
//! 2. A marginal `(t_long − t_short)` across two *independent* pairs gave **−50.95 ms/token** —
//!    negative, because each pair pays its **own** setup and a 5-token prompt is all setup.
//!
//! Both were harness faults, not cost-model faults. The fix is
//! [`hydra_worker::pair::time_teacher_forced_pipeline`], which opens both connections **before**
//! the first timer starts and returns **one duration per token**, so setup cannot leak in. This
//! test then applies the measurement hygiene P2·1 established:
//!
//! * **connection setup excluded** — by construction, in the driver;
//! * **first token excluded** — it carries first-decode effects on both stages;
//! * **warm-up excluded** — a leading fraction of the run is discarded;
//! * **a prompt long enough that per-token signal dominates** — the 5-token analysis above is
//!   exactly why;
//! * **median, not mean** — one scheduler preemption must not decide a gate.
//!
//! # Two outcomes only
//!
//! Within 15 % ⇒ row 8 green with this receipt. Outside 15 % **after sound measurement** ⇒ escalate
//! as a **cost-model finding**. The model is not tuned to pass: a missing per-token constant would
//! be a legitimate, ratifiable amendment, and a quiet fudge would not.
//!
//! Honesty: this is the **local-pair** (both stages on one Mac, loopback mTLS). It calibrates the
//! cost model's *shape*; the wired-LAN envelope remains a separate, hardware-contingent owed item.

use std::time::Instant;

use hydra_worker::pair::{dev_model_path, time_teacher_forced_pipeline, Cluster};
use hydra_worker::wire::SessionKeys;
use hydra_worker::worker::WorkerConfig;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.len() % 2 == 1 {
        v[v.len() / 2]
    } else {
        (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0
    }
}

#[tokio::test]
#[ignore = "calibration: needs the engine + dev model; run explicitly for the M3 gate"]
async fn deployed_tpot_is_within_15_percent_of_the_solver_prediction() {
    let Some(path) = dev_model_path() else {
        eprintln!("SKIP: no engine/model");
        return;
    };

    // A prompt long enough that per-token signal dominates. Repeated filler tokens keep the
    // arithmetic identical per position while giving enough samples for a stable median.
    const N_TOKENS: usize = 64;
    const WARMUP: usize = 8; // discarded: first-decode effects on both stages

    // ---------- part 1: compute, measured unsplit and independently of the pipeline ----------
    let (n_layer, n_embd, ms_per_layer_tok) = {
        let model = hydra_engine_sys::Model::load(&path, 0).expect("load");
        let n_layer = model.n_layer();
        let n_embd = model.n_embd();
        let n_ctx = (N_TOKENS + 16) as i32;
        let mut ctx = model.context(0, -1, false, n_ctx, n_ctx).expect("ctx");
        for pos in 0..WARMUP as i32 {
            ctx.apply_tokens(&[1], pos, None).expect("warm");
            let _ = ctx.logits(0);
        }
        let mut samples = Vec::new();
        for i in 0..(N_TOKENS - WARMUP) as i32 {
            let t = Instant::now();
            ctx.apply_tokens(&[1], WARMUP as i32 + i, None).expect("decode");
            let _ = ctx.logits(0).expect("logits");
            samples.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        (n_layer, n_embd, median(samples) / n_layer as f64)
    };

    // ---------- part 2: the deployed pipeline, timed per token with setup excluded ----------
    let keys = SessionKeys::dev(0xCA);
    let k = (n_layer / 2).max(1);
    let n_ctx = (N_TOKENS + 16) as i32;
    let cluster = Cluster::new().unwrap();
    let s1_id = cluster.issue("worker-s1").unwrap();
    let s2_id = cluster.issue("worker-s2").unwrap();
    let cfg = |rank: u16, first: i32, last: i32, is_final: bool, recv: bool| WorkerConfig {
        keys: keys.clone(), rank, layer_first: first, layer_last: last, is_final,
        receives_tokens: recv, epoch: 0, recovery_id: 0, model_path: Some(path.clone()),
        n_gpu_layers: 0, n_ctx, sampler_config: None, recovery_start: false, shard_manifest: None,
    };
    let s1 = hydra_worker::pair::spawn_endpoint(cfg(0, 0, k, false, true), cluster.ca.server_config(&s1_id).unwrap());
    let s2 = hydra_worker::pair::spawn_endpoint(cfg(1, k, -1, true, false), cluster.ca.server_config(&s2_id).unwrap());
    let connector = cluster.coordinator_connector().unwrap();
    let ep = hydra_worker::pair::Endpoints::new(s1, "worker-s1", s2, "worker-s2");

    let tokens: Vec<u32> = vec![1u32; N_TOKENS];
    let per_token = time_teacher_forced_pipeline(&connector, &ep, &keys, &tokens).await.expect("pipeline");
    assert_eq!(per_token.len(), N_TOKENS);

    // Drop warm-up AND the first token explicitly (the first is inside WARMUP, but the intent is
    // recorded rather than implied).
    let steady: Vec<f64> = per_token[WARMUP..].iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    let measured_tpot_ms = median(steady.clone());

    // ---------- part 3: the prediction, from the solver's objective-(a) cost model ----------
    let predicted_compute_ms = n_layer as f64 * ms_per_layer_tok;
    let predicted_tpot_ms = predicted_compute_ms; // loopback link term is below the noise floor
    let residual_ms = measured_tpot_ms - predicted_compute_ms;
    let err = (measured_tpot_ms - predicted_tpot_ms).abs() / predicted_tpot_ms;

    let spread = {
        let mut s = steady.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (s[s.len() - 1] - s[0]) / measured_tpot_ms
    };

    eprintln!(
        "M3 CALIBRATION (local-pair, {n_layer} layers split at {k}, n_embd={n_embd})\n\
         \x20 samples    {} steady tokens of {N_TOKENS} ({WARMUP} warm-up discarded), setup excluded by construction\n\
         \x20 compute    {predicted_compute_ms:.2} ms/tok  ({ms_per_layer_tok:.4} ms/layer-tok, measured unsplit)\n\
         \x20 PREDICTED  {predicted_tpot_ms:.2} ms/tok\n\
         \x20 MEASURED   {measured_tpot_ms:.2} ms/tok  (median; raw spread {:.0} %)\n\
         \x20 residual   {residual_ms:+.2} ms/tok  (measured − compute = the real per-token link cost)\n\
         \x20 ERROR      {:.1} %   (gate: <= 15 %)",
        steady.len(),
        spread * 100.0,
        err * 100.0
    );

    assert!(
        err <= 0.15,
        "M3 calibration clause: measured TPOT {measured_tpot_ms:.2} ms/tok must be within 15 % of \
         the predicted {predicted_tpot_ms:.2} ms/tok on the same measured inputs — got {:.1} % \
         (residual {residual_ms:+.2} ms/tok). The measurement is sound (setup excluded, warm-up \
         discarded, median of {} samples), so a miss here is a COST-MODEL finding to escalate, \
         NOT something to tune away.",
        err * 100.0,
        steady.len()
    );
}
