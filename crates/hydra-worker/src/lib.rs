//! # hydra-worker
//!
//! The Hydra stage worker (BLUEPRINT §2 `hydra-worker`, M2 sub-slice A) and the `--local-pair`
//! harness (sub-slice B). A worker is a **thin effect executor**: it runs the real, DST-tested
//! `hydra-state` stage-session state machine for control-plane frames and the `hydra-engine-sys`
//! engine for data-plane compute, over real `hydra-proto` frames on `hydra-transport` TCP+mTLS.
//! There is no parallel "simple" implementation — the machine the simulator checked is the one
//! that runs.
//!
//! - [`wire`]   — `Frame` (fence + `Body` union) codec + the F1 fence check.
//! - [`worker`] — [`Worker`] (`Stage` SM + engine) and the async serve loop.
//!
//! - [`pair`]   — the `--local-pair` runner: two workers as real mTLS endpoints on localhost, the
//!   teacher-forced NO_SAMPLE bit-exact anchor, and a `kill -9`/restart switch from day one (so the
//!   later D1 recovery DoD runs against an existing kill-switch).

pub mod bootstrap;
pub mod pair;
pub mod sampler;
pub mod wire;
pub mod worker;

pub use bootstrap::Bootstrap;
pub use wire::{Msg, SessionKeys, WireError};
pub use worker::{serve_conn, Worker, WorkerConfig, WorkerError};
