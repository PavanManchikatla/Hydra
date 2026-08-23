//! **Audit Wave 1c (C3 external half · H7 · M5 · the `i32::try_from` rule) — the pre-FFI gate.**
//!
//! `wire::decode` proves a boundary frame is *self*-consistent (`wire_limits.rs`). These tests prove
//! the half the codec cannot: that a self-consistent frame is also consistent with **this engine**
//! — its `n_embd`, its `n_batch`, its `n_ctx`, its `n_vocab` — and that every refusal happens in
//! the worker, **before** `hydra_apply` is called. The distinguishing assertion is the error kind:
//! [`WorkerError::PreFfi`] is only constructible by the worker's own checks, so it cannot be the
//! engine reporting a failure after the fact.
//!
//! Engine-gated (needs the dev GGUF): the bounds under test are the engine's own numbers, and a
//! stub engine would be asserting against values the test itself invented.
//!
//! # The blind oracle, named (standing instruction)
//!
//! Before 1c nothing in the tree could express "the declared shape and the engine disagree": the
//! shape was discarded at decode, the engine derived the position count from the byte length, and
//! `as i32` silently folded every out-of-range position and token id into some in-range one. The
//! rule-14 bit-exact anchor — the standing regression gate — was green throughout, because it
//! only ever sends frames that *are* consistent. An anchor proves the happy path is exact; it is
//! structurally blind to a peer that lies, so its green said nothing about this seam. That is the
//! same shape as `wire_limits.rs`'s old enshrining control: a harness that cannot produce the
//! failure it nominally guards.

use hydra_worker::pair::dev_model_path;
use hydra_worker::wire::{self, SessionFence};
use hydra_worker::worker::{Worker, WorkerConfig, WorkerError};

fn worker(path: String, is_final: bool, n_ctx: i32) -> Worker {
    let fence = SessionFence::dev(0xA1);
    Worker::new(WorkerConfig {
        fence,
        rank: if is_final { 1 } else { 0 },
        layer_first: if is_final { 12 } else { 0 },
        layer_last: if is_final { -1 } else { 12 },
        is_final,
        receives_tokens: !is_final,
        epoch: 0,
        recovery_id: 0,
        model_path: Some(path),
        n_gpu_layers: 0,
        n_ctx,
        sampler_config: None,
        recovery_start: false,
        shard_manifest: None,
    })
    .expect("worker")
}

fn pre_ffi(r: Result<Vec<Vec<u8>>, WorkerError>) -> (&'static str, i64, i64) {
    match r {
        Err(WorkerError::PreFfi { what, value, bound }) => (what, value, bound),
        other => panic!("expected a PreFfi refusal, got {other:?}"),
    }
}

/// **C3, external half.** A frame that is perfectly self-consistent at `n_embd = 1024` — dims,
/// bytes and `n_positions` all agree — describes a model this stage does not hold (the dev model
/// is 896 wide). It must be refused by the worker, naming both widths, before the FFI.
#[test]
fn a_self_consistent_boundary_of_the_wrong_width_is_refused_before_the_ffi() {
    let Some(path) = dev_model_path() else {
        eprintln!("skip: engine/model unavailable");
        return;
    };
    let n_ctx = 32;
    let mut w = worker(path, true, n_ctx);
    let fence = SessionFence::dev(0xA1);

    let (what, value, bound) = pre_ffi(w.on_frame(&wire::encode_fwd(&fence, 0, 0, true, &vec![0.1f32; 1024])));
    assert_eq!(what, "FWD n_embd vs engine n_embd");
    assert_eq!((value, bound), (1024, 896), "the refusal names the declared width and the engine's");

    // One float short is just as wrong: the bound is equality, not "fits".
    let (what, ..) = pre_ffi(w.on_frame(&wire::encode_fwd(&fence, 0, 0, true, &vec![0.1f32; 895])));
    assert_eq!(what, "FWD n_embd vs engine n_embd");

    // Control: the right width is applied (the engine is reached and returns normally).
    let replies = w.on_frame(&wire::encode_fwd(&fence, 0, 0, true, &vec![0.1f32; 896])).expect("a correct boundary applies");
    assert_eq!(replies.len(), 1, "the final stage acks the position");
}

/// **The `i32::try_from` rule + `[0, n_ctx)`.** Every network-derived position used to be `as i32`:
/// `2^32 + 3` became position 3 and `-1` became a position the engine would index with. Now a
/// position that does not fit, or is outside the context, is refused by the worker — on both the
/// token-ingest and the boundary-ingest paths.
#[test]
fn a_position_outside_the_context_or_i32_is_refused_before_the_ffi() {
    let Some(path) = dev_model_path() else {
        eprintln!("skip: engine/model unavailable");
        return;
    };
    let n_ctx = 32;
    let fence = SessionFence::dev(0xA1);
    let boundary = vec![0.1f32; 896];

    let mut final_stage = worker(path.clone(), true, n_ctx);
    for bad in [n_ctx as i64, -1, i32::MAX as i64 + 1, (1i64 << 32) + 3, i64::MIN, i64::MAX] {
        let (what, value, bound) = pre_ffi(final_stage.on_frame(&wire::encode_fwd(&fence, 0, bad, true, &boundary)));
        assert_eq!(what, "FWD first_input_pos");
        assert_eq!((value, bound), (bad, n_ctx as i64), "the refusal carries the offending value verbatim");
    }
    // Control: the last legal position is applied.
    final_stage.on_frame(&wire::encode_fwd(&fence, 0, (n_ctx - 1) as i64, true, &boundary)).expect("pos n_ctx-1 is legal");

    let mut ingress = worker(path, false, n_ctx);
    for bad in [n_ctx as i64, -1, (1i64 << 32) + 3] {
        let (what, value, ..) = pre_ffi(ingress.on_frame(&wire::encode_apply_token(&fence, 0, bad, 1, true)));
        assert_eq!(what, "APPLY_TOKEN input_pos");
        assert_eq!(value, bad);
    }
    ingress.on_frame(&wire::encode_apply_token(&fence, 0, 0, 1, true)).expect("pos 0 is legal");
}

/// **M5 (worker side).** `token_id < n_vocab`, checked before the FFI. The dev model's vocabulary
/// is 151 936; the id one past it, and `u32::MAX`, are refused naming the bound; the last legal id
/// is applied.
#[test]
fn an_out_of_vocabulary_token_is_refused_before_the_ffi() {
    let Some(path) = dev_model_path() else {
        eprintln!("skip: engine/model unavailable");
        return;
    };
    let fence = SessionFence::dev(0xA1);
    let mut ingress = worker(path, false, 32);

    let (what, value, bound) = pre_ffi(ingress.on_frame(&wire::encode_apply_token(&fence, 0, 0, 151_936, true)));
    assert_eq!(what, "APPLY_TOKEN token_id vs n_vocab");
    assert_eq!((value, bound), (151_936, 151_936));
    let (.., bound) = pre_ffi(ingress.on_frame(&wire::encode_apply_token(&fence, 0, 0, u32::MAX, true)));
    assert_eq!(bound, 151_936);
    ingress.on_frame(&wire::encode_apply_token(&fence, 0, 0, 151_935, true)).expect("the last legal id applies");
}

/// **H7 — `n > n_batch` is refused before the FFI, at the engine-sys layer.** The wire cannot carry
/// more than one position in v1 (M4), so the worker's own `n_positions ≤ n_batch` check is
/// unreachable from the network — but `hydra-engine-sys` is a public API with multi-position
/// callers (the engine-sys anchors apply whole prompts), so the bound lives there too and is
/// exercised there: a context with `n_batch = 4` refuses a 5-position apply **without** calling
/// into the shim (the error is the wrapper's own `code 8`, before `hydra_apply`), and accepts 4.
#[test]
fn a_position_count_above_n_batch_is_refused_by_the_wrapper_before_the_shim() {
    let Some(path) = dev_model_path() else {
        eprintln!("skip: engine/model unavailable");
        return;
    };
    let model = hydra_engine_sys::Model::load(&path, 0).expect("load");
    let mut ctx = model.context(0, 12, true, 16, 4).expect("context n_batch=4");
    let n_embd = model.n_embd() as usize;

    let five = vec![0.1f32; 5 * n_embd];
    let err = ctx.apply_boundary(&five, 5, 0, None).unwrap_err();
    assert_eq!(err.what, "n_positions exceeds n_batch (audit H7)");
    let err = ctx.apply_tokens(&[1, 1, 1, 1, 1], 0, None).unwrap_err();
    assert_eq!(err.what, "n_positions exceeds n_batch (audit H7)");

    // C3 at the wrapper: an explicit `n_positions` that the buffer does not match is refused even
    // when the buffer is a multiple of `n_embd` — the old `len / n_embd` derivation would have
    // silently "corrected" it.
    let two = vec![0.1f32; 2 * n_embd];
    let err = ctx.apply_boundary(&two, 1, 0, None).unwrap_err();
    assert_eq!(err.what, "boundary_in shape mismatch (len != n_positions × n_embd)");
    let err = ctx.apply_boundary(&two, 0, 0, None).unwrap_err();
    assert_eq!(err.what, "n_positions must be >= 1");

    // Position range: `pos0 + n` must stay inside `n_ctx = 16`.
    let err = ctx.apply_tokens(&[1, 1, 1, 1], 13, None).unwrap_err();
    assert_eq!(err.what, "position range escapes [0, n_ctx)");
    let err = ctx.apply_tokens(&[1], -1, None).unwrap_err();
    assert_eq!(err.what, "position range escapes [0, n_ctx)");

    // Controls: exactly n_batch positions at the last legal offset, and a valid boundary apply.
    let mut out = vec![0f32; 4 * n_embd];
    ctx.apply_tokens(&[1, 1, 1, 1], 0, Some(&mut out)).expect("n == n_batch is legal");
    ctx.apply_boundary(&out, 4, 4, None).expect("a matching 4-position boundary applies");
    // (positions are contiguous in the KV: 0..4 tokens, 4..8 boundary, then 8..12 and 12..16.)
    ctx.apply_tokens(&[1, 1, 1, 1], 8, None).expect("contiguous apply");
    ctx.apply_tokens(&[1, 1, 1, 1], 12, None).expect("pos0 + n == n_ctx is legal");
}
