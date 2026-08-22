//! `hydra-bench` — the per-node **capability benchmark** for M3 placement.
//!
//! **P1·2 (banked)** produced the first real 3-node data with a short fixed workload.
//! **P2·1 (this)** makes it the *startup benchmark*: a **sustained 30–120 s** measurement whose
//! windows feed `hydra-sched`'s aggregator and EWMA, so the number the scheduler places layers
//! from is a steady-state estimate rather than a two-second sample.
//!
//! Why sustained matters, from P1·2's own data: the VMs are **burstable** Azure instances. A short
//! benchmark can land entirely inside a burst credit and report a capability the box cannot hold —
//! and an over-stated device is handed too many layers, which stalls the whole pipeline (it runs at
//! the speed of its slowest stage). The sustained window is what makes throttling visible, and the
//! `spread`/`stable` fields are what report it instead of hiding it in an average.
//!
//! Modes:
//!   * default — sustained: repeated fixed-depth decode windows until the 30 s minimum is met
//!     (capped at 120 s), aggregated by `hydra_sched::capability`.
//!   * `HYDRA_BENCH_QUICK=1` — the original P1·2 short workload, **kept verbatim** so the banked
//!     `docs/heterogeneity.md` methodology stays reproducible.
//!
//! Runs **locally on each node** (no networking). Output ends with machine-readable `BENCH ...`
//! lines a runner collects over SSH.
//!
//! Honesty: CPU-backend numbers on the given box; not a tuned throughput target. The **ratio**
//! across nodes, not the absolute value, is what drives placement.

use std::time::Instant;

use hydra_engine_sys::{Model, ENGINE_AVAILABLE};
use hydra_sched::capability::{BenchConfig, CapabilityRegistry, Sample, SustainedBench};

fn model_path() -> Option<String> {
    if !ENGINE_AVAILABLE {
        return None;
    }
    let default = concat!(env!("CARGO_MANIFEST_DIR"), "/../../models/qwen2.5-0.5b-instruct-fp16.gguf");
    std::env::var("HYDRA_TEST_MODEL")
        .ok()
        .filter(|p| std::path::Path::new(p).exists())
        .or_else(|| std::path::Path::new(default).exists().then(|| default.to_string()))
}

fn env_f64(key: &str) -> Option<f64> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let node = std::env::var("HYDRA_NODE").unwrap_or_else(|_| "unknown".to_string());
    let arch = std::env::consts::ARCH;
    let Some(path) = model_path() else {
        eprintln!("SKIP: no engine/model (dev-environment artifacts)");
        println!("BENCH node={node} arch={arch} status=skipped_no_engine");
        return Ok(());
    };

    // Model load is measured separately — cold-replacement recovery cares about it, and it must
    // never be folded into the per-token capability number.
    let t_load = Instant::now();
    let model = Model::load(&path, 0)?; // CPU backend (n_gpu_layers=0) — the deterministic DoD backend
    let load_ms = t_load.elapsed().as_secs_f64() * 1000.0;
    let n_layer = model.n_layer();
    let n_embd = model.n_embd();

    let prompt_len = 32usize;
    let decode_steps = 24usize; // the fixed P1·2 quick-mode workload (unchanged, for reproducibility)
    // The sustained mode uses DURATION-targeted windows instead of a fixed step count. A fixed
    // count is not comparable across a heterogeneity set: 24 steps is ~0.4 s on the Mac but ~1.6 s
    // on the slowest node, so the fast box is measured in windows short enough for OS scheduler
    // jitter to dominate while the slow box is not. Targeting ~1 s of work per window makes every
    // device's samples mean the same thing.
    let window_target_s = env_f64("HYDRA_BENCH_WINDOW_S").unwrap_or(1.0);
    let max_window_steps = 512usize;
    let n_ctx = (prompt_len + max_window_steps + 8) as i32;
    // Deterministic dummy token ids in range (content is irrelevant to timing).
    let tok = |i: usize| ((i * 2654435761) % (model.n_vocab().max(2) as usize - 1) + 1) as i32;

    // Prefill once: every decode window then runs at the SAME context depth, so windows are
    // directly comparable and no growing-context drift is mistaken for throttling.
    let mut ctx = model.context(0, -1, false, n_ctx, n_ctx)?;
    let t_pre = Instant::now();
    for pos in 0..prompt_len {
        ctx.apply_tokens(&[tok(pos)], pos as i32, None)?;
    }
    let prefill_s = t_pre.elapsed().as_secs_f64();
    let prefill_tok_s = prompt_len as f64 / prefill_s;

    // One decode window: `decode_steps` single-token applies (the TPOT-dominating path), then the
    // KV is truncated back to the prompt so the next window starts from the same depth.
    let decode_window = |ctx: &mut hydra_engine_sys::Context<'_>| -> Result<f64, Box<dyn std::error::Error>> {
        let t = Instant::now();
        for step in 0..decode_steps {
            let pos = prompt_len + step;
            ctx.apply_tokens(&[tok(pos)], pos as i32, None)?;
            let _ = ctx.logits(0)?; // force the logits read each step, as a real decode does
        }
        let secs = t.elapsed().as_secs_f64();
        ctx.kv_truncate(prompt_len as i32)?;
        Ok(secs)
    };

    println!("\n== hydra-bench: node={node} arch={arch} ==");
    println!("   model: n_layer={n_layer} n_embd={n_embd}");
    println!("   load: {load_ms:.0} ms");
    println!("   prefill: {prompt_len} tok in {prefill_s:.2}s → {prefill_tok_s:.2} tok/s");

    if std::env::var("HYDRA_BENCH_QUICK").is_ok() {
        // ---- P1·2 methodology, preserved verbatim so docs/heterogeneity.md stays reproducible ----
        let decode_s = decode_window(&mut ctx)?;
        let decode_tok_s = decode_steps as f64 / decode_s;
        let ms_per_tok = decode_s * 1000.0 / decode_steps as f64;
        let ms_per_layer_tok = ms_per_tok / n_layer.max(1) as f64;
        println!("   decode:  {decode_steps} tok in {decode_s:.2}s → {decode_tok_s:.2} tok/s ({ms_per_tok:.1} ms/tok, {ms_per_layer_tok:.3} ms/layer-tok)");
        println!("BENCH node={node} arch={arch} n_layer={n_layer} load_ms={load_ms:.0} prefill_tok_s={prefill_tok_s:.2} decode_tok_s={decode_tok_s:.2} ms_per_layer_tok={ms_per_layer_tok:.4} mode=quick");
        return Ok(());
    }

    // ---------------------------- P2·1: the sustained startup benchmark ----------------------------
    let cfg = BenchConfig {
        min_duration_s: env_f64("HYDRA_BENCH_MIN_S").unwrap_or(hydra_sched::capability::DEFAULT_MIN_DURATION_S),
        max_duration_s: env_f64("HYDRA_BENCH_MAX_S").unwrap_or(hydra_sched::capability::DEFAULT_MAX_DURATION_S),
        ..Default::default()
    };
    let mut bench = SustainedBench::new(cfg);
    println!("   sustained: measuring {:.0}–{:.0}s of steady-state decode …", cfg.min_duration_s, cfg.max_duration_s);
    // Warm-up windows are pushed like any other and discarded by the aggregator — the discard
    // policy lives in one place (hydra-sched), not duplicated in every measurement tool.
    while !bench.should_stop() {
        // Duration-targeted window: decode until ~`window_target_s` of work has happened, counting
        // steps. Self-adapting, so a fast and a slow node contribute equally-meaningful samples.
        let t = Instant::now();
        let mut steps = 0usize;
        while t.elapsed().as_secs_f64() < window_target_s && steps < max_window_steps {
            let pos = prompt_len + steps;
            ctx.apply_tokens(&[tok(pos)], pos as i32, None)?;
            let _ = ctx.logits(0)?;
            steps += 1;
        }
        let secs = t.elapsed().as_secs_f64();
        ctx.kv_truncate(prompt_len as i32)?;
        let ms_per_layer_tok = secs * 1000.0 / steps.max(1) as f64 / n_layer.max(1) as f64;
        bench.push(Sample::new(ms_per_layer_tok, secs)?);
        // `finish()` succeeds exactly when the post-warm-up window has met the minimum, so the
        // stopping rule reads from the aggregator rather than re-deriving the policy here.
        if bench.finish().is_ok() {
            break;
        }
    }

    let m = bench.finish()?;
    let decode_tok_s = 1000.0 / (m.ms_per_layer_tok * n_layer.max(1) as f64);
    let ms_per_tok = m.ms_per_layer_tok * n_layer.max(1) as f64;

    // Seed the registry EWMA with this measurement — the same type the coordinator will hold, so
    // the tool and the scheduler agree on what a capability estimate IS.
    let mut reg = CapabilityRegistry::new();
    reg.observe(&node, arch, m);
    let ewma = reg.get(&node).and_then(|d| d.ms_per_layer_tok()).unwrap_or(m.ms_per_layer_tok);

    println!(
        "   decode (sustained): {:.2} tok/s ({ms_per_tok:.1} ms/tok, {:.3} ms/layer-tok) over {:.1}s, {} windows (+{} warm-up discarded)",
        decode_tok_s, m.ms_per_layer_tok, m.duration_s, m.samples_used, m.warmup_discarded
    );
    println!("   spread: {:.1}%  → {}", m.spread * 100.0, if m.is_stable() { "STABLE" } else { "UNSTABLE (re-measure; do not place on this)" });
    println!(
        "BENCH node={node} arch={arch} n_layer={n_layer} load_ms={load_ms:.0} prefill_tok_s={prefill_tok_s:.2} decode_tok_s={decode_tok_s:.2} ms_per_layer_tok={:.4} mode=sustained",
        m.ms_per_layer_tok
    );
    println!(
        "BENCH_SUSTAINED node={node} ms_per_layer_tok={:.4} ewma={ewma:.4} windows={} warmup={} duration_s={:.1} spread={:.4} stable={}",
        m.ms_per_layer_tok, m.samples_used, m.warmup_discarded, m.duration_s, m.spread, m.is_stable()
    );
    Ok(())
}
