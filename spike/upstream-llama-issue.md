# [Feature request] Generic layer-window hook: run layers `[il_start, il_end)` with an injectable / extractable boundary residual

> Draft for ggml-org/llama.cpp. **Filing paused pending owner sign-off** on file-new-vs-comment:
> related open issues #22436 (pipeline parallelism via tcp/ip) and #23568 (hybrid TP+PP) are in
> the same problem space but ask for *end-user* multi-node features; this asks for the *low-level
> primitive* that would enable them. Pinned reference: checkout @ `13f2b28b`.

### Prerequisites
- [x] Running latest code (checkout `13f2b28b`).
- [x] Searched open/closed **issues** (found related #22436, #23568 — different ask; see below) and **Discussions** (#20252 pipeline parallelism).
- [x] This is a new, useful enhancement.

## Feature description
Expose a small, **architecture-agnostic** option to run only transformer layers
`[il_start, il_end)`, where a non-first shard consumes an **injected** hidden state as the
layer-`il_start` input and a non-last shard emits the **raw residual** entering layer `il_end`
instead of logits:
```c
// llama_context_params (or a setter)
int32_t il_start;   // default 0
int32_t il_end;     // default -1 => n_layer
```
honored by the shared graph driver so each arch's builder doesn't re-implement it.

## Motivation
Cross-machine **pipeline / layer sharding** (each node runs a contiguous layer range and passes
the boundary residual to the next) needs exactly this primitive. It is the building block under
the end-user features requested in **#22436** (pipeline parallelism over TCP/IP) and **#23568**
(TP+PP) — today there is no supported way to run a layer subrange starting from an injected
residual and read out a boundary residual, so every such effort re-patches the graph builders.

Two of the three pieces already exist and work well:
- **Inject**: `llama_batch.embd` feeds an external `[n_embd, n_tokens]` residual through
  `build_inp_embd` (unscaled). 👍
- **Per-layer residual capture**: `llm_graph_result::t_layer_inp[il]` + `get_layer_inp(il)` and
  the `embeddings_layer_inp` machinery already record each layer's input hidden state. 👍

The only missing primitive is **restricting the layer loop to a range** (and, for a boundary
shard, skipping the final norm + lm_head and surfacing the layer-`il_end` input residual).

## Supporting evidence: two bugs found while prototyping this
While implementing a stopgap per-arch patch (below), two upstream issues surfaced that anyone
doing partial-graph execution will hit:
1. **Embeddings-context defaults to non-causal attention** — a context reused to extract a
   mid-model residual must `llama_set_causal_attn(ctx, true)`, or the residual diverges from the
   causal model. Easy to miss; worth documenting or defaulting per arch.
2. **`inp_out_ids` dangles on a non-final shard** — the output-id `get_rows` is keyed on
   `il == n_layer - 1`; when a shard's last built layer is `< n_layer-1`, `inp_out_ids` is never
   consumed, and `llm_graph_input_out_ids::set_input` then dereferences its null buffer
   (`ggml_backend_buffer_is_host(NULL)` → assert). Keying it on the shard's actual last layer
   fixes it.

## Stopgap we use today (happy to upstream a generic version)
A ~47-line patch adds `il_start/il_end` to `llama_context_params`/`llama_cparams`, loops
`[l0,l1)`, keys the `inp_out_ids` `get_rows` on the shard's last layer, and for a boundary shard
exposes the pre-norm residual. Verified **bit-exact** vs the unsplit model (F32 boundary, CPU and
Metal). It's per-arch (`llama.cpp`, `qwen2.cpp`) only because each dense arch has its own builder
in `src/models/*.cpp` — hence this request for a generic hook.

## Why not RPC / `--split-mode layer`?
Those distribute one logical graph across local devices; they don't give independent per-shard
execution with an injectable boundary residual and logits-from-injected-hidden-state, which
cross-machine pipeline sharding needs.

---
*Context: this came up building [Hydra](https://github.com/PavanManchikatla/Hydra), a
crash-safe LAN pipeline-inference runtime.*
