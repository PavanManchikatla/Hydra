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
5. Mutations are CONSTANT flips: `EnableUnservable`, `ResetTruncates`, `AttemptFencing`,
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
| Baseline liveness, Mutations 1 & 3 | `BaselineLiveness.cfg`, `Mut1Unservable.cfg`, `Mut3AttemptFence.cfg` | **NOT YET RUN here** (process ceiling). Expected: baseline liveness green under fairness; Mut1 violates `PostDecisionLoss` (lasso: LOST participant after durable decision, no supersession enabled); Mut3 violates `ServiceSafety`/`TupleSafety` via a stale INITIAL attempt. CI must run these before the transition core is called certified. |

**Certification claim permitted today:** "bounded-model-checked transition core with one
protocol defect found and fixed (I25), two mutations confirmed live, two mutations and
liveness pending CI." Nothing stronger.

## Roadmap after the core certifies
- **Model v2 (positions & sampler):** input/output position discipline (I13),
  GENERATION_COMMIT alignment (I19), sampler rollback/installation (I15/I17),
  teacher-forced replay (I8), partial-trailing-record rule.
- **Model v3 (data plane):** retain buffers, R2/R3′ release conditions, bounded-lag D1,
  Strategy A catch-up window.
- Every TLC counterexample becomes a directed DST scenario in `hydra-sim` (blueprint §4).
