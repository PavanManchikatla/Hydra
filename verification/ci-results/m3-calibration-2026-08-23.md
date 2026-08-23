# M3 gate row 8 — the calibration receipt (PRODUCTION-SHAPED path)

**Date:** 2026-08-23 · **Machine:** dev Mac (Apple M2, 8 GB), CPU backend, **debug profile throughout**
· **Model:** `models/qwen2.5-0.5b-instruct-fp16.gguf` (24 layers, `n_embd` 896, `n_vocab` 151 936)
· **Ruling executed:** PROJECT_STATE §7.25 — *the calibration gate measures the production-shaped
path (real sampling, no digest witness)*.

Rule 16: this file is the evidence. The numbers below are quoted verbatim from the test's own
stdout; nothing is restated from a job status or a green check-mark.

---

## What the gate asks

§7.23 restated the M3 DoD's 15 % clause: enumeration is optimal by construction (independently
asserted in `hydra-sched::solver`'s tests), so comparing the search against itself is vacuous. The
residual check is **calibration** — *the deployed placement's **measured** TPOT must land within
15 % of the solver's **predicted** TPOT on the same measured inputs.* The gate tests the **cost
model against reality**.

## The model being calibrated (§7.24, ratified)

```
TPOT = Σ_stages (fixed_i + layers_i · per_layer_i) + Σ_crossings (protocol + rtt + bytes/throughput)
```

On loopback the `rtt + bytes/throughput` term is below the noise floor and was **not invented**.

## Coefficient provenance — every one sourced independently of what it predicts

| Coefficient | Value | Where it comes from | Why it cannot be a residual |
|---|---|---|---|
| `fixed` | **4.714 ms/stage** | windowed-context decomposition, `calibration.rs::decompose` | timed through the **engine alone** — no pipeline, no mTLS, no coordinator |
| `per_layer` | **0.5927 ms/layer-tok** | same decomposition | same |
| crossing **A** (`APPLY_TOKEN` out / `FWD` back) | **0.463 ms** | `protocol_microbench.rs`, echo peer with **zero inference** | no model in the loop at all |
| crossing **B** (`FWD` out / `APPLIED_ACK` back) | **0.431 ms** | same | same |
| crossing **C** (`SAMPLE_NEXT` out / `SAMPLED` back) | **0.154 ms** | same | same |

Crossing **C** is measured **separately**, not assumed equal to A/B: it carries no `n_embd`
boundary, and it is measurably ~2.9× cheaper. Reusing one number for all three would have priced
the cheapest crossing as if it were the most expensive.

**Position matching.** Per-token decode cost grows with context depth. The decomposition therefore
runs over **exactly the positions the pipeline decodes at** — `[64, 136)` — rather than near zero.
Otherwise the prediction would be systematically low for a *measurement* reason and the model would
have been blamed for it.

## Measurement hygiene (P2·1's standard, applied)

* connection setup **excluded by construction** — both mTLS connections and the whole 64-token
  prompt prefill complete **before the first timer starts** (`pair::time_generation_pipeline`);
* **8 warm-up tokens discarded**, **64 steady samples** kept;
* **median**, never mean — one preemption spike cannot drag the estimate;
* every timed step is a **complete decode cycle** (`SAMPLE_NEXT` → `SAMPLED` → feed the sampled
  token back through both stages), so what is reported is TPOT and nothing else;
* **two out-of-sample splits** — `k = 12/24` and `k = 8/24` — predicted from coefficients derived
  from a configuration that is not a pipeline at all.

## What makes this path production-shaped

1. **Real sampling.** The token comes from S_P's `SAMPLED` reply to a real `SAMPLE_NEXT`, fed back
   autoregressively. The coordinator holds no sampler state (spec §1.4).
2. **No digest witness.** `Worker::retain_and_ack` now emits the `APPLIED_ACK` `output_checksum`
   **only for a teacher-forced (`NO_SAMPLE`) apply**. On a decode apply the coordinator's token
   comes from `SAMPLED`, nothing in the protocol reads the field, and hashing 151 936 floats there
   was 9.607 ms/token of pure cost. See §7.26.

**Nothing is subtracted from the measurement and nothing extra is added to the prediction.**

---

## RESULT — quoted verbatim

```
COEFFICIENTS (windowed-context decomposition — engine only, no pipeline)
  positions [64,136) · full 18.94 · lower 11.85 · upper 11.80 ms/tok
  fixed = 4.714 ms/stage · per_layer = 0.5927 ms/layer-tok
  protocol = 0.463 (A) + 0.431 (B) + 0.154 (C) = 1.048 ms/token (independent microbench)
PRODUCTION SPLIT k=12/24: predicted 24.70 ms/tok  measured 25.41 ms/tok  residual +0.71  ERROR 2.9 %  [64 steady samples after 8 discarded, 64-token prompt]
PRODUCTION SPLIT k=8/24: predicted 24.70 ms/tok  measured 24.65 ms/tok  residual -0.06  ERROR 0.2 %  [64 steady samples after 8 discarded, 64-token prompt]
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 34.40s
```

**Worst error across the two out-of-sample splits: 2.9 %, against a 15 % gate. Row 8 is MET.**

Test: `crates/hydra-worker/tests/calibration.rs::production_shaped_tpot_is_within_15_percent_of_the_solver_prediction`
(`#[ignore]` — needs the engine and the dev model; run explicitly for the gate).

---

## Corroboration — the anchor path, banked as secondary

The same model and the same decomposition method applied to the **bit-exact anchor** path
(`run_teacher_forced_pipeline`), whose final stage *does* pay the logits witness. The witness is
attributed as a **harness** term added to the prediction of the harness path — **never** to the
production model.

```
COEFFICIENTS (windowed-context decomposition — engine only, no pipeline)
  positions [0,136) · full 18.21 · lower 11.66 · upper 11.44 ms/tok
  fixed = 4.889 ms/stage · per_layer = 0.5551 ms/layer-tok
ANCHOR SPLIT k=12/24: predicted 33.60 ms/tok  measured 33.34 ms/tok  residual -0.26  ERROR 0.8 %  [model 23.99 + harness witness 9.61]
ANCHOR SPLIT k=8/24: predicted 33.60 ms/tok  measured 32.91 ms/tok  residual -0.69  ERROR 2.1 %  [model 23.99 + harness witness 9.61]
```

Test: `calibration.rs::anchor_path_tpot_is_within_15_percent_when_the_harness_witness_is_attributed`.

**Why the corroboration is worth banking:** one set of coefficients, derived from a configuration
that is neither of them, predicts **two differently-shaped paths** — the production decode loop
(three crossings, sampler, no witness) and the anchor harness (two crossings, no sampler, witness).
That is stronger evidence for the cost model's *shape* than either result alone.

**An unforced cross-check that fell out of it.** The two paths were measured minutes apart on the
same box: production **24.65 / 25.41** ms/tok versus anchor **32.91 / 33.34** — a gap of ~7.9–8.3
ms/token. That is what §7.25 predicted the digest witness was worth (9.607 ms), net of the third
crossing (+0.154) and the sampler's own per-token work that the production path adds and the anchor
path does not. The two measurements agree without having been made to.

---

## Honest annotations carried with this receipt

* **Local-pair, loopback mTLS, one Mac.** This calibrates the cost model's **shape**. It is not a
  wired-LAN number and no LAN number may be implied from it — the wired-LAN envelope stays a
  hardware-contingent owed item (gate row 15).
* **Debug profile** for every coefficient and every measurement, so all of them share one build.
  A release build would move the absolute numbers; the calibration is the *relationship* between
  prediction and measurement, and both sides moved together by construction.
* **0.5 B model, 2 stages.** §7.23's uncomfortable result still stands and is not softened by this
  receipt: at this model size on these links, **not splitting is the optimal placement**, and a
  fixed per-stage cost of 4.7 ms makes that hold *a fortiori*.
* The `#[ignore]` attribute is deliberate — the test needs the engine and a git-ignored model, so
  it cannot run in the standing CI. It is a **gate-time, run-explicitly** measurement, and this
  file is how its result enters the record.
