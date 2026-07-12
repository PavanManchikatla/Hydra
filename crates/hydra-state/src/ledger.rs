//! Token ledger + the four watermarks (spec §2.3, §2.6a). The ledger is the durable record of
//! what happened (I3); the four watermarks are **distinct quantities with distinct advance
//! conditions** — never a shared counter:
//!
//! - `ledger_durable_pos`     — advances on ledger fsync / segment admission.
//! - `prefill_stable_pos`     — advances **only** on a durable `INPUT_CHUNK_COMMIT` (input side).
//! - `generation_durable_pos` — advances **only** on a durable `GENERATION_COMMIT` meeting I19.
//! - `recovery_goal_pos`      — derived per §2.3c.
//!
//! Sampling runs ahead of durability: the provisional window `(generation_durable_pos, sampled_pos]`
//! is representable and **rolls back** on recovery (I7b/I15) — erasing everything provisional,
//! including luck. Emission is gated on `generation_durable_pos` (emit-after-commit, I6). Recovery
//! replay is teacher-forced from the ledger (I8): there is **no API** to sample a committed
//! position, so re-sampling history is unrepresentable, not merely untested.

use hydra_proto::{InputPos, OutputPos};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenOrigin {
    Prompt,
    ToolResult,
    Generated,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TokenEntry {
    pub position: OutputPos,
    pub token_id: u32,
    pub origin: TokenOrigin,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LedgerError {
    /// A sample was attempted after cancellation.
    Cancelled,
    /// I19 equalities (`generated_through == sampled_pos == last_output_pos`) not met.
    I19Mismatch { generated_through: OutputPos, sampled: OutputPos },
}

#[derive(Clone, Debug, Default)]
pub struct Ledger {
    /// Durable committed tokens — the teacher-forcing source of truth (I3/I8), positions
    /// `[0, generation_durable_pos]`.
    committed: Vec<TokenEntry>,
    /// Sampled-ahead provisional tokens at `(generation_durable_pos, sampled_pos]`.
    provisional: Vec<TokenEntry>,

    ledger_durable_pos: i64,     // OutputPos, -1 = none
    prefill_stable_pos: i64,     // InputPos,  -1 = none
    generation_durable_pos: i64, // OutputPos
    recovery_goal_pos: i64,      // OutputPos (derived §2.3c)
    sampled_pos: i64,            // OutputPos, provisional frontier
    emitted_pos: i64,            // OutputPos, <= generation_durable_pos

    cancel_cutoff_pos: Option<i64>,
}

impl Ledger {
    pub fn new() -> Self {
        Self {
            ledger_durable_pos: -1,
            prefill_stable_pos: -1,
            generation_durable_pos: -1,
            recovery_goal_pos: -1,
            sampled_pos: -1,
            emitted_pos: -1,
            ..Default::default()
        }
    }

    // ---- typed watermark accessors (I13: input vs output never interchangeable) ----
    pub fn generation_durable_pos(&self) -> OutputPos {
        OutputPos(self.generation_durable_pos)
    }
    pub fn prefill_stable_pos(&self) -> InputPos {
        InputPos(self.prefill_stable_pos)
    }
    pub fn ledger_durable_pos(&self) -> OutputPos {
        OutputPos(self.ledger_durable_pos)
    }
    pub fn recovery_goal_pos(&self) -> OutputPos {
        OutputPos(self.recovery_goal_pos)
    }
    pub fn sampled_pos(&self) -> OutputPos {
        OutputPos(self.sampled_pos)
    }
    pub fn emitted_pos(&self) -> OutputPos {
        OutputPos(self.emitted_pos)
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel_cutoff_pos.is_some()
    }
    /// Number of provisional (sampled-but-uncommitted) tokens.
    pub fn provisional_len(&self) -> usize {
        self.provisional.len()
    }

    /// S_P samples the next token, extending the provisional window **beyond** the durable
    /// frontier. This is the only way to add a generated token, so a committed position can never
    /// be re-sampled (I8 teacher forcing enforced structurally).
    pub fn sample_next(&mut self, token_id: u32) -> Result<OutputPos, LedgerError> {
        if self.is_cancelled() {
            return Err(LedgerError::Cancelled);
        }
        let pos = self.sampled_pos + 1;
        debug_assert!(pos > self.generation_durable_pos, "sampling must extend beyond the durable frontier");
        self.provisional.push(TokenEntry { position: OutputPos(pos), token_id, origin: TokenOrigin::Generated });
        self.sampled_pos = pos;
        Ok(OutputPos(pos))
    }

    /// Commit the provisional window as a durable `GENERATION_COMMIT` (I19). Advances **only**
    /// `generation_durable_pos` (and `ledger_durable_pos`) to `sampled_pos`, enforcing I19's
    /// equalities: `generated_through == sampled_pos == last_output_pos`.
    pub fn commit_generation(&mut self, generated_through: OutputPos) -> Result<(), LedgerError> {
        let last = self.sampled_pos;
        if generated_through.get() != last {
            return Err(LedgerError::I19Mismatch { generated_through, sampled: OutputPos(last) });
        }
        self.committed.append(&mut self.provisional);
        self.generation_durable_pos = last;
        if last > self.ledger_durable_pos {
            self.ledger_durable_pos = last;
        }
        Ok(())
    }

    /// Roll back the provisional window on recovery (I7b/I15): erase everything above
    /// `generation_durable_pos`. `sampled_pos` returns to the durable frontier.
    pub fn rollback_provisional(&mut self) {
        self.provisional.clear();
        self.sampled_pos = self.generation_durable_pos;
    }

    /// Advance `prefill_stable_pos` on a durable `INPUT_CHUNK_COMMIT` (input side; §2.4).
    pub fn commit_input_chunk(&mut self, last_input_pos: InputPos) {
        if last_input_pos.get() > self.prefill_stable_pos {
            self.prefill_stable_pos = last_input_pos.get();
        }
    }

    /// Set the derived recovery goal (§2.3c); DECODING ⇒ goal = `generation_durable_pos`.
    pub fn set_recovery_goal(&mut self, goal: OutputPos) {
        self.recovery_goal_pos = goal.get();
    }

    /// Emit-after-commit (I6): tokens become emittable only once committed. Returns the newly
    /// emittable committed tokens (positions `(emitted_pos, generation_durable_pos]`).
    pub fn emittable(&mut self) -> Vec<TokenEntry> {
        let from = self.emitted_pos + 1;
        let to = self.generation_durable_pos;
        let out: Vec<TokenEntry> =
            self.committed.iter().copied().filter(|e| e.position.get() >= from && e.position.get() <= to).collect();
        if to >= from {
            self.emitted_pos = to;
        }
        out
    }

    /// Cancellation (I9): `cancel_cutoff_pos = generation_durable_pos`; committed-but-unemitted
    /// tokens flush through the cutoff; provisional-only state is suppressed.
    pub fn cancel(&mut self) -> Vec<TokenEntry> {
        self.cancel_cutoff_pos = Some(self.generation_durable_pos);
        self.provisional.clear(); // only provisional (I7b/I24) suppressed
        self.sampled_pos = self.generation_durable_pos;
        self.emittable() // flush committed-but-unemitted through the cutoff
    }

    pub fn cancel_cutoff_pos(&self) -> Option<OutputPos> {
        self.cancel_cutoff_pos.map(OutputPos)
    }
}
