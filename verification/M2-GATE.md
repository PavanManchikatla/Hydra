# M2 Gate Evidence — Two-node real pipeline

> Mirrors the shape of `M1-GATE.md`. Every component of the **amended M2 DoD** (BLUEPRINT §3 +
> the 2026-07-12 owner amendment) with evidence pointers (test names / commits / CI runs),
> deferrals named, and the honesty annotations carried. **Hardware reality:** the dev box is an
> 8 GB Apple-Silicon laptop (PROJECT_STATE §9); the correctness DoD is met on a small model
> (`Qwen2.5-0.5B` fp16) + `--local-pair` + containerized CI + a Tailscale WAN VM, per the amendment.

## Status: components (a)–(d) MET on the amended terms; (e) 70B + the 7B split are hardware-contingent (named below). Awaiting the owner's gate decision.

---

## (a) End-to-end generation, two-node split, streamed over SSE

| Piece | Status | Evidence |
|---|---|---|
| OpenAI `/v1/chat/completions` + SSE, dense ids, `Last-Event-ID` resume, `Idempotency-Key` | ✅ | `hydra-coordinator` `session.rs`/`server.rs`/`event_log.rs`; `tests/session_http.rs` (7 tests) — emit-after-commit **proven by absence**, byte-identical `Last-Event-ID` resume at every cut |
| Two-node split pipeline (S1 `[0,k)` → S_P `[k,-1)` + sampler) | ✅ | `--local-pair` two-worker teacher-forced anchor `tests/local_pair.rs::two_worker_teacher_forced_no_sample_bit_exact`; **worker→worker DIRECT FWD** `tests/local_pair.rs::direct_worker_to_worker_fwd_is_bit_exact` |
| Sampler @ S_P (Philox, snapshots, I14/I15/I17) | ✅ | `tests/sampler_pipeline.rs` (greedy==argmax bit-exact; seeded reproducible; SAMPLE_NEXT idempotent; INSTALL round-trip) |
| Tokenizer + UTF-8-safe incremental detok (I6) | ✅ | `hydra-tokenizer` (delegated to llama.cpp; `Utf8Streamer`); incremental==batch over emoji-split, round-trip==reference |
| **7B model end-to-end** | ⛔ **hardware-contingent (aborted, memory)** | The engine loads full model weights per worker (windowed compute); two in-process workers each load the whole model → 7B Q4 ≈ 4.5 GB × 2 = 9 GB > 8 GB. **Aborted without ceremony** (owner-sanctioned memory-check). Proven on 0.5B; the pipeline is model-agnostic. → §8 owed (real M2 hardware) |

## (b) Two-tier equivalence

| Tier | Bar | Status | Evidence |
|---|---|---|---|
| **Exact** (same backend, greedy) | split == unsplit **bit-exact** | ✅ | `two_worker_teacher_forced_no_sample_bit_exact` (BLAKE3 digest equality); `sampler_pipeline::greedy_sample_across_pipeline_matches_unsplit_argmax`; spike Check C (max_abs 0.0) |
| **Mixed-backend** (cross-arch) | top-k / same-tokens under tolerance (I8; bit-exactness NOT expected) | ✅ | **WAN run: 12/12 greedy argmax agreement** arm64 S1 ↔ x86-64 S_P over Tailscale (`docs/wan-run.md`); labeled mixed-tier; deterministic replay IDENTICAL |

## (c) Kill −9 a worker mid-generation → D1 recovery, no dup/missing text, SSE id continuity

| Scenario | Status | Evidence |
|---|---|---|
| Single full-range S_P (D0-class), 3 adversarial kill −9 windows, byte-identical | ✅ | C-part-2 flagship `tests/d1_recovery.rs::d1_recovery_three_kill_windows_are_byte_identical_to_an_uninterrupted_seeded_run` (Steady / SampledAhead=I7b/I15 / BetweenFsyncAndEmit); real subprocess `kill -9` |
| **Two-stage D1** (kill EITHER worker; survivor Case-A freeze; failed stage rebuilt) | ✅ | `tests/d1_two_stage.rs` — kill S_P (replacement rebuilt from **durable BOUNDARIES**), kill S1 (rebuilt from tokens, S_P survivor); byte-identical, disk-truth, continuity |
| BOUNDARY_COPY durability + R3′ release (D1 substrate) | ✅ | `hydra_worker::retain::R3Buffer` + `hydra_coordinator::BoundaryStore`; `tests/boundary_durability.rs` — a recovery-needed boundary is never released before `DURABILITY_ACK` |
| **Real `kill -9` of a TWO-node pipeline in CI** (`docker kill`) | ✅ **GREEN** | `.github/workflows/container-2node.yml` (permanent) + `bin/hydra-2node-ci.rs`; two containers, real mTLS, `docker kill`; semantic gate `CONTAINER_2NODE_RECOVERY_OK` verified in the log — run [29705614366](https://github.com/PavanManchikatla/Hydra/actions/runs/29705614366) |
| SSE id continuity / disk truth (I19, no position twice) | ✅ | `hydra-coordinator/tests/d1_recovery.rs` (CI-safe) + `recovery::verify` in every kill-window test |

## (d) Kill the coordinator mid-generation → restart resumes from the commit stream; `Last-Event-ID` gapless

| Piece | Status | Evidence |
|---|---|---|
| Durable commit stream (real `hydra-wal`; INITIAL/GENERATION_COMMIT; I19 on write) | ✅ | `hydra-coordinator/commit_stream.rs`; `tests/commit_stream.rs` |
| Recovery reader — reconstruct from the durable ledger alone (I3); no position twice | ✅ | `hydra-coordinator/recovery.rs::{read, verify}`; `tests/d1_recovery.rs` |
| `Last-Event-ID` byte-identical resume (event log = pure function of the durable prefix) | ✅ | `tests/session_http.rs::last_event_id_resume_yields_byte_identical_suffix`; `event_log.rs` unit test (every cut point) |

## (e) 70B Q4 across the pair — record tok/s honestly

| Status | Note |
|---|---|
| ⛔ **hardware-contingent** | 70B Q4 ≈ 40 GB — impossible on the 8 GB dev box. Needs real M2/M3 hardware. → §8 owed item (real-hardware run). |

## Amended-DoD components (owner 2026-07-12)

| Component | Status | Evidence |
|---|---|---|
| `--local-pair` (coordinator + 2 workers, real TCP+mTLS on localhost) | ✅ | `hydra-local-pair` runner + `local_pair.rs` (incl. real subprocess `kill -9`/restart) |
| **Containerized two-node CI** (`docker kill` = kill −9, permanent workflow) | ✅ **GREEN** | seam 4 — `container-2node.yml` (run 29705614366; semantic line `CONTAINER_2NODE_RECOVERY_OK` present) |
| **Tailscale VM** real-second-machine + WAN data point | ✅ | `docs/wan-run.md` — Mac ↔ Azure VM over Tailscale; 12/12 mixed-tier, ~1.12 tok/s WAN, WAN kill-window byte-identical (det→resumed 25.4 s) |
| Wired-LAN performance envelope | 🕗 **owed — M3 gate** | Honesty rule: no wired-LAN number implied by local-pair / container / WAN runs (§8) |
| **Honesty rule (d)** | ✅ carried | Every perf figure annotated (WAN/Tailscale, in-process local, cross-arch mixed-tier) and never compared to wired-LAN targets |

## Deferrals / owed (named, per the honesty discipline)

- **7B split** — aborted on the memory-check (8 GB box; two full-model loads). → real M2 hardware.
- **70B Q4** — hardware-contingent. → real M2/M3 hardware.
- **Wired-LAN perf envelope** — owed at the M3 gate.
- **Multi-node split-stage FWD recovery with boundary transport at scale** — the two-stage demo (seam 3) proves the machinery in-process; the containerized/real-hardware scale run rides with M3.
- **M1 full flip** — still CI-gated on honest TLC verdicts (rule 16); independent of M2 (§0b, §7.16).
