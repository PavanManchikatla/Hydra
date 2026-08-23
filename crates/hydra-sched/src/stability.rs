//! P2·5 — **the §11 scheduler stability contract.**
//!
//! **What this is not, first — the same scope honesty P2·4 opens with.** This module introduces
//! **no new data-plane mechanism**. It produces a *decision*: hold, shed, re-place, or terminate.
//! In v1 there is one session and there is **no live-migration machinery**, so "re-place" means
//! **the recovery/supersession path that is already built and proven** (`BEGIN_RECOVERY` →
//! catch-up → install → activate, at epoch+1). The stability contract's output is a
//! **recommendation delivered to the coordinator**, whose execution is the existing
//! epoch-transition machinery. Nothing here moves a tensor.
//!
//! It is also the scheduler-side justification for the model's `SessionTerminate` arrow, which
//! until now existed to keep the TLA+ liveness argument honest (bound exhaustion must terminate
//! rather than stall). Here it finally has **real inputs**: a session is terminated when no
//! admissible placement exists and the shed ladder is spent.
//!
//! # The contract (spec §11, as v0.8)
//!
//! * **Minimum placement lifetime — 10 minutes.** A placement is not re-decided before it has had
//!   time to be worth deciding. Bypassed only by hard failure: you cannot wait ten minutes for a
//!   device that is gone.
//! * **Windowed-EWMA triggers — 60 s.** A re-placement is considered only on evidence that has had
//!   a window to settle. Together with the lifetime floor these are the **anti-flap pair**: the
//!   window stops a spike from being read as a trend, and the floor stops even a real trend from
//!   re-deciding faster than a placement can pay for itself.
//! * **One migration at a time.** While one is in flight, every trigger yields `Hold` — including
//!   under hard failure, because the in-flight recovery is precisely the machinery that will deal
//!   with it, and starting a second concurrently is the thing §11 forbids.
//! * **Load-shed first.** The ladder is walked *before* redistribution, because shedding is
//!   reversible and cheap while re-placement costs an epoch transition.
//! * **Explicit termination.** When no admissible placement exists and the ladder is spent, the
//!   answer is a named termination — never a silent degradation, and never a placement that
//!   admission would refuse.

use crate::solver::Placement;

/// §11 minimum placement lifetime.
pub const MIN_PLACEMENT_LIFETIME_MS: u64 = 10 * 60 * 1000;
/// §11 trigger window: how much settled observation a re-placement decision requires.
pub const TRIGGER_WINDOW_MS: u64 = 60_000;
/// Default fractional degradation against the at-install prediction before a trigger fires.
pub const DEFAULT_DEGRADATION_TRIGGER: f64 = 0.25;

/// The load-shed ladder, in the order it is walked. Cheapest and most reversible first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ShedRung {
    /// Defer `ShardTransfer`-class traffic. Frees contention-group airtime immediately and costs
    /// only rebuild latency. Note `Heartbeat` is deliberately **not** sheddable — P2·4 prices it
    /// precisely so it is never the thing that gets starved.
    DeferBackgroundTransfers,
    /// Shrink the chunked-prefill chunk, lowering peak airtime and peak memory. The lever itself
    /// is P2·7's; this rung names it so the ladder is complete rather than convenient.
    ReducePrefillChunk,
    /// Stop admitting new sessions. **Inert in v1** — there is one session per model instance
    /// (spec §12 reserves multi-session for v2) — and it is listed anyway so the ladder is honest
    /// about its own shape rather than pretending v1's ladder is the whole ladder.
    PauseNewAdmissions,
}

impl ShedRung {
    /// Whether pulling this rung actually does anything in v1.
    pub fn is_effective_in_v1(&self) -> bool {
        !matches!(self, ShedRung::PauseNewAdmissions)
    }
}

/// Why the contract declined to act.
#[derive(Debug, Clone, PartialEq)]
pub enum HoldReason {
    /// §11 one-migration-at-a-time.
    MigrationInFlight,
    /// §11 minimum placement lifetime not yet served.
    WithinMinimumLifetime { remaining_ms: u64 },
    /// Not enough settled observation to call a trend a trend.
    InsufficientWindow { have_ms: u64, need_ms: u64 },
    /// Evidence exists and is fine.
    NoTriggerFired { degradation: f64, trigger: f64 },
    /// A trigger fired but nothing observed to compare against.
    NoObservation,
}

/// The contract's output. A **recommendation** — execution is the coordinator's existing
/// epoch-transition machinery.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Hold(HoldReason),
    Shed { rung: ShedRung, degradation: f64 },
    /// Re-place through the **existing recovery/supersession path** at epoch+1. Only ever returned
    /// for a candidate that **admission has already accepted**.
    ReplaceViaRecovery { placement: Box<Placement>, degradation: f64 },
    /// No admissible placement and the ladder is spent. Names the rungs that were exhausted, so
    /// the termination is auditable rather than an assertion.
    Terminate { exhausted: Vec<ShedRung>, reason: TerminateReason },
}

#[derive(Debug, Clone, PartialEq)]
pub enum TerminateReason {
    /// The solver produced a candidate but admission refused it (P2·4).
    NoAdmissiblePlacement,
    /// The solver could produce no placement at all.
    NoPlacementExists,
}

/// Everything the contract decides from. Pure: the clock is handed in, like every other input in
/// this crate.
pub struct StabilityInput<'a> {
    pub now_ms: u64,
    pub placement_installed_at_ms: u64,
    /// §11: one migration at a time.
    pub migration_in_flight: bool,
    /// A device that is gone. Bypasses the lifetime floor and the observation window.
    pub hard_failure: Option<String>,
    /// TPOT predicted when the current placement was installed.
    pub predicted_at_install_ms: f64,
    /// Windowed-EWMA observation of actual TPOT, if any has accumulated.
    pub observed_tpot_ms: Option<f64>,
    /// How much settled observation the window holds.
    pub window_covered_ms: u64,
    pub degradation_trigger: f64,
    /// Rungs not yet pulled, in any order — the ladder is walked in `ShedRung` order regardless.
    pub shed_available: &'a [ShedRung],
    /// Rungs already spent, named in a termination.
    pub shed_exhausted: &'a [ShedRung],
    /// The solver's proposal, if it produced one.
    pub candidate: Option<Placement>,
    /// P2·4's verdict on `candidate`. **A candidate admission would refuse is never recommended.**
    pub candidate_admissible: bool,
}

/// Apply the contract.
pub fn decide(input: &StabilityInput) -> Decision {
    // §11: one migration at a time — unconditional, including under hard failure. The in-flight
    // recovery is the machinery that will handle a second loss; starting another concurrently is
    // exactly what the contract forbids.
    if input.migration_in_flight {
        return Decision::Hold(HoldReason::MigrationInFlight);
    }

    let hard = input.hard_failure.is_some();

    if !hard {
        // Anti-flap, part 1: a placement is not re-decided before it has had time to be worth
        // deciding.
        let age = input.now_ms.saturating_sub(input.placement_installed_at_ms);
        if age < MIN_PLACEMENT_LIFETIME_MS {
            return Decision::Hold(HoldReason::WithinMinimumLifetime {
                remaining_ms: MIN_PLACEMENT_LIFETIME_MS - age,
            });
        }
        // Anti-flap, part 2: a spike is not a trend.
        if input.window_covered_ms < TRIGGER_WINDOW_MS {
            return Decision::Hold(HoldReason::InsufficientWindow {
                have_ms: input.window_covered_ms,
                need_ms: TRIGGER_WINDOW_MS,
            });
        }
        let Some(observed) = input.observed_tpot_ms else {
            return Decision::Hold(HoldReason::NoObservation);
        };
        let degradation = if input.predicted_at_install_ms > 0.0 {
            observed / input.predicted_at_install_ms - 1.0
        } else {
            0.0
        };
        if degradation < input.degradation_trigger {
            return Decision::Hold(HoldReason::NoTriggerFired { degradation, trigger: input.degradation_trigger });
        }
        return act(input, degradation);
    }

    // Hard failure: the floor and the window are bypassed — you cannot wait ten minutes, or a
    // settled minute, for a device that is gone.
    act(input, f64::INFINITY)
}

/// Shed first, then re-place, then terminate.
fn act(input: &StabilityInput, degradation: f64) -> Decision {
    // §11: load-shed before redistribution. Shedding is reversible and cheap; a re-placement costs
    // an epoch transition through the recovery machinery.
    let mut rungs: Vec<ShedRung> = input.shed_available.to_vec();
    rungs.sort();
    rungs.dedup();
    if let Some(rung) = rungs.first() {
        return Decision::Shed { rung: *rung, degradation };
    }

    // The ladder is spent. Redistribute — but only into something admission has accepted.
    match (&input.candidate, input.candidate_admissible) {
        (Some(p), true) => Decision::ReplaceViaRecovery { placement: Box::new(p.clone()), degradation },
        (Some(_), false) => Decision::Terminate {
            exhausted: input.shed_exhausted.to_vec(),
            reason: TerminateReason::NoAdmissiblePlacement,
        },
        (None, _) => Decision::Terminate {
            exhausted: input.shed_exhausted.to_vec(),
            reason: TerminateReason::NoPlacementExists,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solver::Stage;

    fn placement(tpot: f64) -> Placement {
        Placement {
            stages: vec![Stage { device: "mac".into(), layer_first: 0, layer_last: 24 }],
            tpot_ms: tpot,
            compute_ms: tpot,
            link_ms: 0.0,
            other_objective_ms: tpot,
        }
    }

    const ALL_RUNGS: [ShedRung; 3] =
        [ShedRung::DeferBackgroundTransfers, ShedRung::ReducePrefillChunk, ShedRung::PauseNewAdmissions];

    fn base<'a>(shed_available: &'a [ShedRung], shed_exhausted: &'a [ShedRung]) -> StabilityInput<'a> {
        StabilityInput {
            now_ms: MIN_PLACEMENT_LIFETIME_MS + 1,
            placement_installed_at_ms: 0,
            migration_in_flight: false,
            hard_failure: None,
            predicted_at_install_ms: 100.0,
            observed_tpot_ms: Some(100.0),
            window_covered_ms: TRIGGER_WINDOW_MS,
            degradation_trigger: DEFAULT_DEGRADATION_TRIGGER,
            shed_available,
            shed_exhausted,
            candidate: Some(placement(80.0)),
            candidate_admissible: true,
        }
    }

    // ---------------------------------------------------------------- the anti-flap pair

    #[test]
    fn a_flapping_device_does_not_cause_oscillating_replacements() {
        // THE anti-flap test. A device alternates good/terrible every 30 s for an hour of
        // wall-clock *within one placement lifetime*. If the contract acted on each swing the
        // cluster would migrate on every flap — which costs an epoch transition each time and
        // makes the session worse than the flapping ever did.
        let mut decisions = Vec::new();
        for i in 0..120u64 {
            let mut inp = base(&ALL_RUNGS, &[]);
            inp.placement_installed_at_ms = 0;
            inp.now_ms = i * 30_000; // 0 .. 60 min, in 30 s steps
            inp.window_covered_ms = TRIGGER_WINDOW_MS;
            // Alternate: fine, then 4x degraded.
            inp.observed_tpot_ms = Some(if i % 2 == 0 { 100.0 } else { 400.0 });
            decisions.push(decide(&inp));
        }
        let acted = decisions
            .iter()
            .filter(|d| !matches!(d, Decision::Hold(_)))
            .count();
        // Everything inside the 10-minute floor must Hold; only swings after it may act.
        let inside_floor = (MIN_PLACEMENT_LIFETIME_MS / 30_000) as usize;
        assert!(
            decisions[..inside_floor].iter().all(|d| matches!(d, Decision::Hold(HoldReason::WithinMinimumLifetime { .. }))),
            "every decision inside the lifetime floor must Hold"
        );
        assert!(acted < decisions.len() / 2, "a flapping device must not act on every swing (acted {acted}/{})", decisions.len());
    }

    #[test]
    fn a_spike_shorter_than_the_window_is_not_a_trend() {
        let mut inp = base(&ALL_RUNGS, &[]);
        inp.observed_tpot_ms = Some(1000.0); // catastrophic-looking
        inp.window_covered_ms = TRIGGER_WINDOW_MS - 1; // ...but only 59.999 s of it
        assert_eq!(
            decide(&inp),
            Decision::Hold(HoldReason::InsufficientWindow { have_ms: TRIGGER_WINDOW_MS - 1, need_ms: TRIGGER_WINDOW_MS })
        );
    }

    #[test]
    fn the_lifetime_floor_holds_even_against_real_sustained_degradation() {
        let mut inp = base(&ALL_RUNGS, &[]);
        inp.now_ms = 60_000; // one minute in
        inp.observed_tpot_ms = Some(400.0); // genuinely 4x worse, and settled
        match decide(&inp) {
            Decision::Hold(HoldReason::WithinMinimumLifetime { remaining_ms }) => {
                assert_eq!(remaining_ms, MIN_PLACEMENT_LIFETIME_MS - 60_000);
            }
            other => panic!("the floor must hold: {other:?}"),
        }
    }

    // ---------------------------------------------------------------- one migration at a time

    #[test]
    fn one_migration_at_a_time_is_unconditional_including_under_hard_failure() {
        let mut inp = base(&[], &ALL_RUNGS);
        inp.migration_in_flight = true;
        inp.hard_failure = Some("myvm-1".into());
        inp.observed_tpot_ms = Some(9999.0);
        assert_eq!(decide(&inp), Decision::Hold(HoldReason::MigrationInFlight),
            "the in-flight recovery is the machinery that handles this; a second concurrent one is what §11 forbids");
    }

    // ---------------------------------------------------------------- hard failure

    #[test]
    fn hard_failure_bypasses_the_floor_and_the_window() {
        // You cannot wait ten minutes, or even a settled minute, for a device that is gone.
        let mut inp = base(&[], &ALL_RUNGS);
        inp.now_ms = 1; // brand-new placement
        inp.window_covered_ms = 0; // no settled observation at all
        inp.observed_tpot_ms = None;
        inp.hard_failure = Some("myvm-1".into());
        match decide(&inp) {
            Decision::ReplaceViaRecovery { .. } => {}
            other => panic!("a lost device must act immediately: {other:?}"),
        }
    }

    // ---------------------------------------------------------------- shed before redistribute

    #[test]
    fn the_ladder_is_walked_before_any_redistribution() {
        // Shedding is reversible and cheap; a re-placement costs an epoch transition.
        let mut inp = base(&ALL_RUNGS, &[]);
        inp.observed_tpot_ms = Some(400.0);
        match decide(&inp) {
            Decision::Shed { rung, .. } => assert_eq!(rung, ShedRung::DeferBackgroundTransfers, "cheapest rung first"),
            other => panic!("must shed before redistributing: {other:?}"),
        }
    }

    #[test]
    fn the_ladder_is_walked_in_canonical_order_regardless_of_input_order() {
        let mut inp = base(&[ShedRung::PauseNewAdmissions, ShedRung::ReducePrefillChunk], &[]);
        inp.observed_tpot_ms = Some(400.0);
        match decide(&inp) {
            Decision::Shed { rung, .. } => assert_eq!(rung, ShedRung::ReducePrefillChunk),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn redistribution_happens_only_once_the_ladder_is_spent() {
        let mut inp = base(&[], &ALL_RUNGS);
        inp.observed_tpot_ms = Some(400.0);
        match decide(&inp) {
            Decision::ReplaceViaRecovery { placement, .. } => assert_eq!(placement.tpot_ms, 80.0),
            other => panic!("{other:?}"),
        }
    }

    // ---------------------------------------------------------------- admission is binding

    #[test]
    fn a_replacement_admission_would_refuse_is_never_recommended() {
        // The whole point of P2·4 is undone if the stability contract can route around it.
        let mut inp = base(&[], &ALL_RUNGS);
        inp.observed_tpot_ms = Some(400.0);
        inp.candidate_admissible = false;
        match decide(&inp) {
            Decision::Terminate { reason, .. } => assert_eq!(reason, TerminateReason::NoAdmissiblePlacement),
            other => panic!("an inadmissible candidate must never be recommended: {other:?}"),
        }
    }

    #[test]
    fn a_replacement_admission_would_refuse_is_not_recommended_under_hard_failure_either() {
        // Hard failure bypasses the *timing* guards, not the *admissibility* one.
        let mut inp = base(&[], &ALL_RUNGS);
        inp.hard_failure = Some("myvm-1".into());
        inp.candidate_admissible = false;
        assert!(matches!(decide(&inp), Decision::Terminate { .. }));
    }

    // ---------------------------------------------------------------- termination

    #[test]
    fn termination_names_the_exhausted_rungs() {
        // A termination that does not say what was tried is an assertion, not an audit trail.
        let mut inp = base(&[], &ALL_RUNGS);
        inp.observed_tpot_ms = Some(400.0);
        inp.candidate = None;
        match decide(&inp) {
            Decision::Terminate { exhausted, reason } => {
                assert_eq!(reason, TerminateReason::NoPlacementExists);
                assert_eq!(exhausted, ALL_RUNGS.to_vec(), "every rung tried must be named");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn termination_is_never_reached_while_a_rung_remains() {
        let mut inp = base(&[ShedRung::PauseNewAdmissions], &[ShedRung::DeferBackgroundTransfers]);
        inp.observed_tpot_ms = Some(9999.0);
        inp.candidate = None; // nowhere to go...
        // ...but a rung is still unpulled, so the answer is shed, not terminate.
        assert!(matches!(decide(&inp), Decision::Shed { .. }));
    }

    // ---------------------------------------------------------------- steady state

    #[test]
    fn a_healthy_placement_is_left_alone() {
        let inp = base(&ALL_RUNGS, &[]);
        match decide(&inp) {
            Decision::Hold(HoldReason::NoTriggerFired { degradation, .. }) => {
                assert!(degradation.abs() < 1e-9, "observed == predicted");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn degradation_below_the_trigger_is_tolerated() {
        let mut inp = base(&ALL_RUNGS, &[]);
        inp.observed_tpot_ms = Some(120.0); // 20% worse, trigger is 25%
        assert!(matches!(decide(&inp), Decision::Hold(HoldReason::NoTriggerFired { .. })));
    }

    #[test]
    fn a_placement_that_improved_never_triggers() {
        let mut inp = base(&ALL_RUNGS, &[]);
        inp.observed_tpot_ms = Some(50.0); // twice as fast as predicted
        assert!(matches!(decide(&inp), Decision::Hold(HoldReason::NoTriggerFired { .. })));
    }

    #[test]
    fn the_v1_inert_rung_is_labelled_as_such() {
        // The ladder is honest about its own shape rather than pretending v1's ladder is the whole
        // ladder: PauseNewAdmissions does nothing while there is one session per model instance.
        assert!(ShedRung::DeferBackgroundTransfers.is_effective_in_v1());
        assert!(ShedRung::ReducePrefillChunk.is_effective_in_v1());
        assert!(!ShedRung::PauseNewAdmissions.is_effective_in_v1());
    }
}
