//! Diagnostic for the M3 calibration residual (gate row 8).
//!
//! The sound calibration measurement showed the deployed two-stage pipeline costing **32.97 ms/tok**
//! against a predicted **17.61 ms/tok** — a **+15.37 ms/tok** residual the cost model does not
//! price. The solver's objective (a) prices compute as `layers × ms_per_layer_tok`, i.e. **purely
//! proportional to layer count**. This test asks whether that is true.
//!
//! If a decode carries a **fixed per-context cost** — graph dispatch, scheduler setup, output
//! marshalling — independent of how many layers the context runs, then a two-stage pipeline pays it
//! **twice** while the unsplit reference pays it **once**, and the model is missing a per-stage
//! constant. That would be a real cost-model gap, not a measurement artefact.

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

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// Time steady-state single-token decode through a context windowed to `[l0, l1)`.
fn time_window(model: &Model, l0: i32, l1: i32, embeddings: bool, n_embd: usize) -> f64 {
    let n_ctx = 96;
    let mut ctx = model.context(l0, l1, embeddings, n_ctx, n_ctx).expect("ctx");
    let mut out = vec![0f32; n_embd];
    let apply = |ctx: &mut hydra_engine_sys::Context<'_>, pos: i32, out: &mut Vec<f32>| {
        if embeddings {
            ctx.apply_tokens(&[1], pos, Some(out)).expect("apply");
        } else {
            ctx.apply_tokens(&[1], pos, None).expect("apply");
            let _ = ctx.logits(0).expect("logits");
        }
    };
    for pos in 0..8 {
        apply(&mut ctx, pos, &mut out);
    }
    let mut s = Vec::new();
    for i in 0..40 {
        let t = std::time::Instant::now();
        apply(&mut ctx, 8 + i, &mut out);
        s.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    median(s)
}

#[test]
#[ignore = "diagnostic for M3 gate row 8; needs the engine + dev model"]
fn decode_cost_is_not_purely_proportional_to_layer_count() {
    let Some(path) = model_path() else {
        eprintln!("SKIP: no engine/model");
        return;
    };
    let model = Model::load(&path, 0).expect("load");
    let n_layer = model.n_layer();
    let n_embd = model.n_embd() as usize;
    let k = n_layer / 2;

    // The unsplit reference, and the two halves the pipeline actually runs.
    let full = time_window(&model, 0, -1, false, n_embd);
    let lower = time_window(&model, 0, k, true, n_embd); // boundary-emitting first stage
    let upper = time_window(&model, k, -1, false, n_embd); // logits-producing final stage

    // If cost were purely proportional, lower + upper would equal full. The excess is the fixed
    // per-context cost paid one extra time by splitting.
    let excess = lower + upper - full;
    let per_stage_fixed = excess; // one extra context's worth

    eprintln!(
        "FIXED-COST DIAGNOSTIC ({n_layer} layers, split at {k})\n\
         \x20 full  [0,{n_layer})  {full:.2} ms/tok\n\
         \x20 lower [0,{k})       {lower:.2} ms/tok   ({:.1} % of full for {:.0} % of the layers)\n\
         \x20 upper [{k},{n_layer})      {upper:.2} ms/tok   ({:.1} % of full for {:.0} % of the layers)\n\
         \x20 lower+upper          {:.2} ms/tok\n\
         \x20 EXCESS over full     {excess:+.2} ms/tok   <- the fixed per-context cost, paid twice by a 2-stage split",
        lower / full * 100.0,
        k as f64 / n_layer as f64 * 100.0,
        upper / full * 100.0,
        (n_layer - k) as f64 / n_layer as f64 * 100.0,
        lower + upper,
    );

    assert!(
        per_stage_fixed > 0.0,
        "if this is not positive, splitting is free and the calibration residual is NOT a fixed \
         per-stage cost — look elsewhere (got {per_stage_fixed:+.2} ms/tok)"
    );
}
