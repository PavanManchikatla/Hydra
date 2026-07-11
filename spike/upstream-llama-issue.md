# [Feature request] Generic, arch-agnostic layer-window hook (run layers `[a,b)` with injected/extracted boundary residual)

> Draft to file against https://github.com/ggml-org/llama.cpp. Not yet filed — posting is a
> public action pending maintainer/owner sign-off. Pinned checkout for line refs: `13f2b28b`.

## Problem
For **pipeline / layer sharding across machines** (each node runs a contiguous layer range and
passes the boundary residual to the next), one needs to:
1. run only transformer layers `[il_start, il_end)`,
2. on a non-first shard, use an **injected** hidden state as the layer-`il_start` input instead
   of token embeddings, and
3. on a non-last shard, read out the **raw residual** entering layer `il_end` (pre-final-norm),
   instead of logits.

Today (2)/(3) are *almost* possible with existing hooks, but (1) is not exposed, and each dense
arch has its own graph builder in `src/models/*.cpp`, so a layer-window has to be patched into
**every** arch's builder separately (we currently maintain it for `llama.cpp` and `qwen2.cpp`).
The refactor into per-arch builders makes a one-shot patch impossible to keep generic.

## What already exists (and works well)
- **Inject**: `llama_batch.embd` feeds an external `[n_embd, n_tokens]` residual through
  `build_inp_embd` (the vector-embeddings path; passes through unscaled). 👍
- **Per-layer residual capture**: `llm_graph_result::t_layer_inp[il]` + `get_layer_inp(il)` and
  the `embeddings_layer_inp` machinery already record every layer's input hidden state.

The only missing primitive is **restricting the layer loop to a range** (and, for a boundary
shard, skipping the final norm + lm_head and surfacing `t_layer_inp[il_end]` as an output).

## Proposed API
A small, generic option consumed by the shared graph driver rather than each arch:
```c
// llama_context_params (or a setter)
int32_t il_start;   // default 0
int32_t il_end;     // default -1 => n_layer
```
and have the shared graph scaffolding (`llm_graph_context` / the per-arch loop harness) honor
the window uniformly, so arch files don't each re-implement it. A boundary shard
(`il_end < n_layer`) would expose the raw layer-`il_end` input residual via the existing
embeddings output path.

## What we did as a stopgap (works, but per-arch)
A ~47-line patch that adds `il_start/il_end` to `llama_context_params`/`llama_cparams` and, in
each arch builder, (a) loops `[l0,l1)`, (b) keys the `inp_out_ids` `get_rows` on the shard's last
layer `l1-1` (not `n_layer-1`) so the output-id input isn't left dangling, and (c) for a boundary
shard exposes the pre-norm residual as `t_embd`. Verified **bit-exact** vs the unsplit model
(F32 boundary, CPU and Metal). We'd happily upstream a generic version if the maintainers are open
to the `il_start/il_end` shape.

## Why not RPC / tensor-split?
`LLAMA_SPLIT_MODE_LAYER` / `tensor_split` and the RPC backend distribute one logical graph across
devices — they don't give independent per-shard execution with an injectable boundary residual and
logits-from-injected-hidden-state, which is what cross-machine pipeline sharding needs.
