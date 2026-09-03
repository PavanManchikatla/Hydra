# Hydra v0.10.1 — Model-checking guide (HydraActivationCore)

## Files
- `HydraActivationCore.tla` — action-style TLA+ model of the **v0.10** transition core
  (BEGIN_RECOVERY Cases A/B/B′, reset, activation intent/commit/abort/complete/finalize
  with abort finality, unservable/supersession, SessionTerminate, crashes/restarts,
  candidate checkpoints).
- Configs: `BaselineSafety.cfg` (symmetry, invariants), `BaselineLiveness.cfg`
  (NO symmetry — required for liveness; temporal properties), `Mut1Unservable.cfg`,
  `Mut2Reset.cfg`, `Mut3AttemptFence.cfg`, `Mut4AbortFinality.cfg`.

Run: `java -cp tla2tools.jar tlc2.TLC -workers auto -deadlock -config <cfg> HydraActivationCore.tla`
(`-deadlock` because TERMINAL is deliberately absorbing; stuck-state detection is the
liveness properties' job. Use `-checkpoint 1` + `-recover` on time-limited machines.)

## Modeling choices
1. Action-style TLA+ (not PlusCal): the reviewed action list is action-shaped; PlusCal's
   process structure fights multi-role async models.
2. Network = grow-only message set: duplication/reordering free; loss = never received;
   `WF` on receive actions supplies eventual delivery.
3. Durable WAL writes are separate actions from sends, so every decided-but-untold crash
   window is reachable.
4. EventuallyStable is literal: crashes bounded by `MaxCrashes` and not fair; productive
   actions weakly fair.
5. Mutations are CONSTANT flips: `EnableUnservable`, `ResetTruncates`, `AttemptFencing`, `RestartDerivesByMax` (Mut5, 2026-09-02: restart derives the target epoch by MIN — spec §6.5a),
   `AbortGuardEnabled`.
5b. **Fairness is per-(stage, message-class)** (v0.10.1 patch): receive actions are
   parameterized over their bounded discriminators (epoch, recovery_id, attempt), with
   `WF_vars` quantified over those constant domains. Aggregate-action WF alone would allow
   an old message to starve a required delivery; within one (type, epoch/attempt/reset-id)
   class, messages are identical up to idempotence, so class-level fairness equals
   per-message fairness for this model. The fair receive set covers BEGIN, RESET, COMMIT,
   **ABORT**, and FINALIZE per (stage, class); the fair coordinator set includes the
   progress-restoring actions `CoordBeginRecovery`, `CoordResetAttempt`,
   `CoordAbortActivation`, and `SessionTerminate` (weak fairness never forces them while a
   successful transition keeps them disabled — it only forbids stuttering forever when they
   are the defined recovery path). Both were completed in the final v0.10.1 patch after
   review caught an omitted `StageRecvAbortAt` obligation that would have let a stale
   PREACTIVE(attempt 1) stage starve attempt 2 forever — a fairness-model artifact that a
   failing liveness run would otherwise have misattributed to the protocol. Required before
   `BaselineLiveness.cfg` results can certify `EventuallyStable`.
6. Bounded-model artifacts (counter exhaustion) route to `SessionTerminate` per spec §11;
   `StateConstraint` caps `|msgs| ≤ 20` — all certification claims are bounded-model claims.

## VERIFICATION RESULTS (machine-generated section — update only from actual TLC runs)

Environment: TLC 2.19 / OpenJDK 21, 1 core, sandbox with ~2-minute process ceiling
(runs below are therefore bounded in TIME as well as state; CI must rerun to fixpoint).

| Run | Config | Result |
|---|---|---|
| Defect hunt (pre-fix model) | baseline | **TLC-1 FOUND**: AbortSafety violated ~13 s in — coordinator crash after durable ABORT replayed commit, counted stale pre-abort acks, durably completed an aborted attempt. Fixed via `AbortGuardEnabled` guards; spec invariant **I25**. |
| Property debugging | baseline | **TLC-2**: naive `SERVICEABLE => all stages currently ACTIVE_FINAL` violated by crash-after-FINALIZED-ack — unsatisfiable under asynchrony; properties corrected to evidence-based `ServiceSafety` + `TupleSafety`. **TLC-3**: added `SessionTerminate` (spec §11 arrow was missing from the model). |
| Baseline safety (fixed model, incl. `AbortFinality`; post fairness-parameterization) | `BaselineSafety.cfg` | ~1.9M and ~1.4M states in two time-bounded runs, depth ≥ 30, **zero violations** (checkpoint written; run to fixpoint in CI). |
| Mutation 2 | `Mut2Reset.cfg` | **Fires as designed**: `CaseBPure` violated in an 8-state trace (rebuild past truncate_to → label-only reset → Case B replay). |
| Mutation 4 | `Mut4AbortFinality.cfg` | **Fires as designed**: `Inv` (AbortFinality/AbortSafety) violated in a 14-state trace reproducing TLC-1. |
| Baseline liveness, Mutations 1 & 3 | `BaselineLiveness.cfg`, `Mut1Unservable.cfg`, `Mut3AttemptFence.cfg` | Superseded by the 2026-07-11 gate run below (no longer process-ceiling-limited). |

### Gate run — 2026-07-11 (TLC 2.19 / OpenJDK 26, Apple M2, no process ceiling)
Run via `verification/run-tlc.sh`; raw output under `verification/results/`. `-workers auto`,
`-deadlock`. Config bounds unchanged (`|msgs| ≤ 20`, MaxEpoch 1 / MaxRId 1 / MaxAttempt 2 /
MaxPos 1 / MaxCrashes 2). All claims remain bounded-model claims.

| Run | Config | Result |
|---|---|---|
| Mutation 1 | `Mut1Unservable.cfg` | **Fires as designed**: `PostDecisionLoss` temporal property violated — 18-state counter-example ending in stuttering (a LOST participant after the durable decision, with `EnableUnservable=FALSE` no supersession is enabled, so `Progress` is never restored). 105,095 distinct states; TLC exit 13. |
| Baseline safety | `BaselineSafety.cfg` | **In progress toward fixpoint** — ≥ 24.4M distinct states (90M generated), depth ≥ 133, **zero violations**. (Large bounded space; still draining the queue. Prior sandbox runs saw the same: zero violations.) |
| Mutation 3 | `Mut3AttemptFence.cfg` | **In progress** — ≥ 2.0M distinct states explored, no violation surfaced yet; expected to violate `ServiceSafety`/`TupleSafety` via a stale INITIAL attempt. Not yet complete → not yet certified; escalate if it drains clean. |
| Baseline liveness | `BaselineLiveness.cfg` | **Pending** (queued behind baseline safety in the runner). Expected green under fairness. |

**Certification claim permitted today:** "bounded-model-checked transition core with one
protocol defect found and fixed (I25); **three** mutations confirmed live (Mut2 `CaseBPure`,
Mut4 `AbortFinality`, **Mut1 `PostDecisionLoss`**); baseline safety exploring to fixpoint with
zero violations; Mut3 + baseline liveness runs in progress." Nothing stronger until Mut3 fires
and the baseline runs reach fixpoint.

### Gate run — 2026-07-13 (CI run 29222085672, v0.10.4 model, `.github/workflows/tlc.yml` `long`)
First `long`-job run on the F-LIVENESS-FAIR-repaired model (headSha `9372ee1`). Read via the
rule-12 semantic log-read (the Classify `verdict=` lines, **not** the run-status page). Verbatim
receipt: `ci-results/run-29222085672.md`. Inner time-box `timeout 320m`, `-checkpoint 60`.

| Run | Config | Verdict | Detail |
|---|---|---|---|
| Mutation 1 | `Mut1Unservable.cfg` | **GREEN — fires as designed on v0.10.4** | `violated=1`; temporal property violated, 23-state stuttering lasso; 467,199 distinct. |
| Baseline safety | `BaselineSafety.cfg` | **INCONCLUSIVE (time-boxed)** | `violated=0 complete=0`; 327.9M distinct, 380,964 states on queue at the 320m kill — **no fixpoint**. Checkpoint uploaded → `recover=true`. |
| Baseline liveness | `BaselineLiveness.cfg` | **INCONCLUSIVE (time-boxed)** | `violated=0 complete=0`; 25.0M distinct, 758,325 on queue — **no clean drain** (the v0.10.4 repair means no early counterexample stop, so the drain is CI-scale). Checkpoint uploaded → `recover=true`. |
| Mutation 3 | `Mut3AttemptFence.cfg` | **INCONCLUSIVE (time-boxed)** | `violated=0 complete=0`; 379.7M distinct, 390,911 on queue — **did NOT fire, no clean drain** → contingency ladder NOT triggered (needs a genuine `complete` with no violation). Checkpoint uploaded → `recover=true`. |

**Consequence:** this run did **not** conclude the M1 full-flip gate. Only Mut1 is green on the
new model; baseline-safety→fixpoint, baseline-live→clean drain, and Mut3→fire remain **pending**,
to be reached by re-dispatching `recover=true` (which also exercises the `-recover` round-trip for
the first time). **No certification claim beyond the paragraph above is licensed** — in particular
"all configs conclusive on v0.10.4" is NOT yet true.

### Local smoke — 2026-09-03 (spec §6.5a: restart derives from the WAL and fences forward)

The model was amended per the design authority's ruling of 2026-09-02 (H10(d) → protocol
amendment): `CoordCrash` sets the coordinator's volatile variables to ⊥ (`goal`, the durable
commit-stream frontier, excepted), `CoordRestart` derives `DTarget/DRId/DAttempt/…` from `wal`,
classifies in F-UNSERVABLE order and fences forward to `(epoch+1, rid+1)`; new invariant
`IntentFence`; new mutation **Mut5** (`RestartDerivesByMax = FALSE`: the target epoch derived by
MIN). Run through the project's own flags (`-workers auto -deadlock`, rule 18), quoted verbatim:

| Config | Result (quoted) | Reading |
|---|---|---|
| `Mut5RestartMin.cfg` | `Error: Invariant Inv is violated.` — `Finished in 03s` | **Fires as designed** (`IntentFence`): deriving the target by MIN re-opens the old epoch, and a stale durable INTENT outranks the derived attempt. |
| `smoke/Mut2-CaseBPure.cfg` | `Model checking completed. No error has been found.` — 890 000 distinct states, 0 left on queue — `Finished in 50s` | **⛔ ESCALATED-SUBSUMED.** Mut2's designed trace (label-only reset → Case B replay after a restart) used the restart-replay path §6.5a removes; a fenced-forward restart never replays. What Mut2 should sabotage now is the design authority's ruling — the CI smoke prints `verdict=ESCALATED-SUBSUMED` and is red by design until then (PROJECT_STATE §7.77). |
| `smoke/Mut4-AbortFinality.cfg` | `Model checking completed. No error has been found.` — 946 195 distinct states, 0 left on queue — `Finished in 53s` | **⛔ ESCALATED-SUBSUMED.** Mut4's trace (TLC-1: the aborted attempt resurrected by the restart, then completed by stale acks) needs `CoordRestart → ACTIVATION_INTENT_DURABLE`, which no longer exists. Same status as Mut2. |

**Checkpoints from every earlier long run are void** (the state space changed with the model);
the standing bounds are unchanged. The CI `tlc.yml` smoke carries a Mut5 step and the long matrix a
`mut5` leg (`expect: violation`); the next scheduled long run is the first on the amended model.
**Re-measured 2026-09-03T01:54Z after the derivation repair** (the DST found `DUnserv` matching ANY completion; now `DTargetCid`-scoped, and `DecisionMonotone` strengthened to the code's form — PROJECT_STATE §7.77); the first smoke at 00:33Z read the same way on the pre-repair model. `BaselineSafetyFast.cfg` also drains clean (76 351 distinct states, 3 s).
**Re-measured again at 03:04Z after Case A was extended** (spec §1.3 with §6.5a: a stage REBUILDING / FROZEN_READY at base — caught up for an activation that never committed because the coordinator crashed first — now takes Case A; the product restart oracle's third window found the model and the code dropping that BEGIN as Case C): Mut5 `Error: Invariant Inv is violated.` (11 s); Mut2 clean (890 000 distinct, 2 min 52 s); Mut4 clean (946 195 distinct, 2 min 09 s); BaselineSafetyFast clean (76 351 distinct, 6 s) — the same counts, because the smoke bounds do not reach a crash between reconstruction and COMMIT.
**Re-measured a fourth time at 03:48Z after the served-fence refinement** (spec §6.5a: a durable COMPLETE re-enters finalization only while the activation has not served — `DComplete /\ servedCount = 0`; a served activation's crash is outside any transaction and fences forward, because the stages' data-plane tail beyond the durable frontier needs the BEGIN's truncation; forced by the product restart oracle's first two windows once the control-WAL codec stopped zeroing every COMPLETE's tuple): Mut5 `Error: Invariant Inv is violated.` (3 s); Mut2 clean (889 076 distinct, 54 s); Mut4 clean (945 271 distinct, 1 min 52 s); BaselineSafetyFast clean (76 279 distinct, 7 s) — the counts move by −0.1 %, the verdicts not at all.
Rule 6 note: the model was NOT adjusted to make Mut2/Mut4 fire again — the drain-clean result is the
finding, recorded, not repaired.

## Roadmap after the core certifies
- **Model v2 (positions & sampler):** input/output position discipline (I13),
  GENERATION_COMMIT alignment (I19), sampler rollback/installation (I15/I17),
  teacher-forced replay (I8), partial-trailing-record rule.
- **Model v3 (data plane):** retain buffers, R2/R3′ release conditions, bounded-lag D1,
  Strategy A catch-up window.
- Every TLC counterexample becomes a directed DST scenario in `hydra-sim` (blueprint §4).
