--------------------------------- MODULE HydraCert ---------------------------------
(***************************************************************************************)
(* TIER-2 CERTIFICATION WRAPPER — BOUNDS ONLY, SEMANTICS UNTOUCHED.                     *)
(*                                                                                     *)
(* Why this module exists (PROJECT_STATE §6, design-authority-authorized 2026-08-22):   *)
(* the large-bound TLC legs never drain inside CI's time-box. Every weekly run is a     *)
(* FRESH ~5h27m box over a 320M+-state space, so `baseline-safety` never reaches        *)
(* fixpoint and `Mut3` never fires — the record has read INCONCLUSIVE since 2026-07-13. *)
(* The ratified strategy is "certified-small beats forever-inconclusive-large": certify *)
(* at reduced bounds that actually DRAIN, and keep the large-bound runs on record as    *)
(* depth-bounded clean evidence.                                                       *)
(*                                                                                     *)
(* WHY A WRAPPER AND NOT AN EDIT TO HydraActivationCore.tla:                           *)
(*   - Standing rule 13: any `.tla` change voids every prior checkpoint. The core       *)
(*     module stays byte-identical, so the July banked checkpoints remain valid.        *)
(*   - The certified claim must be about THE SAME MODEL. This module adds no action,    *)
(*     no variable, no invariant and no property. It declares ONE constant and ONE      *)
(*     state constraint. `Spec`, `Inv`, `Progress`, `PostDecisionLoss`,                 *)
(*     `EventualService` and `Sym` are inherited from the core, unchanged.              *)
(*                                                                                     *)
(* WHAT MAY AND MAY NOT BE TUNED (standing rules 6 and 9):                              *)
(*   `MaxMsgs` is a BOUND, not model logic. The bound-search iterates it UPWARD until   *)
(*   the mutations fire. It is NEVER lowered to make a leg look clean, and no model     *)
(*   logic is touched to make any check pass. Under the ratified acceptance criterion   *)
(*   (PROJECT_STATE §6) a candidate bound certifies only if ALL FOUR mutations fire at  *)
(*   it AND both baselines reach conclusive verdicts at it. A mutation that goes silent *)
(*   at a candidate bound is a MASKED FAILURE and disqualifies that bound.              *)
(***************************************************************************************)
EXTENDS HydraActivationCore

CONSTANT MaxMsgs      \* certification bound on |msgs|

(* The certification bound must be a SUB-bound of the core's own StateConstraint        *)
(* (`Cardinality(msgs) <= 20`). If a search ever needed MaxMsgs > 20 the result would   *)
(* no longer be a *reduced*-bound certification and the large-bound INCONCLUSIVE runs   *)
(* would no longer dominate it — a different regime that must be ESCALATED, not         *)
(* silently entered. This ASSUME turns that into a loud TLC failure rather than drift.  *)
ASSUME MaxMsgs \in Nat /\ MaxMsgs <= 20

(* The ONLY definition this module adds. Identical in FORM to the core's               *)
(* `StateConstraint == Cardinality(msgs) <= 20`, with the literal replaced by the model *)
(* constant so the certification bound is declared in the .cfg — and therefore appears  *)
(* verbatim in the receipt (rule 16: the bound defines the claim, so the bound must be  *)
(* readable off the evidence, not inferred from a filename).                            *)
CertConstraint == Cardinality(msgs) <= MaxMsgs

=====================================================================================
