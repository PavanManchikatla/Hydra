//! Tokenizer + incremental detokenizer, delegating to **llama.cpp's own tokenizer** via
//! `hydra-engine-sys` (the engine that computes is the engine that tokenizes — a tokenizer that
//! differs from the model's by one merge rule produces silent garbage no invariant catches).
//!
//! The model is loaded **vocab-only** where possible (no weights → low coordinator memory); if that
//! path is unavailable a full-model load is a documented dev artifact. Detokenization is incremental
//! and UTF-8-boundary-safe via [`Utf8Streamer`].

use hydra_engine_sys::{EngineError, Model};

use crate::utf8::Utf8Streamer;

#[derive(Debug, thiserror::Error)]
pub enum TokenizerError {
    #[error("engine: {0}")]
    Engine(#[from] EngineError),
}

/// The coordinator's tokenizer — a thin wrapper over the model's vocab.
pub struct Tokenizer {
    model: Model,
    n_vocab: i32,
}

impl Tokenizer {
    /// Load only the vocab (no weights) — the low-memory coordinator path.
    pub fn load_vocab_only(path: &str) -> Result<Tokenizer, TokenizerError> {
        let model = Model::load_vocab_only(path)?;
        Ok(Tokenizer { n_vocab: model.n_vocab(), model })
    }

    /// Reuse an already-loaded model as the tokenizer (e.g. when the coordinator also holds weights,
    /// or in tests) — avoids a second load.
    pub fn from_model(model: Model) -> Tokenizer {
        Tokenizer { n_vocab: model.n_vocab(), model }
    }

    pub fn n_vocab(&self) -> i32 {
        self.n_vocab
    }

    /// Tokenize `text`. `add_special` prepends BOS (and model-defined specials); `parse_special`
    /// recognizes special-token text (e.g. chat markers) rather than tokenizing them literally.
    pub fn encode(&self, text: &str, add_special: bool, parse_special: bool) -> Result<Vec<u32>, TokenizerError> {
        Ok(self.model.tokenize_ex(text, add_special, parse_special)?.into_iter().map(|t| t as u32).collect())
    }

    /// The raw display bytes for one token (special tokens render empty).
    pub fn piece(&self, token: u32) -> Result<Vec<u8>, TokenizerError> {
        Ok(self.model.token_to_piece(token as i32, false)?)
    }

    /// Batch-detokenize: concatenate the pieces (the reference the incremental path must match).
    pub fn decode_bytes(&self, tokens: &[u32]) -> Result<Vec<u8>, TokenizerError> {
        let mut out = Vec::new();
        for &t in tokens {
            out.extend_from_slice(&self.model.token_to_piece(t as i32, false)?);
        }
        Ok(out)
    }

    /// A deterministic digest of the whole vocab (every token's faithful piece, specials included) —
    /// the `tokenizer_hash` admission field. Two models share it iff they share a tokenizer.
    pub fn tokenizer_hash(&self) -> Result<[u8; 32], TokenizerError> {
        let mut h = blake3::Hasher::new();
        h.update(b"hydra.tokenizer_hash.v1");
        h.update(&(self.n_vocab as u32).to_le_bytes());
        for t in 0..self.n_vocab {
            let piece = self.model.token_to_piece(t, true)?;
            h.update(&(piece.len() as u32).to_le_bytes());
            h.update(&piece);
        }
        Ok(*h.finalize().as_bytes())
    }

    /// Start an incremental detokenization stream over this tokenizer.
    pub fn incremental(&self) -> IncrementalDetokenizer<'_> {
        IncrementalDetokenizer { tok: self, stream: Utf8Streamer::new() }
    }
}

/// Feeds token pieces (bytes) through a [`Utf8Streamer`]: emits text only at UTF-8 boundaries, so a
/// codepoint split across two tokens is never delivered broken (I6).
pub struct IncrementalDetokenizer<'t> {
    tok: &'t Tokenizer,
    stream: Utf8Streamer,
}

impl IncrementalDetokenizer<'_> {
    /// Feed one token; return any text now emittable (possibly empty if it completed a partial glyph).
    pub fn push_token(&mut self, token: u32) -> Result<String, TokenizerError> {
        let piece = self.tok.piece(token)?;
        Ok(self.stream.push(&piece))
    }

    /// End of stream — flush any residual (a well-formed stream returns "").
    pub fn finish(&mut self) -> String {
        self.stream.finish()
    }
}
