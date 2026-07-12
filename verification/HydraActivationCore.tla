------------------------------ MODULE HydraActivationCore ------------------------------
(***************************************************************************************)
(* Hydra Session Protocol v0.10 — transition core (package v0.10.1).                   *)
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
    EnableUnservable, ResetTruncates, AttemptFencing, AbortGuardEnabled

ASSUME EnableUnservable \in BOOLEAN /\ ResetTruncates \in BOOLEAN
       /\ AttemptFencing \in BOOLEAN /\ AbortGuardEnabled \in BOOLEAN

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
    stFinal,           \* [Stages -> BOOLEAN] : holds ACTIVATION_FINALIZED evidence
    \* ---- sampler-candidate abstraction (I24) ----
    installedCkpt,     \* Nat: id of installed sampler checkpoint
    candidateCkpt,     \* Nat: 0 = none; else a prepared, uncommitted candidate id
    segCommitted,      \* set of durably committed candidate ids
    \* ---- bookkeeping ----
    crashes,           \* crash counter (bounded by MaxCrashes)
    caseBviolation,    \* set TRUE if Case B's applied<=truncate_to assertion trips
    servedCount        \* number of ServeDataPlane events (diagnostic)

vars == << msgs, wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
           truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable,
           complId, predCompl, stState, stEpoch, stRId, stAttempt, stGen, stApplied,
           stFinal, installedCkpt, candidateCkpt, segCommitted, crashes,
           caseBviolation, servedCount >>

--------------------------------------------------------------------------------------
(* Helpers *)

StateConstraint == Cardinality(msgs) <= 20

Send(m)  == msgs' = msgs \cup {m}
Wal(r)   == wal'  = wal  \cup {r}

ReadyAcks     == { m \in msgs : m.t = "READY"     /\ m.tgt = recTarget /\ m.r = rId }
CommitAcks(a) == { m \in msgs : m.t = "COMMITTED" /\ m.tgt = recTarget /\ m.r = rId
                                                   /\ m.a = a }
FinalAcks(a)  == { m \in msgs : m.t = "FINALIZED" /\ m.tgt = recTarget /\ m.a = a }
AcksFrom(S)   == { m.s : m \in S }

AllReady      == AcksFrom(ReadyAcks)        = Stages
AllCommitted  == AcksFrom(CommitAcks(attempt)) = Stages
AllFinalized  == AcksFrom(FinalAcks(attempt))  = Stages

Min(x,y) == IF x < y THEN x ELSE y

--------------------------------------------------------------------------------------
(* Init: session admitted; INITIAL activation pending at epoch 0 through the same     *)
(* machinery as recovery (v0.9 §6.6, activation_kind = INITIAL).                       *)

Init ==
    /\ msgs = {} /\ crashes = 0 /\ caseBviolation = FALSE /\ servedCount = 0
    /\ wal = { [t |-> "BEGIN", base |-> -1, tgt |-> 0, r |-> 0, trunc |-> 0] }
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
    /\ stFinal  = [s \in Stages |-> FALSE]
    /\ installedCkpt = 1 /\ candidateCkpt = 0 /\ segCommitted = {1}

--------------------------------------------------------------------------------------
(* -------- Recovery start / BEGIN_RECOVERY delivery (spec §1.3) -------- *)

CoordBeginRecovery ==      \* new semantic recovery (failure while SERVICEABLE)
    /\ cState = "SERVICEABLE" /\ activeEpoch < MaxEpoch
    /\ \E s \in Stages : stState[s] = "LOST"           \* a reason to recover
    /\ Wal([t |-> "BEGIN", base |-> activeEpoch, tgt |-> activeEpoch + 1,
            r |-> 0, trunc |-> truncateTo])
    /\ cState' = "RECOVERY_STARTED" /\ recTarget' = activeEpoch + 1
    /\ rId' = 0 /\ attempt' = 0 /\ actKind' = "RECOVERY"
    /\ completeDurable' = FALSE /\ unservable' = FALSE
    /\ goal' = truncateTo    \* DECODING regime: goal = truncate_to (§2.3c); catch-up
                             \* still exercises movement because replacements start at 0
    /\ UNCHANGED << msgs, activeEpoch, truncateTo, tupleGen, tupleApplied, complId,
        predCompl, stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFinal,
        installedCkpt, candidateCkpt, segCommitted, crashes, caseBviolation, servedCount >>

CoordStartSuperseding ==   \* §6.7 step 3: supersede a decided-but-unservable activation
    /\ cState = "SUPERSEDING" /\ recTarget < MaxEpoch
    /\ Wal([t |-> "BEGIN", base |-> recTarget, tgt |-> recTarget + 1,
            r |-> 0, trunc |-> truncateTo])
    /\ cState' = "RECOVERY_STARTED"
    /\ predCompl' = complId
    /\ recTarget' = recTarget + 1 /\ rId' = 0 /\ attempt' = 0 /\ actKind' = "RECOVERY"
    /\ completeDurable' = FALSE /\ unservable' = FALSE
    /\ UNCHANGED << msgs, activeEpoch, truncateTo, goal, tupleGen, tupleApplied,
        complId, stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFinal,
        installedCkpt, candidateCkpt, segCommitted, crashes, caseBviolation, servedCount >>

SendBeginRecovery ==
    /\ cState = "RECOVERY_STARTED"
    /\ Send([t |-> "BEGIN", base |-> recTarget - 1, tgt |-> recTarget,
             r |-> rId, trunc |-> truncateTo])
    /\ cState' = "RECONSTRUCTING"
    /\ UNCHANGED << wal, activeEpoch, recTarget, rId, attempt, actKind, truncateTo,
        goal, tupleGen, tupleApplied, completeDurable, unservable, complId, predCompl,
        stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, servedCount >>

StageRecvBeginAt(s, tEpoch, r0) ==
    \E m \in msgs : /\ m.t = "BEGIN" /\ m.tgt = tEpoch /\ m.r = r0
        /\ \/ (* --- Case A: first application at base --- *)
              /\ stState[s] \in {"ACTIVE_FINAL", "FROZEN"} /\ stEpoch[s] = m.base
              /\ stState'  = [stState  EXCEPT ![s] = "FROZEN"]
              /\ stEpoch'  = [stEpoch  EXCEPT ![s] = m.tgt]
              /\ stRId'    = [stRId    EXCEPT ![s] = m.r]
              /\ stApplied'= [stApplied EXCEPT ![s] = Min(stApplied[s], m.trunc)]
              /\ stFinal'  = [stFinal  EXCEPT ![s] = FALSE]
              /\ Send([t |-> "RACK", s |-> s, tgt |-> m.tgt, r |-> m.r])
              /\ UNCHANGED << stAttempt, caseBviolation >>
           \/ (* --- Case B: PURE replay to a frozen stage of this transition --- *)
              /\ stState[s] = "FROZEN" /\ stEpoch[s] = m.tgt /\ m.r >= stRId[s]
              /\ caseBviolation' = (caseBviolation \/ stApplied[s] > m.trunc)
              /\ stRId' = [stRId EXCEPT ![s] = m.r]
              /\ Send([t |-> "RACK", s |-> s, tgt |-> m.tgt, r |-> m.r])
              /\ UNCHANGED << stState, stEpoch, stAttempt, stApplied, stFinal >>
           \/ (* --- Case B': locally-decidable completed activation --- *)
              /\ stState[s] = "ACTIVE_FINAL" /\ stEpoch[s] = m.tgt /\ stFinal[s]
              /\ Send([t |-> "ERR_COMPLETED", s |-> s, tgt |-> m.tgt])
              /\ UNCHANGED << stState, stEpoch, stRId, stAttempt, stApplied, stFinal,
                              caseBviolation >>
    /\ UNCHANGED << wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stGen, installedCkpt, candidateCkpt, segCommitted, crashes, servedCount >>

StageRejoin(s) ==          \* a LOST stage joins the in-flight reconstruction: fresh
    /\ stState[s] = "LOST" \* shard (abstracts ATTACH_CONTEXT_SHARD), applied = 0
    /\ cState \in {"RECONSTRUCTING", "RECOVERY_STARTED"}
    /\ stState'  = [stState  EXCEPT ![s] = "FROZEN"]
    /\ stEpoch'  = [stEpoch  EXCEPT ![s] = recTarget]
    /\ stRId'    = [stRId    EXCEPT ![s] = rId]
    /\ stApplied'= [stApplied EXCEPT ![s] = 0]
    /\ stFinal'  = [stFinal  EXCEPT ![s] = FALSE]
    /\ Send([t |-> "RACK", s |-> s, tgt |-> recTarget, r |-> rId])
    /\ UNCHANGED << wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stAttempt, stGen, installedCkpt, candidateCkpt, segCommitted,
        crashes, caseBviolation, servedCount >>

StageRebuildStep(s) ==     \* CatchUpOrRebuild: advance toward goal, then READY-ack
    /\ stState[s] \in {"FROZEN", "REBUILDING"} /\ stEpoch[s] = recTarget
    /\ cState \in {"RECONSTRUCTING", "READY_ALL"}
    /\ IF stApplied[s] < goal
       THEN /\ stState'   = [stState  EXCEPT ![s] = "REBUILDING"]
            /\ stApplied' = [stApplied EXCEPT ![s] = stApplied[s] + 1]
            /\ msgs' = msgs
       ELSE /\ stState'   = [stState  EXCEPT ![s] = "FROZEN_READY"]
            /\ stApplied' = stApplied
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
        stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, servedCount >>

StageRecvResetAt(s, nr) ==
    \E m \in msgs :
        /\ m.t = "RESET" /\ m.newr = nr /\ m.tgt = stEpoch[s] /\ nr > stRId[s]
        /\ stState[s] \in {"FROZEN", "REBUILDING", "FROZEN_READY", "PREACTIVE"}
        /\ stState'   = [stState EXCEPT ![s] = "FROZEN"]
        /\ stRId'     = [stRId   EXCEPT ![s] = m.newr]
        /\ stApplied' = [stApplied EXCEPT ![s] =
                            IF ResetTruncates THEN Min(stApplied[s], m.trunc)
                                              ELSE stApplied[s]]      \* MUTATION 2
        /\ Send([t |-> "RSACK", s |-> s, tgt |-> m.tgt, r |-> m.newr])
    /\ UNCHANGED << wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stEpoch, stAttempt, stGen, stFinal, installedCkpt, candidateCkpt,
        segCommitted, crashes, caseBviolation, servedCount >>

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
        stAttempt, stGen, stApplied, stFinal, installedCkpt, candidateCkpt,
        segCommitted, crashes, caseBviolation, servedCount >>

CoordSendCommit ==
    /\ cState = "ACTIVATION_INTENT_DURABLE"
    /\ Send([t |-> "COMMIT", tgt |-> recTarget, r |-> rId, a |-> attempt,
             gens |-> tupleGen, ap |-> tupleApplied])
    /\ cState' = "COMMITTING"
    /\ UNCHANGED << wal, activeEpoch, recTarget, rId, attempt, actKind, truncateTo,
        goal, tupleGen, tupleApplied, completeDurable, unservable, complId, predCompl,
        stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, servedCount >>

StageRecvCommitAt(s, a0) ==
    \E m \in msgs :
        /\ m.t = "COMMIT" /\ m.a = a0 /\ m.tgt = stEpoch[s] /\ m.r = stRId[s]
        /\ m.gens[s] = stGen[s] /\ m.ap = stApplied[s]
        /\ (AttemptFencing => m.a >= stAttempt[s])                    \* MUTATION 3
        /\ \/ /\ stState[s] = "FROZEN_READY"
              /\ stState'   = [stState   EXCEPT ![s] = "PREACTIVE"]
              /\ stAttempt' = [stAttempt EXCEPT ![s] = m.a]
           \/ /\ stState[s] = "PREACTIVE" /\ stAttempt[s] = m.a      \* idempotent replay
              /\ UNCHANGED << stState, stAttempt >>
        /\ Send([t |-> "COMMITTED", s |-> s, tgt |-> m.tgt, r |-> m.r, a |-> m.a])
    /\ UNCHANGED << wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stEpoch, stRId, stGen, stApplied, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, servedCount >>

CoordAbortActivation ==                            \* pre-decision only (I21)
    /\ cState = "COMMITTING" /\ ~completeDurable
    /\ Wal([t |-> "ABORT", tgt |-> recTarget, r |-> rId, a |-> attempt])
    /\ Send([t |-> "ABORT", tgt |-> recTarget, r |-> rId, a |-> attempt])
    /\ cState' = "READY_ALL"
    /\ UNCHANGED << activeEpoch, recTarget, rId, attempt, actKind, truncateTo, goal,
        tupleGen, tupleApplied, completeDurable, unservable, complId, predCompl,
        stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, servedCount >>

StageRecvAbortAt(s, a0) ==
    \E m \in msgs :
        /\ m.t = "ABORT" /\ m.a = a0 /\ m.tgt = stEpoch[s] /\ m.r = stRId[s]
        /\ stState[s] = "PREACTIVE" /\ stAttempt[s] = m.a /\ ~stFinal[s]
        /\ stState' = [stState EXCEPT ![s] = "FROZEN_READY"]
    /\ UNCHANGED << msgs, wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stEpoch, stRId, stAttempt, stGen, stApplied, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, servedCount >>

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
        stAttempt, stGen, stApplied, stFinal, installedCkpt, candidateCkpt,
        segCommitted, crashes, caseBviolation, servedCount >>

CoordSendFinalize ==
    /\ cState = "ACTIVATION_COMPLETE"
    /\ Send([t |-> "FINALIZE", tgt |-> recTarget, r |-> rId, a |-> attempt,
             cid |-> complId])
    /\ cState' = "FINALIZING"
    /\ UNCHANGED << wal, activeEpoch, recTarget, rId, attempt, actKind, truncateTo,
        goal, tupleGen, tupleApplied, completeDurable, unservable, complId, predCompl,
        stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, servedCount >>

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
        predCompl, stEpoch, stRId, stAttempt, stGen, stApplied, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, servedCount >>

CoordBecomeServiceable ==
    /\ cState = "FINALIZING" /\ AllFinalized /\ ~unservable
    /\ cState' = "SERVICEABLE" /\ activeEpoch' = recTarget
    /\ UNCHANGED << msgs, wal, recTarget, rId, attempt, actKind, truncateTo, goal,
        tupleGen, tupleApplied, completeDurable, unservable, complId, predCompl,
        stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, servedCount >>

ServeDataPlane ==
    /\ cState = "SERVICEABLE"
    /\ servedCount' = servedCount + 1 /\ servedCount < 2         \* bound diagnostics
    /\ UNCHANGED << msgs, wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFinal,
        installedCkpt, candidateCkpt, segCommitted, crashes, caseBviolation >>

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
        stEpoch, stRId, stAttempt, stGen, stApplied, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, servedCount >>

--------------------------------------------------------------------------------------
(* -------- Crashes and restarts -------- *)

StageCrash(s) ==            \* shard loss: LOST + new stage generation
    /\ crashes < MaxCrashes /\ stState[s] # "LOST"
    /\ crashes'   = crashes + 1
    /\ stState'   = [stState   EXCEPT ![s] = "LOST"]
    /\ stGen'     = [stGen     EXCEPT ![s] = stGen[s] + 1]
    /\ stApplied' = [stApplied EXCEPT ![s] = 0]
    /\ stFinal'   = [stFinal   EXCEPT ![s] = FALSE]
    /\ UNCHANGED << msgs, wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stEpoch, stRId, stAttempt, installedCkpt, candidateCkpt,
        segCommitted, caseBviolation, servedCount >>

CoordCrash ==
    /\ crashes < MaxCrashes /\ cState # "CRASHED"
    /\ crashes' = crashes + 1 /\ cState' = "CRASHED"
    /\ candidateCkpt' = 0                       \* volatile candidates die with C's peer
    /\ UNCHANGED << msgs, wal, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFinal,
        installedCkpt, segCommitted, caseBviolation, servedCount >>

CoordRestart ==             \* phase-specific restart rule (spec §6.5), driven by WAL
    \* BRANCH PRIORITY IS LOAD-BEARING (F-UNSERVABLE): `unservable` MUST be tested before the
    \* `completeDurable` branches. A superseded-but-completed activation (unservable=TRUE with a
    \* durable COMPLETE still present for this epoch) must resume SUPERSEDING, never re-enter
    \* ACTIVATION_COMPLETE/finalization — that would reopen the I22 hole. This model was already
    \* correct; the spec prose (§6.5) and the Rust impl were the layers that shadowed the order.
    \* Do not "simplify" or reorder this IF/ELSIF chain.
    /\ cState = "CRASHED"
    /\ cState' =
         IF unservable                      THEN "SUPERSEDING"
         ELSE IF completeDurable /\ ~AllFinalized THEN "ACTIVATION_COMPLETE"
         ELSE IF completeDurable /\ AllFinalized  THEN "FINALIZING"
         ELSE IF AbortGuardEnabled /\ AttemptAborted(attempt)
                                                  THEN "READY_ALL"   \* TLC-1 / I25; MUTATION 4
         ELSE IF \E rec \in wal : rec.t = "INTENT" /\ rec.tgt = recTarget
                                   /\ rec.r = rId /\ rec.a = attempt
                                            THEN "ACTIVATION_INTENT_DURABLE"
         ELSE                                    "RECOVERY_STARTED"
    /\ UNCHANGED << msgs, wal, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFinal,
        installedCkpt, candidateCkpt, segCommitted, crashes, caseBviolation, servedCount >>

--------------------------------------------------------------------------------------
(* -------- Candidate-checkpoint abstraction (spec §2.6b, I24) -------- *)

PrepareCandidate ==
    /\ cState = "SERVICEABLE" /\ candidateCkpt = 0
    /\ candidateCkpt' = installedCkpt + 1
    /\ UNCHANGED << msgs, wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFinal,
        installedCkpt, segCommitted, crashes, caseBviolation, servedCount >>

CommitSegmentAndInstall ==
    /\ candidateCkpt # 0
    /\ segCommitted' = segCommitted \cup {candidateCkpt}
    /\ installedCkpt' = candidateCkpt /\ candidateCkpt' = 0
    /\ UNCHANGED << msgs, wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFinal,
        crashes, caseBviolation, servedCount >>

DropCandidate ==            \* admission failure / cancellation: no trace
    /\ candidateCkpt # 0 /\ candidateCkpt' = 0
    /\ UNCHANGED << msgs, wal, cState, activeEpoch, recTarget, rId, attempt, actKind,
        truncateTo, goal, tupleGen, tupleApplied, completeDurable, unservable, complId,
        predCompl, stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFinal,
        installedCkpt, segCommitted, crashes, caseBviolation, servedCount >>

SessionTerminate ==         \* spec SS11: no admissible placement => explicit terminal
    /\ cState \in {"RECOVERY_STARTED", "RECONSTRUCTING", "READY_ALL", "SUPERSEDING"}
    /\ \E s \in Stages : stState[s] = "LOST"
    /\ Wal([t |-> "TERMINAL", tgt |-> recTarget])
    /\ cState' = "TERMINAL"
    /\ UNCHANGED << msgs, activeEpoch, recTarget, rId, attempt, actKind, truncateTo,
        goal, tupleGen, tupleApplied, completeDurable, unservable, complId, predCompl,
        stState, stEpoch, stRId, stAttempt, stGen, stApplied, stFinal, installedCkpt,
        candidateCkpt, segCommitted, crashes, caseBviolation, servedCount >>

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
DecisionMonotone   == completeDurable => (\E rec \in wal : rec.t = "COMPLETE")  \* I10a/WAL
AbortFinality      ==                                                     \* I25 (TLC-1)
    ~\E ab \in wal, co \in wal :
        /\ ab.t = "ABORT" /\ co.t = "COMPLETE"
        /\ ab.tgt = co.tgt /\ ab.r = co.r /\ ab.a = co.a

Inv == /\ ServiceSafety /\ TupleSafety /\ CaseBPure /\ NoPreactiveServe
       /\ AbortSafety   /\ CandidateIsolation /\ DecisionMonotone /\ AbortFinality

--------------------------------------------------------------------------------------
(* -------- Liveness (check with Fairness; smaller bounds recommended) -------- *)

RecoveryInProgress == cState \in {"RECOVERY_STARTED","RECONSTRUCTING","READY_ALL",
                                  "ACTIVATION_INTENT_DURABLE","COMMITTING",
                                  "ACTIVATION_COMPLETE","FINALIZING","SUPERSEDING"}
Serviceable == cState = "SERVICEABLE"
Terminal    == cState = "TERMINAL"

Progress          == RecoveryInProgress ~> (Serviceable \/ Terminal)
PostDecisionLoss  == (completeDurable /\ ~Serviceable
                        /\ \E s \in Stages : stState[s] = "LOST")
                     ~> (unservable \/ Serviceable)
EventualService   == <>Serviceable \/ <>Terminal

========================================================================================
