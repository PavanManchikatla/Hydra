//! Segment-checkpoint candidate isolation (spec §2.6b / §3 tool-call flow; invariant **I24**).
//!
//! The tool-call path is `PAUSED_TOOL → PREPARE_SEGMENT_CHECKPOINT → SEGMENT_COMMIT → install →
//! mini-prefill → SAMPLE_NEXT`. `PREPARE_SEGMENT_CHECKPOINT` **clones** the installed checkpoint,
//! **advances the candidate**, and **returns it without touching live state**; installation happens
//! **only after `SEGMENT_COMMIT` is durable**; an uncommitted candidate is **discarded** (crash,
//! cancellation, failed admission) and leaves **no trace** — segment admission gets the same
//! "state plus tokens, or nothing" property as a generation commit (I24).
//!
//! This is the pure `(state, event) → (state′, effects[])` SM that the DST harness drives, mirroring
//! `HydraActivationCore.tla` one-to-one:
//!   * `PrepareCandidate`         — `candidateCkpt' = installedCkpt + 1` (installed **UNCHANGED**)
//!   * `CommitSegmentAndInstall`  — `segCommitted' ∪= {cand}; installedCkpt' = cand; candidateCkpt' = 0`
//!   * `DropCandidate`/`CoordCrash` — `candidateCkpt' = 0` (installed **UNCHANGED**)
//!   * invariant `CandidateIsolation == installedCkpt ∈ segCommitted` — the installed checkpoint is
//!     **always** a durably-committed candidate; installing an uncommitted candidate violates it.
//!
//! Mutation `mutation_candidate_leak` (the sixth parity switch, monotone-mutation rule) sabotages
//! exactly this: `PREPARE` **installs the candidate into live state pre-commit**, so `installedCkpt`
//! is a candidate that is not (yet, or ever) in `segCommitted` — the DST harness must catch it.

use std::collections::BTreeSet;

/// The config-defined initial checkpoint id (committed at `INITIAL_COMMIT`; spec §1.4 / §2.6a).
pub const INITIAL_CHECKPOINT_ID: u64 = 1;

/// Events the segment-checkpoint SM receives.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SegmentEvent {
    /// `PREPARE_SEGMENT_CHECKPOINT{base_checkpoint_id, segment_id}` — clone installed, advance the
    /// candidate, return it. Live installed state is never mutated (I24).
    Prepare { segment_id: u64 },
    /// `SEGMENT_COMMIT` durable → the candidate becomes the installed checkpoint (idempotent install).
    Commit,
    /// Admission failure / cancellation → discard the candidate (no trace).
    Drop,
    /// The coordinator (candidate's owning peer) crashed — a volatile, uncommitted candidate dies with
    /// it (TLA+ `CoordCrash`: `candidateCkpt' = 0`).
    CoordCrash,
}

/// Effects the segment-checkpoint SM emits (control-plane acks).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SegmentEffect {
    /// `SEGMENT_CHECKPOINT_READY{candidate_snapshot id, segment_id}` — the prepared candidate.
    CandidateReady { candidate_id: u64, segment_id: u64 },
    /// `SAMPLER_CHECKPOINT_INSTALLED{checkpoint_id}` — after `SEGMENT_COMMIT` durable (I17).
    Installed { checkpoint_id: u64 },
    /// The uncommitted candidate was discarded (crash/cancel/failed admission) — leaves no trace.
    Dropped { candidate_id: u64 },
}

/// S_P's segment-checkpoint state: the installed (live) checkpoint, the (at most one) prepared
/// uncommitted candidate, and the set of durably-committed checkpoint ids.
#[derive(Clone, Debug)]
pub struct SegmentCheckpoint {
    /// The installed, live sampler checkpoint id (what generation samples from).
    installed: u64,
    /// A prepared, not-yet-committed candidate (`None` = none). At most one at a time.
    candidate: Option<u64>,
    /// Durably-committed checkpoint ids (the initial checkpoint + every `SEGMENT_COMMIT`ed candidate).
    seg_committed: BTreeSet<u64>,
}

impl Default for SegmentCheckpoint {
    fn default() -> Self {
        Self::initial()
    }
}

impl SegmentCheckpoint {
    /// The starting state: the config-defined initial checkpoint is installed and committed; no
    /// candidate (TLA+ `Init`: `installedCkpt = 1 ∧ candidateCkpt = 0 ∧ segCommitted = {1}`).
    pub fn initial() -> Self {
        let mut seg_committed = BTreeSet::new();
        seg_committed.insert(INITIAL_CHECKPOINT_ID);
        SegmentCheckpoint { installed: INITIAL_CHECKPOINT_ID, candidate: None, seg_committed }
    }

    /// Step the SM. Pure: mutates only `self` and returns the effects.
    pub fn step(&mut self, ev: SegmentEvent) -> Vec<SegmentEffect> {
        match ev {
            SegmentEvent::Prepare { segment_id } => {
                // Only one candidate in flight (a second PREPARE while one is pending is ignored —
                // the coordinator serializes segment admission).
                if self.candidate.is_some() {
                    return Vec::new();
                }
                // Clone-advance: the candidate id is `installed + 1`; the installed checkpoint is
                // **not** touched (I24). This is the whole point — preparation is side-effect-free.
                let cand = self.installed + 1;
                self.candidate = Some(cand);

                // Mut6 (`mutation_candidate_leak`): the candidate mutates live installed state
                // BEFORE its commit record is durable. `installed` becomes `cand`, which is not in
                // `seg_committed` — `CandidateIsolation` is violated the instant a later check runs
                // (and permanently, if this candidate is dropped/crashed rather than committed).
                #[cfg(feature = "mutation_candidate_leak")]
                {
                    self.installed = cand;
                }

                vec![SegmentEffect::CandidateReady { candidate_id: cand, segment_id }]
            }
            SegmentEvent::Commit => match self.candidate.take() {
                Some(cand) => {
                    // Durable first, then install (order preserves `installedCkpt ∈ segCommitted`):
                    // the candidate is recorded committed, then adopted as the live checkpoint.
                    self.seg_committed.insert(cand);
                    self.installed = cand;
                    vec![SegmentEffect::Installed { checkpoint_id: cand }]
                }
                None => Vec::new(),
            },
            SegmentEvent::Drop => match self.candidate.take() {
                // Discard the uncommitted candidate — installed live state is untouched (no trace).
                Some(cand) => vec![SegmentEffect::Dropped { candidate_id: cand }],
                None => Vec::new(),
            },
            SegmentEvent::CoordCrash => {
                // A volatile, uncommitted candidate dies with its owning peer; the installed
                // (durable) checkpoint is unaffected.
                self.candidate = None;
                Vec::new()
            }
        }
    }

    /// **I24 (candidate isolation):** the installed checkpoint is always a durably-committed one.
    /// Equivalent to the TLA+ `CandidateIsolation == installedCkpt ∈ segCommitted`.
    pub fn candidate_isolation_holds(&self) -> bool {
        self.seg_committed.contains(&self.installed)
    }

    /// The installed (live) checkpoint id.
    pub fn installed(&self) -> u64 {
        self.installed
    }

    /// The prepared, uncommitted candidate id, if any.
    pub fn candidate(&self) -> Option<u64> {
        self.candidate
    }

    /// Is a checkpoint id durably committed?
    pub fn is_committed(&self, id: u64) -> bool {
        self.seg_committed.contains(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prepare → the candidate is offered but the installed live state is untouched (I24), and
    /// isolation holds; commit → the candidate installs and is committed; isolation still holds.
    #[test]
    fn prepare_is_side_effect_free_then_commit_installs() {
        let mut sc = SegmentCheckpoint::initial();
        assert_eq!(sc.installed(), 1);
        assert!(sc.candidate_isolation_holds());

        let eff = sc.step(SegmentEvent::Prepare { segment_id: 7 });
        assert_eq!(eff, vec![SegmentEffect::CandidateReady { candidate_id: 2, segment_id: 7 }]);
        // Live installed state NOT mutated by preparation.
        assert_eq!(sc.installed(), 1, "PREPARE must not touch live installed state (I24)");
        assert!(sc.candidate_isolation_holds());
        assert!(!sc.is_committed(2), "the candidate is not durable yet");

        let eff = sc.step(SegmentEvent::Commit);
        assert_eq!(eff, vec![SegmentEffect::Installed { checkpoint_id: 2 }]);
        assert_eq!(sc.installed(), 2);
        assert!(sc.is_committed(2));
        assert!(sc.candidate_isolation_holds(), "installed is now a committed candidate");
    }

    /// Prepare → drop (admission failure / cancellation) → the candidate leaves NO trace: installed
    /// unchanged, isolation holds, the discarded id never becomes committed.
    #[test]
    fn dropped_candidate_leaves_no_trace() {
        let mut sc = SegmentCheckpoint::initial();
        sc.step(SegmentEvent::Prepare { segment_id: 1 });
        let eff = sc.step(SegmentEvent::Drop);
        assert_eq!(eff, vec![SegmentEffect::Dropped { candidate_id: 2 }]);
        assert_eq!(sc.installed(), 1, "installed live state untouched after a dropped candidate");
        assert_eq!(sc.candidate(), None);
        assert!(!sc.is_committed(2));
        assert!(sc.candidate_isolation_holds());
    }

    /// A coordinator crash discards a volatile uncommitted candidate; the installed checkpoint stands.
    #[test]
    fn coord_crash_discards_the_volatile_candidate() {
        let mut sc = SegmentCheckpoint::initial();
        sc.step(SegmentEvent::Prepare { segment_id: 3 });
        assert_eq!(sc.candidate(), Some(2));
        sc.step(SegmentEvent::CoordCrash);
        assert_eq!(sc.candidate(), None);
        assert_eq!(sc.installed(), 1);
        assert!(sc.candidate_isolation_holds());
    }

    /// The long-deferred **uncommitted-segment-candidate directed scenario** (spec §8 DST list, I24):
    /// S_P prepares a candidate; admission fails / C crashes **before `SEGMENT_COMMIT` is durable**;
    /// the live sampler must show **no trace** of the segment. Both pre-commit windows are exercised,
    /// and after each the installed checkpoint is exactly what it was before the prepare, the discarded
    /// candidate id is never committed, and a stray late `Commit` cannot resurrect the discarded
    /// candidate (nothing to commit → no-op).
    #[test]
    fn uncommitted_segment_candidate_leaves_the_live_sampler_untouched() {
        for admission_fails_via_drop in [true, false] {
            let mut sc = SegmentCheckpoint::initial();
            let installed_before = sc.installed();
            let committed_before = sc.is_committed(installed_before);
            assert!(committed_before && sc.candidate_isolation_holds());

            // S_P prepares a segment candidate (clone-advance-return; side-effect-free).
            let ready = sc.step(SegmentEvent::Prepare { segment_id: 42 });
            let cand = match ready.as_slice() {
                [SegmentEffect::CandidateReady { candidate_id, .. }] => *candidate_id,
                other => panic!("expected SEGMENT_CHECKPOINT_READY, got {other:?}"),
            };
            assert_eq!(sc.installed(), installed_before, "PREPARE never touches live installed state (I24)");

            // The window closes WITHOUT a durable SEGMENT_COMMIT: either admission fails (Drop) or the
            // coordinator (candidate's owning peer) crashes.
            if admission_fails_via_drop {
                assert_eq!(sc.step(SegmentEvent::Drop), vec![SegmentEffect::Dropped { candidate_id: cand }]);
            } else {
                assert!(sc.step(SegmentEvent::CoordCrash).is_empty());
            }

            // NO TRACE: installed live state unchanged, isolation holds, the candidate never committed,
            // no candidate lingers, and a stray late COMMIT is a no-op (cannot resurrect it).
            assert_eq!(sc.installed(), installed_before, "live sampler shows no trace of the uncommitted segment");
            assert!(sc.candidate_isolation_holds());
            assert!(!sc.is_committed(cand), "the discarded candidate is never durably committed");
            assert_eq!(sc.candidate(), None, "no candidate lingers after the window closes");
            assert!(sc.step(SegmentEvent::Commit).is_empty(), "a late COMMIT cannot resurrect a discarded candidate");
            assert_eq!(sc.installed(), installed_before);
        }
    }

    /// The faithful build keeps isolation across an arbitrary prepare/commit/drop/crash mix; the
    /// mutated build (`mutation_candidate_leak`) breaks it the moment a candidate is prepared but not
    /// committed. This asserts the invariant is the exact fault line the mutation trips.
    #[test]
    fn isolation_holds_faithfully_and_the_mutation_breaks_it() {
        let mut sc = SegmentCheckpoint::initial();
        sc.step(SegmentEvent::Prepare { segment_id: 9 });
        // Before any COMMIT: faithful build isolates the candidate; the mutated build has already
        // leaked it into `installed`.
        #[cfg(not(feature = "mutation_candidate_leak"))]
        assert!(sc.candidate_isolation_holds(), "faithful: an uncommitted candidate never installs");
        #[cfg(feature = "mutation_candidate_leak")]
        assert!(!sc.candidate_isolation_holds(), "mutation_candidate_leak: candidate leaked into live state pre-commit");
    }
}
