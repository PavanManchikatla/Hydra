# PROJECT_STATE.md — Hydra Living Status Document

> **What this file is.** The single, always-current narrative of the Hydra project: what it is, what has happened, what is true right now, what is owed, and what happens next. It is written so that any human or AI agent can read this one file and act correctly without archaeology through commits or chat logs.
> **Update rule (binding on the coding agent):** this file is updated in the *same commit* as any milestone progress, gate decision, finding, verification result, or package amendment. A commit that changes project reality without updating this file is a process defect. Each update appends to §12 (changelog) with date + summary.

**Last updated:** 2026-07-11 · **Package version:** v0.10.2 · **Phase:** M0 ratified → **M1 in progress** (state machines + deterministic simulation) · **Repo:** github.com/PavanManchikatla/Hydra

---

## 1. Project identity

**Hydra** is an open-source, trusted-LAN inference runtime that runs a single large open-weight LLM (70B-class dense; MoE later) by **pipeline-sharding contiguous layer ranges across 2–3 heterogeneous desktop-class machines** (CUDA desktop, Apple Silicon Macs, CPU nodes). Its differentiator is not speed — physics caps a 70B at roughly 2–7 tok/s on wired desktop hardware — but **correctness under failure**: crash-safe sessions, exactly-once token semantics, teacher-forced recovery, and generation streams that survive any single machine dying mid-sentence without duplicating or losing a single visible token.

Phones are **not** workers in v1 (clients/draft-hosts only, later). One active session per model instance. WAN, MoE, speculative decoding, paged KV, and untrusted swarms are reserved hooks, not v1 scope.

The project was produced through an unusual pipeline worth knowing: research report → protocol spec hardened across **nine adversarial expert-review rounds** (v0.1→v0.9, each round finding real transition-boundary bugs) → **TLA+/TLC model checking** (which found a tenth defect that all nine human rounds missed) → an implementation blueprint executed by an autonomous coding agent, gated at every milestone by a design authority. The layering rule that emerged is doctrine: *every defect is caught by a more mechanical layer than the one that created it* (prose → spec → model checker → simulator → chaos tests). Defend this layering against any urge to shortcut it.

## 2. Authority & document map (reading order for a new agent/human)

| Order | File | Role | Authority |
|---|---|---|---|
| 0 | `PROJECT_STATE.md` | You are here — current truth | Narrative authority |
| 1 | `BLUEPRINT.md` | What to build, order, DoD gates, fixed decisions | Process authority; may not be re-decided by an agent |
| 2 | `docs/hydra-session-protocol.md` (v0.10.1 + v0.10.2 amendments) | Messages, state machines, **invariants I1–I25** | **Correctness authority — on any conflict, this wins** |
| 3 | `docs/hydra-proto.fbs` + `docs/wal-records.fbs` (crate build copy in `crates/hydra-proto/schemas/`) | Wire + WAL payload schemas | Generated code is source of truth; shadow structs forbidden |
| 4 | `docs/WAL-FORMAT.md` | On-disk format, fsync rules, torn-write contract | Governs `hydra-wal` byte-for-byte |
| 5 | `verification/` (TLA+ model + 6 configs + README) | Machine-checked transition core + mutation tests | CI gate for any transition-logic change |
| 6 | `docs/federated-llm-inference-report.md` | Research rationale, landscape, feasibility | Consult for *why*; don't re-litigate |

**Standing process rules:** (a) an agent proceeds autonomously *within* a milestone; a human gates *between* milestones via each DoD; (b) any finding that changes a package decision (defaults, formats, scope) **pauses for ratification** before the next milestone; findings that don't, proceed and are logged here; (c) verification deviations **escalate with traces** — the model/spec is never adjusted to make a run pass; (d) **commit attribution:** the **owner is the primary author + committer on every commit** (repo-local `user.name`/`user.email` = owner's GitHub noreply); every commit ends with `Co-Authored-By: Claude <noreply@anthropic.com>`; **git history rewrites are prohibited** (one authorized rewrite was done 2026-07-11 to backfill attribution; never again); (e) **push cadence:** small logical commits, conventional messages, **push after every commit** — local never leads remote by more than a day; (f) **TLC runs locally are smoke-only** (parse, Mut2, Mut4); baseline/liveness/Mut1/Mut3 are **CI-owned** (`.github/workflows/tlc.yml`) — thermal policy, §9.

## 3. Status snapshot (as of last update)

- **Gates passed:** M−1 ✅ (ratified) · M0 ✅ (ratified) · v0.10.2 amendments applied & committed.
- **Active:** M1 in progress. `hydra-state` **slice 1 landed**: the coordinator activation transaction (spec §6.6) — intent→commit→complete→finalize→serviceable, abort reversal (I21), the **I25 abort-finality** guard, and phase-specific restart (§6.5); pure `(state,event)→(state′,effects[])`, effect-IDs per WAL-FORMAT §4; executable invariants I25/I16/I10a; **Mut4 mutation parity demonstrated** (checker catches the I25 violation under `--features mutation_no_abort_finality`). Next slices: stage-session SM, recovery Cases A/B/B′/C + reset, unservable/supersession, watermarks/ledger, then `hydra-sim` (10M-step DST). TLC gate re-running (see §6).
- **Blockers:** none. **Awaiting owner decision:** file the drafted upstream llama.cpp issue (public GitHub action) — the M0-review ruling left the bracket unfilled, so it is **held** pending an explicit yes.
- **Workspace health:** `cargo test --workspace` green (**32 tests** across `hydra-proto`, `hydra-wal`, `hydra-transport`, `hydra-state`; +1 under the Mut4 feature build); spike reproduces from clean build.

## 4. Milestone ledger

| Milestone | Scope | Status | Evidence |
|---|---|---|---|
| **M−1** Engine feasibility spike | Prove shard execution over llama.cpp via narrow FFI | ✅ **PASS, ratified** | F32 split **bit-exact (0.0)** vs unsplit across 15 combos (k∈{1,4,12,18,23}×3 prompts), CPU **and** Metal; KV truncate+replay exact; boundary extraction == true `l_out-{k-1}`; 47-line per-arch patch (`spike/llama-cpp-layer-window.patch`); `spike/FINDINGS.md`. Commits `a863d53` (spike PASS), `04000ae` (Metal close-out), `0174a7f` (v0.10.2 ratify) |
| **M0** Skeleton + protocol types | proto / wal / transport crates | ✅ **MET, ratified** | `hydra-proto` (generated schemas authoritative, hard limits pre-alloc, HYFR+BLAKE3 framing, InputPos/OutputPos newtypes, I19 validator); `hydra-wal` (WAL-FORMAT exact + full §5 contract: truncate-every-offset, bit-flip, crash-during-rotation, I19 crash-window); `hydra-transport` (TCP+mTLS, cluster CA, rogue-CA rejected at handshake). Commits `83d82f5` (proto), `98e507e` (wal), `08afc27` (transport); 28 tests green |
| **M1** State machines + DST | `hydra-state` pure; `hydra-sim`; I1–I25 as executable checks; mutation parity; TLC suite green | 🔧 **IN PROGRESS** (slice 1 of ~5) | Slice 1: coordinator activation transaction + I25 guard + §6.5 restart; effect-IDs; I25/I16/I10a checks; **Mut4 parity** green. DoD (remaining): stage SM, recovery/reset/unservable/watermarks; `hydra-sim` 10M+ steps / ≥1,000 seeds zero violations; all directed scenarios; **4** sim mutations caught (Mut4 done); TLC green across all 6 configs |
| **M2** Two-node real pipeline | engine host, sampler@S_P, tokenizer/detok, OpenAI+SSE API; 7B then 70B | ⛔ gated on M1 | Two-tier equivalence: exact (homogeneous backend) + mixed-backend tolerances; kill −9 recovery; coordinator-restart resume |
| **M3** Heterogeneity + scheduler + chaos | benchmarks, contention-group prober, placement solver, §11 stability, chunked prefill, 3rd node | ⛔ gated | nightly chaos with zero silent corruption (golden-token replay detector) |
| **M4** Product hardening | pairing UX, shard distribution, dashboard, docs, security checklist | ⛔ gated | non-author setup < 30 min |

## 5. Repository map (what belongs in git — and what never does)

```
hydra/
├── PROJECT_STATE.md            # this file (root)
├── BLUEPRINT.md
├── README.md                   # project summary + entry pointer to this file
├── docs/                       # protocol spec, research report, WAL-FORMAT.md, *.fbs schemas
├── verification/               # .tla + 6 .cfg + VERIFICATION-README + run-gate.sh (results/ git-ignored)
├── crates/                     # hydra-proto | hydra-wal | hydra-transport | (M1:) hydra-state, hydra-sim
├── spike/                      # M−1 sources, CMake, llama-cpp-layer-window.patch, FINDINGS.md,
│                               #   upstream-llama-issue.md, sweep scripts + result logs
├── vendor/llama.cpp            # PINNED SUBMODULE (13f2b28b) — a pointer, not 200MB of source
└── scripts/                    # regen-proto.sh, tlc runners
```

**NEVER committed (this is why the folder is ~14 GB while the repo is a few MB):** `vendor/llama.cpp` *contents* beyond the submodule pointer; any `build/` trees; `*.gguf` models; TLC `states/` metadirs and `tla2tools.jar`; `target/`; generated `.rs` only if the regen script is deterministic (current decision: generated code **is** committed for reviewability; the regen script must reproduce it byte-identically).

## 6. Verification status (the certification ledger)

**Claim currently permitted (do not overstate):** *bounded-model-checked transition core; one real protocol defect found and fixed via model checking; mutations 1, 2, 4 confirmed firing as designed; baseline safety deep-clean and running to fixpoint; baseline liveness queued; mutation 3 still hunting its counterexample.*

| Run | Config | Result |
|---|---|---|
| Defect hunt (pre-fix) | baseline | **TLC-1 FOUND** (~13 s): crash-after-durable-ABORT resurrected & completed an aborted activation via stale acks → spec invariant **I25**, model guard, Mutation 4 |
| Baseline safety | `BaselineSafety.cfg` | Deepest local run reached **98M states / 26.6M distinct / depth 143, zero violations**, then **SIGTERM** (thermal policy — *not* a drain, no fixpoint). **CI-owned now** (`long` job → fixpoint, checkpointed). |
| Baseline liveness | `BaselineLiveness.cfg` | **CI-owned** (`long` job); expected green under fairness (v0.10.1 per-(stage, message-class) WF incl. ABORT; coordinator progress actions fair) |
| Mut1 (no unservable path) | `Mut1Unservable.cfg` | ✅ **fired as designed**: `PostDecisionLoss` liveness violation, 18-state stuttering lasso |
| Mut2 (label-only reset) | `Mut2Reset.cfg` | ✅ fired: `CaseBPure` violated, 8-state trace |
| Mut3 (no attempt fencing) | `Mut3AttemptFence.cfg` | **CI-owned** (`long` job). All local runs ended by **SIGTERM** (thermal; deepest ~17M distinct/depth 102, no violation — *not* drains). Expected `ServiceSafety`/`TupleSafety` violation via a stale INITIAL attempt. **Contingency triggers only on a CI run that completes clean:** (a) `\|msgs\| ≤ 30`; (b) then `MaxAttempt = 3`; (c) then escalate with stats — never adjust model logic |
| Mut4 (no abort finality) | `Mut4AbortFinality.cfg` | ✅ fired: I25 violation, 14-state trace reproducing TLC-1 |

**Model-quality findings on record:** TLC-2 (naive global "SERVICEABLE ⇒ all stages ACTIVE_FINAL" is unsatisfiable under asynchrony — properties are evidence-based + tuple-safety on live gen-matching stages); TLC-3 (`SessionTerminate` arrow required); fairness completeness patch (omitted `StageRecvAbortAt` WF would have starved a PREACTIVE stage — a fairness artifact that would masquerade as a protocol liveness bug).

## 7. Findings & decisions log (chronological; the "why is it like this" record)

1. **Architecture (report):** pipeline sharding across LAN; TP only inside a machine/fast island — TP's ~160 collectives/token is infeasible over Wi-Fi/Ethernet latency. prima.cpp (ICLR 2026) is the closest prior art and the placement-algorithm reference.
2. **Protocol hardening v0.1→v0.9:** nine review rounds; every defect lived at a *transition boundary* (start, convergence, completion, state transfer, activation edge). Key machinery each round forced: token ledger + four watermarks; exactly-once via context-scoped `applied_pos`; input-vs-output position typing (I13); execution contexts/shards (I12); idempotent three-case BEGIN_RECOVERY (I11); restorable sampler snapshots + installation (I15/I17); atomic `GENERATION_COMMIT` (I19); PREACTIVE + stage-visible finalization (I20/I21); unservable-supersession closing the 2PC blocking hole (I22); truncating attempt reset (I23); candidate isolation (I24); unified INITIAL/RECOVERY activation with `activation_attempt_id`.
3. **TLC-1 → I25 (v0.10):** aborted activation attempts are permanently dead; restart classifies durable ABORT as attempt-terminal. Found by machine, missed by all nine human rounds — the justification for the layering doctrine.
4. **v0.10.1 package hardening:** six named TLC configs incl. Mut4; I25 made normative everywhere; authoritative `hydra-proto.fbs` + `WAL-FORMAT.md` (+ later `wal-records.fbs` with `INPUT_CHUNK_COMMIT` advancing `prefill_stable_pos`; version encoding `0x0100`); per-message-class fairness; M2 acceptance split into exact-tier vs mixed-backend-tier.
5. **M−1 spike findings (agent):** (a) llama.cpp already exposes boundary *extraction* (`t_layer_inp`/embeddings path) and *injection* (`batch.embd`); only loop-windowing needed patching — 47 lines, **per-architecture** (each model family has its own graph builder). (b) Two upstream bugs isolated: embeddings-context defaults to non-causal attention; `inp_out_ids` dangles when a shard isn't the final layer. (c) **The FP16 finding:** split mechanism is exact; *all* deviation is boundary-payload precision. Residual-stream massive activations (~1560, the attention-sink phenomenon) mean FP16's ulp ≈ 1.0 there → ~0.04 logit max-abs (argmax/top-10 stable). Not a bug; arithmetic.
6. **v0.10.2 ruling (design authority):** payload precision is per-session `{f32|f16|int8_blockq}`; **f16 production default** (within I8 semantic continuity); **f32 mandatory for golden-token/exact test tier** (FP16 boundaries would make that gate flaky); v1 model families pinned to the patched builders (llama + qwen2); spike sweep re-validation required on every submodule bump; M−1 DoD text ratified to the mechanism-at-F32 reading.
7. **Metal close-out:** F32 split-vs-unsplit bit-exact on Metal too (same-backend kernels — proves mechanism, *not* cross-backend equivalence, which remains M2's mixed tier); FP16 boundary cost ~4× lower on Metal (0.003–0.014) — accumulation-order artifact, noted, no action.
8. **Process precedent set:** the FP16 default change paused for ratification (retroactively at first occurrence; explicit rule thereafter).

## 8. Owed work & open risks

| Item | Owner | Due |
|---|---|---|
| **int8+blockscale boundary characterization** (item f, half-done). int8 boundaries **forbidden** until measured — the ~1560 outlier makes this a real test | agent | M2 prep |
| Upstream llama.cpp layer-window issue: **drafted** (`spike/upstream-llama-issue.md`), filing awaits owner sign-off | owner decision | — |
| TLC: baseline-safety fixpoint; baseline-liveness; Mut1; **Mut3 fire-or-contingency** | **CI-owned** (`.github/workflows/tlc.yml` `long` job) | M1 DoD (d) |
| Per-arch engine patch = maintenance liability; re-validate on every submodule bump; retire when/if upstream lands a hook | standing | every bump |
| Model v2/v3 (positions+sampler; data plane R2/R3′) per VERIFICATION-README roadmap | agent | alongside M2/M3 |
| Coordinator-disk-loss = session loss (documented D-mode limitation); D2 mirroring opt-in later | design | v1 accepted |

**Honest performance envelope (unchanged from report):** 70B Q4 ≈ 2–7 tok/s wired desktop+Mac, 1.5–4 over good Wi-Fi; TTFT tens of seconds at 4k prompt; recovery <15 s target (4k ctx, D1). Hydra's value is correctness + running-at-all, not speed.

## 9. Environment & toolchain facts (agent machine)

Apple Silicon M2, 8 GB RAM (→ small models locally; 70B targets need M2-milestone hardware), macOS, Metal + CPU backends; Rust/cargo; cmake 3.28; flatc 25.12.19; OpenJDK 26 (headless, substituted for temurin cask — approved) + TLC 2.19 (`tla2tools.jar`, git-ignored). Concurrent TLC runs require unique `-metadir` (collision incident logged and fixed). **Thermal constraint (environment fact):** long TLC runs overheat the laptop and are killed at session end — so **long model-checking is CI-owned** (`.github/workflows/tlc.yml`); local TLC is **smoke-only** (parse, Mut2, Mut4), and any exceptional local run must use `-checkpoint 1 -workers 1 nice -n 19` and be `-recover`-resumable. Two Mut3 local runs already burned heat for zero retained progress — not a third.

## 10. Glossary (project-specific terms a newcomer will hit immediately)

**Fence tuple** — identifiers on every message (cluster/manifest/session/epoch/recovery_id/attempt/context/generations) enabling rejection of stale traffic (F1/F2). **Ledger** — durable token history at C; single source of truth (I3). **applied_pos vs sampled_pos** — input/KV progress vs sampled-output progress; never interchangeable (I13). **GENERATION_COMMIT** — atomic record binding tokens + the sampler snapshot for the same prefix (I19). **PREACTIVE/ACTIVE_FINAL** — reversible vs finalized activation states; data plane only in the latter (I20). **Supersession** — replacing a decided-but-unservable activation at epoch+1 instead of blocking (I22). **Strategy A/B** — survivor-preserving recovery with catch-up vs full-state rebuild in a fresh context. **DST** — deterministic simulation testing; the sim that asserts I1–I25 under adversarial schedules. **Mutation parity** — the sim must catch the same four sabotages the TLA+ mutations encode.

## 11. Update protocol for this file (agent-binding)

1. Update **in the same commit** as: milestone progress, gate verdicts, findings, TLC/sim results, amendments, owed-item changes.
2. Sections 3, 4, 6, 8 must always reflect *now*; §7 is append-only history; §12 gets one line per update.
3. Never delete history; strike-through + supersede.
4. Gate reports to the design authority may quote this file; therefore it must never be aspirational — only verified facts, with evidence pointers (test names, log paths, commit hashes).
5. If reality and this file disagree, fixing this file is the *first* task.

## 12. Changelog of this document

- **2026-07-11** — Initial version, authored by the design authority; current through: M−1 PASS ratified, M0 ratified, v0.10.2 applied, Metal sweep done, Mut1 confirmed, Mut3/baselines running, M1 green-lit, upstream-issue decision pending with owner.
- **2026-07-11** — Adopted into repo root by the coding agent and reconciled with repo reality (§11.5): filled commit-hash evidence pointers in §4; corrected §6 to reflect the session-teardown kill (baseline-safety reached 26.6M distinct/depth 143 zero-violations but no fixpoint; Mut3 SIGTERM-killed, not a clean drain — both re-running via `run-gate.sh`); corrected §2/§5 schema paths to `docs/`; added FINDINGS Metal FP16 ~4×-lower note; M0-review upstream-issue bracket was left unfilled → held. Repo publish recorded in the following entry.
- **2026-07-11** — **M1 slice 1 landed** (`hydra-state`): coordinator activation transaction (spec §6.6) + I25 abort-finality guard + §6.5 phase-specific restart; pure state machine with WAL-FORMAT §4 effect-IDs; executable invariants I25/I16/I10a; Mut4 mutation parity green (checker catches the resurrected-abort I25 violation). Workspace 32 tests. TLC Mut3 still exploring (65M states / 17.4M distinct / depth 102, no violation yet, not drained — no contingency yet). Commit `1cc16f6`.
- **2026-07-11** — **Published to https://github.com/PavanManchikatla/Hydra** (`git push -u --force-with-lease origin main`; remote was empty, so no history rewrite was needed). Pre-push audit clean: **58 tracked files, pack 169.6 KiB**, no `*.gguf`/`build`/`target`/`states`/`*.out`/`*.jar` tracked, no secrets, `vendor/llama.cpp` a submodule **gitlink (mode 160000 @ 13f2b28)** — a pointer, not vendored source. No tags. Pushed at commit `acee34e` (this changelog entry adds a follow-up commit).
- **2026-07-11** — **Consolidated directives applied.** (1) **Commit attribution:** owner set as primary author+committer repo-wide via a one-time authorized history rewrite (`423286a` → `b314a77`; all 11 commits re-authored to `Pavan Manchikatla <91258136+PavanManchikatla@users.noreply.github.com>`, co-author trailer normalized to `Claude <noreply@anthropic.com>`), force-pushed with `--force-with-lease`; GitHub API confirms `author_login=PavanManchikatla`. Future history rewrites prohibited (§2d). (2) **Push cadence** standing rule added (§2e). (3) **TLC thermal policy:** local long runs retired (all prior ended by SIGTERM — thermal, never drains, contingency never triggered); model checking offloaded to CI (`.github/workflows/tlc.yml`: required smoke = parse+Mut2+Mut4+bounded baseline; `long` = checkpointed baseline-safety/liveness/Mut1/Mut3, weekly + dispatch); §6/§8 rows → CI-owned, §9 records the constraint. Mut3 drain-clean contingency transfers to CI unchanged.
