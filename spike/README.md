# M−1 — Engine feasibility spike (THROWAWAY)

> This directory is **not** the product. It exists to retire the single largest schedule
> risk in `BLUEPRINT.md`: whether the narrow llama.cpp/ggml FFI can support shard-style
> execution. A failed spike may reshape `hydra-engine-sys`; it must **not** change the
> protocol. Only after the findings note (`FINDINGS.md`) exists may M2 be scheduled.

## What must be proven (BLUEPRINT §3, M−1)

One small GGUF (1–3B), two contiguous layer ranges, one/two local processes, CPU backend
first (then CUDA/Metal):

- **(a)** load an arbitrary contiguous layer range only
- **(b)** inject boundary activations into the first layer of a range; extract after the last
- **(c)** KV allocated only for assigned layers
- **(d)** truncate KV at an input position and replay
- **(e)** run the final range to logits **without sampling** and retain them
- **(f)** round-trip FP16 (and int8+scales) boundary tensors between backends
- **(g)** tokenizer / RoPE / config metadata identical across both shard loads

## Definition of Done

1. Prompt applied through shard A → shard B produces final logits matching **unsplit
   llama.cpp on the same CPU backend within 1e‑3 max‑abs**.
2. KV truncate+replay reproduces those logits.
3. `FINDINGS.md` (one page) records any FFI-boundary changes needed by `hydra-engine-sys`.

## Approach — decided

**Minimal per-arch engine patch** (not a ggml reimplementation): the pinned tree already
exposes `llama_batch.embd` (inject a boundary residual) and the residual stream, so we window
the transformer loop to `[il_start, il_end)` and reuse llama.cpp's exact ops — giving a
bit-exact match at full precision. Engine delta: `llama-cpp-layer-window.patch` (47 lines).
The spike links against the vendored, pinned llama.cpp build in `../vendor/llama.cpp/build`.

## Result: **PASS** — see [`FINDINGS.md`](FINDINGS.md)

F32 boundary split == unsplit logits **bit-exactly** (0.0 max-abs) across split points
k ∈ {1,4,12,18,23} × 3 prompts; KV truncate+replay exact; FP16 boundary payload costs ~0.04
logit max-abs with stable argmax (spec I8). The lone FFI-boundary finding is payload precision.

## Build & run
```
# 1. apply the patch to the vendored engine and rebuild libllama
git -C ../vendor/llama.cpp apply ../../spike/llama-cpp-layer-window.patch   # (already applied in-tree)
cmake --build ../vendor/llama.cpp/build --target llama -j8
# 2. build and run the spike
cmake -B build -DCMAKE_BUILD_TYPE=Release && cmake --build build -j8
./build/ref_logits  -m ../models/qwen2.5-0.5b-instruct-fp16.gguf -p "The capital of France is"
./build/shard_split -m ../models/qwen2.5-0.5b-instruct-fp16.gguf -k 12
```

## Layout

```
spike/
├── README.md        # this file
├── FINDINGS.md      # the required one-page note (the actual M−1 deliverable)
├── CMakeLists.txt   # builds the spike against vendored llama.cpp/ggml
└── src/             # spike sources
```
