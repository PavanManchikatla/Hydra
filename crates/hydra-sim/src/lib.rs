//! # hydra-sim
//!
//! Deterministic simulation testing (DST) harness for `hydra-state` (BLUEPRINT §4). A seeded,
//! single-threaded discrete-event simulator drives the pure state machines under an **adversarial
//! schedule** — message drop/duplicate/reorder/delay, actor crash-restart at any step, and WAL
//! writes that may crash before becoming durable — and asserts the invariants after **every
//! step**. Every failure reproduces from its `(seed, schedule)`, printed on failure.
//!
//! **What this slice drives.** Two tracks share one adversarial scheduler and one per-step
//! invariant check:
//!
//! * **Coordinator activation track** (the crash-window/abort/finality core where TLC-1 lived):
//!   the `ACTIVATION_COMMIT_INTENT → COMMIT → COMPLETE → FINALIZE` transaction, catching the two
//!   coordinator-side mutations **Mut4 (I25 AbortFinality)** and **Mut1 (I22 PostDecisionLoss)**
//!   through *randomized* runs.
//! * **Stage recovery/activation track** (new this slice): real [`Stage`] state machines are
//!   driven through their reconstruction+activation lifecycle — catch-up, `RESET_RECOVERY_ATTEMPT`,
//!   `BEGIN_RECOVERY` Case B replay, and the `COMMIT/ABORT/COMMIT` attempt-fencing sequence — so
//!   the two stage-side mutations **Mut2 (CaseBPure, from a label-only reset)** and **Mut3 (F2
//!   AttemptFence, from missing attempt fencing)** are now caught by *randomized* runs too, not
//!   just the directed SM-level tests. `check_stage` runs on every stage after every step.
//!
//! The stage track models the coordinator's reconstruction *orchestration* (the phase that emits
//! `BEGIN_RECOVERY`/`RESET`) directly from the scheduler — that orchestration is not yet in the
//! coordinator transition core, and the `Stage` SM is the unit under test for Mut2/Mut3. Wiring
//! the real stage acks back into the coordinator's commit loop is a later fidelity extension; it
//! is deliberately kept separate here so the proven Mut1/Mut4 catch behavior is not disturbed.

use hydra_state::coordinator::WalKindTag;
use hydra_state::stage::{StageEvent, StageState};
use hydra_state::{
    invariants, ActivationKind, ActivationTuple, ControlMsg, CoordEvent, CoordState, Coordinator,
    Effect, Epoch, RecoveryId, SessionId, Stage, WalRecord,
};

pub mod rng;
pub use rng::Rng;

/// A detected failure, fully reproducible from `(seed, schedule)`.
#[derive(Debug, Clone)]
pub struct Failure {
    pub seed: u64,
    pub step: u64,
    pub invariant: String,
    pub detail: String,
    /// The exact action sequence taken (re-running the same seed reproduces it).
    pub schedule: Vec<String>,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "SIM FAILURE: {} — {}", self.invariant, self.detail)?;
        writeln!(f, "  reproduce with seed={} (step {})", self.seed, self.step)?;
        writeln!(f, "  schedule ({} actions):", self.schedule.len())?;
        for (i, a) in self.schedule.iter().enumerate() {
            writeln!(f, "    {i:>4}: {a}")?;
        }
        Ok(())
    }
}

/// One scheduler action: either a coordinator event or a stage event addressed to a rank.
#[derive(Clone, Debug)]
enum Action {
    Coord(CoordEvent),
    Stage(usize, StageEvent),
}

/// The simulated world: one coordinator + an abstract stage environment (ack emitters) for the
/// coordinator track, plus **real [`Stage`] machines** for the recovery/activation track, all
/// manipulated by one adversarial scheduler.
pub struct World {
    coord: Coordinator,
    /// Real stage state machines (the Mut2/Mut3 units under test). `stages[0]` is the recovery
    /// subject for the stage-track intents; the rest sit idle so their invariants stay checked.
    stages: Vec<Stage>,
    n_stages: u16,
    rng: Rng,
    /// A WAL write emitted but not yet durable (crash here loses it — the decided-but-untold window).
    pending_wal: Option<WalKindTag>,
    /// Stage COMMITTED acks in flight for the current attempt (drop = never delivered; dup = redelivery).
    commit_acks: Vec<u16>,
    finalize_acks: Vec<u16>,
    ack_attempt: u32,
    lost: Vec<u16>,
    schedule: Vec<String>,
    step_no: u64,
    /// A fresh activation transaction is started whenever the coordinator reaches a quiescent
    /// absorbing state — so one run exercises many independent transactions (many abort windows).
    round: u32,
    /// RNG-chosen adversarial goal for the current round. Coordinator-track intents: 0 = happy,
    /// 1 = abort-then-crash (the TLC-1/Mut4 window), 2 = complete-then-lose (post-decision/Mut1).
    /// Stage-track intents: 3 = reset-after-catch-up (Mut2/CaseBPure window), 4 = stale-attempt
    /// commit (Mut3/F2 window). Still fully randomized — the intent is seeded — but each round
    /// drives toward one interesting window.
    round_intent: u8,
    /// Steps taken in the current round; a round that runs too long (e.g. an abort-crash loop that
    /// never completes) is force-reset so intents keep cycling and no run gets stuck.
    round_steps: u32,
    /// Recovery params for the current stage-track round (seeded per round): the Case-B target
    /// epoch, the truncate frontier `t`, and the catch-up `goal > t`. The stage is caught up past
    /// `t`, RESET to `t`, then Case-B replayed at `t`: faithful RESET truncates (clean); Mut2's
    /// label-only RESET leaves `applied = goal > t`, tripping CaseBPure.
    rec_target: Epoch,
    rec_truncate: i64,
    rec_goal: i64,
}

const N_INTENTS: usize = 5;

impl World {
    pub fn new(seed: u64, n_stages: u16) -> Self {
        let mut w = World {
            coord: Coordinator::new_initial(SessionId([7; 16]), n_stages, 1),
            stages: Vec::new(),
            n_stages,
            rng: Rng::new(seed),
            pending_wal: None,
            commit_acks: Vec::new(),
            finalize_acks: Vec::new(),
            ack_attempt: 0,
            lost: Vec::new(),
            schedule: Vec::new(),
            step_no: 0,
            round: 0,
            round_intent: 1, // first round targets the abort-crash window
            round_steps: 0,
            rec_target: 1,
            rec_truncate: 1,
            rec_goal: 3,
        };
        w.setup_stages();
        w
    }

    fn new_round(&mut self) {
        self.round = self.round.wrapping_add(1);
        self.round_intent = self.rng.below(N_INTENTS) as u8;
        self.round_steps = 0;
        let mut sid = [7u8; 16];
        sid[0] = self.round as u8;
        sid[1] = (self.round >> 8) as u8;
        self.coord = Coordinator::new_initial(SessionId(sid), self.n_stages, 1);
        self.pending_wal = None;
        self.commit_acks.clear();
        self.finalize_acks.clear();
        self.lost.clear();
        self.setup_stages();
        self.schedule.push(format!("--- new round {} (intent {}) ---", self.round, self.round_intent));
    }

    /// Initialize the real stage machines for the current round's intent. For the stage-track
    /// intents `stages[0]` is placed at the start of the target recovery sequence; other stages
    /// (and all stages under coordinator-track intents) sit idle in `FROZEN_READY`.
    fn setup_stages(&mut self) {
        self.stages.clear();
        match self.round_intent {
            3 => {
                // reset-after-catch-up (Mut2). Seeded params keep the sequence varied but legal.
                self.rec_target = 1;
                self.rec_truncate = 1 + self.rng.below(2) as i64; // 1..=2
                self.rec_goal = self.rec_truncate + 1 + self.rng.below(3) as i64; // t+1..=t+3
                // A survivor already at the target epoch, applied 0, about to catch up.
                self.stages.push(Stage::frozen(0, self.rec_target, 0, 0));
                for r in 1..self.n_stages {
                    self.stages.push(Stage::frozen_ready(r, self.rec_target, 0));
                }
            }
            _ => {
                // intent 4 (stale-attempt commit, Mut3) and the coordinator-track intents: all
                // stages FROZEN_READY at epoch 0. Intent 4 drives stages[0] through commit/abort/
                // commit/stale-commit; the others stay idle.
                for r in 0..self.n_stages {
                    self.stages.push(Stage::frozen_ready(r, 0, 0));
                }
            }
        }
    }

    fn fail(&self, invariant: &str, detail: impl Into<String>) -> Failure {
        Failure {
            seed: 0,
            step: self.step_no,
            invariant: invariant.to_string(),
            detail: detail.into(),
            schedule: self.schedule.clone(),
        }
    }

    fn interpret(&mut self, effects: Vec<Effect>) {
        for e in effects {
            match e {
                Effect::WriteWal { record, .. } => {
                    self.pending_wal = match record {
                        WalRecord::ActivationCommitIntent { .. } => Some(WalKindTag::Intent),
                        WalRecord::ActivationComplete { .. } => Some(WalKindTag::Complete),
                        WalRecord::ActivationAbort { .. } => Some(WalKindTag::Abort),
                        _ => self.pending_wal,
                    };
                }
                Effect::Send { msg, .. } => match msg {
                    ControlMsg::CommitActivation { tuple } => {
                        self.commit_acks = (0..self.n_stages).filter(|r| !self.lost.contains(r)).collect();
                        self.ack_attempt = tuple.attempt;
                    }
                    ControlMsg::FinalizeActivation { tuple, .. } => {
                        self.finalize_acks = (0..self.n_stages).filter(|r| !self.lost.contains(r)).collect();
                        self.ack_attempt = tuple.attempt;
                    }
                    ControlMsg::ActivationCommitAbort { .. } => {
                        self.commit_acks.clear();
                    }
                },
            }
        }
    }

    /// Build the set of currently-possible scheduler actions (enabled coordinator actions,
    /// deliverable acks/WAL, real-stage recovery events, and low-probability adversarial injections).
    ///
    /// Each round belongs to **one track**: coordinator-track intents (0–2) drive the coordinator
    /// activation transaction; stage-track intents (3–4) drive the real `Stage` sequence to
    /// completion with the coordinator idle. Racing the two in one round let the coordinator's
    /// happy path reach `Serviceable` (→ `new_round`, wiping the stages) before the longer
    /// catch-up→reset→replay sequence finished, so Mut2 was never observed — the tracks are
    /// separated by round instead. Both are invariant-checked identically every step.
    fn candidates(&mut self) -> Vec<Action> {
        use CoordEvent::*;
        let mut c: Vec<Action> = Vec::new();
        if self.round_intent >= 3 {
            // stage-track round: the scheduler focuses on the stage sequence (Mut2/Mut3).
            self.stage_candidates(&mut c);
            return c;
        }
        for ev in [
            StagesReconstructed,
            ProceedWriteIntent,
            ProceedSendCommit,
            ProceedAbort,
            ProceedWriteComplete,
            ProceedSendFinalize,
            ProceedBecomeServiceable,
            ProceedRecordUnservable,
            ProceedStartSuperseding,
        ] {
            if self.coord.enabled(&ev) {
                c.push(Action::Coord(ev));
            }
        }
        if let Some(tag) = self.pending_wal {
            c.push(Action::Coord(WalDurable(tag)));
        }
        for &r in &self.commit_acks {
            c.push(Action::Coord(StageCommitted { rank: r, attempt: self.ack_attempt }));
        }
        for &r in &self.finalize_acks {
            c.push(Action::Coord(StageFinalized { rank: r, attempt: self.ack_attempt }));
        }
        // Intent-driven adversarial bias (still randomized: `round_intent` is seeded).
        let intent = self.round_intent;
        let aborted = self.coord.attempt_aborted(self.coord.attempt());
        // intent 1 (abort-crash / TLC-1): abort a not-yet-aborted attempt, then crash before retry.
        if intent == 1 && self.coord.state() == CoordState::Committing && !aborted {
            c.push(Action::Coord(ProceedAbort));
            c.push(Action::Coord(ProceedAbort));
            c.push(Action::Coord(ProceedAbort));
        }
        if intent == 1 && self.coord.state() == CoordState::ReadyAll && aborted {
            c.push(Action::Coord(Crash));
            c.push(Action::Coord(Crash));
            c.push(Action::Coord(Crash));
        }
        // intent 2 (complete-lose / post-decision): lose a pending-finalize participant.
        if intent == 2 && self.coord.state() == CoordState::Finalizing {
            if let Some(&r) = self.finalize_acks.first() {
                for _ in 0..5 {
                    c.push(Action::Coord(StageLost { rank: r }));
                }
            }
        }
        // low-rate baseline adversity for all intents (keeps general coverage without derailing).
        if self.coord.state() != CoordState::Crashed && self.rng.chance(1, 20) {
            c.push(Action::Coord(Crash));
        }
        if self.coord.state() == CoordState::Crashed {
            c.push(Action::Coord(Restart));
        }
        if !self.lost_candidate().is_empty() && self.rng.chance(1, 40) {
            c.push(Action::Coord(StageLost { rank: self.lost_candidate()[0] }));
        }
        c
    }

    /// Stage-track candidate generation. For intent 3 it walks `stages[0]` through
    /// catch-up → RESET → Case-B replay; for intent 4 through commit → abort → commit → stale
    /// commit. Each is a *legal* protocol sequence: faithful stages stay clean, and the active
    /// mutation turns it into the invariant violation the checker catches.
    fn stage_candidates(&mut self, c: &mut Vec<Action>) {
        if self.stages.is_empty() {
            return;
        }
        let s = &self.stages[0];
        // Offer the target event three times so it dominates coordinator noise in the pick.
        let bias = |c: &mut Vec<Action>, ev: StageEvent| {
            for _ in 0..3 {
                c.push(Action::Stage(0, ev.clone()));
            }
        };
        match self.round_intent {
            3 => {
                let (t, goal, target) = (self.rec_truncate, self.rec_goal, self.rec_target);
                match s.state() {
                    // catch up toward the goal. RebuildStep advances `applied` while below the
                    // goal and, once `applied == goal`, flips the stage to FROZEN_READY — so this
                    // must keep being offered *at* the goal (not just strictly below it), or the
                    // stage stalls in REBUILDING and the round resets before RESET/BEGIN.
                    StageState::Frozen | StageState::Rebuilding if s.recovery_id() == 0 => {
                        bias(c, StageEvent::RebuildStep { goal });
                    }
                    // caught up (FROZEN_READY at recovery_id 0): RESET truncating back to `t`
                    StageState::FrozenReady if s.recovery_id() == 0 => {
                        bias(c, StageEvent::RecvReset { target, new_recovery_id: 1, truncate_to: t });
                    }
                    // reset done (FROZEN, recovery_id bumped): Case-B BEGIN_RECOVERY replay at `t`
                    StageState::Frozen if s.recovery_id() == 1 => {
                        bias(
                            c,
                            StageEvent::RecvBegin { base: 0, target, recovery_id: 1, truncate_to: t },
                        );
                    }
                    _ => {}
                }
            }
            4 => {
                let ep: Epoch = 0;
                let rid: RecoveryId = 0;
                let tup = |attempt| ActivationTuple {
                    kind: ActivationKind::Recovery,
                    epoch: ep,
                    recovery_id: rid,
                    attempt,
                    sampler_checkpoint_id: 1,
                };
                match (s.state(), s.attempt(), s.highest_attempt()) {
                    // fresh: commit attempt 1
                    (StageState::FrozenReady, _, 0) => bias(c, StageEvent::RecvCommit { tuple: tup(1) }),
                    // PREACTIVE at attempt 1: abort it
                    (StageState::Preactive, 1, _) => bias(c, StageEvent::RecvAbort { attempt: 1 }),
                    // post-abort FROZEN_READY (fence floor 1): retry at attempt 2
                    (StageState::FrozenReady, _, 1) => bias(c, StageEvent::RecvCommit { tuple: tup(2) }),
                    // PREACTIVE at attempt 2: deliver the stale attempt-1 COMMIT (fenced faithfully;
                    // accepted under Mut3 → attempt regresses below highest → F2 AttemptFence)
                    (StageState::Preactive, 2, _) => bias(c, StageEvent::RecvCommit { tuple: tup(1) }),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn lost_candidate(&self) -> Vec<u16> {
        (0..self.n_stages).filter(|r| !self.lost.contains(r)).collect()
    }

    /// Advance one step: pick an enabled action, apply it, interpret effects, check invariants.
    pub fn step(&mut self) -> Option<Failure> {
        self.step_no += 1;
        // Liveness watchdog FIRST: a wedged post-decision state has no action that progresses it,
        // so it must be caught before the quiescence reset would paper over it (I22 / Mut1).
        if self.coord.post_decision_deadlock() {
            return Some(self.fail(
                "I22 PostDecisionLoss",
                "coordinator wedged after a durable decision with a lost participant",
            ));
        }
        // A successful (or terminal) transaction is an absorbing state — start a fresh activation
        // transaction so one run exercises MANY of them (many abort / loss windows), rather than
        // spinning on re-finalizing the completed one.
        if matches!(self.coord.state(), CoordState::Serviceable | CoordState::Terminal) {
            self.new_round();
            return None;
        }
        // Round step-cap: a round that runs too long (e.g. an abort-crash loop that never
        // completes) is reset so intents keep cycling and no run wedges in one window forever.
        self.round_steps += 1;
        if self.round_steps > 60 {
            self.new_round();
            return None;
        }
        let cands = self.candidates();
        if cands.is_empty() {
            self.new_round(); // quiescent absorbing state → start a fresh activation transaction
            return None;
        }
        let idx = self.rng.below(cands.len());
        let action = cands[idx].clone();
        self.schedule.push(format!("{action:?}"));

        match action {
            Action::Coord(ev) => {
                // Crash loses any in-flight (non-durable) WAL write — the decided-but-untold window.
                if matches!(ev, CoordEvent::Crash) {
                    self.pending_wal = None;
                }
                // Ack delivery: normally remove (delivered); sometimes keep (duplicate → redelivery).
                match &ev {
                    CoordEvent::StageCommitted { rank, .. } => {
                        if self.rng.chance(3, 4) {
                            self.commit_acks.retain(|r| r != rank);
                        }
                    }
                    CoordEvent::StageFinalized { rank, .. } => {
                        if self.rng.chance(3, 4) {
                            self.finalize_acks.retain(|r| r != rank);
                        }
                    }
                    CoordEvent::StageLost { rank } => {
                        self.lost.push(*rank);
                        self.finalize_acks.retain(|r| r != rank);
                    }
                    CoordEvent::WalDurable(_) => {
                        self.pending_wal = None;
                    }
                    _ => {}
                }
                let effects = self.coord.step(ev);
                self.interpret(effects);
            }
            Action::Stage(i, ev) => {
                if let Some(st) = self.stages.get_mut(i) {
                    st.step(ev);
                }
            }
        }

        // ---- invariant check after every step (spec §2.7): coordinator + every real stage ----
        if let Some(v) = invariants::check(&self.coord).into_iter().next() {
            return Some(self.fail(v.invariant, v.detail));
        }
        for st in &self.stages {
            if let Some(v) = invariants::check_stage(st).into_iter().next() {
                return Some(self.fail(v.invariant, v.detail));
            }
        }
        None
    }
}

/// Run one seed for up to `max_steps`. Returns the first failure, or `None` if clean.
pub fn run(seed: u64, n_stages: u16, max_steps: u64) -> Option<Failure> {
    let mut w = World::new(seed, n_stages);
    for _ in 0..max_steps {
        if let Some(mut f) = w.step() {
            f.seed = seed;
            return Some(f);
        }
    }
    None
}

/// Run `seeds` seeds × `steps` steps. Returns the first failure across all seeds (with its seed).
pub fn run_many(base_seed: u64, seeds: u64, n_stages: u16, steps: u64) -> Result<u64, Failure> {
    let mut total = 0u64;
    for s in 0..seeds {
        let seed = base_seed.wrapping_add(s).wrapping_mul(0x100000001B3);
        if let Some(f) = run(seed, n_stages, steps) {
            return Err(f);
        }
        total += steps;
    }
    Ok(total)
}
