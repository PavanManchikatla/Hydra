# M3 GATE — evidence table

**Assembled 2026-08-23; row 8 CLOSED the same day on the production-shaped receipt (§7.25 ruling executed, §7.26). Status: rows 1–14 MET; row 15 is a permanently-annotated hardware-contingent non-blocker.**

Format follows `M1-GATE.md` / `M2-GATE.md`. Every row is a receipt or an explicit deferral; a row
that is not green says so, and says why. Rule 16 applies throughout: CI outcomes appear as quoted
`verdict=` lines or receipt files, never as job status.

**Environment reality:** all cloud VMs died 2026-08-05 by plan. Every real-multi-machine result is
**historical/banked** and permanently valid as *demonstrated*; nothing multi-machine can be re-run
without new hardware. Mac + container-CI only from here.

---

## (a) Track-A DoD components

| # | DoD component | Status | Evidence |
|---|---|---|---|
| 1 | **3-node heterogeneous cluster runs** | ✅ **MET (real hardware, banked)** | `THREENODE_WAN_OK` — Mac arm64 `[0,14)` → myVm-2 x86 `[14,21)` → myVm-1 x86 `[21,24)`, all legs WAN/Tailscale, **12/12 argmax agreement** vs the Mac unsplit reference, ~0.81 tok/s. `docs/wan-run.md` |
| 2 | **Gate condition (i)** — full recovery on the direct-FWD topology | ✅ **CLOSED (real hardware)** | `THREENODE_KILL_OK` (`343134c`): real `pkill -9` of myVm-1's S_P → survivors freeze Case A → replacement rebuilt from S2's durable boundaries → S2 re-links → resume. **All three assertions held**; detection→resumed 23.8 s WAN. Middle-stage S2 kill also green and **unconditional** (§7.19 closed) |
| 3 | **Gate condition (ii)** — per-shard weights | ✅ **CLOSED** | P2·10a/b. Signed manifest + splitter determinism on the real dev GGUF; engine shard-load; **rule-14 anchor green with shard-loaded weights** (`shard_anchor.rs`), engine layer `max_abs=0.000e0`. Memory **1202.09 → 601.04 MiB/worker (−50.0 %)** |
| 4 | **Heterogeneity measured, not inferred** | ✅ **MET (real hardware, banked)** | `docs/heterogeneity.md`: 41.5 / 22.0 / 10.4 tok/s → ratio **4.0 : 2.1 : 1.0**. Finding: capability does **not** track RAM/vCPU (a 4 GiB box beat an 8 GB one ~2×) |
| 5 | **Startup benchmark (P2·1)** | ✅ **MET** | `hydra-sched::capability`, 30–120 s sustained + EWMA. Fed P1·2's recorded numbers it reproduces the **4.0 : 2.1 : 1.0** ratio *and* the **deployed 14/7/3 split** exactly |
| 6 | **Link prober + contention groups (P2·2)** | ⚠️ **MET AS LOGIC ONLY** | `hydra-sched::link`: ordered-pair asymmetric matrix, union-find contention closure, conservative unknown-contends bias. **Validated as logic against the recorded topology SHAPE, not against a probed RTT matrix** — that run banked "latency-bound WAN" and "sub-ms VNet" qualitatively and never banked numbers, and none were invented. A real probe matrix needs hardware that no longer exists |
| 7 | **Placement solver (P2·3)** | ✅ **MET** | `hydra-sched::solver`, exhaustive over ordered subsets × contiguous splits, memory-filtered, link-priced. **Optimal by construction**, asserted by independent re-enumeration. Reproduces P1·2's deployed 14/7/3 under objective (b) |
| 8 | **Placement calibration — the restated 15 % clause** | ✅ **MET — 2.9 % worst error against a 15 % gate** | **Receipt: `verification/ci-results/m3-calibration-2026-08-23.md`** (quoted). Measured on the **production-shaped** path per the §7.25 ruling — real `SAMPLE_NEXT`→`SAMPLED` sampling, and **no digest witness**: `Worker::retain_and_ack` now emits the `APPLIED_ACK` `output_checksum` only for a teacher-forced apply, which removed 9.607 ms/token of harness work from the deployed decode loop (§7.26, rule-10 IMPLEMENTATION). **Nothing subtracted from the measurement, nothing extra added to the prediction.** Coefficients independently sourced and **position-matched** to the pipeline's own decode range `[64,136)`: `fixed = 4.714 ms/stage`, `per_layer = 0.5927 ms/layer-tok` from the engine-only windowed decomposition; the three crossings measured **separately** by the zero-inference microbench (`0.463 + 0.431 + 0.154 = 1.048 ms/token`). **`k=12/24`: predicted 24.70 / measured 25.41 → 2.9 %. `k=8/24`: predicted 24.70 / measured 24.65 → 0.2 %** — two out-of-sample splits, setup and prefill excluded by construction, 8 warm-up discarded, 64 steady samples, median. **The §7.24 two-term amendment is sufficient; no third model term, and the model was never tuned.** Anchor path banked as **corroboration** (witness attributed as a harness constant): 0.8 % / 2.1 % |
| 9 | **Admission control + KV reservation (P2·4)** | ✅ **MET** | `hydra-sched::admission`. KV computed from config, shard bytes from the real splitter output, §11 headroom (memory 15–30 % clamped, compute ≥ 20 %), refuses-never-squeezes with named shortfalls, contention-group airtime shared across traffic classes at **min-not-sum** capacity |
| 10 | **§11 stability contract (P2·5)** | ✅ **MET** | `hydra-sched::stability`. 10-min lifetime floor + 60 s window tested **as the anti-flap pair**; one-migration-at-a-time unconditional; hard failure bypasses timing guards but **never** admissibility; termination names the exhausted rungs |
| 11 | **Thermal/memory telemetry (P2·6)** | ✅ **MET, with platform honesty** | The M0 `Heartbeat` fields filled with **provenance**; `Unavailable` carries no value, so a sensorless container reports `Unknown`, never `Nominal`. macOS `soc_temp_dc` is **Unavailable** — no public API, and private SMC access was not taken on |
| 12 | **Chunked prefill (P2·7)** | ✅ **MET, one limitation named** | `INPUT_CHUNK_COMMIT` (WAL id 4); `prefill_stable_pos` advances only post-`fdatasync`; backwards chunks refused. **`chunked_prefill_is_bit_exact_with_unchunked_prefill` green on the rule-14 harness.** Interruption resumes from the durable boundary with every position applied exactly once. **Commit-stall back-pressure now proven** by the row-14 disk-fault arm (`a_stalled_fdatasync_never_advances_the_prefill_watermark`) |
| 13 | **D0 mode (P2·8)** | ✅ **MET** | `DurabilityMode::{D0,D1}`. A latent defect fixed: D0 was still emitting `BOUNDARY_COPY`. **`a_d0_run_emits_no_boundary_copy_traffic_whatsoever` — exactly 0 frames counted at the receiver**, with a D1 control counting exactly 12 so the zero is provably mode-caused |
| 14 | **Zero silent corruption under chaos** | ✅ **MET — BOTH ARMS CLOSED 2026-08-23** | Recovery paths byte-identical to an uninterrupted seeded run at the three-assertion bar (`d1_recovery`, `three_node_recovery`, `survivor_reactivate`, `d1_two_stage`). **(a) JITTER:** `tc netem` arm added to the standing `container-2node` workflow (50 ms ± 20 ms normal delay, 1 % loss, 25 % reorder on `docker0`), re-running the docker-kill recovery and asserting `CONTAINER_2NODE_RECOVERY_OK` **from the log** (rule 16). **VERIFIED BY AN ACTUAL RUN — 32612884434, receipt `verification/ci-results/chaos-jitter-32612884434.md`:** netem genuinely installed (`qdisc netem ... delay 50ms 20ms loss 1% reorder 25% 50%`) and `verdict=GREEN (two-node docker-kill recovery held under tc netem jitter)`. **Container-CI-only and correctness-only, annotated as such** — `tc` needs `NET_ADMIN` + a real netns so the Mac cannot host it, and the arm proves jitter never changes what the pipeline computes, never a latency number. **(b) DISK FAULT:** `chaos_disk.rs`, 5 tests injecting `fdatasync` failure **and** stall at the real `Durability` seam — **a failed durable write never advances a watermark** (prefill/generation/checkpoint, incl. across ten retries so no half-progress creeps), **an unwritable session fails EXPLICITLY per I9** (a silent `Ok` with an unmoved watermark is the forbidden outcome), and recovery resumes from the position that **actually landed**, covering each exactly once. **One suite, three debts** — also closes P2·7's chunk-stall and P2·8's multi-stage D0 kill |
| 15 | **Wired-LAN performance envelope** | 📌 **OWED — hardware-contingent, and NOT a Track-A blocker** (ruled 2026-08-23: hardware-contingent items are phased out of Track-A; this stays a permanently-annotated owed item) | Never measured. `--local-pair` and container-CI prove **correctness only, never LAN numbers**. The honesty rule stands: no LAN number may be implied from a loopback or container run. Needs hardware the project does not have |

---

## (b) Standing anchors — all green at gate time

| Anchor | Result |
|---|---|
| `two_worker_teacher_forced_no_sample_bit_exact` (rule 14) | ✅ green |
| `direct_worker_to_worker_fwd_is_bit_exact` | ✅ green |
| `greedy_sample_across_pipeline_matches_unsplit_argmax` | ✅ green |
| `two_worker_anchor_is_bit_exact_with_shard_loaded_weights` (P2·10b) | ✅ green |
| `chunked_prefill_is_bit_exact_with_unchunked_prefill` (P2·7) | ✅ green |
| Workspace | **252 passed / 0 failed / 7 ignored** (2026-08-23, at flip time); clippy clean |
| M1 (upstream gate) | ✅ **PASSED (FULL)** 2026-08-23 — `verification/ci-results/m1-flip-2026-08-23.md` |

---

## (c) Deferrals carried honestly into the gate

| Item | Why it is deferred, in its own terms |
|---|---|
| **Wired-LAN envelope** | Hardware-contingent; VMs dead 2026-08-05. **Owed, not waived** |
| **Contention groups validated as logic** | The recorded WAN run banked latency-bound/sub-ms **qualitatively**; no RTT matrix exists to validate against, and none was invented |
| ~~Calibration clause~~ | **CLOSED** (row 8) — measured on the production-shaped path at **2.9 %** worst error. What stays annotated: it is **local-pair, loopback, debug profile, 0.5 B, 2 stages**, so it calibrates the cost model's *shape* and implies **no LAN number**. And §7.23's uncomfortable result is unchanged: at this model size on these links the optimal placement is **not to split**, which a 4.7 ms/stage fixed cost only reinforces |
| **`mutation_preactive_maroon`** (6th sim mutation) | Still deferred **with its original reason**: the sim cannot reach the window until **supersession rounds feed the stage track**. Interim coverage is the directed regression `recovery.rs::preactive_stage_reverts_on_begin_recovery`. *Do not contort the scheduler to manufacture the window early* |
| **7B re-measure** | Optional micro-slice, authorized, **not started**. P2·10b changed the arithmetic (≈2.25 GB × 2 on paper) but the paper figure ignores KV, activations and unevenly-split globals — **a re-measure, never a feasibility claim** |
| **70B** | Hardware-contingent; the per-worker ceiling is retired but ~40 GB aggregate still needs machines that do not exist |
| ~~Chunked-prefill commit-stall back-pressure~~ | **CLOSED** by the row-14 disk-fault arm |
| ~~D0 kill on the forwarding topology~~ | **CLOSED** — already covered by `d1_two_stage_kill_s1_...from_tokens...`, which uses NO boundary store at all (Strategy-B on the forwarding topology). Annotated in place rather than duplicated under a `d0_` name |
| ~~Nightly chaos suite~~ | **CLOSED** — both arms built (row 14) |

---

## VERDICT — **M3 TRACK-A: ✅ PASSED**

**Flipped 2026-08-23** under the design authority's delegation: *on the row-8 production-shaped
receipt, with rows 1–14 unchanged, the flip is mechanical and delegated.* Both conditions hold and
are checkable from this file:

* **Row 8 is MET** on its own receipt — `verification/ci-results/m3-calibration-2026-08-23.md`,
  worst error **2.9 %** across two out-of-sample splits against a **15 %** gate, measured on the
  **production-shaped** path (real sampling, no digest witness) exactly as the §7.25 ruling
  specified. Nothing was subtracted from the measurement and nothing extra added to the prediction.
* **Rows 1–7 and 9–14 are unchanged** since the table was assembled, and every one of them still
  points at the receipt it pointed at then.

### What the flip does and does not claim

**Claimed, verbatim, and no further:**

> *A 3-node heterogeneous cluster runs a pipeline-sharded model with per-shard weights and full
> recovery on the direct-FWD topology; capability, links, placement, admission, stability and
> telemetry are measured rather than assumed; the placement cost model predicts measured TPOT
> within **2.9 %** on the production-shaped path; and no chaos arm — worker kill, `tc netem`
> jitter, or disk fault — produces a silent corruption.*

**Not claimed:**

* **No wired-LAN performance number.** Row 15 is owed, not waived (below). The honesty rule stands:
  nothing measured on loopback or in a container may be reported as a LAN figure.
* **No claim that splitting is fast.** §7.23's result is carried into the gate unchanged: for a
  model that fits on one device, on these links, the optimal placement is **not to split**, and
  row 8's 4.7 ms/stage fixed cost makes that hold *a fortiori*. Hydra's claim has always been
  **correctness and running-at-all**, not parallel speedup, and the solver proves it rather than
  asserting it.
* **No claim that the contention-group prober was validated against real numbers.** Row 6 is green
  as **logic only**, and says so.

### Row 15 — the permanent annotation

**Wired-LAN performance envelope: OWED, hardware-contingent, and NOT a Track-A blocker** (ruled
2026-08-23: hardware-contingent items are phased out of Track-A). It was never measured, and after
the cloud VMs died on 2026-08-05 it cannot be measured without hardware the project does not have.
It travels **with** this verdict rather than being cleared by it: **no LAN number may ever be
implied from a loopback or container run**, in this file, in `PROJECT_STATE.md`, or in the README.

### Carried forward into M4

Every deferral in section (c) above is carried, unchanged and in its own terms — `mutation_preactive_maroon`,
the 7B re-measure, 70B, the contention-matrix validation, and now the teacher-forced prefill/rebuild
digest (§7.26). None of them is closed by this flip.

**M4 opens.**
