# Submodule bump validation — `vendor/llama.cpp` `13f2b28b` → `f280b269`

**Status: BOTH BINDING CONDITIONS DISCHARGED. THE PIN HAS NOT BEEN MOVED.**
Moving it is sequenced with audit **L1**'s fork pin, and creating that fork is an **owner action**
(§8). See "Why the pin is still at `13f2b28b`" below — it is a deliberate stop, not unfinished work.

| | |
|---|---|
| **Current pin** | `13f2b28b098623391b1aacfd27995e1c8b7de9a9` — 2026-07-11, *"DeepseekV4: clear cache only for seq rather than full (#25521)"* |
| **Candidate** | `f280b26983ad0fdb705a0d9ebf0503e76f2899b0` — 2026-08-25, *"metal : per-device tuned (Q, NE) for flash-attn vec (#26570)"* |
| **Distance** | **642 commits, 6 weeks** |
| **Validated in** | an isolated `git worktree` under the scratchpad. **`vendor/llama.cpp` was never touched** — its patched working tree is what every green engine result in this session depends on, and §0(b) warns it must not be "cleaned". |

---

## Condition (ii) — diff review of the seven patched files

> *"A patch that applies without conflict is not thereby still correct."* — §0(c)

### Churn

| File | Upstream commits | Δ |
|---|---:|---|
| `src/llama-model.cpp` | 37 | +494 |
| `src/llama-context.cpp` | 15 | +443 |
| `include/llama.h` | 13 | +73 |
| `src/llama-graph.cpp` | 10 | +481 |
| `src/llama-cparams.h` | 2 | +5 |
| `src/models/llama.cpp` | **0** | — |
| `src/models/qwen2.cpp` | **0** | — |

**Both per-arch graph builders are untouched**, which is where most of the ~47-line window patch lives.

### Does it apply?

```
git apply --check          -> error: patch failed: include/llama.h:319
                              error: patch failed: src/llama-model.cpp:2309
git apply --3way --check   -> Applied patch to all 7 files cleanly.
```

Strict apply fails on **context drift only**. Re-extracting the diff after a 3-way apply and
comparing added/removed lines against the canonical patch: **identical**. The patch's *content*
transfers exactly.

### The finding condition (ii) exists to catch — an ABI change the patch does not touch

`llama_model_params` changed underneath us:

```
+  enum llama_load_mode load_mode;
-  bool use_mmap;
-  bool use_direct_io;
-  bool use_mlock;
+  bool load_mtp;
```

Three booleans replaced by an enum. **The shim is insulated** — `hydra_engine.cpp` only ever calls
`llama_model_default_params()` / `llama_context_default_params()` and then assigns named fields, so
it never names a removed one. Verified by grep, not assumed. **But this is exactly the shape that a
clean `git apply` would have hidden**: the patch's own hunk into `llama_model_default_params()`
carried `/*.use_mmap =*/ true,` as *context*, which is why strict apply failed there.

### Positional-initializer audit (the silent-corruption risk)

Both defaults functions use **positional aggregate initialisers with comment labels** — a field
inserted at the wrong ordinal silently assigns to the wrong member and nothing warns. Field order
was therefore checked against struct order, member by member, after the 3-way apply:

* `llama_model_params` — **18/18 aligned**, `il_load_start`/`il_load_end` in the correct slots.
* `llama_context_params` — aligned; `il_start`/`il_end` land last, matching the struct.
  (`/*.sampler =*/` vs field `samplers` is an **upstream comment-label typo**, not a misalignment —
  the values sit in the right slots.)

### The three semantic hook sites

| Hook | Site | Verdict |
|---|---|---|
| Boundary **injection** | `llm_graph_context::build_inp_embd` | Context unchanged — `inp->embd`, `n_embd_inp`, `res->t_inp_embd`, `add_input` all present and unmoved |
| Shard **load window** | `load_tensors` tensor-creation filter | `tn.bid`, `flags`, `TENSOR_NOT_REQUIRED` unchanged |
| Partial tensor count | `ml.done_getting_tensors(partial)` | signature unchanged |

### Does it build?

**Yes.** Full `cmake --build --target llama` of the patched candidate: **EXIT=0, zero errors, zero
warnings mentioning the patch.** Release, `GGML_METAL=ON`, `GGML_BLAS=Apple`, matching the vendored
build's own cache settings.

---

## Condition (i) — the full 15-combination spike sweep, re-run

`spike/shard_split`, `k ∈ {1,4,12,18,23}` × 3 prompts, **CPU backend (`-ngl 0`) = the DoD backend**.
Raw log: [`spike-sweep-bump-f280b269.log`](spike-sweep-bump-f280b269.log). Quoted per rule 16.

**15 / 15 combinations `=== M-1 DoD: PASS ===`, EXIT=0 throughout.**

| Check | Result across all 15 |
|---|---|
| **A** — shard-A residual vs unsplit `l_out-{k-1}` | **`max_abs=0.000e+00`** — 15/15 exact |
| **C** — F32 split vs unsplit logits (**the DoD test**) | **`max_abs=0.000e+00`** — 15/15 **bit-exact**, argmax identical, **top10=10/10** everywhere |
| **D** — KV truncate + replay | **`max_abs=0.000e+00`** — 15/15 exact |
| **B** — FP16 boundary payload (item f) | 2.832e-02 … 5.664e-02, argmax stable, top10=10/10 |
| **E** — int8_blockq (item f, **stays FORBIDDEN**) | 6.421e-02 … 2.298e+00 — consistent with §7.11's ruling |

**⚑ The three prompts are named here because the original sweep's were not** — `FINDINGS.md` says
"× 3 prompts" and never records which. A sweep whose inputs are unrecorded cannot be *re-run*, only
*re-performed with different inputs*, and BLUEPRINT §1.2 makes re-running it **binding on every
bump**. The prompts used:

* **P1** `The capital of France is`
* **P2** `In a shocking finding, scientists discovered a herd of unicorns living in a remote valley`
* **P3** `def fibonacci(n):`

---

## ⚑⚑ A FINDING THE SWEEP TURNED UP THAT IS **NOT** ABOUT THE BUMP

The Metal spot-check (`-ngl 99`, k=12) **fails Check D on P3** — and **fails identically on the
current pin**:

```
bumped f280b269 : [Check D] KV truncate@pos2 + replay ... max_abs=4.059e-04  -> FAIL
pinned 13f2b28b : [Check D] KV truncate@pos2 + replay ... max_abs=4.059e-04  -> FAIL
```

**Byte-identical on both engines, so the bump introduces no regression** — that is the control, and
it is the reason this section exists rather than an escalation. What it *does* establish:

1. **On Metal, KV truncate + replay is not bit-exact** for P3 (4 tokens, truncate at pos 2). It is
   exact for P1 and P2, and exact for **all 15** CPU combinations. The value is stable across runs
   and across engine versions, so it is deterministic behaviour, not noise.
2. **`FINDINGS.md`'s Metal close-out claims "KV truncate+replay: exact" for the full 15-combination
   Metal sweep.** That claim does not hold for P3. Whether the original Metal sweep used prompts
   where it happens to hold, or overstated, **cannot be determined — the prompts were never
   recorded.** This is the unrecorded-inputs problem above, with a consequence attached.
3. **⛔ Every one of the project's 31 engine-test configurations sets `n_gpu_layers: 0`.** The
   rule-14 bit-exact anchors, the shard anchor, `d1_recovery`, three-node recovery and the
   calibration receipt are **CPU-only evidence**. Metal is exercised by *nothing* in the suite — so
   this behaviour, on the exact operation teacher-forced recovery is built from, is invisible to
   every gate the project has.

**Stated precisely, and no wider:** 4.059e-04 is well inside the 1e-3 split-vs-unsplit tolerance and
did not move the argmax here. It is **not** a demonstrated token divergence. What it is, is a
demonstrated **non-exactness on the backend the product will actually run on**, underneath claims
that are phrased as *byte-identical*. Those claims are true of the evidence that exists; the
evidence is CPU. Recorded as a §8 owed item and a §6.0 "cannot see" line rather than argued either
way here.

---

## Why the pin is still at `13f2b28b`

Both conditions are met and the candidate is validated — but §8 sequences the bump **with audit
L1's fork pin, so the layer-window patch is re-ported once**. The patch exists only as an
**uncommitted working-tree modification** of the submodule; bumping to `f280b269` today would
reproduce exactly that defect at a newer SHA and re-port the patch a second time when the fork
lands — the precise outcome the sequencing was written to prevent.

**Creating `PavanManchikatla/llama.cpp` is an owner action** (a new remote repository under the
owner's account), which §8 states explicitly. **The agent-side work is done and banked here**, so
the remaining step is mechanical: push this validated patched tree as the fork, tag it, repoint the
submodule, and `spike/*.patch` becomes provenance rather than the mechanism.
