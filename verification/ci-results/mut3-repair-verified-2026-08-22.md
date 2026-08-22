# Receipt — Mut3 repair verified · runs 32605432737 (standing) + 32605434178 (cert, MaxCrashes=2)

**2026-08-22, post-`AttemptFencing` ack-gating repair (§7.22).** Model: `HydraActivationCore.tla`
with `CommitAcks`/`FinalAcks` gated as `(AttemptFencing => m.a = a)`.

## PREDICTION VERIFIED — Mut3 FIRES

```text
config=baseline-safety expect=clean violated=0 complete=1 verdict=GREEN
config=baseline-safety-fast expect=clean violated=0 complete=1 verdict=GREEN
config=mut1 expect=violation violated=1 complete=0 verdict=GREEN
config=mut3 expect=violation violated=1 complete=0 verdict=GREEN
config=mut3-fast expect=violation violated=1 complete=0 verdict=GREEN

config=baseline-live msgs=8 crashes=2 attempt=2 expect=clean violated=0 complete=1 verdict=GREEN
config=baseline-safety msgs=20 crashes=2 attempt=2 expect=clean violated=0 complete=1 verdict=GREEN
config=baseline-safety msgs=8 crashes=2 attempt=2 expect=clean violated=0 complete=1 verdict=GREEN
config=mut1 msgs=20 crashes=2 attempt=2 expect=violation violated=1 complete=0 verdict=GREEN
config=mut1 msgs=8 crashes=2 attempt=2 expect=violation violated=1 complete=0 verdict=GREEN
config=mut2 msgs=20 crashes=2 attempt=2 expect=violation violated=1 complete=0 verdict=GREEN
config=mut2 msgs=8 crashes=2 attempt=2 expect=violation violated=1 complete=0 verdict=GREEN
config=mut3 msgs=20 crashes=2 attempt=2 expect=violation violated=1 complete=0 verdict=GREEN
config=mut3 msgs=8 crashes=2 attempt=2 expect=violation violated=0 complete=1 verdict=MASKED-FAILURE
config=mut4 msgs=20 crashes=2 attempt=2 expect=violation violated=1 complete=0 verdict=GREEN
config=mut4 msgs=8 crashes=2 attempt=2 expect=violation violated=1 complete=0 verdict=GREEN
```

`mut3` fires in **4 s**, `120956 states generated, 49062 distinct`. The **17-state counterexample
walks the predicted path exactly**, ending:

```text
State 15: <StageRecvFinalizeAt ...>
State 16: <StageRecvFinalizeAt ...>
State 17: <CoordBecomeServiceable ...>
   cState = "SERVICEABLE"
   attempt = 2
   stAttempt = (s1 :> 2 @@ s2 :> 1)
```

A stage left at attempt 1 while the coordinator is at attempt 2, at SERVICEABLE ⇒ **`TupleSafety`**.

## Honest bound note

At `|msgs| ≤ 8, MaxCrashes = 2` Mut3 still reads `MASKED-FAILURE`. The
stale-`COMMIT`-survives-into-attempt-2 path needs message room; that bound is simply too tight.
Recorded, not hidden. `|msgs| ≤ 20` fires.

## Re-verified unaffected (observed, not assumed)

* `mut1` / `mut2` / `mut4` — `verdict=GREEN` at every bound run.
* `baseline-safety` — `complete=1 verdict=GREEN` at the **standing** bounds, post-repair.
* `baseline-safety` / `baseline-live` — `complete=1 verdict=GREEN` at cert bounds, `MaxCrashes = 2`.
* Local rule-7-permitted mutations returned state counts **identical to pre-edit** (Mut2 834,
  Mut4 18 637 distinct) — the strongest available evidence the faithful model did not move.

`baseline-live` at the **standing** bounds was still running when this receipt was written and is
**NOT claimed**. It is the fifth and last M1 flip receipt.
