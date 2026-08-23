# M3 GATE — evidence table

**Assembled 2026-08-23. Status: GATE-READY WITH TWO COMPONENTS OPEN — PAUSED for the owner's verdict.**

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
| 8 | **Placement calibration — the restated 15 % clause** | ⛔ **NOT MET — harness unsound** | §7.23 restated the clause as *measured vs predicted TPOT within 15 % on the same inputs*. `calibration.rs` implements it and **fails for a measurement reason**: the driver folds the one-time mTLS handshake into per-token timing (a 3-token probe reported a 171 ms/token "link"; a cross-pair marginal reported **−50.95 ms/token**). **Needs steady-state token-loop timing inside `pair.rs`.** The assertion is left in place and failing rather than softened. §8 owed |
| 9 | **Admission control + KV reservation (P2·4)** | ✅ **MET** | `hydra-sched::admission`. KV computed from config, shard bytes from the real splitter output, §11 headroom (memory 15–30 % clamped, compute ≥ 20 %), refuses-never-squeezes with named shortfalls, contention-group airtime shared across traffic classes at **min-not-sum** capacity |
| 10 | **§11 stability contract (P2·5)** | ✅ **MET** | `hydra-sched::stability`. 10-min lifetime floor + 60 s window tested **as the anti-flap pair**; one-migration-at-a-time unconditional; hard failure bypasses timing guards but **never** admissibility; termination names the exhausted rungs |
| 11 | **Thermal/memory telemetry (P2·6)** | ✅ **MET, with platform honesty** | The M0 `Heartbeat` fields filled with **provenance**; `Unavailable` carries no value, so a sensorless container reports `Unknown`, never `Nominal`. macOS `soc_temp_dc` is **Unavailable** — no public API, and private SMC access was not taken on |
| 12 | **Chunked prefill (P2·7)** | ✅ **MET, one limitation named** | `INPUT_CHUNK_COMMIT` (WAL id 4); `prefill_stable_pos` advances only post-`fdatasync`; backwards chunks refused. **`chunked_prefill_is_bit_exact_with_unchunked_prefill` green on the rule-14 harness.** Interruption resumes from the durable boundary with every position applied exactly once. **Not proven: commit-stall back-pressure mid-prefill** (§8) |
| 13 | **D0 mode (P2·8)** | ✅ **MET** | `DurabilityMode::{D0,D1}`. A latent defect fixed: D0 was still emitting `BOUNDARY_COPY`. **`a_d0_run_emits_no_boundary_copy_traffic_whatsoever` — exactly 0 frames counted at the receiver**, with a D1 control counting exactly 12 so the zero is provably mode-caused |
| 14 | **Zero silent corruption under chaos** | ⚠️ **PARTIAL** | Every recovery path proven **byte-identical to an uninterrupted seeded run** at the three-assertion bar (`d1_recovery`, `three_node_recovery`, `survivor_reactivate`, `d1_two_stage`), and the container-2node CI runs weekly. **The nightly `tc netem` jitter + disk-full chaos suite is NOT built** — §8 |
| 15 | **Wired-LAN performance envelope** | ⛔ **OWED — hardware-contingent** | Never measured. `--local-pair` and container-CI prove **correctness only, never LAN numbers**. The honesty rule stands: no LAN number may be implied from a loopback or container run. Needs hardware the project does not have |

---

## (b) Standing anchors — all green at gate time

| Anchor | Result |
|---|---|
| `two_worker_teacher_forced_no_sample_bit_exact` (rule 14) | ✅ green |
| `direct_worker_to_worker_fwd_is_bit_exact` | ✅ green |
| `greedy_sample_across_pipeline_matches_unsplit_argmax` | ✅ green |
| `two_worker_anchor_is_bit_exact_with_shard_loaded_weights` (P2·10b) | ✅ green |
| `chunked_prefill_is_bit_exact_with_unchunked_prefill` (P2·7) | ✅ green |
| Workspace | **245 passed / 0 failed**; clippy clean |
| M1 (upstream gate) | ✅ **PASSED (FULL)** 2026-08-23 — `verification/ci-results/m1-flip-2026-08-23.md` |

---

## (c) Deferrals carried honestly into the gate

| Item | Why it is deferred, in its own terms |
|---|---|
| **Wired-LAN envelope** | Hardware-contingent; VMs dead 2026-08-05. **Owed, not waived** |
| **Contention groups validated as logic** | The recorded WAN run banked latency-bound/sub-ms **qualitatively**; no RTT matrix exists to validate against, and none was invented |
| **Calibration clause** | Harness unsound (setup folded into per-token timing). Named precisely; assertion left failing |
| **`mutation_preactive_maroon`** (6th sim mutation) | Still deferred **with its original reason**: the sim cannot reach the window until **supersession rounds feed the stage track**. Interim coverage is the directed regression `recovery.rs::preactive_stage_reverts_on_begin_recovery`. *Do not contort the scheduler to manufacture the window early* |
| **7B re-measure** | Optional micro-slice, authorized, **not started**. P2·10b changed the arithmetic (≈2.25 GB × 2 on paper) but the paper figure ignores KV, activations and unevenly-split globals — **a re-measure, never a feasibility claim** |
| **70B** | Hardware-contingent; the per-worker ceiling is retired but ~40 GB aggregate still needs machines that do not exist |
| **Chunked-prefill commit-stall back-pressure** | Needs a chunk-boundary-aware driver in `pair.rs` |
| **D0 kill on the forwarding topology** | Strategy-B is proven in the D0-class full-range shape; the multi-stage D0 combination is untested |
| **Nightly chaos suite** | `tc netem` jitter + disk-full-on-WAL not built |

---

## Verdict sought

**M3 Track-A is complete except rows 8, 14 and 15.** Rows 6 and 8 are the two where the project's
own standard says the honest answer is "not yet": one because the hardware to validate against is
gone, one because my measurement harness is wrong and I would rather report that than pass a gate by
lowering its bar.

**PAUSED at the gate for the owner's verdict.**
