//! # hydra-coordinator
//!
//! The coordinator (BLUEPRINT §2 `hydra-coordinator`, M2 slice 5). It owns the session lifecycle,
//! the durable **commit stream**, the token ledger, group commit (spec §3), and — in later
//! sub-slices — the OpenAI/SSE surface and D1 recovery orchestration. **Emit-after-commit** (I6/I9)
//! is the law: nothing is shown to a client above `generation_durable_pos`.
//!
//! - [`commit_stream`] (sub-slice A) — `INITIAL_COMMIT` / `GENERATION_COMMIT` on a real `hydra-wal`
//!   disk file; `snapshot(q)` embedded from the SAMPLED ring; I19 validated on write.

pub mod commit_stream;
pub mod event_log;
pub mod recovery;
pub mod server;
pub mod session;

pub use commit_stream::{CommitError, CommitStream, Durability, GroupBatch, GroupCommitter, WalFenceCtx};
pub use event_log::{Event, EventLog};
pub use recovery::{CommitStreamStats, RecoveryError, RecoveryState};
pub use server::{router, AppState, GenFn};
pub use session::{CommitOutcome, PieceSource, SampledToken, Session, TokenizerPieces};
