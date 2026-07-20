//! `hydra-bench` — P1·2: a per-node **capability benchmark** for M3 placement.
//!
//! The M3 placement solver (P2·3) balances contiguous layer ranges across heterogeneous nodes so each
//! pipeline stage takes roughly equal wall-time. That needs a real measurement of each node's compute
//! capability — this binary produces the **first real data** for the startup benchmark (P2·1). It runs
//! **locally on each node** (no networking): load the model, then time prefill (batched) and decode
//! (single-token autoregressive, the per-token latency that dominates TPOT) through the **full** layer
//! range. The full-model tok/s is the node's capability; the layer count makes it per-layer comparable.
//!
//! Output ends with a machine-readable `BENCH ...` line the runner collects over SSH.
//!
//! Honesty: CPU-backend numbers on the given box; not a tuned throughput target. Reported per node so
//! the asymmetry (not the absolute value) drives the placement decision.

use std::time::Instant;

use hydra_engine_sys::{Model, ENGINE_AVAILABLE};

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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let node = std::env::var("HYDRA_NODE").unwrap_or_else(|_| "unknown".to_string());
    let arch = std::env::consts::ARCH;
    let Some(path) = model_path() else {
        eprintln!("SKIP: no engine/model (dev-environment artifacts)");
        println!("BENCH node={node} arch={arch} status=skipped_no_engine");
        return Ok(());
    };

    // Warm the box a touch, then measure model load (cold-replacement recovery cares about this).
    let t_load = Instant::now();
    let model = Model::load(&path, 0)?; // CPU backend (n_gpu_layers=0) — the deterministic DoD backend
    let load_ms = t_load.elapsed().as_secs_f64() * 1000.0;
    let n_layer = model.n_layer();
    let n_embd = model.n_embd();

    // A fixed synthetic workload so every node runs the identical thing.
    let prompt_len = 32usize;
    let decode_steps = 24usize;
    let n_ctx = (prompt_len + decode_steps + 8) as i32;
    // Deterministic dummy token ids in range (content is irrelevant to timing).
    let tok = |i: usize| ((i * 2654435761) % (model.n_vocab().max(2) as usize - 1) + 1) as i32;

    // Prefill: apply `prompt_len` tokens into a fresh full-range context, timed as a batch of single
    // applies (matching the pipeline's per-position batching).
    let mut ctx = model.context(0, -1, false, n_ctx, n_ctx)?;
    let t_pre = Instant::now();
    for pos in 0..prompt_len {
        ctx.apply_tokens(&[tok(pos)], pos as i32, None)?;
    }
    let prefill_s = t_pre.elapsed().as_secs_f64();
    let prefill_tok_s = prompt_len as f64 / prefill_s;

    // Decode: single-token autoregressive applies (the TPOT-dominating path), timed.
    let t_dec = Instant::now();
    for step in 0..decode_steps {
        let pos = prompt_len + step;
        ctx.apply_tokens(&[tok(pos)], pos as i32, None)?;
        let _ = ctx.logits(0)?; // force the logits read each step (as a real decode does)
    }
    let decode_s = t_dec.elapsed().as_secs_f64();
    let decode_tok_s = decode_steps as f64 / decode_s;
    let ms_per_tok = decode_s * 1000.0 / decode_steps as f64;
    // Per-layer-token: the node's cost to push one token through one transformer layer (comparable
    // across nodes with the same model; the placement solver's unit).
    let ms_per_layer_tok = ms_per_tok / n_layer.max(1) as f64;

    println!("\n== hydra-bench: node={node} arch={arch} ==");
    println!("   model: n_layer={n_layer} n_embd={n_embd}");
    println!("   load: {load_ms:.0} ms");
    println!("   prefill: {prompt_len} tok in {prefill_s:.2}s → {prefill_tok_s:.2} tok/s");
    println!("   decode:  {decode_steps} tok in {decode_s:.2}s → {decode_tok_s:.2} tok/s ({ms_per_tok:.1} ms/tok, {ms_per_layer_tok:.3} ms/layer-tok)");
    println!(
        "BENCH node={node} arch={arch} n_layer={n_layer} load_ms={load_ms:.0} prefill_tok_s={prefill_tok_s:.2} decode_tok_s={decode_tok_s:.2} ms_per_layer_tok={ms_per_layer_tok:.4}"
    );
    Ok(())
}
