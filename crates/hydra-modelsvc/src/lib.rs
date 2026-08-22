//! # hydra-modelsvc
//!
//! GGUF splitter + signed manifest (BLUEPRINT §1.7 / §1.9; M3 gate-condition (ii), P2·10a).
//!
//! Splits a GGUF model into **per-stage shard files** so a worker loads only its assigned layers'
//! weights (retiring the "full model per worker" memory ceiling), with a manifest that binds a
//! **per-tensor BLAKE3** map + the three **admission hashes** (tokenizer / chat-template /
//! inference-config, computed over the GGUF's own metadata — engine-free) + the **layer-range map**,
//! **Ed25519-signed**. A worker verifies the manifest before loading a shard and **refuses** anything
//! that doesn't verify (the security posture extending to model distribution).
//!
//! This crate is **pure Rust with no engine dependency** — it parses/writes the documented GGUF wire
//! format directly. The shard-**load** path in `hydra-engine-sys` (P2·10b) consumes these shards.

pub mod gguf;
pub mod manifest;
pub mod split;

pub use gguf::Gguf;
pub use manifest::{Manifest, ShardEntry};
pub use split::{split, Shard, SplitOutput};
