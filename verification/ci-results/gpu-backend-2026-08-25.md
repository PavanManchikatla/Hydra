# The GPU gate row — engine-gated suite re-run at `n_gpu_layers: 99`

**Owner-ruled 2026-08-25 (Decision 2):** the CPU-only evidence base is a **gate row**, not an
accepted limitation. *"Re-run the engine suite at n_gpu_layers: 99. If the byte-identity assertions
survive, the claim strengthens and the row closes. If they do not, the recovery claim gets a
per-backend qualifier in the README before v1 ships."*

**This is the receipt.** Raw per-suite table: [`gpu-sweep-ngl99-2026-08-25.txt`](gpu-sweep-ngl99-2026-08-25.txt).

---

## The lever, and why it can be trusted

All 31 engine-test configurations hard-code `n_gpu_layers: 0`. Editing 31 literals would have to be
undone, and **a lever that must be reverted is one a future session cannot re-pull** — so the
override is intercepted at the single load boundary in `hydra-engine-sys`
(`Model::load` / `Model::load_shard`), which covers every caller including the binaries and leaves
**0, the DoD backend, as the default**. It is opt-in by explicit environment variable and announces
itself on stderr — the same contract as `HYDRA_FORCE_ENGINE_STUB`.

```
HYDRA_TEST_NGL=99 cargo test -p hydra-worker --test <suite> -- --test-threads=1
```

**Rule 12 — the lever is infrastructure, so it was verified before anything was concluded from it.**
The marker `load_tensors:  MTL0_Mapped model buffer size` appears **only** at `ngl=99` and never at
the default, so layers demonstrably moved to the GPU. Both anomalies reported below are likewise
**absent at `ngl=0`** on the same suites: 0 aborts and 0 OOM messages in the control.

## Verdict

**88 passed / 5 failed / 5 ignored across 30 engine-gated suites.**

> ### ⚑ Every byte-identity and argmax assertion that RAN TO COMPLETION on Metal PASSED.
>
> **None of the 5 failures is an equality assertion.** Every one died on `peer closed connection`
> or `EngineError code 5`, with 8–17 GPU out-of-memory messages in the same log.

| Assertion (on Metal) | Result |
|---|---|
| `split_vs_unsplit_f32_bit_exact_through_crate_api` | ✅ ok |
| `two_worker_teacher_forced_no_sample_bit_exact` (rule 14) | ✅ ok |
| `greedy_sample_across_pipeline_matches_unsplit_argmax` (rule 14) | ✅ ok |
| `direct_worker_to_worker_fwd_is_bit_exact` | ✅ ok |
| `two_worker_anchor_is_bit_exact_with_shard_loaded_weights` | ✅ ok |
| `shard_loaded_weights_are_bit_exact_with_the_unsplit_model` | ✅ ok |
| `chunked_prefill_is_bit_exact_with_unchunked_prefill` | ✅ ok |
| `d1_recovery_three_kill_windows_are_byte_identical_to_an_uninterrupted_seeded_run` | ✅ ok |
| `three_node_kill_middle_s2_rebuilds_from_upstream_durable_boundaries_byte_identical` | ✅ ok |
| `three_node_chained_direct_fwd_is_byte_identical_to_unsplit_greedy` | ✅ ok |
| `incremental_detok_is_byte_identical_to_batch` · `last_event_id_resume_yields_byte_identical_suffix` | ✅ ok |

**Two recovery byte-identity tests and two three-node ones did not run to completion** — see
residual 2. They neither passed nor failed on their assertions; they never reached them.

---

## Residual 1 — a Metal **teardown** abort (a real defect, and not a correctness one)

**9 of 30 suites report `test result: ok`, all tests passed, and then the process aborts:**

```
vendor/llama.cpp/ggml/src/ggml-metal/ggml-metal-device.m:622: GGML_ASSERT([rsets->data count] == 0) failed
(signal: 6, SIGABRT)
```

A resource-set leak check in ggml's Metal device teardown. **The assertions have already been
evaluated and reported when it fires** — `chunked_prefill_is_bit_exact_with_unchunked_prefill ... ok`
followed by `test result: ok. 5 passed; 0 failed` and *then* the abort.

**Absent at `ngl=0` on the same suites**, so it is specific to GPU offload.

**What it means for the product, stated plainly:** a Metal-backed worker would **abort on shutdown**.
That is an operational defect worth fixing, not a computation defect — nothing it touches has
produced a wrong value. It is upstream `llama.cpp` code, and the **bump candidate `f280b269` carries
substantial Metal churn** (its tip commit is a Metal change), so whether the bump fixes it is worth
checking when the fork lands. **Not checked here** — this receipt reports the pinned engine.

## Residual 2 — GPU out-of-memory on this 8 GB machine (hardware contingency)

The three suites with failures each spin up **two or three workers, every one mapping the full model
into the GPU working set**, against a device reporting:

```
ggml_metal_device_init: recommendedMaxWorkingSetSize  =  5726.63 MB
error: Insufficient Memory (00000008:kIOGPUCommandBufferCallbackErrorOutOfMemory)
```

| Suite | Failures | OOM messages | Cause |
|---|---:|---:|---|
| `d1_two_stage` | 2 | 16 | worker died mid-test → TLS `UnexpectedEof` |
| `three_node_recovery` | 2 | 17 | `EngineError code 5` during prefill/feedback |
| `sampler_pipeline` | 1 | 8 | `seeded_sampling_is_reproducible_across_two_full_runs` — two full pipelines |

**These are the same class as the 3-machine DoD row: a limit of the hardware the project has, not a
property of the code.** On CPU all three suites pass; the models simply fit. **Owed, not waived** —
the multi-worker recovery suites remain unverified on Metal until a machine with a larger GPU working
set runs them.

---

## What this row now establishes, and what it does not

**Establishes:** the split/recovery machinery is **bit-exact on Metal** wherever it could be
measured — including the rule-14 anchors, shard-loaded weights, chunked prefill, direct FWD, D1
recovery, and one of the three-node recovery paths. **The byte-identity claim is materially stronger
than it was this morning, when it rested on 31 configurations that all pinned the CPU.**

**Does not establish:** anything about the **multi-worker recovery suites on Metal** (residual 2), and
it does not remove the **M−1 sweep's Check D result** — Metal's KV truncate+replay is still not
bit-exact for a 4-token prompt truncated at position 2 (`max_abs=4.059e-04`, deterministic, identical
on the pinned and candidate engines). **That case is narrower than anything the recovery suites
exercise**, and the recovery suites that did run were byte-identical on Metal — but it is not
excluded either, and it is why `spike/FINDINGS.md` is corrected in the same commit as this receipt.

**And it says nothing about CUDA**, which no machine in this project has ever run.
