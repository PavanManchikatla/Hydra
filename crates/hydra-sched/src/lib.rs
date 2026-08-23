//! # hydra-sched
//!
//! The **pure** inputs the M3 scheduler decides from. P2·1 (device capability: the startup
//! benchmark + its EWMA) lands here first; P2·2 (link prober) and P2·3 (placement solver) join it.
//!
//! **No engine, no I/O, no clocks, no randomness.** Timing samples are *handed in* by whatever
//! actually ran the work, exactly as `hydra-state` is handed events. That keeps every scheduling
//! decision deterministic, replayable, and runnable in container CI on a box with no GPU and no
//! model — the same separation that let the state machines be model-checked (BLUEPRINT §1.4).
//!
//! The measurement itself (running real decode steps through the engine) lives in the
//! `hydra-bench` binary, which owns the engine dependency and feeds its samples here.

pub mod admission;
pub mod capability;
pub mod link;
pub mod solver;
pub mod stability;
