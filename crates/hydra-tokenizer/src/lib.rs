//! # hydra-tokenizer
//!
//! Coordinator-side tokenization + incremental detokenization (BLUEPRINT M2 slice 4).
//!
//! **Binding decision:** Hydra does **not** reimplement tokenization. It delegates to llama.cpp's
//! own tokenizer through `hydra-engine-sys` ([`Tokenizer`]) — a tokenizer that differed from the
//! model's by one merge rule would produce silent garbage no invariant catches. The detokenizer is
//! incremental and **UTF-8-boundary-safe** ([`Utf8Streamer`]): pieces are bytes, buffered until a
//! codepoint completes, never emitted invalid (the substrate under spec I6).
//!
//! [`Admission`] computes the `tokenizer_hash` / `chat_template_hash` / `rendered_prompt_bytes_hash`
//! that `INITIAL_COMMIT` (spec §2.6a) carries so recovery can prove it replays the same rendering.

pub mod admission;
pub mod tokenizer;
pub mod utf8;

pub use admission::{chat_template_hash, rendered_prompt_bytes_hash, Admission, ChatMessage, ChatTemplate};
pub use tokenizer::{IncrementalDetokenizer, Tokenizer, TokenizerError};
pub use utf8::Utf8Streamer;
