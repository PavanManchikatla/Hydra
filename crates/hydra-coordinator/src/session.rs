//! The generation **session** — where emit-after-commit is enforced structurally.
//!
//! **The gate is absolute:** an event exists (and thus can be emitted) *only* after its tokens are
//! durable. Events are appended to the log **inside** [`Session::commit_group`], **after**
//! `append_generation_commit` (the `fdatasync`'d durable write) returns — so if durability
//! stalls/fails, no event is created and nothing leaves the process past the last durable position.
//! This is proven by absence, not just presence (see `tests/session_gate.rs`).
//!
//! Token pieces are **bytes** run through one persistent [`Utf8Streamer`] across commit boundaries,
//! so a glyph split across a group commit buffers and is emitted whole once complete (I6). The event
//! log is a pure function of the durable committed tokens (spec §8) — resume replays it.

use hydra_tokenizer::{Tokenizer, Utf8Streamer};

use crate::commit_stream::{CommitError, CommitStream, GroupCommitter, WalFenceCtx};
use crate::event_log::{Event, EventLog};

/// Maps a token id to its raw display bytes. `hydra-tokenizer`'s [`Tokenizer`] in production; a stub
/// in tests (so the gate/resume/backpressure logic is CI-covered without the engine). Not `Send` —
/// the real tokenizer holds a non-`Send` engine handle, so a `Session` lives on one thread (the
/// generation pump's), like a worker.
pub trait PieceSource {
    fn piece(&self, token: u32) -> Vec<u8>;
}

/// [`PieceSource`] over the real llama.cpp-delegated tokenizer.
pub struct TokenizerPieces(pub Tokenizer);

impl PieceSource for TokenizerPieces {
    fn piece(&self, token: u32) -> Vec<u8> {
        self.0.piece(token).unwrap_or_default()
    }
}

/// One sampled token from S_P: its output position, id, and the post-sample snapshot(q) the commit
/// will embed.
#[derive(Clone, Debug)]
pub struct SampledToken {
    pub output_pos: i64,
    pub token_id: u32,
    pub snapshot: Vec<u8>,
}

/// What a commit attempt produced.
#[derive(Debug)]
pub enum CommitOutcome {
    /// Committed durably; carries the 0-or-1 newly-emittable events (0 when the commit ended
    /// mid-glyph — the bytes emit with a later commit).
    Committed(Vec<Event>),
    /// Nothing buffered (or below the count threshold for a count-triggered attempt).
    Nothing,
    /// Backpressure: the bounded emit buffer is full — the commit stage pauses (spec §8), leaving
    /// the group buffered until the client drains.
    Paused,
}

/// A single generation session.
pub struct Session {
    commit: CommitStream,
    fence: WalFenceCtx,
    group: GroupCommitter,
    detok: Utf8Streamer,
    pieces: Box<dyn PieceSource>,
    log: EventLog,
    /// Events emitted to the client but not yet drained (backpressure counter).
    pending_emit: usize,
    emit_capacity: usize,
}

impl Session {
    pub fn new(commit: CommitStream, fence: WalFenceCtx, pieces: Box<dyn PieceSource>, k: usize, emit_capacity: usize) -> Session {
        Session {
            commit,
            fence,
            group: GroupCommitter::new(k),
            detok: Utf8Streamer::new(),
            pieces,
            log: EventLog::new(),
            pending_emit: 0,
            emit_capacity: emit_capacity.max(1),
        }
    }

    pub fn durable_pos(&self) -> i64 {
        self.commit.generation_durable_pos()
    }
    pub fn log(&self) -> &EventLog {
        &self.log
    }
    pub fn last_event_id(&self) -> u64 {
        self.log.last_id()
    }
    pub fn buffered(&self) -> usize {
        self.group.len()
    }

    /// Buffer a sampled token — **not** durable and **not** emitted yet.
    pub fn push_sampled(&mut self, s: SampledToken) {
        self.group.push(s.output_pos, s.token_id, s.snapshot);
    }

    fn buffer_full(&self) -> bool {
        self.pending_emit >= self.emit_capacity
    }

    /// Commit iff the k-count threshold is reached (the count trigger, spec §3).
    pub fn try_commit_by_count(&mut self) -> Result<CommitOutcome, CommitError> {
        if !self.group.count_ready() {
            return Ok(CommitOutcome::Nothing);
        }
        self.commit_group()
    }

    /// The deadline trigger (50 ms; the async loop calls this when the timer fires): commit whatever
    /// is buffered, even below k.
    pub fn commit_on_deadline(&mut self) -> Result<CommitOutcome, CommitError> {
        self.commit_group()
    }

    fn commit_group(&mut self) -> Result<CommitOutcome, CommitError> {
        if self.group.is_empty() {
            return Ok(CommitOutcome::Nothing);
        }
        // Backpressure: don't commit (hence don't emit) while the client's buffer is full.
        if self.buffer_full() {
            return Ok(CommitOutcome::Paused);
        }
        let batch = self.group.take().unwrap();
        // The DURABLE write. If it fails/stalls, we return the error and NOTHING is emitted — the
        // gate is enforced by this ordering (event append happens only past this line).
        self.commit.append_generation_commit(&self.fence, batch.first_pos, batch.last_pos, &batch.tokens, &batch.snapshot)?;

        // Durable now. Derive the event text from the just-durable tokens via the persistent detok.
        let mut text = String::new();
        for &(_pos, tok) in &batch.tokens {
            let piece = self.pieces.piece(tok);
            text.push_str(&self.detok.push(&piece));
        }
        let mut events = Vec::new();
        if !text.is_empty() {
            events.push(self.log.append(text, batch.last_pos));
            self.pending_emit += 1;
        }
        Ok(CommitOutcome::Committed(events))
    }

    /// End of generation: flush the final partial group and any trailing detok bytes.
    pub fn finish(&mut self) -> Result<Vec<Event>, CommitError> {
        let mut events = Vec::new();
        if !self.group.is_empty() {
            if let CommitOutcome::Committed(mut evs) = self.commit_group()? {
                events.append(&mut evs);
            }
        }
        let tail = self.detok.finish();
        if !tail.is_empty() {
            events.push(self.log.append(tail, self.durable_pos()));
            self.pending_emit += 1;
        }
        Ok(events)
    }

    /// The client read/acked `n` events — relieve backpressure so committing can resume.
    pub fn client_drained(&mut self, n: usize) {
        self.pending_emit = self.pending_emit.saturating_sub(n);
    }

    /// Resume: a byte-identical replay of events after `last_event_id`.
    pub fn events_since(&self, last_event_id: u64) -> Vec<Event> {
        self.log.since(last_event_id).to_vec()
    }
}
