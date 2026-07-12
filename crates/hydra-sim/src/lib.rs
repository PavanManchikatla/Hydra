//! # hydra-sim
//!
//! Deterministic simulation testing (DST) harness for `hydra-state` (BLUEPRINT §4). A seeded,
//! single-threaded discrete-event simulator drives the pure state machines under an **adversarial
//! schedule** — message drop/duplicate/reorder/delay, actor crash-restart at any step, and WAL
//! writes that may crash before becoming durable — and asserts the invariants after **every
//! step**. Every failure reproduces from its `(seed, schedule)`, printed on failure.
//!
//! **This slice** drives the coordinator activation transaction (the crash-window/abort/finality
//! core, where TLC-1 lived) and catches the two coordinator-side mutations, **Mut4 (I25)** and
//! **Mut1 (PostDecisionLoss)**, through *randomized* runs. Full stage integration (adding the
//! stage-side Mut2/Mut3 parities), the real `hydra-wal` torn-write virtual disk, and the directed
//! scenario + TLC-trace replays land in subsequent slices.

use hydra_state::coordinator::WalKindTag;
use hydra_state::{invariants, ControlMsg, CoordEvent, CoordState, Coordinator, Effect, SessionId, WalRecord};

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

/// The simulated world: one coordinator + an abstract stage environment (ack emitters) + the
/// in-flight WAL/message state the adversarial scheduler manipulates.
pub struct World {
    coord: Coordinator,
    n_stages: u16,
    rng: Rng,
    /// A WAL write emitted but not yet durable (crash here loses it — the decided-but-untold window).
    pending_wal: Option<WalKindTag>,
    /// Stage COMMITTED acks in flight for the current attempt (drop = never delivered; dup = redelivered).
    commit_acks: Vec<u16>,
    finalize_acks: Vec<u16>,
    ack_attempt: u32,
    lost: Vec<u16>,
    schedule: Vec<String>,
    step_no: u64,
    /// A fresh activation transaction is started whenever the coordinator reaches a quiescent
    /// absorbing state — so one run exercises many independent transactions (many abort windows).
    round: u32,
    /// RNG-chosen adversarial goal for the current round: 0 = happy, 1 = abort-then-crash
    /// (the TLC-1/Mut4 window), 2 = complete-then-lose (the post-decision/Mut1 window). Still fully
    /// randomized — the intent is seeded — but each round drives toward one interesting window.
    round_intent: u8,
    /// Steps taken in the current round; a round that runs too long (e.g. an abort-crash loop that
    /// never completes) is force-reset so intents keep cycling and no run gets stuck.
    round_steps: u32,
}

impl World {
    pub fn new(seed: u64, n_stages: u16) -> Self {
        World {
            coord: Coordinator::new_initial(SessionId([7; 16]), n_stages, 1),
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
        }
    }

    fn new_round(&mut self) {
        self.round = self.round.wrapping_add(1);
        self.round_intent = self.rng.below(3) as u8;
        self.round_steps = 0;
        let mut sid = [7u8; 16];
        sid[0] = self.round as u8;
        sid[1] = (self.round >> 8) as u8;
        self.coord = Coordinator::new_initial(SessionId(sid), self.n_stages, 1);
        self.pending_wal = None;
        self.commit_acks.clear();
        self.finalize_acks.clear();
        self.lost.clear();
        self.schedule.push(format!("--- new round {} (intent {}) ---", self.round, self.round_intent));
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
    /// deliverable acks/WAL, and low-probability adversarial injections).
    fn candidates(&mut self) -> Vec<CoordEvent> {
        use CoordEvent::*;
        let mut c = Vec::new();
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
                c.push(ev);
            }
        }
        if let Some(tag) = self.pending_wal {
            c.push(WalDurable(tag));
        }
        for &r in &self.commit_acks {
            c.push(StageCommitted { rank: r, attempt: self.ack_attempt });
        }
        for &r in &self.finalize_acks {
            c.push(StageFinalized { rank: r, attempt: self.ack_attempt });
        }
        // Intent-driven adversarial bias (still randomized: `round_intent` is seeded).
        let intent = self.round_intent;
        let aborted = self.coord.attempt_aborted(self.coord.attempt());
        // intent 1 (abort-crash / TLC-1): abort a not-yet-aborted attempt, then crash before retry.
        if intent == 1 && self.coord.state() == CoordState::Committing && !aborted {
            c.push(ProceedAbort);
            c.push(ProceedAbort);
            c.push(ProceedAbort);
        }
        if intent == 1 && self.coord.state() == CoordState::ReadyAll && aborted {
            c.push(Crash);
            c.push(Crash);
            c.push(Crash);
        }
        // intent 2 (complete-lose / post-decision): lose a pending-finalize participant.
        if intent == 2 && self.coord.state() == CoordState::Finalizing {
            if let Some(&r) = self.finalize_acks.first() {
                for _ in 0..5 {
                    c.push(StageLost { rank: r });
                }
            }
        }
        // low-rate baseline adversity for all intents (keeps general coverage without derailing).
        if self.coord.state() != CoordState::Crashed && self.rng.chance(1, 20) {
            c.push(Crash);
        }
        if self.coord.state() == CoordState::Crashed {
            c.push(Restart);
        }
        if !self.lost_candidate().is_empty() && self.rng.chance(1, 40) {
            c.push(StageLost { rank: self.lost_candidate()[0] });
        }
        c
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
        let ev = cands[idx].clone();
        self.schedule.push(format!("{ev:?}"));

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

        // ---- invariant check after every step (spec §2.7) ----
        invariants::check(&self.coord).into_iter().next().map(|v| self.fail(v.invariant, v.detail))
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
