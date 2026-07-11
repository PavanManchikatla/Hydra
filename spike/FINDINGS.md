# M−1 Engine Feasibility Spike — Findings Note

**Result: PASS.** Shard-style execution over llama.cpp/ggml is feasible through a narrow FFI.
A prompt applied through shard A → shard B reproduces unsplit llama.cpp's final logits
**bit-exactly** on the CPU backend; KV truncate+replay reproduces them exactly. The one
FFI-boundary consideration for `hydra-engine-sys` is the **activation-payload precision** of
the boundary tensor (below). Per BLUEPRINT §3, M2 may now be scheduled.

## Setup
- Engine: vendored llama.cpp, pinned submodule `13f2b28b`, CPU backend (`n_gpu_layers=0`), Apple M2.
- Model: `Qwen2.5-0.5B-Instruct` F16 GGUF (arch `qwen2`, 24 layers, n_embd 896, GQA, NEOX RoPE).
- Split: shard A = layers `[0,k)`, shard B = layers `[k,24)`. Swept `k ∈ {1,4,12,18,23}` × 3 prompts.
- Code: `spike/src/shard_split.cpp`; engine delta: `spike/llama-cpp-layer-window.patch` (47 lines, 5 files).

## What was proven (all 15 split×prompt combinations)

| Check | What | Result |
|---|---|---|
| A | shard-A boundary residual == unsplit `l_out-{k-1}` (eval-callback ground truth) | **0.0 max-abs (exact)** |
| C | split with **F32** boundary vs unsplit logits — *the DoD "same-backend" test* | **0.0 max-abs (exact)**, argmax + top-10 identical |
| D | KV `seq_rm` truncate at a position + replay vs pre-truncate logits | **0.0 max-abs (exact)** |
| B | split with **FP16** boundary round-trip (item f) | 0.03–0.06 max-abs on logits; **argmax + top-10 stable** |

Checklist (BLUEPRINT §3): (a) run an arbitrary contiguous layer range — ✅ (k swept 1..23);
(b) inject + extract boundary activations — ✅ (exact); (d) KV truncate+replay — ✅ (exact);
(e) final range → logits without sampling — ✅ (raw `llama_get_logits_ith`);
(f) FP16 boundary round-trip — ✅ (characterized); (g) tokenizer/RoPE/config identical across
shard loads — ✅ (single GGUF, shared model handle).

## Approach — minimal engine patch (not reimplementation)

The pinned tree already exposes the two hooks needed, so the patch reuses llama.cpp's exact
compute graph (hence the bit-exact match) rather than reimplementing the forward:
- **Inject** a boundary residual as the layer-`k` input via `llama_batch.embd` (the vector-
  embeddings input path; passes through unscaled — verified in `build_inp_embd`).
- **Extract** the raw residual entering layer `k` via `res->t_embd` in embeddings mode
  (`llama_get_embeddings_ith`).
- **Window** the transformer loop to `[il_start, il_end)`, added to `llama_context_params`
  (baked at context creation so the reserved graph honors it). A boundary shard (`il_end <
  n_layer`) skips the final norm/lm_head result and instead exposes the raw pre-norm residual.

Patched files: `include/llama.h`, `src/llama-cparams.h`, `src/llama-context.cpp`,
`src/models/llama.cpp`, `src/models/qwen2.cpp`. Each arch has its own graph builder in
`src/models/*.cpp`, so **the window must be applied per-arch** (a `llama`-family model uses
`llama.cpp`; Qwen2.5 uses `qwen2.cpp`). This is the main "FFI-boundary change" for
`hydra-engine-sys`: it must carry a small per-arch graph patch, or upstream a generic
layer-window option.

### Two bugs found and fixed during the spike (recorded for the real FFI)
1. **Embeddings context defaults to non-causal attention** — a shard reused for extraction
   must call `llama_set_causal_attn(ctx, true)`; the LM is causal.
2. **`inp_out_ids` dangling-input crash** — the output-id selection `get_rows` was keyed on
   `il == n_layer-1`; for a non-final shard it never fired, leaving `inp_out_ids` unmaterialized
   → null-buffer assert in `set_input`. Fixed by keying on the shard's last layer `il == l1-1`.

## The one real FFI-boundary finding: boundary payload precision

The split is exact at full precision; **all deviation comes from quantizing the boundary
tensor**. FP16's ulp is ≈1.0 at the residual stream's massive-activation magnitudes
(observed ~1560 at one dim), so an FP16 boundary yields ~0.04 max-abs on logits. Argmax and
top-10 stay stable, consistent with spec **I8** (cross-precision drift is documented
semantic-continuity behavior, not a bug), and well inside the M2(b) mixed-backend tolerance
(top-k(10) ≥ 9/10).

**Recommendation for `hydra-engine-sys` / session config (spec §1.3):** treat the FP16 default
as semantic-continuity-preserving but **not** bit-reproducible. Where strict logit
reproducibility is required (e.g. exact-equivalence tests, D1 recovery onto the *same*
backend), pass the boundary at **F32** — it is bit-exact here. This is a payload-precision
default, **not** a protocol change (the protocol already parameterizes payload dtype).

## Deferred (not required for the DoD; scoped to M2+)
- **Range-only weight loading.** The spike loads the whole GGUF per shard and runs only its
  range. A real worker loads only its shard's tensors — trivial via GGUF tensor subsetting in
  `hydra-modelsvc` (M2); no execution-path risk (execution over a range is proven exact).
- **Range-only KV allocation** (item c, memory). llama.cpp allocates KV for all `n_layer`;
  only the shard's range is written. Correct but over-allocated; scope a KV-init patch to
  allocate `k_l/v_l` for `[il_start,il_end)` only (prima.cpp does this).
- **int8+scales boundary payload** (item f) — expected worse than FP16; measure in M2.
- **Cross-backend boundary (CPU→Metal)** — I8 semantic continuity; belongs in M2(b) golden-token tests.

## Reproduce
```
cmake --build vendor/llama.cpp/build --target llama -j8   # after applying the patch
cmake -S spike -B spike/build && cmake --build spike/build -j8
spike/build/shard_split -m models/qwen2.5-0.5b-instruct-fp16.gguf -k 12
```
