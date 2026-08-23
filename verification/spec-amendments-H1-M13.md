# Proposed spec amendments — audit H1 and M13

**Status: DRAFTED, NOT APPLIED. ⏸️ Awaiting ratification.** Both findings touch protocol semantics,
so per standing rule 11 nothing is changed unilaterally: the amendment is proposed, and on
ratification it lands **spec → TLA+ → code in one commit**, with a TLC smoke re-run because the
model changes.

> **Why these two are different from every other audit finding.** The rest of Wave 1 fixes code that
> disagreed with the spec. These two are places where **the spec does not say**, and two conforming
> implementations could make incompatible choices. The defect is the silence, so the repair has to
> be to the document first.

---

## H1 — the attempt fence is bounded below but not above

### What the spec says today

**§1.1 (F2, control plane):**

> *"a stage rejects any activation control message whose `activation_attempt_id` is **below** its
> highest accepted attempt for the (session, epoch)."*

**§6.4:** an activation-only failure retries *"under `activation_attempt_id + 1`, same
`recovery_id`"* — so the coordinator advances by exactly one, every time.

`HydraActivationCore.tla` implements the rule verbatim:

```tla
/\ (AttemptFencing => m.a >= stAttempt[s])      \* MUTATION 3
   ...
   /\ stAttempt' = [stAttempt EXCEPT ![s] = m.a]
```

### The gap

The floor is a **one-sided** bound. A stage accepts **any** attempt at or above its floor, and then
**adopts it as the new floor**. Nothing — in the spec or the model — constrains how far above.

**Consequence: a single frame can permanently deny activation for the rest of the epoch.** A message
carrying `activation_attempt_id = u32::MAX` is accepted (it is ≥ the floor), the stage's floor
becomes `u32::MAX`, and every subsequent legitimate attempt — which the coordinator produces by
incrementing from its own counter — is **below the floor and is fenced forever**. The session cannot
activate, cannot recover, and §6.4's bound-exhaustion path resolves it via §11 explicit termination:
the session dies.

Under the **honest-worker assumption** this is not an attack, and that matters for severity — but it
is squarely an **accident** case, which the assumption does not excuse (the same reasoning that
moved M1's read timeout into v1). A corrupted field that survives the BLAKE3 frame tag, a buggy
peer, or a replayed frame from a future epoch all produce it, and the failure is **permanent and
silent**: the stage is behaving exactly as specified.

### Proposed amendment (the design authority's prior, adopted)

> **§1.1, F2 — replace the activation-fencing sentence with:**
>
> **Activation fencing.** A stage accepts an activation control message only if its
> `activation_attempt_id` lies in the window `{highest_accepted_attempt, highest_accepted_attempt +
> 1}` for the (session, epoch); anything below is stale and anything above is unreachable. Both are
> rejected with `ERR_FENCED`. On accepting `highest_accepted_attempt + 1` the stage adopts it as its
> new highest.

**Why a window of exactly two is both sufficient and tight:**

* **Sufficient** — §6.4 gives the coordinator exactly one way to advance an attempt: `+1`. A stage
  that has accepted attempt *n* can therefore only ever legitimately be offered *n* (an idempotent
  replay, which §6.6 step 2 requires it to re-ack) or *n+1* (the next retry). No legitimate message
  ever carries *n+2*.
* **Tight** — it makes the fence **unforgeable-past**: a peer cannot jump the floor, because the
  floor advances only in the increments the coordinator is specified to produce. The one-sided rule
  let a single message consume the entire remaining attempt space.
* **It does not weaken I4 or I25.** Below-floor rejection is unchanged, so stale-attempt fencing —
  the property Mut3 exists to catch — is untouched. The amendment only adds an upper edge.

### Change set on ratification

| Layer | Change |
|---|---|
| **Spec** | §1.1 F2 sentence above; a note in §6.4 that the coordinator's `+1` is what makes the window sufficient |
| **TLA+** | `m.a >= stAttempt[s]` → `m.a \in {stAttempt[s], stAttempt[s] + 1}` |
| **Code** | The stage SM's attempt check in `hydra-state`, and an `ERR_FENCED` reply |
| **Tests** | A directed regression: an above-window attempt is rejected **and the stage's floor does not move** — the second half is the real property, since a rejection that still advanced the floor would reproduce the denial |
| **TLC** | Smoke re-run (parse, Mut2, Mut4 locally per rule 7); the CI-owned legs re-run because the transition system changed. **Mut3 must still fire** — if the window change silenced it, the fence would have been weakened, not tightened |

---

## M13 — a reset does not define its effect on the attempt floor

### What the spec says today

**§6.4:** a reconstruction-invalidating failure ⇒

> *"`RESET_RECOVERY_ATTEMPT` to all survivors (truncate + FROZEN, `recovery_id + 1`, same base/target
> epoch), recompute the set, resume from prepare/attach."*

**Nothing** in §6.4, §6.6 or I23 says what happens to `activation_attempt_id` — the coordinator's
counter or the stage's floor — across a reset.

### The gap, and why the model did not surface it

The model **made a choice the spec never licensed**:

```tla
CoordResetAttempt ==
    ...
    /\ rId' = rId + 1
    /\ UNCHANGED << ..., attempt, ..., stAttempt, ... >>    \* both preserved

StageRecvResetAt(s, nr) ==
    ...
    /\ UNCHANGED << stAttempt, ... >>                       \* floor preserved
```

Both sides preserve the attempt counter, and that choice is **self-consistent**, so TLC has nothing
to complain about. It has been checked to fixpoint and is green. **The model is not wrong — it is
silently opinionated**, and its opinion is invisible to anyone reading the spec.

### The hazard

A second implementation reading §6.4 could just as reasonably conclude that a reset — which
increments `recovery_id`, truncates, and returns stages to `FROZEN` — starts a **fresh reconstruction
attempt**, and therefore restarts `activation_attempt_id` at 0.

That reading is **coherent, and fatal**, because F2 scopes the stage's floor to **(session, epoch)**
and a reset does *not* change the epoch. So:

1. before the reset, stages accepted attempt *n* and their floor is *n*;
2. the reset increments `recovery_id`, epoch unchanged;
3. the coordinator restarts attempts at 0;
4. every `COMMIT_ACTIVATION` at attempt 0 is **below the floor** and is fenced;
5. the reconstruction can never activate. Attempts exhaust, and §6.4's bound-exhaustion path
   terminates the session.

**Two conforming implementations, one deadlock.** Nothing detects it: every component is following
its own reading of the text.

### Proposed amendment

> **§6.4, appended to the `RESET_RECOVERY_ATTEMPT` sentence:**
>
> A reset **does not reset the activation-attempt space.** `activation_attempt_id` is monotonic per
> **(session, epoch)** and is unaffected by `recovery_id` advancing: the coordinator continues from
> its current attempt, and a stage **retains** its highest accepted attempt across the reset. Only a
> new epoch begins a fresh attempt space.
>
> *(Rationale, normative in force: the attempt id exists to fence stale activation traffic within an
> epoch — §1.1's F2 scopes it to (session, epoch) precisely so a straggler cannot be confused with a
> live message. Restarting it while the epoch persists would make pre-reset stragglers
> indistinguishable from post-reset messages, which is the exact confusion `activation_attempt_id`
> was introduced in v0.9 to remove.)*

**This ratifies what the model already does** — but the point is that it stops being an accident.
Today the property holds because one file happens to say `UNCHANGED`; afterwards it holds because
the spec requires it and the model is checked against the requirement.

### Change set on ratification

| Layer | Change |
|---|---|
| **Spec** | §6.4 sentence above; a cross-reference from I23 so the attempt space is part of what "attempt-reset convergence" means |
| **TLA+** | No transition change — but an **invariant** making the preservation checkable rather than incidental: across a `RESET`, `stAttempt` is non-decreasing and the coordinator's `attempt` is unchanged. Without it, a future edit could silently adopt the other reading and TLC would still pass |
| **Code** | Assert it in the stage SM's reset handler (`hydra-state`) — the reset must not touch the attempt floor |
| **Tests** | The DST scenario **"attempt restart from FROZEN_READY after catch-up"** (§7's v0.9 list) extended: after the reset, an activation at the *pre-reset* attempt is still fenced, and one at *pre-reset + 1* is accepted. That pair is what distinguishes the two readings |
| **TLC** | Smoke re-run; CI legs re-run because a new invariant is added. The new invariant must **hold on the current model unchanged** — if it fails, the model was making the other choice and the finding is larger than a silence |

---

## Sequencing note

Both amendments touch the same stage-side attempt check, so they should land **together, in one
commit**, with one TLC re-run rather than two. H1 narrows the window; M13 pins what the window's
lower edge means across a reset. Applying either alone would leave the check half-specified in
exactly the way this audit is about.
