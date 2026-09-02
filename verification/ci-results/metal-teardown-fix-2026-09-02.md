# The Metal teardown abort — Hydra-side lifetime excluded, then fixed (2026-09-02)

**Directive 2026-09-01 item 4(a):** *exclude Hydra-side lifetime first — enumerate which `Model`/`Context`
instances are alive at process exit in the aborting suites; drop them explicitly; re-run those suites
on the real arm at `n_gpu_layers 99` through the §1 script.* Every line below is `scripts/test-receipt.sh`
output (§7.68), quoted. Toolchain: the default `rustc 1.93.1`; pin `c00bcebf` on `f280b269`; ngl 99.

## 1. What was alive at exit — found by reading, then confirmed by the oracle

`crates/hydra-worker/src/worker.rs` (`Engine::try_new`, pre-fix):

```rust
let model: &'static Model = Box::leak(Box::new(model));
```

with the struct comment *"leaked to `'static` (one per worker process, freed at process exit)"*. The
second half was false: nothing frees a `Box::leak`. Every worker's `Model` — hence every Metal buffer
it owns — was alive at process exit by construction, in every suite that builds a `Worker` with a model.
ggml's Metal device lives in a function-local static (`ggml_metal_device_get`, a
`std::vector<std::unique_ptr<…>>`) whose destructor runs at exit and asserts
`[rsets->data count] == 0` — every buffer released. #22593's *"rsets_rm is never called"* was the
wrong root cause (§7.67); the buffers simply outlived the device because their owner was never dropped.

**Pre-fix oracle (rule 19 — the oracle must produce the failure), unfixed tree `41ca8cd`, 4 of the
aborting suites at ngl 99:**

```
arm=real exit=101 running=4 readable=4 mangled=0 passed=15 failed=0 ignored=0 ggml_assert=4 stub_msgs=0 wall=10m57s verdict=RED (cargo exit=101 with no failing result line — read the stderr)
```

Every suite passed its tests (15 passed / 0 failed across the four) and then the process aborted —
`ggml_assert=4`, one per suite, cargo exit 101, `verdict=RED (cargo exit=101 with no failing result
line)`. The script's RED-without-a-failing-line branch is exactly the shape this abort has.

## 2. The fix (two halves, both Hydra-side)

1. **`Engine` no longer leaks** — the model is boxed to a raw pointer, `ctx` is `ManuallyDrop`, and
   `Drop for Engine` releases the context first and then un-leaks the model (`worker.rs`).
2. **An exit drain in the shim** (`hydra_engine.cpp`): every live model/context is tracked;
   `std::atexit` registers a drain AFTER the first successful model load — so it is registered after
   ggml's device static (constructed during that load) and therefore runs BEFORE its destructor —
   and releases whatever is still alive at exit. Frees are idempotent (a drained handle is skipped).
   This is what covers test harnesses whose endpoint threads are abandoned at exit, and any product
   exit path that returns from `main` with a worker alive.

## 3. Post-fix, the same four suites at ngl 99 (tree `a00089d`)

```
arm=real exit=0 running=4 readable=4 mangled=0 passed=15 failed=0 ignored=0 ggml_assert=0 stub_msgs=0 wall=8m50s verdict=GREEN
verdict=GREEN (every arm GREEN; counts cross-checked: running == readable on each)
```

## 4. Post-fix, the other five aborting non-OOM suites at ngl 99

```
arm=real exit=0 running=5 readable=5 mangled=0 passed=22 failed=0 ignored=0 ggml_assert=0 stub_msgs=0 wall=10m51s verdict=GREEN
verdict=GREEN (every arm GREEN; counts cross-checked: running == readable on each)
```

## 5. Post-fix, the three GPU-OOM suites at ngl 99 (§8: still OOM on 8 GB — the question here is only `ggml_assert`)

```
arm=real exit=101 running=3 readable=3 mangled=0 passed=7 failed=2 ignored=0 ggml_assert=0 stub_msgs=0 wall=6m18s verdict=RED (failed=2)
  failed: test seeded_sampling_is_reproducible_across_two_full_runs ... FAILED
  failed: test three_node_kill_s_p_rebuilds_from_durable_boundaries_and_relinks_byte_identical ... FAILED
verdict=NOT-GREEN (at least one arm is INCONCLUSIVE or RED — see the arm lines above)
```

## 6. Full workspace, both arms, default ngl (the seam's regression receipt)

```
arm=real exit=0 running=94 readable=94 mangled=0 passed=392 failed=0 ignored=7 ggml_assert=0 stub_msgs=0 wall=79m44s verdict=GREEN
arm=stub exit=0 running=94 readable=94 mangled=0 passed=392 failed=0 ignored=7 ggml_assert=0 stub_msgs=1 wall=31m32s verdict=GREEN
verdict=GREEN (every arm GREEN; counts cross-checked: running == readable on each)
```

clippy 1.98.0 `-D warnings`: real arm exit=0, stub arm exit=0.

## Verdict

**THE ABORT IS GONE ON EVERY ABORTING SUITE.** Hydra-side lifetime was the cause, not the engine: with the owner dropped (or drained
at exit) the device tears down clean at the pinned engine. **No upstream filing is warranted**
(directive: *only if a leak-free case still aborts*), and none is drafted. #22593 stays as it is.

What this does NOT change: the three GPU-OOM suites remain unverified on Metal for the hardware reason
§8 records; and Metal's KV truncate+replay non-exactness (§7.63) is a separate, characterised fact.
