//! Golden smoke test: reproduce the M-1 spike's split-vs-unsplit **bit-exact (F32)** result
//! *through the crate API*. If loading a shard-A range → boundary → shard-B range reproduces the
//! unsplit model's logits to 0.0 max-abs, the FFI wrapping changed no semantics the spike proved.
//!
//! Skips cleanly (passes) when the engine isn't linked or the model file is absent — both are
//! dev-environment artifacts (a small git-ignored GGUF; the vendored build tree). Set
//! `HYDRA_TEST_MODEL` to override the default model path.

use hydra_engine_sys::{Model, ENGINE_AVAILABLE};

fn model_path() -> Option<String> {
    if let Ok(p) = std::env::var("HYDRA_TEST_MODEL") {
        return std::path::Path::new(&p).exists().then_some(p);
    }
    let default = concat!(env!("CARGO_MANIFEST_DIR"), "/../../models/qwen2.5-0.5b-instruct-fp16.gguf");
    std::path::Path::new(default).exists().then(|| default.to_string())
}

fn max_abs(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max)
}

fn argmax(v: &[f32]) -> usize {
    let mut bi = 0;
    for i in 1..v.len() {
        if v[i] > v[bi] {
            bi = i;
        }
    }
    bi
}

#[test]
fn split_vs_unsplit_f32_bit_exact_through_crate_api() {
    if !ENGINE_AVAILABLE {
        eprintln!("SKIP: engine unavailable (vendored llama.cpp build tree not built)");
        return;
    }
    let Some(path) = model_path() else {
        eprintln!("SKIP: no model (set HYDRA_TEST_MODEL or place models/qwen2.5-0.5b-instruct-fp16.gguf)");
        return;
    };

    // CPU backend (deterministic, the DoD reference); small model per --local-pair memory discipline.
    let model = Model::load(&path, 0).expect("load model");
    let n_layer = model.n_layer();
    let n_embd = model.n_embd() as usize;
    assert!(n_layer >= 2 && n_embd > 0);

    let toks = model.tokenize("The capital of France is").expect("tokenize");
    let n = toks.len() as i32;
    assert!(n >= 1);
    let n_ctx = n + 8;
    let k = n_layer / 2; // mid-network split

    // ---- reference: unsplit full model -> logits at the last position ----
    let ref_logits = {
        let mut ctx = model.context(0, -1, false, n_ctx, n).expect("full ctx");
        ctx.apply_tokens(&toks, 0, None).expect("apply full");
        ctx.logits(n - 1).expect("ref logits")
    };

    // ---- shard A: layers [0, k), extract the boundary residual for every position ----
    let mut boundary = vec![0f32; toks.len() * n_embd];
    {
        let mut a = model.context(0, k, true, n_ctx, n).expect("shard A ctx");
        a.apply_tokens(&toks, 0, Some(&mut boundary)).expect("apply A");
    }

    // ---- shard B: layers [k, end), consume the boundary -> logits at the last position ----
    let split_logits = {
        let mut b = model.context(k, -1, false, n_ctx, n).expect("shard B ctx");
        b.apply_boundary(&boundary, 0, None).expect("apply B");
        b.logits(n - 1).expect("split logits")
    };

    assert_eq!(ref_logits.len(), split_logits.len());
    let d = max_abs(&ref_logits, &split_logits);
    eprintln!(
        "split-vs-unsplit through crate API: max_abs={d:.3e}, argmax {}=={} (n={n}, k={k}, n_embd={n_embd})",
        argmax(&ref_logits),
        argmax(&split_logits)
    );
    assert_eq!(
        argmax(&ref_logits),
        argmax(&split_logits),
        "argmax must match (F32 split is exact)"
    );
    assert_eq!(d, 0.0, "F32 split-vs-unsplit must be bit-exact through the crate API (spike Check C)");
}
