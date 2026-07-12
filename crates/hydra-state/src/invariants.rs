//! Executable invariants over coordinator state — ported one-to-one from the TLA+ `Inv`
//! (and, as later slices land, from spec §2.7's I1–I25). The simulator calls [`check`] after
//! **every** step; production builds call it in debug assertions at transition boundaries.
//!
//! Slice 1 covers the WAL-level activation invariants: **I25 abort-finality** (the TLC-1
//! defect), decision monotonicity (I10a/WAL), and evidence-based serviceability (I16/I18).

use crate::coordinator::{Coordinator, CoordState};
use crate::stage::{Stage, StageState};
use crate::WalRecord;

/// A detected invariant violation. Every simulator failure prints the offending invariant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub invariant: &'static str,
    pub detail: String,
}

/// Check all slice-1 invariants over the coordinator. Empty result = all hold.
pub fn check(c: &Coordinator) -> Vec<Violation> {
    let mut v = Vec::new();
    abort_finality(c, &mut v);
    decision_monotone(c, &mut v);
    service_safety(c, &mut v);
    v
}

/// Check a stage's local invariants (F2 attempt fencing — the Mut3 detector).
pub fn check_stage(s: &Stage) -> Vec<Violation> {
    let mut v = Vec::new();
    // **F2 / I4 (attempt fencing):** a stage bound into PREACTIVE/ACTIVE_FINAL must be at the
    // highest attempt it has accepted — it never regresses onto a stale (lower) attempt. Mut3
    // (no fencing) lets a stale COMMIT pull `attempt` below `highest_attempt`.
    if matches!(s.state(), StageState::Preactive | StageState::ActiveFinal)
        && s.attempt() != s.highest_attempt()
    {
        v.push(Violation {
            invariant: "F2 AttemptFence",
            detail: format!(
                "stage in {:?} at attempt {} but highest accepted is {}",
                s.state(),
                s.attempt(),
                s.highest_attempt()
            ),
        });
    }
    // **CaseBPure (I11 + I23):** BEGIN_RECOVERY Case B is a *pure replay* — it must never see a
    // stage advanced past `truncate_to`. Legitimate post-catch-up advancement is handled by
    // RESET_RECOVERY_ATTEMPT, which truncates. Mut2's label-only reset leaves the stage advanced,
    // tripping this on the next Case-B replay.
    if s.caseb_violated() {
        v.push(Violation {
            invariant: "CaseBPure",
            detail: "Case B replay saw applied > truncate_to (RESET failed to truncate)".into(),
        });
    }
    v
}

/// **I25 (abort finality, TLC-1):** for any `(epoch, recovery_id, attempt)`, a durable ABORT and
/// a durable COMPLETE are mutually exclusive, permanently.
fn abort_finality(c: &Coordinator, out: &mut Vec<Violation>) {
    for rec in c.wal() {
        if let WalRecord::ActivationComplete { tuple, .. } = rec {
            let key = tuple.attempt_key();
            let has_abort = c.wal().iter().any(|r| {
                matches!(r, WalRecord::ActivationAbort { epoch, recovery_id, attempt }
                    if (*epoch, *recovery_id, *attempt) == key)
            });
            if has_abort {
                out.push(Violation {
                    invariant: "I25 AbortFinality",
                    detail: format!("COMPLETE and ABORT both durable for attempt {key:?}"),
                });
            }
        }
    }
}

/// **DecisionMonotone (I10a/WAL):** the volatile `completed` view is backed by a durable
/// COMPLETE record (a decision is never claimed without its WAL evidence).
fn decision_monotone(c: &Coordinator, out: &mut Vec<Violation>) {
    let has_complete = c.wal().iter().any(|r| matches!(r, WalRecord::ActivationComplete { .. }));
    if c.completed() != has_complete {
        out.push(Violation {
            invariant: "DecisionMonotone",
            detail: format!("completed()={} but durable COMPLETE present={}", c.completed(), has_complete),
        });
    }
}

/// **ServiceSafety (I16/I18):** serviceability rests on the durable decision — a coordinator in
/// `Serviceable` must have a durable COMPLETE. (The finalized-evidence half is asserted at the
/// transition into `Serviceable`; see the coordinator's `ProceedBecomeServiceable` guard.)
fn service_safety(c: &Coordinator, out: &mut Vec<Violation>) {
    if c.state() == CoordState::Serviceable && !c.completed() {
        out.push(Violation {
            invariant: "I16 ServiceSafety",
            detail: "SERVICEABLE without a durable ACTIVATION_COMPLETE".into(),
        });
    }
}
