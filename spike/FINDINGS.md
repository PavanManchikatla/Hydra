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

## Backend sweep — Metal (v0.10.2 close-out)

Re-ran the full 15-combination sweep with all layers offloaded to the Apple M2 GPU (`-ngl 99`,
confirmed `layer N assigned to device MTL0`). On a single backend the split reuses the same
kernels as the unsplit run, so — as on CPU — the F32 split is **bit-exact vs unsplit on that
same backend**:

| Metric (Metal, 15 combos) | Result |
|---|---|
| F32 split vs Metal-unsplit logits | **0.0 max-abs (bit-exact)**, argmax + top-10 identical, all 15 |
| KV truncate+replay | exact |
| FP16 boundary payload cost | 0.003–0.014 max-abs on logits (lower than CPU's 0.03–0.06), argmax stable, top-10 = 10/10 |

The FP16 boundary cost is **~4× lower on Metal than CPU** (0.003–0.014 vs 0.03–0.06) — an
accumulation-order artifact of Metal's kernels, not a correctness signal; **no action**.

Satisfies the ruling: split-vs-unsplit **on Metal** agree within 1e‑3 (in fact 0.0). Note this is
*same-backend* agreement; **Metal-vs-CPU** token drift (different kernels) is a separate,
expected I8 effect and is deferred to the M2(b) golden-token tests — which, per the v0.10.2
amendment, run over **f32** boundaries only.

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

## int8 + block-scale boundary characterization (item f, second half — DONE, M2 prep, 2026-07-12)
`int8_blockq` boundary payload characterized: **symmetric per-block int8, block size QK=32**
(ggml Q8_0 convention; the `.fbs` leaves block size to the codec) matching the wire schema's
`DType.I8_BLOCKQ` + `Tensor.block_scales`. Harness: `src/shard_split.cpp` **Check E** (scale =
block-amax/127, quantize→dequantize the boundary, run shard B, compare logits). Sweep: 5 splits
`k∈{1,4,12,18,23}` × prompts, CPU (DoD backend) + Metal spot-check.

**Result — int8_blockq is far lossier than FP16 and does NOT meet the M2(b) mixed-backend tolerance:**

| split k | int8 logit max-abs (CPU) | argmax | top-10 |
|---|---|---|---|
| 1 (early) | 0.067–0.073 | stable | 10/10 |
| 4 | 1.35–1.68 | stable | 9–10/10 |
| 12 (mid) | **1.80–1.81** | stable | **8/10** |
| 18 | 1.57–1.75 | stable | 9–10/10 |
| 23 (late) | 0.20–0.26 | stable | 10/10 |

- **F32 boundary stays bit-exact (0.0)** at every combo (mechanism unaffected — only the payload precision differs).
- Peak int8 cost **~1.8 logit max-abs at mid-network splits (k=4–18)** — **~40× worse than FP16's ~0.04** for the same split. **argmax stays stable everywhere**, but **top-10 overlap drops to 8/10** at the worst mid-split — **below M2(b)'s ≥ 9/10 mixed-backend bar**.
- **Root cause = the massive-activation outlier.** Check E's diagnostic: global **max|x| = 1624.6 at dim 62**; it dominates its QK=32 block, giving block scale **12.79** — so the other ~31 dims in that block quantize with a step of ~12.8 and any dim below ~6.4 collapses to 0. The outlier is the error, exactly as predicted.
- **Backend-invariant, unlike FP16.** CPU **1.805** ≈ Metal **1.834** at k=12 — the int8 error is *outlier/quantization*-dominated (a property of the residual), not accumulation-order-dominated, so it does not shrink on Metal the way FP16 does (Metal FP16 ~0.010 vs CPU ~0.043, the ~4× documented earlier). This is a qualitatively different, and worse, error source than FP16 drift.

**Ruling.** Naïve `int8_blockq` (QK=32, symmetric) **fails the M2 mixed-backend tolerance** at mid-network split points and is **not usable for v1**. The **standing constraint stands upheld** (v0.10.2 §1.3: `int8_blockq` boundaries MUST NOT be used anywhere) — this measurement is the evidence that the "forbidden until measured" gate was correct, not a formality. **Before any future int8 use (M2+),** the outlier must be handled explicitly: outlier-aware quantization (carry the massive-activation dims — e.g. dim 62 — out-of-band at higher precision), much smaller blocks, or per-channel scales; each must re-run this Check E sweep and clear ≥ 9/10 top-10 at every split before `int8_blockq` is unblocked. `f16` remains the production default; `f32` for the exact-equivalence tier.

## Upstream
Filed a request for a generic (arch-agnostic) layer-window / partial-execution hook so the
per-arch patch can eventually be retired: **https://github.com/ggml-org/llama.cpp/issues/25577**
(draft: [`upstream-llama-issue.md`](upstream-llama-issue.md); cites #22436 / #23568 as motivating
use cases; the two bugs — non-causal embeddings default, dangling `inp_out_ids` — included as
supporting evidence, offered as separate issues if maintainers prefer). **Monitor for replies at
each session start; drafts only — owner approves before any reply posts.** Until a hook lands the
patch is per-arch (`llama`, `qwen2`) and submodule-version-coupled — re-run this sweep on every
`vendor/llama.cpp` bump (BLUEPRINT §1.2, M2 golden-token gate).

## Deferred (not required for the DoD; scoped to M2+)
- **Range-only weight loading.** The spike loads the whole GGUF per shard and runs only its
  range. A real worker loads only its shard's tensors — trivial via GGUF tensor subsetting in
  `hydra-modelsvc` (M2); no execution-path risk (execution over a range is proven exact).
- **Range-only KV allocation** (item c, memory). llama.cpp allocates KV for all `n_layer`;
  only the shard's range is written. Correct but over-allocated; scope a KV-init patch to
  allocate `k_l/v_l` for `[il_start,il_end)` only (prima.cpp does this).
- **Cross-backend boundary (CPU↔Metal)** — I8 semantic continuity; belongs in M2(b) golden-token tests (f32 boundaries only).

## Reproduce
```
cmake --build vendor/llama.cpp/build --target llama -j8   # after applying the patch
cmake -S spike -B spike/build && cmake --build spike/build -j8
spike/build/shard_split -m models/qwen2.5-0.5b-instruct-fp16.gguf -k 12
```
