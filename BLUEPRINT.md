# HYDRA — Master Implementation Blueprint (v0.10.2)
## For an autonomous coding agent. Read this file first; it is the root of the package.

> **Changelog v0.10.1 → v0.10.2** (design-authority review after M−1 passed): (1) §1.3 —
> boundary activation payload precision is now an explicit per-session config `{f32|f16|int8_blockq}`
> with production default `f16`; **golden-token / exact-token-equality tests must run over `f32`
> boundaries, never `f16`**. (2) §1.2 — the v1 supported model family is pinned to the arch families
> whose graph builders carry the M−1 layer-window patch (`llama` + `qwen2`); the patch must be
> re-validated by the spike sweep on every llama.cpp submodule bump, as part of the M2 golden-token
> gate. (3) M−1 DoD text rewritten to state the ratified reading (1e‑3 tests the split mechanism at
> `f32`; payload precisions are characterized separately under item (f)). Rationale: the M−1 FP16
> finding (0.04 logit drift, stable argmax/top‑10 — within spec I8) changed a package default, so it
> paused for ratification per the standing process rule before this amendment.

**What Hydra is:** an open-source, trusted-LAN inference runtime that runs a single large open-weight LLM (70B-class dense, later MoE) by pipeline-sharding it across 2–3 heterogeneous desktop-class machines (CUDA desktop, Apple Silicon Macs, CPU nodes), with crash-safe sessions, exactly-once token semantics, and recoverable generation streams. Phones are **not** workers in v1.

**Package contents and reading order:**

| # | File | Role | Authority |
|---|------|------|-----------|
| 1 | `BLUEPRINT.md` (this file) | What to build, in what order, with acceptance criteria | Governs process & scope |
| 2 | `hydra-session-protocol.md` (v0.10) | **Normative** protocol: messages, state machines, invariants I1–I25 | Governs all correctness behavior. On any conflict, this wins. |
| 3 | `federated-llm-inference-report.md` | Research context: why these choices; landscape; failure modes; scheduler contract (§11 of spec mirrors it) | Rationale; consult, don't re-litigate |
| 4 | `hydra-proto.fbs` + `wal-records.fbs` | **Authoritative** wire + WAL-payload schemas (framing, limits, enums, error codes, evolution rules) | Generated code is the source of truth; no shadow structs |
| 5 | `WAL-FORMAT.md` | **Authoritative** on-disk format: record layout, fsync/dir-sync rules, partial-tail discard, effect IDs, torn-write test contract | Governs `hydra-wal` exactly |
| 6 | `HydraActivationCore.tla` + `BaselineSafety/BaselineLiveness/Mut1–Mut4 .cfg` + `VERIFICATION-README.md` | Machine-checked model of the recovery/activation core + 4 mutation tests (incl. I25 abort finality) | CI gate for any change to transition logic |

**Prime directive for the agent:** the protocol spec's 25 invariants are not documentation — they are the test oracle. Every invariant maps to at least one deterministic-simulation test (Milestone M1) and the implementation is correct exactly when those tests pass under adversarial schedules. Do not "simplify" any transition (activation, reset, abort, supersession) because it seems redundant; nine review rounds and a model checker put each one there, and the TLA+ mutation tests demonstrate what breaks without them.

---

## 1. Fixed decisions (do not re-decide)

1. **Parallelism:** pipeline/layer sharding across machines. Tensor parallelism only *inside* a machine (GPU+CPU co-execution) — never across the LAN. Contiguous layer ranges per stage. Rationale: report Part 2.1.
2. **Compute engine:** `llama.cpp`/`ggml` (MIT), vendored as a git submodule, used as a library. Use `ggml-backend` for the device abstraction (CUDA, Metal, CPU in v1; Vulkan later). Study **prima.cpp** (MIT, arXiv:2504.08791) for its pipelined-ring execution and Halda placement solver; port ideas, and code where license-compatible and clean. **Supported model family (v1, amended v0.10.2):** the dense arch families whose per-arch graph builders carry the M−1 layer-window patch — currently **`llama`** (`src/models/llama.cpp`) and **`qwen2`** (`src/models/qwen2.cpp`). Adding a family means porting the ~47-line window patch to that arch's builder and re-running the spike sweep. **The patch is submodule-version-coupled: every `vendor/llama.cpp` bump MUST re-run the M−1 spike sweep (`spike/shard_split`, all split×prompt combinations) as part of the M2 golden-token gate before the bump is accepted.**
3. **Control plane & networking:** new code, in **Rust** (tokio). Transport behind a trait with two impls: **TCP+mTLS** (default, build first) and **QUIC via quinn** (second); selection per link by measured p95 frame latency. Framing: **FlatBuffers**. **Boundary activation payload precision (amended v0.10.2)** is a per-session config `{f32 | f16 | int8_blockq}`; **production default `f16`** (M−1 measured ~0.04 logit drift with stable argmax/top‑10 — within spec I8 semantic continuity). **`f32` is mandatory for the M2 exact-token-equality test tier: golden-token tests MUST NOT run over `f16` (or `int8`) boundaries.** **`int8_blockq` — RESERVED, not offerable in v1 sessions (amended 2026-07-12).** The naive per-block characterization (symmetric Q8_0-style, QK=32) **failed M2's mixed-backend tolerance** (top-10 8/10 < required 9/10 at mid-network splits) for a *structural* reason — outlier-dominated error (one ~1624 residual dim sets a ~12.8 block scale, crushing sub-6.4 magnitudes in its block), confirmed backend-invariant (a data property, not a kernel one; see `spike/FINDINGS.md`). Re-enabling requires an **outlier-aware scheme** (per-channel scales or outlier-splitting) **with its own characterization clearing ≥ 9/10 at every split** first. The wire schema's `I8_BLOCKQ` dtype stays (append-only evolution; because wire precision is transport-owned, future enablement is a transport change, not a protocol or engine change).
4. **FFI boundary:** Rust worker embeds the C/C++ engine through a narrow `hydra-engine-sys` FFI: `load_shard`, `apply_tokens(range, activations_in) -> activations_out`, `logits()`, `kv_truncate(pos)`, `kv_snapshot/restore` hooks. Keep ALL protocol logic in Rust; the engine only computes.
5. **Topology (v1):** fixed coordinator (best machine, user-designated), 2–3 stage workers, one active session per model instance (spec §1.4 Option A). Client API: OpenAI-compatible HTTP + SSE served by the coordinator.
6. **Durability:** coordinator-local append-only **commit stream** (spec §2.6a: `INITIAL_COMMIT` / `SEGMENT_COMMIT` / `GENERATION_COMMIT` records, BLAKE3-checksummed, partial-tail-discard on open) + control WAL (may share one file with typed records). Durability modes D0/D1 per spec §7; D2 out of v1.
7. **Model format:** GGUF, Q4_K_M reference quantization. A `hydra-modelsvc` tool splits a GGUF into per-stage shard files + signed manifest (per-tensor BLAKE3, tokenizer hash, chat-template hash, inference-config hash). Content-addressed tensor cache is M4, not MVP.
8. **KV cache:** contiguous per-session, `q8_0` default. No paged attention, no TurboQuant in v1 (feature-flag slot reserved).
9. **Security (v1 boundary = one trusted household):** per-device Ed25519 identity, mTLS with a cluster CA created at pairing (QR/PIN), signed placement manifests, hard frame/tensor size caps validated before allocation, API auth token + Host/Origin validation, never bind 0.0.0.0 by default.
10. **Out of scope for this blueprint (do not build):** phones as workers, WAN/NAT traversal, speculative decoding, MoE, beam search, paged KV, multi-session, coordinator election, public swarms. Reserved hooks exist in the spec; leave them as typed-but-unused fields.

## 2. Repository layout

```
hydra/
├── Cargo.toml                  # workspace
├── crates/
│   ├── hydra-proto/            # FlatBuffers schemas + generated code + fence-tuple types,
│   │                           #   position newtypes (InputPos, OutputPos — I13 at type level)
│   ├── hydra-wal/              # commit stream + control WAL: append, fsync, checksum,
│   │                           #   partial-tail discard, replay iterators (spec §1.2, §2.6a)
│   ├── hydra-transport/        # Transport trait; tcp_mtls impl; quic impl; per-link probing
│   ├── hydra-state/            # PURE state machines: coordinator + stage-session +
│   │                           #   activation transaction. No I/O. Mirrors TLA+ actions 1:1.
│   ├── hydra-sim/              # deterministic simulation harness (see §4) driving hydra-state
│   ├── hydra-coordinator/      # binary: sessions, scheduler, commit stream, OpenAI API (axum)
│   ├── hydra-worker/           # binary: engine host, shard cache, telemetry, heartbeats
│   ├── hydra-engine-sys/       # FFI to vendored llama.cpp/ggml
│   ├── hydra-modelsvc/         # GGUF splitter + manifest signer/verifier
│   └── hydra-cli/              # pairing, cluster status, benchmarks
├── vendor/llama.cpp/           # submodule (MIT)
├── verification/               # HydraActivationCore.tla, cfgs, VERIFICATION-README.md
├── docs/                       # the three documents of this package
└── tests/                      # integration + chaos scripts (M2+)
```

**Architecture rule the agent must enforce:** `hydra-state` is pure and synchronous — inputs are (state, event) pairs, outputs are (state′, effects[]). All networking, disk, and engine calls are effects executed by the binaries. This is what makes the DST harness (and the TLA+ correspondence) possible. Any protocol behavior implemented outside `hydra-state` is a defect.

## 3. Milestones, in order, with Definition of Done

### M−1 — Engine feasibility spike (DO THIS FIRST; ≈ agent-days 1–3)
The largest schedule risk is not the protocol — it is whether the narrow llama.cpp/ggml FFI
can support shard-style execution. Before building transport or recovery: one model
(a small GGUF, e.g. 1–3B), two contiguous layer ranges, one or two local processes, CPU
backend first, then CUDA and Metal. Prove, in a throwaway `spike/` directory:
(a) load an arbitrary contiguous layer range only; (b) inject boundary activations into the
first layer of a range and extract them after the last; (c) KV allocated only for assigned
layers; (d) truncate KV at an input position and replay; (e) run the final range to logits
WITHOUT sampling and retain them; (f) round-trip FP16 (and int8+scales) boundary tensors
between backends; (g) tokenizer/RoPE/config metadata identical across both shard loads.
**DoD (ratified reading, v0.10.2):** the 1e‑3 max-abs bar tests the **split mechanism** on a
single backend with the boundary passed at that backend's native precision (**`f32`**): a prompt
applied through shard A → shard B must reproduce unsplit llama.cpp's final logits within 1e‑3
(achieved: **bit-exact, 0.0**, all split×prompt combinations), and KV truncate+replay must
reproduce them (achieved: exact). **Payload precisions (`f16`, `int8`) are characterized
*separately* under feasibility item (f), not gated to 1e‑3** — cross-precision drift is documented
semantic-continuity behavior (spec I8). A one-page findings note records any FFI-boundary changes
needed (`spike/FINDINGS.md`). A failed spike may reshape `hydra-engine-sys`; it must NOT change the
protocol. Only after this note exists may M2 be scheduled. **Status: PASS (ratified).**

### M0 — Skeleton + protocol types (≈ agent-week 1)
Build: workspace; `hydra-proto` by **compiling the provided `hydra-proto.fbs`** (flatc-generated Rust is authoritative — wrap positions in `InputPos`/`OutputPos` newtypes at the API layer so I13 violations don't compile; enforce the schema's hard limits pre-parse); `hydra-wal` implementing **`WAL-FORMAT.md` exactly**, including its §5 torn-write test contract; `hydra-transport` TCP+mTLS with cluster-CA pairing using the schema's framing header.
**DoD:** `cargo test` green; a WAL fuzz test (truncate file at every byte offset, reopen, assert last-complete-record recovery); two processes exchange authenticated framed messages; malformed/oversized frames rejected pre-allocation.

### M1 — State machines + deterministic simulation (the correctness heart; ≈ weeks 2–4)
Build: `hydra-state` implementing, exactly as named in spec v0.10: `BEGIN_RECOVERY` Cases A/B/B′/C, `RESET_RECOVERY_ATTEMPT`, catch-up/rebuild progression, sampler-checkpoint install, the activation transaction (intent → `COMMIT_ACTIVATION` → PREACTIVE → `ACTIVATION_COMPLETE` → `FINALIZE_ACTIVATION` → ACTIVE_FINAL, plus abort and I25 abort-finality), `ACTIVATION_UNSERVABLE` + supersession, `SessionTerminate`, the token ledger with the four watermarks (§2.3), `GENERATION_COMMIT` alignment (I19), emit-after-commit event log with SSE sequence ids, cancellation cutoff (I9).
Build: `hydra-sim` — single-threaded discrete-event simulator: seeded RNG; message drop/duplicate/reorder/delay; crash-restart of any actor at any step; virtual disks with torn-write injection; **an invariant checker asserting I1–I25 after every step** (each invariant is a function over sim state; port them from the spec one-to-one).
**DoD:** (a) 10M+ randomized steps across ≥1,000 seeds with zero invariant violations; (b) every named DST scenario from spec's verification plan implemented as a directed test (mixed-epoch retransmit, zombie survivor, post-decision loss → supersession, reset-after-catch-up, stale INITIAL attempt, torn commit record, sampled-ahead snapshot alignment, uncommitted segment candidate, duplicated SAMPLE_NEXT, cancellation cutoff); (c) **mutation parity:** re-introduce the four TLA+ mutations (no unservable path / label-only reset / no attempt fencing / no abort finality, matching `Mut1–Mut4*.cfg`) behind `#[cfg(feature = "mutation_x")]` and assert the sim *catches each one* — a sim that can't is too weak; (d) CI job runs TLC on `verification/`: `BaselineSafety.cfg` to fixpoint, `BaselineLiveness.cfg` (no symmetry), and `Mut1–Mut4` (baseline runs must report no violation; each mutation must report its designated violation — Mut1 via the `PostDecisionLoss` liveness property).

### M2 — Two-node real pipeline (first tokens; ≈ weeks 5–8)
Build: `hydra-engine-sys` + worker engine host (load shard, apply token ranges, return boundary activations, expose logits at S_P); `hydra-modelsvc`; coordinator wiring `hydra-state` effects to real transport/disk/engine; sampler at S_P (temperature/top-p, Philox RNG with per-token checkpoints, penalty window; grammar deferred); tokenizer + incremental UTF-8-safe detokenizer at coordinator; OpenAI-compatible `/v1/chat/completions` with SSE ids + `Last-Event-ID` resume + `Idempotency-Key`.
Hardware target: `--local-pair` (coordinator + 2 worker processes over real TCP+mTLS on localhost) for development, a containerized two-node pair in CI, and one cloud VM over Tailscale for the real-second-machine data point (see the amended DoD). A 7B–8B GGUF model first, then 70B Q4.
**DoD (original — correctness substance retained):** (a) end-to-end chat with a 7B model split across two nodes, streamed over SSE; (b) two-tier equivalence tests — **exact:** greedy decoding of 20 fixed prompts on a homogeneous deterministic backend (CPU reference; or identical CUDA config) matches single-machine llama.cpp token-for-token; **mixed-backend (CUDA+Metal):** boundary tensors and final logits within documented tolerances, top-k(10) overlap ≥ 9/10 per step over the first 64 steps, and deterministic replay on the same placement reproduces the same tokens (exact token equality across heterogeneous backends is NOT required — spec I8); (c) kill −9 either worker mid-generation → session recovers per D1 (Strategy A) within measured, logged time, resumed stream continues with no duplicated/missing text (SSE id continuity); (d) kill the coordinator mid-generation → restart resumes from commit stream; the client reconnecting with `Last-Event-ID` sees a gapless stream; (e) 70B Q4 runs across the pair; record tok/s honestly.

> **DoD amendment (owner-ratified 2026-07-12 — no second physical machine available).** M2's DoD splits into its two real components; correctness is decoupled from single-host hardware:
> - **(a) Correctness DoD — re-hosted, unchanged in substance.** All correctness demonstrations (kill −9 of either worker with D1 recovery; coordinator crash + `Last-Event-ID` gapless resume; both golden-token tiers) are satisfied by `--local-pair` on the dev machine **plus a containerized two-node integration test in CI** — two containers, **real TCP+mTLS between them**, `docker kill` as the kill −9 mechanism, running on push like the other suites. **Build it as a permanent workflow, not a demo script.**
> - **(b) Real-second-machine credibility + WAN data point.** One cloud-VM run over **Tailscale** (owner provisions when M2 reaches that point — **flag readiness and pause for the VM details**): the same correctness suite plus performance measurement, results annotated **WAN/Tailscale**, **never compared against wired-LAN targets**.
> - **(c) Wired-LAN performance envelope → §8 owed item**, due no later than the **M3 gate** (M3's heterogeneity DoD needs real machines regardless; the hardware decision is formally deferred to M3 planning).
> - **(d) Honesty rule.** PROJECT_STATE's performance table and any README claims carry the "no wired-LAN measurement yet" annotation until the wired run exists — **no LAN numbers implied from local-pair or container runs.**

### M3 — Heterogeneity, scheduler, hardening (≈ weeks 9–12)
Build: startup benchmark (30–120 s sustained per device, EWMA-updated), link prober with **contention-group** discovery (full-mesh probe matrix), Halda-style placement solver (start with exhaustive search over ≤3 stages × layer splits — it's tiny at this scale; HiGHS only if needed), admission control with KV-reservation, §11 stability contract (min placement lifetime, one migration at a time, load-shed ladder, explicit termination), thermal/memory telemetry in heartbeats, chunked prefill, D0 mode, `PAUSED_TOOL` + `SEGMENT_COMMIT` tool-call flow, third node support.
**DoD:** (a) 3-node cluster with deliberately unequal machines picks a placement within 15% of brute-force optimum measured TPOT; (b) chaos suite (network jitter injection via `tc netem`, worker kills, disk-full on WAL) runs the spec's game-day scenarios nightly with zero silent-corruption events (corruption detector = golden-token replay of every recovered session from its ledger); (c) the Thursday-night scenario (background iperf3 load + churn) degrades p99 latency but never correctness.

### M4 — Product hardening (≈ weeks 13–16)
Build: pairing UX (`hydra-cli pair` with QR/PIN), signed shard distribution with staggered downloads, dashboard (read-only web UI on the coordinator), macOS power assertions / Linux systemd units, docs, and the reserved-hook audit (every [RESERVED] spec field exists in `hydra-proto` and is fenced).
**DoD:** a non-author can set up a 3-machine cluster from the README in under 30 minutes; security checklist from report Addendum 2 §E1/D1 passes (no 0.0.0.0 binds, API auth enforced, GGUF parser fuzzed for 24 CPU-hours without crashes).

## 4. Testing doctrine (binding)

1. **Invariants are tests.** `hydra-state` exposes `check_invariants(&SimState) -> Vec<Violation>`; the sim calls it every step; production builds call it in debug assertions at transition boundaries.
2. **Determinism or it didn't happen.** Every sim failure must reproduce from `(seed, schedule)` printed in the failure output.
3. **TLC is the CI gate for transition-logic changes.** Any PR touching `hydra-state`'s activation/recovery/reset code must state which TLA+ action it corresponds to; if it changes semantics, the model changes in the same PR and baseline + mutations rerun.
4. **Golden tokens gate engine changes.** Any llama.cpp submodule bump reruns M2(b).
5. **No test may assert coordinator-side ground truth about remote liveness** (TLC-2): serviceability assertions are evidence-based; stage-side assertions are local.

## 5. Performance targets (honest, from the report)

| Configuration | Target decode | Notes |
|---|---|---|
| 2× desktop-class, wired, 7B Q4 | ≥ 15 tok/s | Sanity target; below this, look for pipeline bubbles |
| Desktop + Mac, wired, 70B Q4 | 2–7 tok/s | Matches prima.cpp-class results |
| Same over good Wi-Fi | 1.5–4 tok/s | Latency-bound; do not chase bandwidth |
| Recovery (Strategy A, 4k ctx, D1) | < 15 s to resumed stream | Measured, logged, asserted in chaos CI |
| TTFT, 4k prompt, 70B | tens of seconds | Chunked prefill; document, don't hide |

These are acceptance thresholds for M2/M3, not marketing numbers. If a target is missed, profile before optimizing: the expected bottlenecks, in order, are per-hop latency, weakest-stage compute, prefill bandwidth.

## 6. Known risks the agent must not "fix" by weakening the spec

- The activation transaction looks heavyweight for a 3-node LAN cluster. It is. It is also the difference between "restarts sometimes duplicate a paragraph" and correctness; every simplification you'll be tempted to make was tried and broken in review rounds 1–9 or by TLC.
- Cross-backend numeric drift (CUDA vs Metal) means recovery onto a different backend changes future tokens. This is *documented semantic-continuity behavior* (spec I8), not a bug; do not add bit-exactness machinery.
- `ggml` APIs move. Pin the submodule; upgrade deliberately with M2(b) as the gate.
- If prima.cpp code is ported, preserve MIT headers and attribute; Cake (evilsocket) is FAIR-licensed — read for ideas, never vendor.

## 7. Verification package status (honest)

`HydraActivationCore.tla` models the v0.10 transition core (26 actions, 6 safety invariants + 3 liveness properties, 3 mutation switches). Status from live checking in this environment: parses and runs under TLC 2.19; **found and fixed TLC-1** (aborted-attempt resurrection — now spec invariant I25); baseline safety explored >1.7M states / depth 31 with zero violations under bounded configs (`Cardinality(msgs) ≤ 20`, small epoch/attempt bounds); mutation 2 confirmed to produce its designed `CaseBPure` violation in 8 states. **Remaining for the agent's CI:** run baseline to fixpoint and mutations 1 (liveness, no symmetry) and 3 on a machine without a 2-minute process ceiling; then keep all four runs as the standing gate per §4.3. The model deliberately covers only the transition core — extend per `VERIFICATION-README.md`'s v2/v3 roadmap as those layers are implemented.
