//! P2·10b — **shard-loaded weights must be semantically invisible.**
//!
//! The M-1 spike and [`bit_exact.rs`] prove the *compute* window: one fully-loaded model, two
//! contexts over layer ranges. This file proves the *load* window: two separate per-stage **shard
//! GGUFs** (produced by `hydra-modelsvc split`), each loading ONLY its own layers' weights, must
//! reproduce the unsplit model's logits **bit-exactly**.
//!
//! That is the whole claim of P2·10b. The memory saving is the payoff metric; bit-exactness is the
//! gate. If shard-loading changed a single float, the caveat could not be removed at any price.
//!
//! Skips cleanly (dev-environment artifacts) when the engine isn't linked, the model is absent, or
//! the shards have not been produced. To produce them:
//! ```text
//! cargo run --release -p hydra-modelsvc --bin hydra-modelsvc -- \
//!     split models/qwen2.5-0.5b-instruct-fp16.gguf models/shards2 --stages 0-12,12-24
//! ```

use hydra_engine_sys::{Model, ENGINE_AVAILABLE};

fn model_path() -> Option<String> {
    if let Ok(p) = std::env::var("HYDRA_TEST_MODEL") {
        return std::path::Path::new(&p).exists().then_some(p);
    }
    let default = concat!(env!("CARGO_MANIFEST_DIR"), "/../../models/qwen2.5-0.5b-instruct-fp16.gguf");
    std::path::Path::new(default).exists().then(|| default.to_string())
}

/// `models/shards2/qwen2-stage{0,1}-L{a}_{b}.gguf` — the 2-stage split matching the rule-14 anchor.
fn shard_paths() -> Option<(String, String)> {
    let dir = std::env::var("HYDRA_TEST_SHARDS")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../models/shards2").to_string());
    let a = format!("{dir}/qwen2-stage0-L0_12.gguf");
    let b = format!("{dir}/qwen2-stage1-L12_24.gguf");
    (std::path::Path::new(&a).exists() && std::path::Path::new(&b).exists()).then_some((a, b))
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
fn shard_loaded_weights_are_bit_exact_with_the_unsplit_model() {
    if !ENGINE_AVAILABLE {
        eprintln!("SKIP: engine unavailable (vendored llama.cpp build tree not built)");
        return;
    }
    let (Some(full), Some((s0, s1))) = (model_path(), shard_paths()) else {
        eprintln!("SKIP: no model/shards (dev artifacts — see the module doc for the split command)");
        return;
    };

    // ---- reference: the unsplit full model. Loaded and DROPPED before the shards load, so peak
    // RSS on the 8 GB dev box stays bounded (the same discipline the rule-14 anchor uses).
    let (toks, n_layer, n_embd, ref_logits) = {
        let model = Model::load(&full, 0).expect("load full model");
        let toks = model.tokenize("The capital of France is").expect("tokenize");
        let n = toks.len() as i32;
        let mut ctx = model.context(0, -1, false, n + 8, n).expect("full ctx");
        ctx.apply_tokens(&toks, 0, None).expect("apply full");
        let lg = ctx.logits(n - 1).expect("ref logits");
        let (nl, ne) = (model.n_layer(), model.n_embd() as usize);
        drop(ctx);
        (toks, nl, ne, lg)
    };
    let n = toks.len() as i32;
    let n_ctx = n + 8;
    let k = n_layer / 2;
    assert_eq!(k, 12, "this fixture is the 24-layer dev model split at 12");

    // ---- stage 0: layers [0, k) from its OWN shard file, extracting the boundary residual ----
    let mut boundary = vec![0f32; toks.len() * n_embd];
    {
        let m0 = Model::load_shard(&s0, 0, k, 0).expect("load stage-0 shard");
        // The shard carries the architecture's block_count verbatim, so the model still describes
        // the FULL network — layer indices keep their global meaning and nothing re-bases them.
        assert_eq!(m0.n_layer(), n_layer, "a shard still reports the full model's layer count");
        assert_eq!(m0.load_window(), Some((0, k)), "the load window is what we asked for");
        let mut a = m0.context(0, k, true, n_ctx, n).expect("stage-0 ctx");
        a.apply_tokens(&toks, 0, Some(&mut boundary)).expect("apply stage 0");
    }

    // ---- stage 1: layers [k, end) from its OWN shard file, consuming the boundary ----
    let shard_logits = {
        let m1 = Model::load_shard(&s1, k, n_layer, 0).expect("load stage-1 shard");
        assert_eq!(m1.load_window(), Some((k, n_layer)));
        let mut b = m1.context(k, -1, false, n_ctx, n).expect("stage-1 ctx");
        b.apply_boundary(&boundary, 0, None).expect("apply stage 1");
        b.logits(n - 1).expect("stage-1 logits")
    };

    assert_eq!(ref_logits.len(), shard_logits.len());
    let d = max_abs(&ref_logits, &shard_logits);
    eprintln!(
        "shard-loaded vs unsplit: max_abs={d:.3e}, argmax {}=={} (n={n}, k={k}/{n_layer}, n_embd={n_embd})",
        argmax(&ref_logits),
        argmax(&shard_logits)
    );
    assert_eq!(argmax(&ref_logits), argmax(&shard_logits), "argmax must match");
    assert_eq!(
        d, 0.0,
        "shard-loaded weights must be BIT-EXACT with the unsplit model — shard-load is a memory \
         optimization and must be semantically invisible (P2·10b binding point 3)"
    );
}

#[test]
fn a_context_escaping_the_loaded_shard_window_is_refused() {
    if !ENGINE_AVAILABLE {
        eprintln!("SKIP: engine unavailable");
        return;
    }
    let Some((s0, _s1)) = shard_paths() else {
        eprintln!("SKIP: no shards (dev artifact)");
        return;
    };
    let m0 = Model::load_shard(&s0, 0, 12, 0).expect("load stage-0 shard");

    // Asking a shard to compute layers it never loaded must FAIL AT THE BOUNDARY. Those tensors
    // were never created, so the alternative is a null-deref deep inside the graph — refuse loudly
    // instead (the same refuse-don't-warn posture the manifest verification takes).
    assert!(m0.context(0, -1, false, 32, 8).is_err(), "[0, n_layer) escapes a [0,12) shard");
    assert!(m0.context(0, 13, false, 32, 8).is_err(), "[0,13) escapes a [0,12) shard by one layer");
    assert!(m0.context(12, 24, false, 32, 8).is_err(), "a wholly-outside window is refused");

    // The window it DID load is fine.
    assert!(m0.context(0, 12, true, 32, 8).is_ok(), "the loaded window itself must work");
}
