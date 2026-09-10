------------------------------ MODULE HydraActivationCore ------------------------------
(***************************************************************************************)
(* Hydra Session Protocol v0.10 — transition core (package v0.10.1).                   *)
(*                                                                                     *)
(* CHANGELOG                                                                            *)
(*   2026-08-22 — Mutation 3 made CONSISTENT. `CommitAcks`/`FinalAcks` now gate their    *)
(*     attempt filter on `AttemptFencing` (`(AttemptFencing => m.a = a)`). Repairs       *)
(*     F-MUT3-UNREACHABLE (PROJECT_STATE §7.22): the sabotage disabled the stage-side    *)
(*     checks but left these coordinator-side filters hard-coded, so a stale-attempt     *)
(*     stage never produced a countable ack, COMPLETE blocked, and ack-counting alone    *)
(*     reconstituted the guarantee — the designed violation was unreachable BY           *)
(*     CONSTRUCTION (12 complete exhaustions to 4 239 954 distinct states found nothing, *)
(*     while the DST catches the same sabotage at median 187 steps). Authorized under    *)
(*     the mutation framework's too-abstract clause: this STRENGTHENS THE DETECTOR, it   *)
(*     does not adjust the model to make a check pass. With AttemptFencing = TRUE the    *)
(*     implication is equivalent to the old equality, so the faithful model is           *)
(*     behaviourally UNCHANGED; only the mutant differs. Rule 13 still voids every prior *)
(*     checkpoint — moot for the reasons recorded under MaxCkpt below.                   *)
(*   2026-09-10 — spec v0.10.5 (ruling item 1): BEGIN_RECOVERY.truncate_to = EMPTY, the D0    *)
(*                relayed branch (DurabilityD0), stFresh + RelayedSourcing (I26), Mut7          *)
(*                (EmptyDiscards = FALSE); truncateTo = goal in the DECODING regime.            *)
(*   2026-08-22 — MaxCkpt added (CONSTANT + `installedCkpt < MaxCkpt` guard on          *)
(*     PrepareCandidate). Repairs F-UNBOUNDED-SEGMENT (PROJECT_STATE §7.21): the        *)
(*     candidate-checkpoint dimension was the one unbounded-in-reality dimension        *)
(*     without a bounding constant, so prepare→commit formed an unbounded self-loop and *)
(*     TLC hit its 65535-state behaviour-length ceiling before ANY baseline could reach *)
(*     fixpoint. A model-bounding omission of the TLC-3 class — the transitions         *)
(*     themselves are faithful (the protocol does permit unboundedly many segments) and *)
(*     the SPEC is unchanged. Rule 13: this voids every prior TLC checkpoint, which is  *)
(*     moot by construction — `-recover` chaining is dead (§8, closed as superseded)    *)
(*     and the tier-2 certification configs are designed to DRAIN inside one time-box.  *)
(*                                                                                     *)
(* Models ONLY: BEGIN_RECOVERY Cases A/B/B'/C, catch-up/rebuild (abstracted),          *)
(* RESET_RECOVERY_ATTEMPT, the activation transaction (intent / commit / abort /       *)
(* complete / finalize), post-decision participant loss + ACTIVATION_UNSERVABLE +      *)
(* superseding recovery, coordinator & stage crash/restart, and a minimal candidate-   *)
(* checkpoint abstraction for I24. No tensors, tokens, KV, networking payloads.        *)
(*                                                                                     *)
(* Network: `msgs` is a monotonically growing set. Delivery = any enabled receive of   *)
(* any element. Messages are never removed => duplication and reordering are free;     *)
(* loss = a message that is simply never received. Durable WAL writes are separate     *)
(* actions from message sends, so every crash window between "decided" and "told       *)
(* anyone" is reachable.                                                               *)
(*                                                                                     *)
(* Mutation switches (all TRUE = faithful v0.9):                                       *)
(*   EnableUnservable : FALSE removes the ACTIVATION_UNSERVABLE/supersession path      *)
(*                      (expect: liveness violation / deadlock on post-decision loss)  *)
(*   ResetTruncates   : FALSE turns RESET_RECOVERY_ATTEMPT into a label-only r-bump    *)
(*                      (Mut2 — RETIRED 2026-09-09: its sabotage is unreachable by construction *)
(*                      under spec §6.5a; CaseBPure stays in Inv. See VERIFICATION-README.)     *)
(*   AbortTerminal    : FALSE keeps the coordinator in COMMITTING after a durable ABORT      *)
(*                      (Mut4, redesigned 2026-09-09 — with AbortGuardEnabled = FALSE)        *)
(*                      (expect: CaseBPure invariant violation after catch-up)         *)
(*   AttemptFencing   : FALSE disables activation_attempt_id fencing                   *)
(*                      (expect: ServiceSafety violation from a stale INITIAL commit)  *)
(***************************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Stages,            \* e.g. {"s1","s2","s3"}; all are required participants
    MaxEpoch,          \* bound on session epochs           (e.g. 2)
    MaxRId,            \* bound on recovery attempts        (e.g. 2)
    MaxAttempt,        \* bound on activation attempts      (e.g. 2)
    MaxPos,            \* bound on abstract applied positions (e.g. 2)
    MaxCrashes,        \* bound on total crash events => EventuallyStable holds
    MaxCkpt,           \* bound on segment/sampler checkpoint ids (see the note below)
    EnableUnservable, ResetTruncates, AttemptFencing, AbortGuardEnabled, AbortTerminal,
    DurabilityD0, EmptyDiscards,   \* spec v0.10.5 (2026-09-10): the D0 relayed topology; Mut7 switch
    RestartDerivesByMax   \* [2026-09-02 §6.5a] FALSE = MUTATION 5: restart derives the TARGET by MIN

(***************************************************************************************)
(* MaxCkpt — the model-bounding constant added 2026-08-22 (F-UNBOUNDED-SEGMENT,        *)
(* PROJECT_STATE §7.21). Classification: a model-bounding OMISSION of the TLC-3 class,  *)
(* not a protocol defect and not a certification-config tweak.                          *)
(*                                                                                     *)
(* `PrepareCandidate`/`CommitSegmentAndInstall` are FAITHFUL to the protocol: a real    *)
(* session may commit unboundedly many segment checkpoints, and `installedCkpt` is a    *)
(* monotonically increasing id. But model checking requires every dimension that is     *)
(* unbounded-in-reality to carry a bounding constant, exactly as MaxEpoch / MaxRId /    *)
(* MaxAttempt / MaxPos / MaxCrashes already do. Checkpoint count is the same class and  *)
(* was simply missed when the candidate abstraction landed — so without a bound the     *)
(* prepare→commit pair is an unbounded self-loop, every behaviour can be extended       *)
(* forever, and TLC dies on its 65535-state behaviour-length ceiling BEFORE any         *)
(* baseline can reach fixpoint. No `|msgs|` state constraint can rescue that: the       *)
(* obstacle is behaviour LENGTH, not state COUNT.                                       *)
(*                                                                                     *)
(* Value: 2–3 is sufficient. The candidate abstraction's whole behavioural repertoire   *)
(* is prepare → {commit | drop | coordinator-crash}. Two committable ids already give   *)
(* an installed checkpoint that a LATER candidate is prepared against (installedCkpt+1  *)
(* is exercised as a genuine successor, not just as 0→1), plus the drop and crash arms  *)
(* interleaved against a non-initial installed value. A third id adds a second such     *)
(* succession without adding a new SHAPE of interleaving. Beyond that the constant only *)
(* lengthens behaviours — which is precisely the cost this bound exists to remove.      *)
(*                                                                                     *)
(* This is a CORE-model change, so rule 13 voids every prior TLC checkpoint. That is    *)
(* moot by construction: `-recover` chaining is dead (PROJECT_STATE §8, closed as       *)
(* superseded) and the tier-2 certification strategy replaces it — the cert configs are *)
(* designed to DRAIN inside one time-box rather than resume across many.                *)
(***************************************************************************************)
ASSUME MaxCkpt \in Nat /\ MaxCkpt >= 1

ASSUME EnableUnservable \in BOOLEAN /\ ResetTruncates \in BOOLEAN
ASSUME DurabilityD0 \in BOOLEAN /\ EmptyDiscards \in BOOLEAN
       /\ AttemptFencing \in BOOLEAN /\ AbortGuardEnabled \in BOOLEAN /\ AbortTerminal \in BOOLEAN
       /\ RestartDerivesByMax \in BOOLEAN

NoGen == 0
Sym == Permutations(Stages)

VARIABLES
    \* ---- network & durable state ----
    msgs,              \* set of message records (grow-only)
    wal,               \* set of durable coordinator records (grow-only)
    \* ---- coordinator volatile/derived ----
    cState,            \* "IDLE" | "RECOVERY_STARTED" | "RECONSTRUCTING" | "READY_ALL"
                       \* | "ACTIVATION_INTENT_DURABLE" | "COMMITTING" | "ACTIVATION_COMPLETE"
                       \* | "FINALIZING" | "SERVICEABLE" | "UNSERVABLE" | "SUPERSEDING"
                       \* | "CRASHED" | "TERMINAL"
    activeEpoch,       \* last epoch whose activation fully finalized (-1 = none yet)
    recTarget, rId, attempt, actKind,
    truncateTo, goal,
    tupleGen,          \* [Stages -> Nat]: shard generations bound by the current intent
    tupleApplied,      \* expected applied_pos bound by the current intent
    completeDurable, unservable,
    complId, predCompl,
    \* ---- per-stage ----
    stState,           \* [Stages -> {"ACTIVE_FINAL","FROZEN","REBUILDING","FROZEN_READY",
                       \*             "PREACTIVE","LOST"}]
    stEpoch, stRId, stAttempt, stGen, stApplied,
    stFresh,           \* [Stages -> Nat]: how many of a stage's applied positions were applied AT ITS
                       \* CURRENT epoch (fresh emissions a downstream stage may be sourced from) —
                       \* spec v0.10.5 §2.3d: exactly-once emission is scoped to (epoch, position)
    stFinal,           \* [Stages -> BOOLEAN] : holds ACTIVATION_FINALIZED evidence
    \* ---- sampler-candidate abstraction (I24) ----
    installedCkpt,     \* Nat: id of installed sampler checkpoint
    candidateCkpt,     \* Nat: 0 = none; else a prepared, uncommitted candidate id
    segCommitted,      \* set of durably committed candidate ids
    \* ---- bookkeeping ----
    crashes,           \* crash counter (bounded by MaxCrashes)
    caseBviolation, sourceViolation,    \* set TRUE if Case B's applied<=truncate_to assertion trips
    sourceViolation,   \* set TRUE if a downstream stage applied a position its upstream never
                       \* freshly emitted at this epoch (spec v0.10.5 I26; checked when applied)
    servedCount        \* number of ServeDataPlane events (diagnostic)

vars == << msgs, wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
           truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable,
           complId, predCompl, stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFresh,
           stFinal, installedCkpt, candidateCkpt, segCommitted, crashes,
           caseBviolation, sourceViolation, servedCount >>

--------------------------------------------------------------------------------------
(* Helpers *)

StateConstraint == Cardinality(msgs) <= 20

(* -------- spec v0.10.5 (design authority 2026-09-10, ruling item 1): the D0 relayed topology -------- *)
(* BEGIN_RECOVERY.truncate_to gains the distinguished sentinel EMPTY: a stage taking Case A with it   *)
(* discards its applied/KV state ENTIRELY and rebuilds from position 0 at the target epoch. H3's     *)
(* bound 0 <= truncate_to < n_ctx stands for every non-sentinel value. The coordinator chooses EMPTY  *)
(* in D0 when a lost stage is DOWNSTREAM of a survivor: the survivor cannot re-emit the positions    *)
(* it holds (spec 2.3d), so the whole pipeline rebuilds. Positions in this model are counts (a       *)
(* stage holding n positions has stApplied = n; 0 = empty), so EMPTY is carried as its own field.    *)
(* The relayed chain: First is the head; every other stage is downstream of it (2-stage model).      *)
First == CHOOSE s \in Stages : TRUE
Upstream(u, d) == u = First /\ d # First
\* The frontier a stage will hold after BEGIN(trunc): a LOST stage rejoins empty. A downstream stage
\* rebuilds from its frontier + 1; an upstream survivor never re-emits what it holds (§2.3d), so the
\* pipeline can be sourced iff no downstream frontier lies BELOW an upstream survivor's. When one does,
\* the BEGIN carries EMPTY and everyone rebuilds from 0. The ruled instance: a lost stage downstream
\* of a survivor (frontier 0 < the survivor's). The same rule also covers a fence-forward over a
\* pipeline whose frontiers disagree (a downstream stage still REBUILDING when the coordinator
\* crashed) — without it the faithful D0 baseline violates RelayedSourcing there (local smoke,
\* 2026-09-10); the product refuses that case by name (PROJECT_STATE §8), so the general rule is the
\* model's/spec's and the product implements the loss instance.
MinI(a, b) == IF a <= b THEN a ELSE b
FrontierAfter(s, trunc) == IF stState[s] = "LOST" THEN 0 ELSE MinI(stApplied[s], trunc)
NeedEmptyFor(trunc) == DurabilityD0 /\ \E u, d \in Stages :
    Upstream(u, d) /\ stState[u] # "LOST" /\ FrontierAfter(d, trunc) < FrontierAfter(u, trunc)
FreshStart(s) == stApplied[s] - stFresh[s]
\* Position p of the upstream u is a FRESH emission at u's current epoch iff it lies in u's window.
InFreshWindow(u, p) == p > FreshStart(u) /\ p <= stApplied[u]
BeginRecs == { rec \in wal : rec.t = "BEGIN" }
LatestBeginAt(t) == CHOOSE b \in BeginRecs : b.tgt = t /\ \A b2 \in BeginRecs : b2.tgt = t => b2.r <= b.r
MaxI(a, b) == IF a >= b THEN a ELSE b

Send(m)  == msgs' = msgs \cup {m}
Wal(r)   == wal'  = wal  \cup {r}

\* ReadyAcks is round-scoped (r), NOT attempt-scoped, so it carries no `m.a` filter and is not
\* part of the fencing surface below.
ReadyAcks     == { m \in msgs : m.t = "READY"     /\ m.tgt = recTarget /\ m.r = rId }
\* [HYDRA 2026-08-22, §7.22 F-MUT3-UNREACHABLE] The attempt filter on the coordinator's evidence
\* sets is GATED ON THE SAME SWITCH as the stage-side checks.
\*
\* Why: Mutation 3 (`AttemptFencing = FALSE`) was an INCONSISTENT sabotage. It disabled the
\* stage-side attempt checks but left these two coordinator-side filters hard-coded to `m.a = a`.
\* A stale-attempt stage therefore never yielded a countable ack, `AllCommitted` never held,
\* `CoordWriteComplete` blocked, and ack-counting alone reconstituted the guarantee the mutation
\* was supposed to remove — so the designed violation was unreachable BY CONSTRUCTION. Twelve
\* complete exhaustions (to 4 239 954 distinct states) found no counterexample, while the
\* implementation-level DST catches the same sabotage at median 187 steps: the model's sabotage
\* was not modelling the implementation's defect (stale acks COUNTED — TLC-1's class).
\*
\* Those exhaustions are kept as a positive result, not discarded: at this abstraction, consistent
\* ack discipline SUBSUMES attempt fencing (defence in depth, §7.22).
\*
\* With `AttemptFencing = TRUE` the implication `(TRUE => m.a = a)` is equivalent to `m.a = a`, so
\* the FAITHFUL model is behaviourally unchanged; only the mutant differs. This strengthens the
\* detector — it does not adjust the model to make a check pass (rule 6 untouched).
CommitAcks(a) == { m \in msgs : m.t = "COMMITTED" /\ m.tgt = recTarget /\ m.r = rId
                                                   /\ (AttemptFencing => m.a = a) }
FinalAcks(a)  == { m \in msgs : m.t = "FINALIZED" /\ m.tgt = recTarget
                                                   /\ (AttemptFencing => m.a = a) }
AcksFrom(S)   == { m.s : m \in S }

AllReady      == AcksFrom(ReadyAcks)        = Stages
AllCommitted  == AcksFrom(CommitAcks(attempt)) = Stages
AllFinalized  == AcksFrom(FinalAcks(attempt))  = Stages

Min(x,y) == IF x < y THEN x ELSE y

--------------------------------------------------------------------------------------
(* Init: session admitted; INITIAL activation pending at epoch 0 through the same     *)
(* machinery as recovery (v0.9 §6.6, activation_kind = INITIAL).                       *)

Init ==
    /\ msgs = {} /\ crashes = 0 /\ caseBviolation = FALSE /\ sourceViolation = FALSE /\ servedCount = 0
    /\ wal = { [t |-> "BEGIN", base |-> -1, tgt |-> 0, r |-> 0, trunc |-> 0, empty |-> FALSE] }
    /\ cState = "RECONSTRUCTING" /\ activeEpoch = -1
    /\ recTarget = 0 /\ rId = 0 /\ attempt = 0 /\ actKind = "INITIAL"
    /\ truncateTo = 0 /\ goal \in 1..MaxPos
    /\ tupleGen = [s \in Stages |-> NoGen] /\ tupleApplied = 0
    /\ completeDurable = FALSE /\ unservable = FALSE
    /\ complId = 0 /\ predCompl = 0
    /\ stState  = [s \in Stages |-> "FROZEN"]
    /\ stEpoch  = [s \in Stages |-> 0]
    /\ stRId    = [s \in Stages |-> 0]
    /\ stAttempt= [s \in Stages |-> 0]
    /\ stGen    = [s \in Stages |-> 1]
    /\ stApplied= [s \in Stages |-> 0]
    /\ stFresh  = [s \in Stages |-> 0]
    /\ stFinal  = [s \in Stages |-> FALSE]
    /\ installedCkpt = 1 /\ candidateCkpt = 0 /\ segCommitted = {1}

--------------------------------------------------------------------------------------
(* -------- Recovery start / BEGIN_RECOVERY delivery (spec §1.3) -------- *)

CoordBeginRecovery ==      \* new semantic recovery (failure while SERVICEABLE)
    /\ cState = "SERVICEABLE" /\ activeEpoch < MaxEpoch
    /\ \E s \in Stages : stState[s] = "LOST"           \* a reason to recover
    \* v0.10.5: the EMPTY choice is made HERE, at WAL-write time, and carried by the durable record —
    \* WAL-before-wire; SendBeginRecovery sends what the record says (a rejoin between the write and
    \* the send must not change the choice).
    /\ Wal([t |-> "BEGIN", base |-> activeEpoch, tgt |-> activeEpoch + 1,
            r |-> 0, trunc |-> goal, empty |-> NeedEmptyFor(goal)])
    /\ cState' = "RECOVERY_STARTED" /\ recTarget' = activeEpoch + 1
    /\ rId' = 0 /\ attempt' = 0 /\ actKind' = "RECOVERY"
    /\ completeDurable' = FALSE /\ unservable' = FALSE
    /\ truncateTo' = goal    \* DECODING regime (§2.3c): truncate_to = goal = the durable frontier —
                             \* a survivor KEEPS its prefix (v0.10.5; before, truncateTo was the
                             \* constant 0 and every survivor silently rebuilt from nothing, which
                             \* is why the model never saw the sourcing problem the product hit)
    /\ UNCHANGED << msgs, activeEpoch, goal, tupleGen, tupleApplied, complId,
        predCompl, stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFresh, stFinal,
        installedCkpt, candidateCkpt, segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

CoordStartSuperseding ==   \* §6.7 step 3: supersede a decided-but-unservable activation
    /\ cState = "SUPERSEDING" /\ recTarget < MaxEpoch
    /\ Wal([t |-> "BEGIN", base |-> recTarget, tgt |-> recTarget + 1,
            r |-> 0, trunc |-> goal, empty |-> NeedEmptyFor(goal)])
    /\ cState' = "RECOVERY_STARTED"
    /\ predCompl' = complId
    /\ recTarget' = recTarget + 1 /\ rId' = 0 /\ attempt' = 0 /\ actKind' = "RECOVERY"
    /\ completeDurable' = FALSE /\ unservable' = FALSE
    /\ truncateTo' = goal
    /\ UNCHANGED << msgs, activeEpoch, goal, tupleGen, tupleApplied,
        complId, stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFresh, stFinal,
        installedCkpt, candidateCkpt, segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

SendBeginRecovery ==
    /\ cState = "RECOVERY_STARTED"
    /\ Send([t |-> "BEGIN", base |-> recTarget - 1, tgt |-> recTarget,
             r |-> rId, trunc |-> truncateTo, empty |-> LatestBeginAt(recTarget).empty])
    /\ cState' = "RECONSTRUCTING"
    /\ UNCHANGED << wal, activeEpoch, recTarget, rId, attempt, actKind, truncateTo,
        goal, tupleGen, tupleApplied, completeDurable, unservable, complId, predCompl,
        stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFresh, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

StageRecvBeginAt(s, tEpoch, r0) ==
    \E m \in msgs : /\ m.t = "BEGIN" /\ m.tgt = tEpoch /\ m.r = r0
        /\ \/ (* --- Case A: first application at base --- *)
              (* 2026-09-03 (§6.5a): REBUILDING / FROZEN_READY at base are admitted too — a stage
                 caught up for an activation that never committed (the coordinator crashed first
                 and fenced forward). Before §6.5a a BEGIN could only reach ACTIVE_FINAL/FROZEN
                 stages; the fence-forward restart made this state reachable, and the product
                 oracle's third window found the model and the code dropping it as Case C. *)
              /\ stState[s] \in {"ACTIVE_FINAL", "FROZEN", "REBUILDING", "FROZEN_READY"} /\ stEpoch[s] = m.base
              /\ stState'  = [stState  EXCEPT ![s] = "FROZEN"]
              /\ stEpoch'  = [stEpoch  EXCEPT ![s] = m.tgt]
              /\ stRId'    = [stRId    EXCEPT ![s] = m.r]
              /\ stApplied'= [stApplied EXCEPT ![s] =
                    IF m.empty THEN (IF EmptyDiscards THEN 0 ELSE stApplied[s])   \* MUTATION 7
                               ELSE Min(stApplied[s], m.trunc)]
              /\ stFresh'  = [stFresh  EXCEPT ![s] = 0]
              /\ stFinal'  = [stFinal  EXCEPT ![s] = FALSE]
              /\ Send([t |-> "RACK", s |-> s, tgt |-> m.tgt, r |-> m.r])
              /\ UNCHANGED << stAttempt, caseBviolation, sourceViolation >>
           \/ (* --- PREACTIVE revert (spec §1.3 model-fidelity, F-LIVENESS-FAIR family 3) ---
                 spec §1.3: "a stage in PREACTIVE receiving BEGIN_RECOVERY for the next
                 attempt/epoch treats it per the abort rule: PREACTIVE is reversible." Case A
                 above accepted only {ACTIVE_FINAL, FROZEN}, so a post-supersession stage left
                 PREACTIVE was marooned (no RESET could outrank its stRId once rId was exhausted).
                 Revert per the abort rule: discard the preactive tuple, freeze at target, adopt
                 r, truncate, ack. (Same body as Case A; mirror of F-UNSERVABLE -- spec right,
                 model previously incomplete.) *)
              /\ stState[s] = "PREACTIVE" /\ stEpoch[s] = m.base
              /\ stState'  = [stState  EXCEPT ![s] = "FROZEN"]
              /\ stEpoch'  = [stEpoch  EXCEPT ![s] = m.tgt]
              /\ stRId'    = [stRId    EXCEPT ![s] = m.r]
              /\ stApplied'= [stApplied EXCEPT ![s] =
                    IF m.empty THEN (IF EmptyDiscards THEN 0 ELSE stApplied[s])   \* MUTATION 7
                               ELSE Min(stApplied[s], m.trunc)]
              /\ stFresh'  = [stFresh  EXCEPT ![s] = 0]
              /\ stFinal'  = [stFinal  EXCEPT ![s] = FALSE]
              /\ Send([t |-> "RACK", s |-> s, tgt |-> m.tgt, r |-> m.r])
              /\ UNCHANGED << stAttempt, caseBviolation, sourceViolation >>
           \/ (* --- Case B: PURE replay to a frozen stage of this transition --- *)
              /\ stState[s] = "FROZEN" /\ stEpoch[s] = m.tgt /\ m.r >= stRId[s]
              /\ caseBviolation' = (caseBviolation \/ stApplied[s] > m.trunc)
              /\ stRId' = [stRId EXCEPT ![s] = m.r]
              /\ Send([t |-> "RACK", s |-> s, tgt |-> m.tgt, r |-> m.r])
              /\ UNCHANGED << stState, stEpoch, stAttempt, stApplied, stFresh, stFinal >>
           \/ (* --- Case B': locally-decidable completed activation --- *)
              /\ stState[s] = "ACTIVE_FINAL" /\ stEpoch[s] = m.tgt /\ stFinal[s]
              /\ Send([t |-> "ERR_COMPLETED", s |-> s, tgt |-> m.tgt])
              /\ UNCHANGED << stState, stEpoch, stRId, stAttempt, stApplied, stFresh, stFinal,
                              caseBviolation, sourceViolation >>
    /\ UNCHANGED << wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stGen, installedCkpt, candidateCkpt, segCommitted, crashes, sourceViolation, servedCount >>

StageRejoin(s) ==          \* a LOST stage joins the in-flight reconstruction: fresh
    /\ stState[s] = "LOST" \* shard (abstracts ATTACH_CONTEXT_SHARD), applied = 0
    /\ cState \in {"RECONSTRUCTING", "RECOVERY_STARTED"}
    /\ stState'  = [stState  EXCEPT ![s] = "FROZEN"]
    /\ stEpoch'  = [stEpoch  EXCEPT ![s] = recTarget]
    /\ stRId'    = [stRId    EXCEPT ![s] = rId]
    /\ stApplied'= [stApplied EXCEPT ![s] = 0]
    /\ stFresh'  = [stFresh  EXCEPT ![s] = 0]
    /\ stFinal'  = [stFinal  EXCEPT ![s] = FALSE]
    /\ Send([t |-> "RACK", s |-> s, tgt |-> recTarget, r |-> rId])
    /\ UNCHANGED << wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stAttempt, stGen, installedCkpt, candidateCkpt, segCommitted,
        crashes, caseBviolation, sourceViolation, servedCount >>

StageRebuildStep(s) ==     \* CatchUpOrRebuild: advance toward goal, then READY-ack
    /\ stState[s] \in {"FROZEN", "REBUILDING"} /\ stEpoch[s] = recTarget
    /\ cState \in {"RECONSTRUCTING", "READY_ALL"}
    /\ IF stApplied[s] < goal
       THEN \* v0.10.5, D0: the relayed data plane — a downstream stage's activation for position p
            \* comes only from its upstream's forward of p, so it applies p after the upstream did,
            \* at the same epoch. (Whether that forward is a FRESH emission is RelayedSourcing's
            \* question, checked as an invariant, not assumed here — so a mutation can violate it.)
            /\ (DurabilityD0 => \A u \in Stages : Upstream(u, s) =>
                                    (stEpoch[u] = stEpoch[s] /\ stApplied[s] < stApplied[u]))
            /\ stState'   = [stState  EXCEPT ![s] = "REBUILDING"]
            /\ stApplied' = [stApplied EXCEPT ![s] = stApplied[s] + 1]
            /\ stFresh'   = [stFresh   EXCEPT ![s] = stFresh[s] + 1]
            /\ sourceViolation' = (sourceViolation \/
                  (DurabilityD0 /\ \E u \in Stages : Upstream(u, s) /\ ~InFreshWindow(u, stApplied[s] + 1)))
            /\ msgs' = msgs
       ELSE /\ stState'   = [stState  EXCEPT ![s] = "FROZEN_READY"]
            /\ stApplied' = stApplied /\ stFresh' = stFresh /\ sourceViolation' = sourceViolation
            /\ Send([t |-> "READY", s |-> s, tgt |-> recTarget, r |-> stRId[s],
                     gen |-> stGen[s], ap |-> stApplied[s]])
    /\ UNCHANGED << wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stEpoch, stRId, stAttempt, stGen, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, servedCount >>

--------------------------------------------------------------------------------------
(* -------- RESET_RECOVERY_ATTEMPT (spec §1.3, I23) -------- *)

CoordResetAttempt ==
    /\ cState \in {"RECONSTRUCTING", "READY_ALL", "ACTIVATION_INTENT_DURABLE",
                   "COMMITTING"}
    /\ ~completeDurable /\ rId < MaxRId
    /\ Wal([t |-> "RESET", tgt |-> recTarget, oldr |-> rId, newr |-> rId + 1])
    /\ Send([t |-> "RESET", tgt |-> recTarget, newr |-> rId + 1, trunc |-> truncateTo])
    /\ rId' = rId + 1 /\ cState' = "RECONSTRUCTING"
    /\ UNCHANGED << activeEpoch, recTarget, attempt, actKind, truncateTo, goal,
        tupleGen, tupleApplied, completeDurable, unservable, complId, predCompl,
        stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFresh, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

StageRecvResetAt(s, nr) ==
    \E m \in msgs :
        /\ m.t = "RESET" /\ m.newr = nr /\ m.tgt = stEpoch[s] /\ nr > stRId[s]
        /\ stState[s] \in {"FROZEN", "REBUILDING", "FROZEN_READY", "PREACTIVE"}
        /\ stState'   = [stState EXCEPT ![s] = "FROZEN"]
        /\ stRId'     = [stRId   EXCEPT ![s] = m.newr]
        /\ stApplied' = [stApplied EXCEPT ![s] =
                            IF ResetTruncates THEN Min(stApplied[s], m.trunc)
                                              ELSE stApplied[s]]      \* MUTATION 2
        /\ stFresh'   = [stFresh EXCEPT ![s] =                        \* the window follows the cut
                            IF ResetTruncates THEN MaxI(0, Min(stApplied[s], m.trunc) - (stApplied[s] - stFresh[s]))
                                              ELSE stFresh[s]]
        /\ Send([t |-> "RSACK", s |-> s, tgt |-> m.tgt, r |-> m.newr])
    /\ UNCHANGED << wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stEpoch, stAttempt, stGen, stFinal, installedCkpt, candidateCkpt,
        segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

--------------------------------------------------------------------------------------
(* -------- Activation transaction (spec §6.6) -------- *)

CoordWriteIntent ==
    /\ cState \in {"RECONSTRUCTING", "READY_ALL"} /\ AllReady /\ attempt < MaxAttempt
    /\ LET gens == [s \in Stages |-> (CHOOSE m \in ReadyAcks : m.s = s).gen]
       IN /\ Wal([t |-> "INTENT", tgt |-> recTarget, r |-> rId, a |-> attempt + 1,
                  gens |-> gens, ap |-> goal])
          /\ tupleGen' = gens /\ tupleApplied' = goal
    /\ attempt' = attempt + 1 /\ cState' = "ACTIVATION_INTENT_DURABLE"
    /\ UNCHANGED << msgs, activeEpoch, recTarget, rId, actKind, truncateTo, goal,
        completeDurable, unservable, complId, predCompl, stState, stEpoch, stRId,
        stAttempt, stGen, stApplied, stFresh, stFinal, installedCkpt, candidateCkpt,
        segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

CoordSendCommit ==
    /\ cState = "ACTIVATION_INTENT_DURABLE"
    /\ Send([t |-> "COMMIT", tgt |-> recTarget, r |-> rId, a |-> attempt,
             gens |-> tupleGen, ap |-> tupleApplied])
    /\ cState' = "COMMITTING"
    /\ UNCHANGED << wal, activeEpoch, recTarget, rId, attempt, actKind, truncateTo,
        goal, tupleGen, tupleApplied, completeDurable, unservable, complId, predCompl,
        stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFresh, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

StageRecvCommitAt(s, a0) ==
    \E m \in msgs :
        /\ m.t = "COMMIT" /\ m.a = a0 /\ m.tgt = stEpoch[s] /\ m.r = stRId[s]
        /\ m.gens[s] = stGen[s] /\ m.ap = stApplied[s]
        /\ (AttemptFencing => m.a \in {stAttempt[s], stAttempt[s] + 1}) \* MUTATION 3 (H1: bounded window)
        /\ \/ /\ stState[s] = "FROZEN_READY"
              /\ stState'   = [stState   EXCEPT ![s] = "PREACTIVE"]
              /\ stAttempt' = [stAttempt EXCEPT ![s] = m.a]
           \/ /\ stState[s] = "PREACTIVE" /\ stAttempt[s] = m.a      \* idempotent replay
              /\ UNCHANGED << stState, stAttempt >>
        /\ Send([t |-> "COMMITTED", s |-> s, tgt |-> m.tgt, r |-> m.r, a |-> m.a])
    /\ UNCHANGED << wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stEpoch, stRId, stGen, stApplied, stFresh, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

CoordAbortActivation ==                            \* pre-decision only (I21)
    /\ cState = "COMMITTING" /\ ~completeDurable
    /\ Wal([t |-> "ABORT", tgt |-> recTarget, r |-> rId, a |-> attempt])
    /\ Send([t |-> "ABORT", tgt |-> recTarget, r |-> rId, a |-> attempt])
    \* MUTATION 4, redesigned 2026-09-09 (design authority, item 3): abort finality's LIVE decision
    \* is "a durable ABORT ends the attempt" — the coordinator leaves COMMITTING, so the stale
    \* ACTIVATION_COMMITTED acks still in the network can never complete it. The first Mut4 (the
    \* I25 write-guard off) only bit through the restart-replay path spec §6.5a removed; the
    \* redesign sabotages the decision itself: with AbortTerminal = FALSE the abort is written and
    \* sent but the coordinator STAYS in COMMITTING, and (with the write-guard also off) the
    \* lingering acks reach CoordWriteComplete — AbortFinality (I25) must fire without any crash.
    /\ cState' = (IF AbortTerminal THEN "READY_ALL" ELSE "COMMITTING")
    /\ UNCHANGED << activeEpoch, recTarget, rId, attempt, actKind, truncateTo, goal,
        tupleGen, tupleApplied, completeDurable, unservable, complId, predCompl,
        stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFresh, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

StageRecvAbortAt(s, a0) ==
    \E m \in msgs :
        /\ m.t = "ABORT" /\ m.a = a0 /\ m.tgt = stEpoch[s] /\ m.r = stRId[s]
        /\ stState[s] = "PREACTIVE" /\ stAttempt[s] = m.a /\ ~stFinal[s]
        /\ stState' = [stState EXCEPT ![s] = "FROZEN_READY"]
    /\ UNCHANGED << msgs, wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stEpoch, stRId, stAttempt, stGen, stApplied, stFresh, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

AttemptAborted(a) ==
    \E rec \in wal : rec.t = "ABORT" /\ rec.tgt = recTarget /\ rec.r = rId /\ rec.a = a

CoordWriteComplete ==                              \* the irrevocable decision
    /\ cState = "COMMITTING" /\ AllCommitted /\ ~completeDurable
    /\ (AbortGuardEnabled => ~AttemptAborted(attempt))   \* TLC-1 / I25; MUTATION 4
    /\ Wal([t |-> "COMPLETE", tgt |-> recTarget, r |-> rId, a |-> attempt,
            cid |-> complId + 1])
    /\ completeDurable' = TRUE /\ complId' = complId + 1
    /\ cState' = "ACTIVATION_COMPLETE"
    /\ UNCHANGED << msgs, activeEpoch, recTarget, rId, attempt, actKind, truncateTo,
        goal, tupleGen, tupleApplied, unservable, predCompl, stState, stEpoch, stRId,
        stAttempt, stGen, stApplied, stFresh, stFinal, installedCkpt, candidateCkpt,
        segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

CoordSendFinalize ==
    /\ cState = "ACTIVATION_COMPLETE"
    /\ Send([t |-> "FINALIZE", tgt |-> recTarget, r |-> rId, a |-> attempt,
             cid |-> complId])
    /\ cState' = "FINALIZING"
    /\ UNCHANGED << wal, activeEpoch, recTarget, rId, attempt, actKind, truncateTo,
        goal, tupleGen, tupleApplied, completeDurable, unservable, complId, predCompl,
        stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFresh, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

StageRecvFinalizeAt(s, a0) ==
    \E m \in msgs :
        /\ m.t = "FINALIZE" /\ m.a = a0 /\ m.tgt = stEpoch[s]
        /\ stState[s] = "PREACTIVE"
        /\ (AttemptFencing => m.a = stAttempt[s])                     \* MUTATION 3
        /\ stState' = [stState EXCEPT ![s] = "ACTIVE_FINAL"]
        /\ stFinal' = [stFinal EXCEPT ![s] = TRUE]
        /\ Send([t |-> "FINALIZED", s |-> s, tgt |-> m.tgt, a |-> m.a])
    /\ UNCHANGED << wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stEpoch, stRId, stAttempt, stGen, stApplied, stFresh, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

CoordBecomeServiceable ==
    /\ cState = "FINALIZING" /\ AllFinalized /\ ~unservable
    /\ cState' = "SERVICEABLE" /\ activeEpoch' = recTarget
    /\ UNCHANGED << msgs, wal, recTarget, rId, attempt, actKind, truncateTo, goal,
        tupleGen, tupleApplied, completeDurable, unservable, complId, predCompl,
        stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFresh, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

ServeDataPlane ==
    /\ cState = "SERVICEABLE"
    /\ servedCount' = servedCount + 1 /\ servedCount < 2         \* bound diagnostics
    /\ UNCHANGED << msgs, wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFresh, stFinal,
        installedCkpt, candidateCkpt, segCommitted, crashes, caseBviolation, sourceViolation >>

--------------------------------------------------------------------------------------
(* -------- Post-decision participant loss (spec §6.7, I22) -------- *)

CoordRecordUnservable ==
    /\ EnableUnservable                                              \* MUTATION 1
    /\ cState \in {"ACTIVATION_COMPLETE", "FINALIZING"}
    /\ completeDurable /\ ~AllFinalized
    /\ \E s \in Stages : stState[s] = "LOST"        \* participant permanently lost
    /\ Wal([t |-> "UNSERVABLE", cid |-> complId])
    /\ unservable' = TRUE /\ cState' = "SUPERSEDING"
    /\ UNCHANGED << msgs, activeEpoch, recTarget, rId, attempt, actKind, truncateTo,
        goal, tupleGen, tupleApplied, completeDurable, complId, predCompl, stState,
        stEpoch, stRId, stAttempt, stGen, stApplied, stFresh, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

--------------------------------------------------------------------------------------
(* -------- Crashes and restarts -------- *)

StageCrash(s) ==            \* shard loss: LOST + new stage generation
    /\ crashes < MaxCrashes /\ stState[s] # "LOST"
    /\ crashes'   = crashes + 1
    /\ stState'   = [stState   EXCEPT ![s] = "LOST"]
    /\ stGen'     = [stGen     EXCEPT ![s] = stGen[s] + 1]
    /\ stApplied' = [stApplied EXCEPT ![s] = 0]
    /\ stFresh'   = [stFresh   EXCEPT ![s] = 0]
    /\ stFinal'   = [stFinal   EXCEPT ![s] = FALSE]
    /\ UNCHANGED << msgs, wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stEpoch, stRId, stAttempt, installedCkpt, candidateCkpt,
        segCommitted, caseBviolation, sourceViolation, servedCount >>

(* [2026-09-02, spec §6.5a — design authority] THE COORDINATOR'S VOLATILE STATE DOES NOT       *)
(* SURVIVE A CRASH. Until this amendment CoordCrash left activeEpoch/recTarget/rId/attempt/…     *)
(* UNCHANGED, so the model quietly assumed a restarted PROCESS still knew them; a real process    *)
(* knows only its WAL. Crash now sets every volatile coordinator variable to a distinguished ⊥   *)
(* and CoordRestart RE-DERIVES them from `wal` (the durable truth) — and fences forward.           *)
CoordCrash ==
    /\ crashes < MaxCrashes /\ cState # "CRASHED"
    /\ crashes' = crashes + 1 /\ cState' = "CRASHED"
    /\ candidateCkpt' = 0                       \* volatile candidates die with C's peer
    /\ activeEpoch' = -1 /\ recTarget' = -1 /\ rId' = -1 /\ attempt' = -1
    /\ actKind' = "BOT" /\ truncateTo' = -1
    /\ tupleGen' = [s \in Stages |-> NoGen] /\ tupleApplied' = -1
    /\ completeDurable' = FALSE /\ unservable' = FALSE /\ complId' = -1 /\ predCompl' = -1
    \* `goal` is NOT volatile: it is the input frontier the COMMIT STREAM records (prompt length /
    \* durable positions), re-read on restart from that durable log — the model keeps it.
    /\ UNCHANGED << msgs, wal, goal, stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFresh, stFinal,
        installedCkpt, segCommitted, caseBviolation, sourceViolation, servedCount >>

(* [2026-09-02, spec §6.5a] RESTART = DERIVE FROM THE WAL, CLASSIFY, FENCE FORWARD.                *)
(*                                                                                               *)
(* Every value the pre-amendment action read from a variable is now a function of `wal`:         *)
(*   dTarget   = max BEGIN.tgt            (the latest recovery's target; Init writes tgt 0)        *)
(*               [MUTATION 5 (RestartDerivesByMax = FALSE): min instead of max]                   *)
(*   dRId      = max over BEGIN.r / RESET.newr at dTarget                                         *)
(*   dAttempt  = max INTENT.a at (dTarget, dRId), 0 if none                                     *)
(*   dEpoch    = max INTENT.tgt, -1 if none  (activeEpoch as the ruling defines it)                *)
(*   dTrunc    = trunc of the latest BEGIN at dTarget                                             *)
(*   dComplete = a COMPLETE at (dTarget, dRId, dAttempt); dComplId = max COMPLETE.cid (0 if none) *)
(*   dUnserv   = an UNSERVABLE naming dComplId                                                    *)
(* Branch order is §6.5's and remains load-bearing (F-UNSERVABLE): UNSERVABLE before COMPLETE.    *)
(* A durable COMPLETE carries completion evidence and is finished (I22); everything else fences   *)
(* forward — a new recovery at rId+1 targeting epoch+1, so an INTENT without completion evidence  *)
(* is never resumed. Bound exhaustion resolves by explicit termination (§11), never a crash loop. *)
(* SOUNDNESS: INTENT and BEGIN are written (Wal) before sent (Send) — "sent implies durable" — so *)
(* the durable maxima are ≥ anything any stage has seen, and the new recovery outranks all of it. *)
Begins    == { rec \in wal : rec.t = "BEGIN" }
Intents   == { rec \in wal : rec.t = "INTENT" }
Completes == { rec \in wal : rec.t = "COMPLETE" }
MaxOf(S, dflt) == IF S = {} THEN dflt ELSE CHOOSE x \in S : \A y \in S : y <= x
MinOf(S, dflt) == IF S = {} THEN dflt ELSE CHOOSE x \in S : \A y \in S : y >= x
\* MUTATION 5 lives HERE, on the TARGET: with fence-forward, a minimum-derived ATTEMPT is harmless by
\* construction (the new epoch resets the attempt space), but a minimum-derived TARGET re-opens a
\* recovery below one already in flight — a stale BEGIN that frozen stages accept as Case B replay.
DTarget   == IF RestartDerivesByMax THEN MaxOf({ b.tgt : b \in Begins }, 0)
                                    ELSE MinOf({ b.tgt : b \in Begins }, 0)
DRId      == MaxOf({ b.r : b \in { bb \in Begins : bb.tgt = DTarget } }
                   \cup { rs.newr : rs \in { r0 \in wal : r0.t = "RESET" /\ r0.tgt = DTarget } }, 0)
DAttemptSet == { i.a : i \in { ii \in Intents : ii.tgt = DTarget /\ ii.r = DRId } }
DAttempt  == MaxOf(DAttemptSet, 0)
DEpoch    == MaxOf({ i.tgt : i \in Intents }, -1)
DTrunc    == (CHOOSE b \in Begins : b.tgt = DTarget /\ \A b2 \in Begins : b2.tgt = DTarget => b2.r <= b.r).trunc
DIntent   == { i \in Intents : i.tgt = DTarget /\ i.r = DRId /\ i.a = DAttempt }
DGens     == IF DIntent = {} THEN [s \in Stages |-> NoGen] ELSE (CHOOSE i \in DIntent : TRUE).gens
DApplied  == IF DIntent = {} THEN 0 ELSE (CHOOSE i \in DIntent : TRUE).ap
DComplete == \E c \in Completes : c.tgt = DTarget /\ c.r = DRId /\ c.a = DAttempt
DComplId  == MaxOf({ c.cid : c \in Completes }, 0)          \* ids stay monotone across restarts
\* The completion THIS target decided (0 if none). An UNSERVABLE names a completion; only one that
\* names the derived target's completion classifies the restart as SUPERSEDING. A superseded
\* completion of an EARLIER target is finished business — the recovery its supersession opened is
\* already in the log as a later BEGIN. (Derivation defect found by the DST, 2026-09-03: the first
\* draft matched ANY completion and re-entered SUPERSEDING at a target that had decided nothing.)
DTargetCid == MaxOf({ c.cid : c \in { cc \in Completes : cc.tgt = DTarget /\ cc.r = DRId /\ cc.a = DAttempt } }, 0)
DUnserv   == DComplete /\ \E u \in wal : u.t = "UNSERVABLE" /\ u.cid = DTargetCid

CoordRestart ==
    /\ cState = "CRASHED"
    \* Re-derive the volatile state from the durable log — every variable, no survivors.
    \* (2026-09-03, found by the CI liveness leg in 5 s — `State 5: Stuttering` from CRASHED: the
    \* first draft assigned recTarget'/rId'/attempt'/actKind' HERE, unconditionally, and then
    \* re-assigned them in the fence-forward branch below, so that branch was UNSATISFIABLE and a
    \* crash without completion evidence could never restart. Every primed variable a branch
    \* decides is now assigned inside that branch, once.)
    /\ activeEpoch' = DEpoch
    /\ truncateTo' = DTrunc
    /\ tupleGen' = DGens /\ tupleApplied' = DApplied         \* the tuple the durable INTENT bound
    /\ complId' = DComplId /\ predCompl' = 0
    \* completeDurable / unservable are decided per branch: a branch that RESUMES the derived
    \* target's completion carries it; a branch that fences FORWARD has moved past it — the old
    \* completion is finished business (§6.5a), and carrying its flag into the new target made
    \* PostDecisionLoss demand a supersession the new recovery never performs (CI liveness leg,
    \* 2026-09-03: a 20-state trace ending in a §11 termination). The code's predicate is
    \* epoch-scoped (`completed()` compares the tuple's epoch) and never had the carry-over.
    /\ IF DUnserv THEN
            \* §6.7: a recorded UNSERVABLE resumes the superseding recovery (F-UNSERVABLE order).
            /\ cState' = "SUPERSEDING" /\ UNCHANGED wal
            /\ recTarget' = DTarget /\ rId' = DRId /\ attempt' = DAttempt
            /\ actKind' = (IF DTarget = 0 THEN "INITIAL" ELSE "RECOVERY")
            /\ completeDurable' = TRUE /\ unservable' = TRUE
       ELSE IF DComplete /\ servedCount = 0 THEN
            \* The decision stands (I22): finish it. FINALIZED evidence is re-collected on the wire.
            \* Only while the activation has NOT served (spec §6.5a refinement, 2026-09-03): the
            \* commit stream (abstracted by servedCount, which survives a crash exactly as the
            \* durable stream does) witnesses service; a crash after service is outside any
            \* transaction and falls through to the fence — the data-plane tail beyond the durable
            \* frontier, which this model does not represent, needs the BEGIN's truncation.
            /\ cState' = "ACTIVATION_COMPLETE" /\ UNCHANGED wal
            /\ recTarget' = DTarget /\ rId' = DRId /\ attempt' = DAttempt
            /\ actKind' = (IF DTarget = 0 THEN "INITIAL" ELSE "RECOVERY")
            /\ completeDurable' = TRUE /\ unservable' = FALSE
       ELSE IF DTarget + 1 <= MaxEpoch /\ DRId + 1 <= MaxRId THEN
            \* FENCE FORWARD: a new recovery strictly above every durable (hence every sent) value.
            /\ Wal([t |-> "BEGIN", base |-> DTarget, tgt |-> DTarget + 1,
                    r |-> DRId + 1, trunc |-> DTrunc, empty |-> NeedEmptyFor(DTrunc)])
            /\ cState' = "RECOVERY_STARTED"
            /\ recTarget' = DTarget + 1 /\ rId' = DRId + 1 /\ attempt' = 0
            /\ actKind' = "RECOVERY"
            /\ completeDurable' = FALSE /\ unservable' = FALSE
       ELSE
            \* Bounds exhausted: §11 explicit termination, never an indefinite restart loop.
            /\ Wal([t |-> "TERMINAL", tgt |-> DTarget]) /\ cState' = "TERMINAL"
            /\ recTarget' = DTarget /\ rId' = DRId /\ attempt' = DAttempt
            /\ actKind' = (IF DTarget = 0 THEN "INITIAL" ELSE "RECOVERY")
            /\ completeDurable' = DComplete /\ unservable' = DUnserv
    /\ UNCHANGED << msgs, goal, stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFresh, stFinal,
        installedCkpt, candidateCkpt, segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

--------------------------------------------------------------------------------------
(* -------- Candidate-checkpoint abstraction (spec §2.6b, I24) -------- *)

PrepareCandidate ==
    /\ cState = "SERVICEABLE" /\ candidateCkpt = 0
    \* F-UNBOUNDED-SEGMENT (§7.21): bound the checkpoint dimension, exactly as MaxEpoch/MaxRId/
    \* MaxAttempt/MaxPos/MaxCrashes bound theirs. Without this the prepare→commit pair is an
    \* unbounded self-loop and NO baseline can ever reach fixpoint (TLC dies on its 65535-state
    \* behaviour-length ceiling first). The protocol genuinely permits unboundedly many segments —
    \* this bounds the MODEL, it does not narrow the protocol.
    /\ installedCkpt < MaxCkpt
    /\ candidateCkpt' = installedCkpt + 1
    /\ UNCHANGED << msgs, wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFresh, stFinal,
        installedCkpt, segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

CommitSegmentAndInstall ==
    /\ candidateCkpt # 0
    /\ segCommitted' = segCommitted \cup {candidateCkpt}
    /\ installedCkpt' = candidateCkpt /\ candidateCkpt' = 0
    /\ UNCHANGED << msgs, wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFresh, stFinal,
        crashes, caseBviolation, sourceViolation, servedCount >>

DropCandidate ==            \* admission failure / cancellation: no trace
    /\ candidateCkpt # 0 /\ candidateCkpt' = 0
    /\ UNCHANGED << msgs, wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFresh, stFinal,
        installedCkpt, segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

SessionTerminate ==         \* spec SS11: no admissible placement => explicit terminal
    /\ cState \in {"RECOVERY_STARTED", "RECONSTRUCTING", "READY_ALL", "SUPERSEDING"}
    \* F-LIVENESS-FAIR (§6.4, TLC-3 model-completeness): terminate on participant loss OR
    \* bound exhaustion (attempt >= MaxAttempt, or reconstruction rId >= MaxRId). Both bounds are
    \* policy-set; the real system policy-bounds attempts and terminates per §11, so the bound's
    \* outcome must be defined here too. This does NOT force premature termination: WF fires only on
    \* *continuous* enablement, and any run that makes progress leaves this cState set (into
    \* COMMITTING/COMPLETE/SERVICEABLE/...), disabling the guard -- so only genuinely-stuck runs,
    \* where it stays enabled forever, are terminated.
    /\ \/ (\E s \in Stages : stState[s] = "LOST")
       \/ attempt >= MaxAttempt
       \/ rId >= MaxRId
    /\ Wal([t |-> "TERMINAL", tgt |-> recTarget])
    /\ cState' = "TERMINAL"
    /\ UNCHANGED << msgs, activeEpoch, recTarget, rId, attempt, actKind, truncateTo,
        goal, tupleGen, tupleApplied, completeDurable, unservable, complId, predCompl,
        stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFresh, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, sourceViolation, servedCount >>

StageRecvBegin(s)    == \E t0 \in 0..MaxEpoch, r0 \in 0..MaxRId : StageRecvBeginAt(s, t0, r0)
StageRecvReset(s)    == \E nr \in 0..MaxRId : StageRecvResetAt(s, nr)
StageRecvCommit(s)   == \E a0 \in 1..MaxAttempt : StageRecvCommitAt(s, a0)
StageRecvAbort(s)    == \E a0 \in 1..MaxAttempt : StageRecvAbortAt(s, a0)
StageRecvFinalize(s) == \E a0 \in 1..MaxAttempt : StageRecvFinalizeAt(s, a0)

--------------------------------------------------------------------------------------
Next ==
    \/ CoordBeginRecovery \/ CoordStartSuperseding \/ SendBeginRecovery
    \/ CoordResetAttempt  \/ CoordWriteIntent      \/ CoordSendCommit
    \/ CoordAbortActivation \/ CoordWriteComplete  \/ CoordSendFinalize
    \/ CoordBecomeServiceable \/ CoordRecordUnservable
    \/ ServeDataPlane \/ CoordCrash \/ CoordRestart \/ SessionTerminate
    \/ PrepareCandidate \/ CommitSegmentAndInstall \/ DropCandidate
    \/ \E s \in Stages :
         StageRecvBegin(s) \/ StageRejoin(s) \/ StageRebuildStep(s)
         \/ StageRecvReset(s) \/ StageRecvCommit(s) \/ StageRecvAbort(s)
         \/ StageRecvFinalize(s) \/ StageCrash(s)

(* EventuallyStable == crashes are bounded (MaxCrashes) + weak fairness on every    *)
(* non-crash action. Crash actions are deliberately NOT fair.                        *)
Fairness ==
    /\ WF_vars(SendBeginRecovery) /\ WF_vars(CoordWriteIntent)
    /\ WF_vars(CoordSendCommit)   /\ WF_vars(CoordWriteComplete)
    /\ WF_vars(CoordSendFinalize) /\ WF_vars(CoordBecomeServiceable)
    /\ WF_vars(CoordRecordUnservable) /\ WF_vars(CoordStartSuperseding)
    /\ WF_vars(CoordRestart)
    /\ WF_vars(CoordBeginRecovery)   /\ WF_vars(CoordResetAttempt)
    /\ WF_vars(CoordAbortActivation) /\ WF_vars(SessionTerminate)
    /\ \A s \in Stages :
         /\ WF_vars(StageRejoin(s)) /\ WF_vars(StageRebuildStep(s))
         /\ \A t0 \in 0..MaxEpoch : \A r0 \in 0..MaxRId :
              WF_vars(StageRecvBeginAt(s, t0, r0))
         /\ \A nr \in 0..MaxRId :      WF_vars(StageRecvResetAt(s, nr))
         /\ \A a0 \in 1..MaxAttempt :
              /\ WF_vars(StageRecvCommitAt(s, a0))
              /\ WF_vars(StageRecvAbortAt(s, a0))
              /\ WF_vars(StageRecvFinalizeAt(s, a0))

Spec == Init /\ [][Next]_vars /\ Fairness

--------------------------------------------------------------------------------------
(* -------- Safety properties -------- *)

(* TLC-2 (property finding): the naive global invariant                              *)
(*   SERVICEABLE => \A s : stState[s] = "ACTIVE_FINAL"                                *)
(* is unsatisfiable in an asynchronous system — a stage can crash between its         *)
(* FINALIZED ack and the coordinator's transition. v0.9's actual promises are         *)
(* (a) evidence-based coordinator safety (I16) and (b) stage-local tuple safety for   *)
(* any live stage whose shard generation still matches the served tuple (I20 + F1).  *)

ServiceSafety ==            \* I16, I18, I22: serviceability rests on durable decision,
    (cState = "SERVICEABLE") => \* non-unservability, and FINALIZED evidence from all
        /\ completeDurable /\ ~unservable /\ AllFinalized

TupleSafety ==              \* I20/F1 ground truth: any live stage that current data
    (cState = "SERVICEABLE") => \* frames would reach (epoch+gen match) must hold the
        \A s \in Stages :        \* exact served tuple — the mutation-3 detector
            (/\ stState[s] = "ACTIVE_FINAL"
             /\ stEpoch[s] = recTarget
             /\ stGen[s]   = tupleGen[s])
            => (stAttempt[s] = attempt /\ stApplied[s] = tupleApplied /\ stFinal[s])
CaseBPure       == ~caseBviolation                            \* I11 + I23
NoPreactiveServe== (cState = "SERVICEABLE") =>
                      \A s \in Stages : stState[s] # "PREACTIVE"          \* I20
AbortSafety     == \A m \in msgs :                                        \* I21
                      (m.t = "ABORT") => ~(completeDurable /\ m.a = attempt
                                           /\ m.tgt = recTarget /\ m.r = rId)
CandidateIsolation == installedCkpt \in segCommitted                      \* I24
DecisionMonotone   ==                                                        \* I10a/WAL
    /\ completeDurable => (\E rec \in wal : rec.t = "COMPLETE")
    \* Strengthened 2026-09-03 (the code's invariant, which the DST holds, was stronger than the
    \* model's): every post-decision coordinator state rests on a durable COMPLETE — a restart may
    \* not re-enter SUPERSEDING / ACTIVATION_COMPLETE / FINALIZING off an earlier target's decision.
    /\ (cState \in {"ACTIVATION_COMPLETE", "FINALIZING", "SUPERSEDING"}) => completeDurable
AbortFinality      ==                                                     \* I25 (TLC-1)
    ~\E ab \in wal, co \in wal :
        /\ ab.t = "ABORT" /\ co.t = "COMPLETE"
        /\ ab.tgt = co.tgt /\ ab.r = co.r /\ ab.a = co.a

(***************************************************************************************)
(* [AUDIT M13, 2026-08-23] ResetPreservesAttemptSpace                                   *)
(*                                                                                     *)
(* The activation-attempt space is per (session, epoch). A RESET advances `recovery_id` *)
(* but NOT the epoch, so it must not restart attempts.                                 *)
(*                                                                                     *)
(* Why this property exists at all is the finding. Spec §6.4 was SILENT on the reset's  *)
(* effect on the attempt space, and this model made the choice implicitly — every reset *)
(* action carries `UNCHANGED << attempt, stAttempt >>`. That choice is self-consistent, *)
(* so TLC had NOTHING TO SAY: the model was not wrong, it was silently opinionated, and *)
(* its opinion was invisible to anyone reading the spec. A second implementation could  *)
(* read "fresh reconstruction ⇒ attempts restart at 0" — coherent, and fatal, because   *)
(* F2's stage floor is epoch-scoped and survives the reset, so every post-reset         *)
(* activation would be fenced by the stage's own floor. Two conforming implementations, *)
(* one permanent deadlock.                                                             *)
(*                                                                                     *)
(* Making it a CHECKED property is the repair: it must hold on the model UNCHANGED (if  *)
(* it fails on first run, the model was making the other choice and the finding is      *)
(* larger than a silence), and a future edit adopting the other reading now fails here  *)
(* instead of passing quietly.                                                         *)
(***************************************************************************************)
ResetPreservesAttemptSpace ==
    [][ (rId' > rId /\ recTarget' = recTarget)
        => (/\ attempt' = attempt
            /\ \A s \in Stages : stAttempt'[s] >= stAttempt[s]) ]_vars

(* [2026-09-02 §6.5a] IntentFence: the coordinator's attempt is never BELOW a durable intent's *)
(* at its current (target, recovery_id). Faithful operation keeps it (attempt advances with every  *)
(* INTENT written); a restart that derived by MIN (MUTATION 5) breaks it in one state.            *)
IntentFence == (cState # "CRASHED") =>
    \A rec \in wal : (rec.t = "INTENT" /\ rec.tgt = recTarget /\ rec.r = rId) => rec.a <= attempt

(* [spec v0.10.5, design authority 2026-09-10] RelayedSourcing (I26): in the D0 relayed topology  *)
(* every position a downstream stage applied AT ITS CURRENT EPOCH lies inside its upstream's        *)
(* fresh-emission window at that epoch — the activation it applied was emitted, not held over.       *)
(* A stage's fresh window is (stApplied - stFresh, stApplied]; the flag is set when a downstream stage *)
(* applies a position outside its upstream's window (the caseBviolation pattern). Faithful EMPTY keeps *)
(* it: the survivor   *)
(* upstream of a lost stage discards everything and re-emits from 0. MUTATION 7 (EmptyDiscards =     *)
(* FALSE: the survivor keeps its stale KV under an EMPTY BEGIN) leaves the upstream's window empty     *)
(* while the replacement applies positions the upstream never re-emitted — byte identity is gone.     *)
RelayedSourcing == ~sourceViolation      \* checked at the moment a downstream stage applies

Inv == /\ ServiceSafety /\ TupleSafety /\ CaseBPure /\ NoPreactiveServe
       /\ AbortSafety   /\ CandidateIsolation /\ DecisionMonotone /\ AbortFinality
       /\ IntentFence   /\ RelayedSourcing

--------------------------------------------------------------------------------------
(* -------- Liveness (check with Fairness; smaller bounds recommended) -------- *)

RecoveryInProgress == cState \in {"RECOVERY_STARTED","RECONSTRUCTING","READY_ALL",
                                  "ACTIVATION_INTENT_DURABLE","COMMITTING",
                                  "ACTIVATION_COMPLETE","FINALIZING","SUPERSEDING"}
Serviceable == cState = "SERVICEABLE"
Terminal    == cState = "TERMINAL"

Progress          == RecoveryInProgress ~> (Serviceable \/ Terminal)
\* 2026-09-03 (spec §6.5a): termination is an admissible outcome here exactly as in `Progress`.
\* A served activation whose coordinator crashes fences forward; when the policy bounds on
\* epochs / recovery ids are exhausted the restart resolves by §11 explicit termination, and the
\* decision it leaves behind (completeDurable, a LOST participant) has no supersession or service
\* left to reach. Before fence-forward the restart re-entered the same target and this case could
\* not arise; the CI liveness leg produced it as a 24-state trace ending in TERMINAL.
PostDecisionLoss  == (completeDurable /\ ~Serviceable
                        /\ \E s \in Stages : stState[s] = "LOST")
                     ~> (unservable \/ Serviceable \/ Terminal)
EventualService   == <>Serviceable \/ <>Terminal

========================================================================================
