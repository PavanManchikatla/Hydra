//! Executable invariants over coordinator state — ported one-to-one from the TLA+ `Inv`
//! (and, as later slices land, from spec §2.7's I1–I25). The simulator calls [`check`] after
//! **every** step; production builds call it in debug assertions at transition boundaries.
//!
//! Slice 1 covers the WAL-level activation invariants: **I25 abort-finality** (the TLC-1
//! defect), decision monotonicity (I10a/WAL), and evidence-based serviceability (I16/I18).

use crate::coordinator::{Coordinator, CoordState};
use crate::segment::SegmentCheckpoint;
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

/// Check the token ledger's watermark/commit invariants (spec §2.7), named by number.
pub fn check_ledger(l: &crate::Ledger) -> Vec<Violation> {
    let mut v = Vec::new();
    let gen = l.generation_durable_pos().get();
    let sampled = l.sampled_pos().get();
    let emitted = l.emitted_pos().get();
    let ledger_durable = l.ledger_durable_pos().get();

    // I2a/I2b (monotone frontiers): generation_durable ≤ sampled; generation_durable ≤ ledger_durable.
    if sampled < gen {
        v.push(Violation { invariant: "I2 SampledFrontier", detail: format!("sampled_pos {sampled} < generation_durable_pos {gen}") });
    }
    if ledger_durable < gen {
        v.push(Violation { invariant: "I2 LedgerFrontier", detail: format!("ledger_durable_pos {ledger_durable} < generation_durable_pos {gen}") });
    }
    // I6 (emit-after-commit): a token is emitted only once committed.
    if emitted > gen {
        v.push(Violation { invariant: "I6 EmitAfterCommit", detail: format!("emitted_pos {emitted} > generation_durable_pos {gen}") });
    }
    // I9 (cancellation cutoff): the cutoff is exactly generation_durable_pos, and no provisional survives.
    if let Some(cutoff) = l.cancel_cutoff_pos() {
        if cutoff.get() != gen {
            v.push(Violation { invariant: "I9 CancelCutoff", detail: format!("cancel cutoff {} != generation_durable_pos {gen}", cutoff.get()) });
        }
        if l.provisional_len() != 0 {
            v.push(Violation { invariant: "I9 CancelCutoff", detail: "provisional tokens survived cancellation".into() });
        }
    }
    v
}

/// Check the segment-checkpoint SM's **I24 candidate isolation** (the Mut6 detector). Mirrors the
/// TLA+ `CandidateIsolation == installedCkpt ∈ segCommitted`: the installed live sampler checkpoint is
/// always a durably-committed one; `mutation_candidate_leak` installs an uncommitted candidate,
/// tripping this.
pub fn check_segment(s: &SegmentCheckpoint) -> Vec<Violation> {
    let mut v = Vec::new();
    if !s.candidate_isolation_holds() {
        v.push(Violation {
            invariant: "I24 CandidateIsolation",
            detail: format!("installed checkpoint {} is not durably committed (uncommitted candidate leaked into live state)", s.installed()),
        });
    }
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

/// **DecisionMonotone (I10a/WAL):** any post-decision coordinator state is backed by a durable
/// COMPLETE for the current epoch — a decision is never claimed without its WAL evidence.
fn decision_monotone(c: &Coordinator, out: &mut Vec<Violation>) {
    let post_decision = matches!(
        c.state(),
        CoordState::ActivationComplete
            | CoordState::Finalizing
            | CoordState::Serviceable
            | CoordState::Superseding
    );
    if post_decision && !c.completed() {
        out.push(Violation {
            invariant: "DecisionMonotone",
            detail: format!("post-decision state {:?} without a durable COMPLETE", c.state()),
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
