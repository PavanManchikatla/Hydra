# Hydra Session Protocol — Specification v0.10.3 (machine-checked transition core)

*Token commits, KV commits, epochs, retries, cancellation, streaming delivery, and recovery for a pipeline-sharded LLM inference cluster. Scope: fixed coordinator, trusted LAN, 2–3 desktop-class stages, one dense GQA model family, contiguous stages, single final-stage sampler, semantic continuity after recovery, no speculative decoding, no mid-session rebalancing, no phones in the critical path. **One active session per model instance.** [RESERVED] marks forward-compatibility hooks.*

*Design slogans (cumulative through v0.8): a retry may never append; a recovery may never invent; a client may never un-see; a transition may never be ambiguous; derived state is rebuilt beside itself, never in place; an input position is not an output position; the ledger is what happened, the pipeline is how far we've caught up; sampling is an action, not a side effect; rollback erases everything provisional, including luck; a recovery isn't over until the WAL says so; a digest can verify state, but only a snapshot can restore it; activation is a commit, not a side effect of readiness; the token and the state it produced are one record or they are nothing; a stage must be able to see the finish line; the first activation is still an activation — and, added in v0.9: **a decision is forever, but serviceability is not; undoing an attempt is not the same as replaying one; and a candidate is not the state until its commit is.***

**The complete lifecycle (v0.9):**
```text
intent → freeze → prepare → attach → rebuild/catch-up → install sampler
       → activation intent (attempt a) → preactivate → durable decision
       → finalize → data plane
   with three defined reversals:
       abort activation attempt   (PREACTIVE → FROZEN_READY; attempt a+1; same recovery_id)
       reset recovery attempt     (any pre-decision state → FROZEN; truncate; recovery_id r+1)
       supersede decided-but-unservable activation  (new recovery at epoch+1; never serve incomplete)
```

**Changelog v0.10 → v0.10.3** (F-UNSERVABLE, found at the implementation/simulation layer by the `hydra-sim` DST driving durability through the real `hydra-wal` codec): §6.5's restart-rule branches are now an explicit **priority-ordered classifier** (first match wins), with `ACTIVATION_UNSERVABLE` evaluated **before** the `ACTIVATION_COMPLETE` branch. The prior prose listed the COMPLETE branch first, so a first-match implementation *shadowed* the UNSERVABLE branch: a coordinator crash in the window after `ACTIVATION_UNSERVABLE` is durable but before the superseding `BEGIN_RECOVERY` would restart into finalization instead of resuming supersession, reopening the I22 hole. The TLA+ model `CoordRestart` was **already correct** (it evaluates `unservable` first and carries it `UNCHANGED` across `CoordCrash`) — this was a spec-text ↔ model *correspondence* gap, so the prose is amended to match the model, not the reverse. Normative addition: an implementation MUST make the `ACTIVATION_UNSERVABLE` fact durable in the coordinator's log at the moment of decision, so the restart classifier can read it. (Skipped v0.10.1/v0.10.2 numbering are the package-hardening + boundary-precision amendments recorded in `BLUEPRINT.md`; this is the first amendment to the spec's transition prose since v0.10.)

**Changelog v0.9 → v0.10** (from TLC model checking of the transition core — `HydraActivationCore.tla`): (1) **TLC-1 (protocol defect, found in 13 s of checking): an aborted `activation_attempt_id` is permanently dead.** Counterexample: attempt 1 is durably ABORTed; C crashes; §6.5's restart rule saw "intent, no COMPLETE" and replayed `COMMIT_ACTIVATION`; the stages' pre-abort `ACTIVATION_COMMITTED` acks still existed in the network (same attempt id — fencing cannot distinguish them); C counted them and durably **completed an activation it had durably aborted**. Fixes, now normative: `ACTIVATION_COMPLETE` may never be written for an attempt with a durable ABORT record, and §6.5 gains a branch — *durable ABORT for the current attempt and no COMPLETE ⇒ the attempt is terminal; proceed to a new intent at attempt+1*. New invariant **I25 (abort finality):** for any (session, epoch, recovery_id, activation_attempt_id), a durable ABORT and a durable COMPLETE are mutually exclusive, permanently. (2) **TLC-2 (property clarification, not a defect):** any invariant of the form "SERVICEABLE ⇒ all stages are currently ACTIVE_FINAL" is unsatisfiable under asynchrony — a stage can crash between its FINALIZED ack and C's transition. The spec's I16 (evidence-based) and I20 (stage-local) are the correct, satisfiable formulations; implementations and tests must assert those, plus tuple-safety on live generation-matching stages, never coordinator-side ground truth about remote liveness. (3) **TLC-3 (model completeness, confirming §11):** pre-decision loss of all viable participants must terminate explicitly (`SessionTerminate`, WAL'd) — the TERMINAL arrow is part of the protocol, not just the scheduler contract. Verification status: baseline safety explored >1.7M states with zero violations (bounded run; resume via TLC checkpoints); mutation tests confirmed live (mutation 2 → `CaseBPure` violation in 8 states; TLC-1 found pre-fix by `AbortSafety`).

**Changelog v0.8 → v0.9** (from external protocol review): (1) **post-decision participant loss** — v0.8's activation was a two-phase commit with 2PC's classic weakness: after the `ACTIVATION_COMPLETE` fsync the decision was irrevocable, abort was forbidden, and a participant that lost its committed shard made finalization impossible forever ("replay finalize" can never succeed against a lost shard; `EventuallyStable` supplies replacements, but a replacement cannot join an already-decided tuple); v0.9 adds `ACTIVATION_UNSERVABLE{completion_id, failed_shards}` + **superseding recovery** (base = the completed epoch, target = +1, `predecessor_completion_id` recorded): reachable PREACTIVE stages are finalized, **no data-plane work is ever served under the incomplete configuration**, and replacement shards are prepared under the new epoch (new invariant **I22**); (2) **attempt restart is now a real state transition, not a label change** — catch-up/rebuild deliberately advance stages beyond `truncate_to`, so v0.8's abort-to-`FROZEN(r+1)` left survivors that would fatally violate Case B's `applied_pos ≤ truncate_to` assertion on the next `BEGIN_RECOVERY`; the new **`RESET_RECOVERY_ATTEMPT`** atomically truncates to `truncate_to`, discards attempt-only artifacts, restores the committed sampler checkpoint at S_P, adopts `new_recovery_id`, and enters `FROZEN` — leaving Case B a *pure idempotent replay rule* (new invariant **I23**); with attempt identities in hand, abort and reset become two-level: **`ACTIVATION_COMMIT_ABORT`** now returns PREACTIVE stages to `FROZEN_READY` (reconstruction intact; next `activation_attempt_id`; same `recovery_id`), while reset is the full truncating restart; (3) **`activation_attempt_id`** — a monotonic per-(session, epoch) activation-transaction generation carried by every activation message; INITIAL (epoch 0, recovery_id 0) previously had *no* fresh attempt identity, so a delayed `COMMIT_ACTIVATION` from an aborted shard-map-A attempt could only be distinguished from map-B's by whole-tuple comparison; stages now fence any activation control message below their highest accepted attempt for the (session, epoch); (4) **segment-checkpoint preparation is explicitly side-effect-free** — `PREPARE_SEGMENT_CHECKPOINT` clones the installed checkpoint, advances the *candidate*, and returns it **without touching live state**; installation happens only after `SEGMENT_COMMIT` is durable; an uncommitted candidate is discarded — giving segment admission the same "state plus tokens, or nothing" property as generation commits (new invariant **I24**).

**Earlier:** v0.8 — atomic `GENERATION_COMMIT` (I19), PREACTIVE/finalize (I20, I21), unified INITIAL/RECOVERY activation. v0.7 — restorable checkpoints + installation (I17), single activation path (I18), I7a/b. v0.6 — durable checkpoints (I15), shards, durable completion (I16), fairness. v0.5 — watermark split, idempotent sampling (I14), catch-up. v0.4 — position discipline (I13), single-session scope, I10a/b. v0.3 — three-case `BEGIN_RECOVERY` (I11), contexts (I12). v0.2 — WAL-before-wire, R3′, ledger, at-least-once SSE, I9.

---

## 0. Roles
**C:** fixed coordinator; owns session lifecycle, the **commit stream** (§2.6a), boundary/event logs, control WAL, placements, admission, tokenizer/detokenizer; constructs only the config-defined initial checkpoint. **S₁…S_P:** contiguous layers; S₁ embeddings; S_P sampler owner and canonical sampler implementation (per shard: latest logits, `SAMPLED` cache with per-position post-sample snapshots, live sampler state, installed checkpoint id, candidate checkpoints). **Client:** OpenAI-compatible HTTP+SSE to C only. Data path C → S₁ → … → S_P → C.

## 1. Identifiers, fencing, transitions

### 1.1 Fence tuple
`cluster_id` · `manifest_hash` · `model_instance_id` [RESERVED] · `placement_version` · `session_id` · `session_epoch` · `recovery_id` (reconstruction attempt; 0 for INITIAL) · **`activation_attempt_id`** (monotonic per (session, epoch); on all activation messages) · `logical_context_id` · `stage_context_generation` · `stage_generation` · `attempt_id` (frame retransmission; telemetry) · `branch_id` [RESERVED].

**F1 (data plane).** Accept iff cluster/manifest/placement match, `session_epoch` current, and the frame's shard is locally attached in a serving-eligible role — **`ACTIVE_FINAL`** for normal decode (never `PREACTIVE`); rebuilding/catching-up for those classes — with matching `stage_context_generation`. Else `ERR_FENCED{…, activation_state}`.
**F2 (control plane).** Epoch/placement/shard/sampler/activation transitions only via precondition-validated messages. **Activation fencing:** a stage rejects any activation control message whose `activation_attempt_id` is below its highest accepted attempt for the (session, epoch).

### 1.2 Coordinator WAL
WAL-before-wire for every transition (`BEGIN_RECOVERY`, `RESET_RECOVERY_ATTEMPT`, placement/shard lifecycle, commit-stream records, `ACTIVATION_COMMIT_INTENT`, `ACTIVATION_COMPLETE`, `ACTIVATION_UNSERVABLE`, aborts, `CANCEL`, admissions). Phase-specific restart (§6.5). No completion record ⇒ in-progress by definition.

### 1.3 Transition machinery
**Epoch transition (I11):** `BEGIN_RECOVERY{session, base e, target e+1, recovery_id r, old_placement_version, truncate_to}` — Case A (first application at base; freeze; truncate applied state > truncate_to (I7a); discard provisional sampled outputs (I7b); enter target; ack). Case B (**pure replay**, to a stage FROZEN/RECOVERING under this exact transition; assert `applied_pos ≤ truncate_to`, fatal on violation — legitimate post-catch-up advancement is handled by `RESET_RECOVERY_ATTEMPT`, never by Case B). Case B′ (a stage in `ACTIVE_FINAL` holding `ACTIVATION_FINALIZED` evidence answers `ERR_RECOVERY_COMPLETED{…, completion_id}` — locally decidable). Case C invalid → `ERR_TRANSITION`.
**Attempt reset (new):**
```text
RESET_RECOVERY_ATTEMPT { session, target_epoch, old_recovery_id, new_recovery_id,
                         truncate_to, committed_sampler_checkpoint_id }
Accepted in: FROZEN, CATCHING_UP, REBUILDING, FROZEN_READY,
             PREACTIVE (iff no ACTIVATION_COMPLETE exists for its tuple).
Atomically: discard applied/KV state above truncate_to; discard attempt-only
  outputs/shards (unless explicitly marked reusable in the new plan); restore the
  committed sampler checkpoint at S_P; adopt new_recovery_id; enter FROZEN; ack.
Never touches: durable ledger, commit stream, stable boundary logs.
Idempotent under (session, target_epoch, new_recovery_id).
```
**Placement:** `PREPARE_PLACEMENT → PLACEMENT_READY`; `INSTALL_PLACEMENT` (WAL) `→ PLACEMENT_INSTALLED` (ready to execute the layer range, nothing more).
**Shard lifecycle:** `CREATE_CONTEXT` (WAL) · `ATTACH_CONTEXT_SHARD → CONTEXT_SHARD_ATTACHED` · rebuild/catch-up · `CONTEXT_READY` / `CATCH_UP_CONTEXT{goal} → CATCH_UP_READY` · sampler install (§2.6b) · activation (§6.6) · `DESTROY_CONTEXT`/`DETACH_SHARD`. All idempotent under their tuples; all WAL'd. **One active session per model instance** (Option A; Option B [RESERVED]; two-session model-check variant required).

## 2. Ledger, positions, watermarks, checkpoints
**Ledger/positions:** as v0.8 — `TokenLedgerEntry{position, token_id, origin, segment_id, rng_checkpoint?}`; input/KV position vs output position (I13): `apply p → KV[p] → logits → optionally sample p+1`.
**Watermarks:** semantic — `ledger_durable_pos`, `generation_durable_pos` (advances only on a complete `GENERATION_COMMIT`), `prefill_stable_pos` (per input segment; applied + mode-durable), `recovery_goal_pos`, `emitted/acked_seq`, `durable_boundary_pos`; derived — `applied_pos(stage, shard)`, retain buffers, and at S_P `next_logits_pos = applied_pos+1`, `sampled_pos ≤ next_logits_pos`, installed checkpoint id, `activation_state`.
**Truncation/goals (2.3c):** input segment ⇒ `truncate_to = prefill_stable_pos`, `goal = segment_end_pos`; DECODING ⇒ both `= generation_durable_pos`; always `truncate_to ≤ goal ≤ ledger_durable_pos`; execution frontier, never ledger frontier.
**Resume rule (2.3d):** below goal ⇒ `APPLY_TOKEN(applied_pos+1)` (or R2-serve); all at goal ⇒ `SAMPLE_NEXT(goal+1)` — only inside a finalized, serviceable activation (I16/I20/I22).
**Shards (2.3e):** logical context vs per-stage shard; `ACTIVE_FINAL` only via activation commit; fresh shards at −1; R2/I1/I14 shard-scoped.
**Input-segment chunks (2.4):** `[a,b)` commits on S_P's `APPLIED_ACK(b−1)` + mode-required durability; `prefill_stable_pos = b−1`.

### 2.6a Commit stream (I19)
Single checksummed append-only stream; recovery recognizes only complete records (partial trailing record discarded). `GENERATION_COMMIT{commit_id, prev, first/last_output_position, token_entries[], committed_sampler_checkpoint_snapshot (REQUIRE generated_through == sampled_pos == last_output_position), checkpoint_id, checksum}` · `SEGMENT_COMMIT{segment_id, token_entries[], resulting candidate snapshot, checkpoint_id, checksum}` · `INITIAL_COMMIT{admission metadata, initial checkpoint}`. `generation_durable_pos` and `committed_sampler_checkpoint_id` advance only on durable records. Because S_P samples ahead, each `SAMPLED{q}` carries `post_sample_state_snapshot(q)`; the record embeds the boundary-matching snapshot, never live state.

### 2.6b Sampler checkpoints — producers, candidates, installation (revised)
`SamplerCheckpoint{checkpoint_id, rng_key, rng_counter, generated_through, serialized_grammar_state, serialized_penalty_state, sampled_pos, sampling_config_hash, state_checksum}`. Producers: C builds only the initial (config-defined) checkpoint; S_P produces all others — per-position snapshots on `SAMPLED`, and for input segments:
```text
PREPARE_SEGMENT_CHECKPOINT{base_checkpoint_id, segment_token_ids, segment_id}:
    candidate := Clone(installed_checkpoint(base));  Advance(candidate, tokens)
    → SEGMENT_CHECKPOINT_READY{candidate_snapshot, resulting_checkpoint_id, checksum}
    LIVE INSTALLED STATE IS NEVER MUTATED (I24).
→ SEGMENT_COMMIT durable → INSTALL_SAMPLER_CHECKPOINT(candidate)
→ SAMPLER_CHECKPOINT_INSTALLED → begin applying the segment.
Uncommitted candidates are discarded (crash, cancellation, failed admission).
```
Installation (`INSTALL_SAMPLER_CHECKPOINT → SAMPLER_CHECKPOINT_INSTALLED`) is idempotent; activation may not commit until the current S_P shard acknowledges the exact checkpoint in the tuple (I17). Segment admission requires an installed sampler shard; S_P failure during `PAUSED_TOOL` ⇒ recovery first.

### 2.7 Invariants
I1 exactly-once apply (shard-scoped) · I2a/I2b durability/stable-frontier monotonicity · I3 ledger determines output · I4 F1/F2 (+ activation-attempt fencing) · I5 durability domains · I6 delivery · I7a/I7b truncatability split · I8 teacher forcing · I9 no hidden committed output · I10a local atomicity · I10b global execution barrier · I11 transition replay idempotence · I12 rebuild isolation · I13 position discipline · I14 exactly-once sampling · I15 sampler rollback · I16 completion follows all `ACTIVATION_COMMITTED` for the matching durable intent · I17 sampler installation before activation · I18 activation commit convergence · I19 durable-prefix alignment (one record or nothing) · I20 completion visibility (`ACTIVE_FINAL` only; never serve from `PREACTIVE`) · I21 unfinalized activation abortability (now: abort ⇒ `FROZEN_READY`, next `activation_attempt_id`, same `recovery_id`; a finalized activation is never aborted) ·
**I22 (post-decision recoverability, new):** participant loss after a durable activation decision cannot force indefinite finalization — the decided activation is either finalized into a serviceable configuration or **superseded by a higher-epoch recovery**, and no data is ever served under the incomplete configuration. ·
**I23 (attempt-reset convergence, new):** restarting an incomplete recovery attempt (`RESET_RECOVERY_ATTEMPT`) returns every surviving participant to a state satisfying the new attempt's truncation and fencing preconditions before reconstruction resumes; Case B remains a pure replay rule. ·
**I24 (candidate isolation):** candidate checkpoints never mutate installed live sampler state before their commit record is durable; an uncommitted candidate leaves no trace. ·
**I25 (abort finality, from TLC-1):** for any (session, epoch, recovery_id, activation_attempt_id), a durable ABORT record and a durable COMPLETE record are mutually exclusive, permanently — completion may never be written for an aborted attempt, and a coordinator restart classifies a durable ABORT as attempt-terminal. Stale pre-abort COMMITTED acknowledgments must never be counted toward completion.

## 3. Commit pipeline
As v0.8: MODEL-PRODUCED (provisional; `SAMPLED{q}` carries snapshot(q)) → COMMITTED (`GENERATION_COMMIT`, I19) → EMITTED (emit-after-commit vs `generation_durable_pos`) → CLIENT-ACKED. Group commit k=8 / 50 ms. `sample_policy: SAMPLE | NO_SAMPLE`; `SAMPLE_NEXT{shard, output_position, sampling_config_hash, expected_sampler_checkpoint_id}` idempotent from retained logits (I14). Tool calls: `PAUSED_TOOL` → prepare candidate (§2.6b) → `SEGMENT_COMMIT` → install → mini-prefill → `SAMPLE_NEXT`.

## 4. Message schema
Header: fence tuple + frame_type + BLAKE3 checksum + declared length, validated before allocation; positions typed. **Data plane:** `APPLY_TOKEN`, `FWD`, `REBUILD_APPLY`, `SAMPLED`(+snapshot), `SAMPLE_NEXT`, `APPLIED_ACK`, `BOUNDARY_COPY`/`DURABILITY_ACK`, `COMMIT_ACK`/`COMMIT_SYNC`. **Control plane:** `BEGIN_RECOVERY`/`RECOVERY_ACK`/`ERR_TRANSITION`/`ERR_RECOVERY_COMPLETED`; **`RESET_RECOVERY_ATTEMPT`/`RESET_ACK`**; placement quartet; shard lifecycle sextet; `PREPARE_SEGMENT_CHECKPOINT`/`SEGMENT_CHECKPOINT_READY`; `INSTALL_SAMPLER_CHECKPOINT`/`SAMPLER_CHECKPOINT_INSTALLED`; `COMMIT_ACTIVATION`/`ACTIVATION_COMMITTED`/`FINALIZE_ACTIVATION`/`ACTIVATION_FINALIZED`/`ACTIVATION_COMMIT_ABORT` (all carrying **`activation_attempt_id`**); `REJOIN`; `CANCEL`/`CLEANED`; `ERR_*`. **Commit-stream/WAL records:** `INITIAL_COMMIT`, `SEGMENT_COMMIT`, `GENERATION_COMMIT`, `ACTIVATION_COMMIT_INTENT`, `ACTIVATION_COMPLETE`, **`ACTIVATION_UNSERVABLE{completion_id, failed_shards, predecessor→superseding link}`**, aborts/resets, admissions.

## 5. Retries and retention
R1 adaptive deadlines; identical logical frame on retry. R2 shard-scoped idempotency; `ERR_GAP` on gaps; in-shard recomputation never authorized. R3′ release requires downstream `APPLIED_ACK ≥ p` ∧ mode-required `DURABILITY_ACK ≥ p`; buffers ≤ max(in-flight window, one durability chunk). R4 exhausted ⇒ suspect ⇒ freeze ⇒ recovery.

## 6. Recovery and activation
**6.1 Detection:** ≥3 missed heartbeats, R4, or new `stage_generation`.
**6.2 Strategy A / 6.3 Strategy B:** as v0.8 (eligibility, catch-up, rebuild, sampler install), ending in the activation commit (§6.6, kind=RECOVERY).
**6.4 Deadlines/attempts (two-level, revised):** every phase has a deadline; a heartbeating stage missing one is reclassified failed. Then: **activation-only failure** (reconstruction intact; e.g., a `COMMIT_ACTIVATION` straggler with shards alive) ⇒ `ACTIVATION_COMMIT_ABORT` (PREACTIVE → FROZEN_READY) and retry activation under `activation_attempt_id + 1`, same `recovery_id`. **Reconstruction-invalidating failure** (a participant's shards/progress lost, or the plan must change) ⇒ `RESET_RECOVERY_ATTEMPT` to all survivors (truncate + FROZEN, `recovery_id + 1`, same base/target epoch), recompute the set, resume from prepare/attach. **Post-decision failure** ⇒ §6.7. Finalized activations are never aborted or reset.
**6.5 Coordinator restart (phase-specific):** replay WAL + commit stream (discard partial trailing record; restore watermarks/checkpoint per I19). The branches below are a **priority-ordered classifier — first match wins — evaluated top to bottom**, mirroring `HydraActivationCore.tla`'s `CoordRestart` `IF/ELSIF` chain exactly. Order is normative: in particular a durable `ACTIVATION_UNSERVABLE` is evaluated **before** the `ACTIVATION_COMPLETE` branch, so a superseded-but-completed activation resumes its superseding recovery and is never re-entered into finalization (**F-UNSERVABLE**; equivalently, the `ACTIVATION_COMPLETE` branch carries an implicit "∧ no durable `ACTIVATION_UNSERVABLE`"). An implementation MUST make the `ACTIVATION_UNSERVABLE` fact durable — its WAL record recorded in the coordinator's durable log at the moment of the decision — so this branch can be read on restart.
1. `ACTIVATION_UNSERVABLE` recorded (for the current epoch's completion) → open/resume the superseding recovery (§6.7).
2. `ACTIVATION_COMPLETE` durable, **not** unservable, all finalized → done (`SERVICEABLE`).
3. `ACTIVATION_COMPLETE` durable, **not** unservable, missing finalized acks → replay `FINALIZE_ACTIVATION`; **if a required participant is permanently lost → §6.7** (record `ACTIVATION_UNSERVABLE`, which reclassifies a subsequent restart to branch 1).
4. **durable ABORT for the current attempt, no COMPLETE → the attempt is terminal; proceed to a new intent at attempt+1 (TLC-1/I25).**
5. commit intent, no `ACTIVATION_COMPLETE` → replay `COMMIT_ACTIVATION` (converge, I18) or abort (I21) or reset (I23).
6. in flight, no commit intent → resume reconstruction under (target, r); Cases A/B reconcile; reset if survivors' progress conflicts with the resumed plan.
7. no activation in flight → fresh transition (or resume INITIAL for a never-activated session).

Stale `ERR_RECOVERY_COMPLETED` remains a fatal audit event.
**6.6 Activation commit (INITIAL | RECOVERY; one mechanism):**
```text
Tuple: { activation_kind, session, epoch (0 for INITIAL), recovery_id,
         activation_attempt_id, placement_version, logical_ctx, active_shard_map,
         recovery_goal_pos, expected_applied_pos per shard, expected_next_logits_pos,
         sampler_checkpoint_id, sampler_state_checksum }
1. C fsyncs ACTIVATION_COMMIT_INTENT{tuple}.
2. COMMIT_ACTIVATION{tuple} → stage validates (incl. its expected_applied_pos and
   attempt fencing) → PREACTIVE(tuple) → ACTIVATION_COMMITTED. Replay ⇒ re-ack (I18).
3. All acks ⇒ C fsyncs ACTIVATION_COMPLETE.
4. FINALIZE_ACTIVATION{completion_id, tuple, record_hash} → PREACTIVE →
   ACTIVE_FINAL(tuple, completion_id); prior shards inactive; S_P adopts installed
   checkpoint → ACTIVATION_FINALIZED.
5. Data plane only after all ACTIVATION_FINALIZED (I16, I20).
Reversals: before step 3's fsync — ACTIVATION_COMMIT_ABORT (→ FROZEN_READY,
attempt+1) or RESET_RECOVERY_ATTEMPT (→ FROZEN, r+1). After step 3 — never
abort/reset; participant loss ⇒ §6.7.
TLC-1 rule (v0.10): step 3 is FORBIDDEN for any attempt with a durable ABORT
record — an aborted attempt id is permanently dead (I25); stale COMMITTED acks
from before the abort must never be counted toward a completion.
```
**6.7 Post-decision participant loss (new; closes the 2PC dead end, I22):**
```text
Precondition: ACTIVATION_COMPLETE durable; a required participant permanently
lost its committed shard (restart with new stage_generation, or removal).
1. C fsyncs ACTIVATION_UNSERVABLE{completion_id, failed_shards}.
2. C finalizes every reachable PREACTIVE participant (the decision stands; their
   state is now the consistent base for supersession). C serves NO data-plane work
   under this configuration — ever.
3. C opens a superseding recovery: base_epoch = the completed activation's epoch,
   target = base + 1, predecessor_completion_id = completion_id; finalized
   survivors take BEGIN_RECOVERY Case A normally (they are ACTIVE_FINAL at base);
   replacement shards are prepared under the new epoch; Strategy A/B as usual
   with truncate_to per §2.3c (nothing was served, so generation_durable_pos is
   unchanged from before the unservable activation).
"All finalized" remains mandatory for SERVING; it is not required for SUPERSEDING.
```

## 7. Durability modes
D0: commit stream + WAL + raw prompt ⇒ Strategy B. D1 (default): + boundary logs durable outside the protected failure domain ⇒ Strategy A. D2: + async-mirrored KV for one fragile stage ⇒ near-immediate failover. Boundary copies synchronous at chunk boundaries, lazy within; contention-group budgets; I5 unconditional.

## 8. Client streaming
Dense SSE `id:`s; UTF-8-aligned events; emission gated on `generation_durable_pos`; event log a pure function of the durable prefix. Resumable at-least-once (`Last-Event-ID`); exactly-once effects via client dedup or ack endpoint; `Idempotency-Key` on creation; bounded buffers with pause-at-commit under TTL.

## 9. Cancellation
WAL `cancel_cutoff_pos = generation_durable_pos`; emit committed-but-unemitted through the cutoff; terminal `cancelled` (I9); `CANCEL` wins at the next frame boundary; only provisional state (I7a/I7b) and uncommitted candidates (I24) suppressed; stages detach shards → `CLEANED`; GC after TTL.

## 10. State machines
```text
SESSION (C):
 CREATED ─INITIAL_COMMIT─▶ PREFILLING ─activation(INITIAL, attempt a)─▶ DECODING ─▶ DRAINING ─▶ CLOSED
    │                                        ▲            │ └▶ PAUSED_TOOL ─candidate▶SEGMENT_COMMIT▶install▶segment─▶ DECODING
    └▶ RECOVERING(target, r):                │            └── DETACHED(TTL) ─▶ CANCELLED
        freeze ▶ prepare ▶ attach ▶ rebuild/catch-up ▶ install sampler
        ▶ ACTIVATION(attempt a): intent ▶ PREACTIVE(all) ▶ COMPLETE ▶ FINALIZE ▶ FINALIZED(all) ▶ resume
        reversals: pre-COMPLETE ─abort─▶ retry activation(a+1) │ ─reset─▶ RECOVERING(target, r+1)
                   post-COMPLETE participant loss ─UNSERVABLE─▶ RECOVERING(target+1, superseding)
 any ─unrecoverable/cancel─▶ FAILED|CANCELLED ─I9 flush─▶ CLOSED

STAGE-SESSION (Sᵢ):
 IDLE ─INSTALL_PLACEMENT + ATTACH─▶ FROZEN…(CATCHING_UP|REBUILDING)…─READY─▶ FROZEN_READY
   FROZEN_READY ─INSTALL_SAMPLER_CHECKPOINT (S_P)─▶ FROZEN_READY(installed)
   FROZEN_READY ─COMMIT_ACTIVATION(tuple, attempt a)─▶ PREACTIVE ─replay─▶ PREACTIVE [re-ack]
   PREACTIVE ─FINALIZE_ACTIVATION─▶ ACTIVE_FINAL(tuple, completion_id)
   PREACTIVE ─ACTIVATION_COMMIT_ABORT─▶ FROZEN_READY(attempt a+1 fence)
   {FROZEN, CATCHING_UP, REBUILDING, FROZEN_READY, PREACTIVE w/o COMPLETE}
        ─RESET_RECOVERY_ATTEMPT─▶ FROZEN(r+1)  [truncate; restore checkpoint; I23]
   ACTIVE_FINAL ─BEGIN_RECOVERY Case A─▶ FROZEN(e+1, r)      [incl. supersession, §6.7]
   ACTIVE_FINAL(completed) ─stale BEGIN_RECOVERY─▶ ERR_RECOVERY_COMPLETED
   FROZEN ─Case B replay─▶ FROZEN [pure replay only]
   any ─CANCEL─▶ RELEASED ─CLEANED─▶ IDLE ;  restart ⇒ new stage_generation ⇒ REJOIN ⇒ IDLE
```

## 11. Scheduler stability contract
As v0.8 (minimum placement lifetime; EWMA triggers; headroom-gated admissibility; one migration at a time; load-shed first; per-failure-set survivable fallback with explicit termination; contention-group airtime shared by all traffic classes).

## 12. Out of scope (reserved)
Option B multi-session transitions; deterministic sampler reconstruction; speculative decoding; beam search; mid-session rebalancing; coordinator election; paged KV; MLA/SSM/SWA KV strategies; phones in the critical path; WAN/NAT; untrusted workers.

---

## Verification plan
**Authority on order-ambiguous prose (from F-UNSERVABLE):** where this spec's prose is ambiguous about the *order* in which transition branches are evaluated, the TLA+ model `HydraActivationCore.tla` is the authority on transition semantics. A prose defect discovered this way (an implementation/simulation-layer finding that the model already handles correctly) amends the prose to match the model — never the reverse without re-ratification of the model.

**Safety (TLA+/PlusCal priority):** I4, I10a/b, I11, I16, I18–I21, **I22, I23, I24**, then I1/I2a/b/I7a/b/I8/I12–I15/I17, plus **I25 (AbortFinality)** as a WAL-level exclusion invariant with its own mutation switch. `PREACTIVE`, `ACTIVATION_UNSERVABLE`, and reset are explicit model states.
**Liveness:** `EventuallyStable` ⊢ Progress — and **I22 upgrades Progress**: participant loss after a durable decision must not falsify it (the superseding path restores an enabled transition; this was the largest remaining liveness hole).
**DST scenarios (I1–I25):** accumulated v0.3–v0.8 regression set, plus v0.9's model-checker targets named by the review: **participant lost after the durable decision** — finalize reachable, record UNSERVABLE, supersede at epoch+1, and assert zero data-plane frames under the incomplete configuration (I22); **attempt restart from FROZEN_READY after catch-up** — survivors at goal=1099 with truncate_to=1023 must be reset, not fatally rejected by Case B, and the abandoned attempt's shards leave no residue (I23); **delayed INITIAL activation messages** — a straggler `COMMIT_ACTIVATION`/abort from shard-map-A (attempt 1) arriving during map-B (attempt 2) must be fenced by `activation_attempt_id` alone (I4/F2); **uncommitted segment candidate** — S_P prepares a candidate, admission fails/C crashes pre-`SEGMENT_COMMIT`, and the live sampler must show no trace of the segment (I24); plus abort-then-retry activation with the same reconstruction (attempt monotonicity), and supersession stacking (a superseding recovery itself suffering post-decision loss — UNSERVABLE chains must terminate under EventuallyStable). Golden-token cross-backend vectors for I3/I8; chaos game-days per report Addendum 2 §H.
