# M1 Gate Evidence — State Machines + Deterministic Simulation

> **Status: assembled for the gate decision (a human gates *between* milestones — BLUEPRINT §3.a).**
> This is the (a)–(f) evidence table specified in `PROJECT_STATE.md` §0(c). It records only
> verified facts with evidence pointers (test names, CI runs, commit hashes). Items still running
> in CI are marked **PENDING** honestly — they are not asserted green.
>
> Spec authority at this gate: `docs/hydra-session-protocol.md` **v0.10.3** (F-UNSERVABLE §6.5
> amendment). Code at commit `9ed9892` (+ this gate commit). Workspace: `cargo test --workspace`
> **53 tests green, 0 warnings**.

---

## (a) Randomized simulation — steps, seeds, violations

| Metric | Value | Evidence |
|---|---|---|
| Local DoD run | **10,000,000 steps × 1,000 seeds → 0 invariant violations** | `hydra-sim` marathon (PROJECT_STATE §12, 2026-07-11 sim-foundation entry); re-confirmed this session: 4M-step + 2M-step faithful runs, 0 violations |
| Invariants checked per step | coordinator (`invariants::check`) + **every** real stage (`invariants::check_stage`) + I22 deadlock watchdog | `crates/hydra-sim/src/lib.rs::World::step` |
| Durability path | through the **real `hydra-wal` codec** (virtual disk; torn-write on crash; real partial-tail-discard scanner on restart), cross-checked vs. the coordinator's durable WAL every restart | `crates/hydra-sim/src/wal_disk.rs`; F-UNSERVABLE found here (§7.10) |
| Reproducibility | every failure prints `(seed, schedule)`; SplitMix64, no wall-clock/OS randomness | `crates/hydra-sim/src/rng.rs`, `Failure` |
| **CI DoD run** | **1,000 seeds × 10,000 steps = 10,000,000 steps × {2,3} stages → 0 violations** | ✅ **GREEN** — `marathon.yml` `long`, run [29182450338](https://github.com/PavanManchikatla/Hydra/actions/runs/29182450338) (both `long (2)` and `long (3)` success); smoke green run [29182414676](https://github.com/PavanManchikatla/Hydra/actions/runs/29182414676) |

## (b) Per-mutation parity — caught, median steps-to-detection, seeds

Randomized detection over 200 seeds each, budget 20,000 steps (catch-rate bar ≥ 95%; a lower rate is a sim-weakness to fix, never a budget to relax). `crates/hydra-sim/tests/dst.rs`, continuously guarded by `marathon.yml` `smoke`.

| Mutation | Invariant | Caught | Median steps | Seeds |
|---|---|---|---|---|
| Mut1 `mutation_no_unservable` | I22 PostDecisionLoss | ✅ **200/200** | 374 | 200 |
| Mut2 `mutation_label_reset` | CaseBPure (I11/I23) | ✅ **200/200** | 250 | 200 |
| Mut3 `mutation_no_attempt_fence` | F2 AttemptFence (I4) | ✅ **200/200** | 187 | 200 |
| Mut4 `mutation_no_abort_finality` | I25 AbortFinality | ✅ **200/200** | 82 | 200 |
| Mut5 `mutation_unservable_restart` | WalCodecDivergence (F-UNSERVABLE: omitted durable UNSERVABLE) | ✅ **200/200** | 658 | 200 |

Mut5 is the monotone-mutation left behind by F-UNSERVABLE (§7.10): it reintroduces the omitted durable-`ACTIVATION_UNSERVABLE`-record behavior, and the WAL-codec cross-check that originally found the defect re-finds it. Every run's output header carries `hydra_sim::SCHED_VERSION` (result-provenance hygiene: medians/catch-rates compare only within one scheduler version).

## (c) Directed scenarios — spec verification-plan checklist

| Verification-plan scenario | Status | Test |
|---|---|---|
| Participant lost after durable decision → supersession (I22) | ✅ | `supersede.rs::post_decision_loss_supersedes_and_recovers` |
| Crash in the superseding window → restart resumes SUPERSEDING (F-UNSERVABLE) | ✅ | `supersede.rs::f_unservable_crash_in_superseding_window_restarts_to_superseding` |
| Attempt restart from FROZEN_READY after catch-up (I23) | ✅ | `recovery.rs::reset_after_catch_up_truncates_then_case_b_ok` |
| Zombie survivor — completed activation locally decidable (Case B′) | ✅ | `recovery.rs::case_b_prime_completed_is_locally_decidable` |
| Case A first-application truncation (I7a) | ✅ | `recovery.rs::case_a_first_application_truncates` |
| Delayed INITIAL / stale attempt fenced by attempt_id (I4/F2) | ✅ | `stage.rs::stale_attempt_is_fenced` |
| Mixed-epoch retransmit fenced (I4/F1) | ✅ | `stage.rs::mixed_epoch_commit_is_rejected_by_f1` |
| Sampled-ahead snapshot alignment / provisional rollback (I7b/I15) | ✅ | `ledger.rs::provisional_window_rolls_back_on_recovery` |
| Cancellation cutoff (I9) | ✅ | `ledger.rs::cancellation_suppresses_provisional_and_flushes_committed` |
| Teacher forcing — committed positions never re-sampled (I8) | ✅ | `ledger.rs::teacher_forcing_never_resamples_a_committed_position` |
| GENERATION_COMMIT durable-prefix alignment (I19) | ✅ | `ledger.rs::i19_equalities_enforced_on_commit` |
| Abort-then-retry, same reconstruction (attempt monotonicity, I21) | ✅ | `coordinator.rs::abort_returns_to_ready_at_next_attempt` |
| Torn commit record (WAL-FORMAT §5) | ✅ | `wal_disk.rs::torn_pending_write_is_discarded_all_variants` + `hydra-wal/tests/torn_write.rs` (M0) |
| **Uncommitted segment candidate (I24)** | ⏸ **DEFERRED → M3** | segment-checkpoint SM not built at M1 (BLUEPRINT scopes it to M3); binding §8 owed-item incl. future `mutation_candidate_leak`. Model-layer coverage: TLA+ `CandidateIsolation` |
| **Duplicated SAMPLE_NEXT — retained-logits idempotency (I14 data-plane)** | ⏸ **DEFERRED → M2** | ledger structural exactly-once covered (see teacher-forcing above); retained-logits idempotent re-serve is M2 data-plane (§8 owed-item) |

## (d) TLC counterexample replays — event-sequence fidelity

Each carries an **action-mapping table** (TLC action → impl event) in its source, a **mutation-gated dual outcome** (faithful build walks the mapped sequence clean; mutation build violates the same invariant at the mapped step), and pivotal spot assertions on trace fields.

| TLC trace | Config / size | Faithful replay | Mutation replay | Mapping table |
|---|---|---|---|---|
| **TLC-1** (aborted-attempt resurrection) | 14-state | `coordinator.rs::tlc1_crash_after_abort_never_completes_aborted_attempt` | — (fix asserted) | `tests/coordinator.rs` doc |
| **Mut4** (no abort finality) | `smoke/Mut4-AbortFinality.cfg`, 14-state | — | `coordinator.rs::mut4_completion_after_abort_is_caught_by_checker` | `tests/coordinator.rs` doc |
| **Mut2** (label-only reset) | `smoke/Mut2-CaseBPure.cfg`, 8-state | `recovery.rs::reset_after_catch_up_truncates_then_case_b_ok` | `recovery.rs::mut2_label_only_reset_trips_case_b` | `tests/recovery.rs` doc |
| **Mut1** (no unservable) | `Mut1Unservable.cfg`, 18-state lasso | `supersede.rs::post_decision_loss_supersedes_and_recovers` | `supersede.rs::mut1_post_decision_loss_deadlocks` | `tests/supersede.rs` doc |

**4/4 TLC counterexamples replayed.**

## (e) TLC six-config status (CI-owned — thermal rule, PROJECT_STATE §9)

CI run [29179427311](https://github.com/PavanManchikatla/Hydra/actions/runs/29179427311) (`tlc.yml`), + local/CI smoke.

| Config | Expect | Status |
|---|---|---|
| `BaselineSafety.cfg` | clean → fixpoint | ⏳ **PENDING** (CI `long`, running; deepest prior local 26.6M distinct / depth 143, 0 violations, no fixpoint) |
| `BaselineLiveness.cfg` | clean | ✅ **GREEN** (CI, run 29179427311) |
| `Mut1Unservable.cfg` | violation | ✅ **FIRED** — PostDecisionLoss, 18-state lasso (CI) |
| `Mut2Reset.cfg` (CaseBPure) | violation | ✅ **FIRED** — 8-state (smoke, local + CI) |
| `Mut3AttemptFence.cfg` | violation | ⏳ **PENDING** (CI `long`, running; expected ServiceSafety/TupleSafety via stale INITIAL attempt) |
| `Mut4AbortFinality.cfg` | violation | ✅ **FIRED** — I25, 14-state (smoke, local + CI) |

Parse (SANY) ✅ green after the F-UNSERVABLE `.tla` comment. **Gate note:** two long configs (baseline-safety fixpoint, Mut3) are still exploring — these are the only outstanding items for a fully-green (e). The `-recover` checkpoint round-trip remains a required verification once a long job hits its 6 h boundary (PROJECT_STATE §0b).

## (f) Invariant coverage map — I1–I25

Each invariant → the executable check (function / test), or an explicit deferral with its layer. **No invariant is unmapped.**

| Inv | What | Checked by |
|---|---|---|
| I1 | exactly-once apply (shard-scoped) | ⏸ M2 data-plane (`APPLY_TOKEN`/applied_pos not modeled in the transition core) |
| I2a/I2b | durability / stable-frontier monotonicity | `invariants::check_ledger` (I2 SampledFrontier, LedgerFrontier) |
| I3 | ledger determines output | structural (`ledger.rs` is the source of truth) + I6 check; cross-backend I3/I8 → M2 golden-token |
| I4 | F1/F2 fencing (+ attempt fencing) | `invariants::check_stage` (F2 AttemptFence); F1 → `stage.rs::mixed_epoch_commit_is_rejected_by_f1` |
| I5 | durability domains | `hydra-wal` (WAL-FORMAT §5, M0) + `wal_disk.rs` restart cross-check |
| I6 | delivery (emit-after-commit) | `invariants::check_ledger` (I6 EmitAfterCommit) |
| I7a/I7b | truncatability split | I7a → `recovery.rs::case_a_first_application_truncates`; I7b → `ledger::rollback_provisional` + test |
| I8 | teacher forcing | structural (`ledger::sample_next` extends only) + `teacher_forcing_never_resamples_a_committed_position`; cross-backend → M2 |
| I9 | no hidden committed output (cancellation cutoff) | `invariants::check_ledger` (I9 CancelCutoff) |
| I10a | local atomicity | `invariants::check` (DecisionMonotone) + effect-ids (`hydra-wal::effect_id`, M0) |
| I10b | global execution barrier | coordinator `all_finalized` guard (`ProceedBecomeServiceable`) + I16; data-plane barrier → M2 |
| I11 | transition replay idempotence | `invariants::check_stage` (CaseBPure) + `stage.rs::commit_replay_is_idempotent` |
| I12 | rebuild isolation | partial — `stage::RebuildStep` (Strategy B); full fresh-context isolation → M2/M3 |
| I13 | position discipline | type-level `InputPos`/`OutputPos` newtypes (`hydra-proto`; I13 violations don't compile) |
| I14 | exactly-once sampling | structural (`ledger::sample_next`); SAMPLE_NEXT retained-logits idempotency → M2 (§8) |
| I15 | sampler rollback | `ledger::rollback_provisional` + `provisional_window_rolls_back_on_recovery` |
| I16 | completion follows all COMMITTED | `invariants::check` (ServiceSafety) |
| I17 | sampler installation before activation | partial — `sampler_checkpoint_id` bound in the activation tuple; real install → M2 |
| I18 | activation commit convergence | coordinator restart replay (`restart()` → IntentDurable) + stage idempotent re-ack |
| I19 | durable-prefix alignment | `invariants::check_ledger` + `i19_equalities_enforced_on_commit` + `hydra-wal` I19-on-read (M0) |
| I20 | completion visibility (ACTIVE_FINAL only) | coordinator `ProceedBecomeServiceable` (all_finalized) + I16; stage `ActiveFinal` gating |
| I21 | unfinalized abortability | `coordinator::ProceedAbort` + `stage::RecvAbort`; `abort_returns_to_frozen_ready` / `_ready_at_next_attempt` |
| I22 | post-decision recoverability | `coordinator::post_decision_deadlock` watchdog + supersession; `supersede.rs` tests |
| I23 | attempt-reset convergence | `stage::RecvReset` (truncates) + `invariants::check_stage` (CaseBPure) + `reset_after_catch_up…` |
| I24 | candidate isolation | ⏸ **M3** (segment-checkpoint SM); partial now: checkpoint advances only via a durable commit record; model-layer TLA+ `CandidateIsolation` (§8 binding owed-item) |
| I25 | abort finality | `invariants::check` (I25 AbortFinality) + `tlc1_…` / `mut4_…` |

**Deferred-with-layer (permitted by the gate spec):** I1, I10b (data-plane halves), I12/I17 (partial), I14 (retention half) → **M2**; I24 → **M3** (binding §8 owed-item). All are explicitly layer-tagged; none are silently dropped.

---

## Gate summary

| Criterion | State |
|---|---|
| (a) randomized 10M+/≥1000 seeds, 0 violations | ✅ **CI green** (run 29182450338, 10M × {2,3} stages) + local |
| (b) all mutation parities caught | ✅ **5/5 at 200/200** (Mut1–Mut4 + Mut5 F-UNSERVABLE monotone-mutation) |
| (c) directed scenarios | ✅ except I24 (→M3) and SAMPLE_NEXT retention (→M2), both deferred-with-owed-item |
| (d) 4 TLC-trace replays | ✅ 4/4, event-sequence fidelity |
| (e) TLC six configs | ✅ 4/6 conclusive; ⏳ baseline-safety fixpoint + Mut3 still running in CI |
| (f) I1–I25 coverage map | ✅ complete; no unmapped invariant |

**Outstanding before a fully-green gate:** only (e) — baseline-safety fixpoint + Mut3 firing (CI-owned, still running, run 29179427311) and the `-recover` checkpoint round-trip. These are CI-execution items, not code gaps. Every code-side criterion (a)–(d), (f) is green. **Paused here for the owner's M1 gate decision.**
