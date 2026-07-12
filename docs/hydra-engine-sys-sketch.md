# `hydra-engine-sys` — FFI surface sketch (design, not code)

> **Status:** design sketch for M2, grounded in the M−1 spike (`spike/FINDINGS.md`, `spike/src/shard_split.cpp`). **Not implemented.** No M2 code lands until the M1 gate passes in full (C1–C3). This documents the *narrow* C/C++ FFI the spike proved sufficient, so M2 starts from a validated surface rather than a guess.
>
> **Prime constraint (BLUEPRINT §1.4):** all protocol logic stays in Rust (`hydra-state`); the engine only computes. This crate is the **only** place `unsafe`/C touches the tree, and it exposes *no* protocol concepts — no sessions, epochs, attempts, WAL. It moves tensors and runs layer ranges. Everything the spike needed is here; nothing it didn't is.

## 1. What the spike proved (the surface must support exactly this)

From `spike/FINDINGS.md`, all validated against `llama.cpp` @ `13f2b28b` with the per-arch layer-window patch (`spike/llama-cpp-layer-window.patch`, arches `llama` + `qwen2`):

- **Load an arbitrary contiguous layer range** `[l0, l1)` only (not the whole model), KV allocated only for those layers.
- **Inject** a boundary residual at the first layer of the range (`batch.embd`) and **extract** it after the last (`get_embeddings` / the `t_layer_inp` path).
- **Run a range to logits without sampling** and retain them (shard S_P only).
- **Truncate KV at an input position and replay** — bit-exact reproduction (`llama_memory_seq_rm` + re-decode; spike Check D, `< 1e-4`).
- **Boundary is exact at F32** (spike Check C: `0.0` max-abs, same backend); **FP16 costs ~0.04 logit max-abs** (argmax/top-10 stable, spec I8); **int8_blockq costs ~1.8 and fails the M2 ≥9/10 top-10 bar (spike Check E) — forbidden for v1.**

## 2. Opaque handles (ownership: Rust owns, C borrows)

```
HydraModel*   // a loaded GGUF's weights for one shard's layer range + metadata
HydraContext* // one live inference context over a HydraModel (its own KV cache)
```
Both are opaque pointers created/destroyed by explicit `*_load`/`*_free` calls. Rust wraps each in an owning handle with a `Drop` that calls `_free`; handles are `!Send`-by-default (a ggml context is not thread-safe) and only made `Send` behind a proven-safe wrapper. No global state.

## 3. Core FFI (the narrow surface — C ABI, `extern "C"`)

```c
// ---- load / free ----
// Load only layers [l0, l1) of a shard file. arch must be a patched family (llama|qwen2);
// returns NULL on unsupported arch or a shard-manifest/tensor-hash mismatch.
HydraModel* hydra_model_load(const char* shard_path, int32_t l0, int32_t l1,
                             HydraLoadParams params, HydraStatus* out_status);
void        hydra_model_free(HydraModel*);

// n_embd, n_layer_total, rope/config hashes, tokenizer hash — for the manifest cross-check
// (I3/I8 config identity). Pure getters, no compute.
HydraModelInfo hydra_model_info(const HydraModel*);

HydraContext* hydra_context_new(HydraModel*, HydraContextParams, HydraStatus*);
void          hydra_context_free(HydraContext*);

// ---- the one compute call: apply a contiguous input-position range ----
// Applies tokens/positions [pos0, pos0+n) through this shard's layers. `boundary_in` is the
// residual entering l0 (NULL iff this shard owns l0==0, i.e. it embeds tokens itself).
// Writes the residual leaving l1-1 into `boundary_out` (NULL iff this shard owns the last
// layer and only logits are wanted). Both are [n * n_embd] f32 in native layout.
// Returns the applied_pos frontier reached (== pos0+n on success). NEVER samples.
HydraStatus hydra_apply(HydraContext*,
                        const HydraToken* tokens,   // NULL if boundary_in is provided
                        const float*      boundary_in,   // [n*n_embd] or NULL
                        int32_t pos0, int32_t n,
                        float* boundary_out,        // [n*n_embd] or NULL
                        int32_t* out_applied_pos);

// ---- logits at S_P (final shard only), retained, unsampled (I14: sampling is Rust's job) ----
HydraStatus hydra_logits(HydraContext*, int32_t at_pos,
                         const float** out_logits, int32_t* out_vocab); // borrowed, ctx-lifetime

// ---- KV lifecycle (recovery: I7a truncate + replay; I15 sampler snapshot is separate) ----
HydraStatus hydra_kv_truncate(HydraContext*, int32_t pos);          // drop KV for positions >= pos
HydraStatus hydra_kv_len(const HydraContext*, int32_t* out_len);
// Opaque KV snapshot/restore for coordinator-driven rebuild isolation (I12). Serialized blob
// owned by the caller; restore validates shape/version.
HydraStatus hydra_kv_snapshot(const HydraContext*, uint8_t* buf, size_t cap, size_t* out_len);
HydraStatus hydra_kv_restore(HydraContext*, const uint8_t* buf, size_t len);
```

### Boundary precision (from the int8 finding, spike §Check E)
`hydra_apply` moves boundaries as **f32** across the FFI. Wire-level precision (`f32|f16|int8_blockq`) is applied by the **Rust transport layer**, not here — the engine always sees f32. Policy the transport enforces: **`f16` production default** (spec §1.3), **`f32` for the exact-equivalence tier and same-backend D1 recovery**, **`int8_blockq` forbidden** (measured to fail M2 ≥9/10 top-10; §7.11). Keeping precision out of the FFI means a future outlier-aware int8 requant is a transport change, not an engine change.

## 4. Status / error model
`HydraStatus` is a plain `int32` enum (`OK`, ` E_ARCH_UNSUPPORTED`, `E_SHARD_MISMATCH`, `E_SHAPE`, `E_OOM`, `E_KV_RANGE`, `E_DECODE`). No C++ exceptions cross the boundary (the patch/harness already avoids throwing across FFI). Every call is total: it returns a status, never aborts. Size caps (frame/tensor) are validated in Rust *before* any FFI call (BLUEPRINT §9), so the engine never allocates on unvalidated sizes.

## 5. Build & submodule coupling (a maintenance liability to keep visible)
- Links `llama.cpp`/`ggml` built from the pinned submodule (`vendor/llama.cpp @ 13f2b28b`) **with the per-arch layer-window patch applied**. The patch is **arch-specific** (`src/models/llama.cpp`, `src/models/qwen2.cpp`) and **submodule-version-coupled**.
- **Every `vendor/llama.cpp` bump MUST re-run the M−1 spike sweep** (`spike/shard_split`, all split×prompt combos, incl. Check C F32-exact / Check D replay / Check E int8) as part of the M2 golden-token gate before the bump is accepted (BLUEPRINT §1.2). Adding a model family = porting the ~47-line window patch to that arch's graph builder + re-running the sweep.
- Retire the patch if/when upstream lands a generic layer-window hook ([#25577](https://github.com/ggml-org/llama.cpp/issues/25577)).

## 6. Explicitly out of this crate (kept in Rust / deferred)
- **Sampling** — S_P returns raw logits; temperature/top-p/Philox/penalties + per-token sampler checkpoints (I14/I15) live in Rust (M2).
- **Tokenizer/detokenizer** — coordinator-side, Rust (M2).
- **Any protocol state** — sessions, epochs, attempts, activation tuples, WAL, fencing: `hydra-state` only. The engine cannot express them.
- **Segment/candidate checkpoints (I24)** — the tool-call/segment flow is M3; no engine surface yet.
- **Reserved hooks** — MoE, paged KV, speculative decode: not in the FFI; added as typed-but-unused only when their milestone opens.

## 7. Open questions for M2 (flagged, not decided here)
1. KV snapshot blob format + version handshake (must survive a coordinator restart replaying onto a possibly-rebuilt context — I12).
2. Zero-copy vs. copy for `boundary_out` at the hot path (per-hop latency is the expected bottleneck, BLUEPRINT §5); the spike copied — measure before optimizing.
3. Backend selection (CUDA/Metal/CPU) surfaced via `HydraContextParams`; cross-backend boundary drift is documented semantic-continuity (I8), gated by M2(b)'s mixed-backend tolerance tier.
