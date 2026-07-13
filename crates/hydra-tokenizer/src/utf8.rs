//! The incremental UTF-8 boundary-safe streamer — the substrate under I6 ("a client may never
//! un-see", and never *see* a broken glyph). Token pieces are **bytes**, not strings: a multi-byte
//! codepoint (an emoji, a CJK character) can be split across token boundaries, so raw concatenation
//! into a `String` would either panic or emit an invalid prefix. This streamer buffers an
//! incomplete trailing sequence until the bytes that complete it arrive, and **never emits an
//! invalid UTF-8 chunk**.
//!
//! Pure — no engine, no I/O. Fully testable by feeding arbitrary byte splits.

/// Accumulates raw bytes and yields only complete-UTF-8 prefixes.
#[derive(Default, Debug)]
pub struct Utf8Streamer {
    buf: Vec<u8>,
}

impl Utf8Streamer {
    pub fn new() -> Self {
        Utf8Streamer { buf: Vec::new() }
    }

    /// Append `bytes` and return the maximal now-decodable text. A trailing **incomplete** multi-byte
    /// sequence is retained for the next `push`; genuinely **invalid** bytes are replaced with U+FFFD
    /// (so the stream always makes progress and the buffer can never grow unboundedly on garbage).
    /// The returned `String` is always valid UTF-8.
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.buf.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.buf) {
                Ok(s) => {
                    out.push_str(s);
                    self.buf.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    // SAFETY: `valid_up_to()` is a validated boundary.
                    out.push_str(unsafe { std::str::from_utf8_unchecked(&self.buf[..valid]) });
                    match e.error_len() {
                        // Incomplete trailing sequence — wait for more bytes.
                        None => {
                            self.buf.drain(..valid);
                            break;
                        }
                        // `n` genuinely-invalid bytes at `valid` — emit a replacement and continue.
                        Some(n) => {
                            out.push('\u{FFFD}');
                            self.buf.drain(..valid + n);
                        }
                    }
                }
            }
        }
        out
    }

    /// End of stream: any residual bytes are an incomplete/invalid trailing sequence → one U+FFFD.
    /// A well-formed complete stream leaves the buffer empty and returns "".
    pub fn finish(&mut self) -> String {
        if self.buf.is_empty() {
            String::new()
        } else {
            self.buf.clear();
            "\u{FFFD}".to_string()
        }
    }

    /// Bytes currently buffered awaiting completion (diagnostics / tests).
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multibyte_split_across_pushes_is_never_broken() {
        // "é" = C3 A9, "😀" = F0 9F 98 80. Feed one byte at a time.
        let text = "aé😀z";
        let bytes = text.as_bytes();
        let mut s = Utf8Streamer::new();
        let mut out = String::new();
        for &b in bytes {
            let chunk = s.push(&[b]);
            // Every emitted chunk is valid UTF-8 by construction (it is a String); crucially, no
            // partial-codepoint garbage is ever emitted.
            out.push_str(&chunk);
        }
        out.push_str(&s.finish());
        assert_eq!(out, text);
        assert_eq!(s.pending(), 0);
    }

    #[test]
    fn incremental_equals_batch_over_arbitrary_splits() {
        let corpus = "Hello, 世界! café ☕ 🚀🎉 naïve Ω ∑ 𝕏 é\u{200d}👩‍💻";
        let bytes = corpus.as_bytes();
        // Try every split point (two-chunk) and assert incremental == batch.
        for cut in 0..=bytes.len() {
            let mut s = Utf8Streamer::new();
            let mut out = s.push(&bytes[..cut]);
            out.push_str(&s.push(&bytes[cut..]));
            out.push_str(&s.finish());
            assert_eq!(out, corpus, "split at {cut} must reproduce the corpus");
        }
    }

    #[test]
    fn invalid_bytes_yield_replacement_and_make_progress() {
        let mut s = Utf8Streamer::new();
        // A lone 0xFF is invalid; a valid "A" follows.
        let out = s.push(&[0xFF, b'A']);
        assert_eq!(out, "\u{FFFD}A");
        assert_eq!(s.pending(), 0);
    }

    #[test]
    fn truncated_trailing_sequence_is_buffered_not_emitted() {
        let mut s = Utf8Streamer::new();
        // First 3 bytes of the 4-byte "😀".
        let out = s.push(&[0xF0, 0x9F, 0x98]);
        assert_eq!(out, "", "an incomplete codepoint is never emitted");
        assert_eq!(s.pending(), 3);
        let out = s.push(&[0x80]);
        assert_eq!(out, "😀");
    }

    #[test]
    fn pseudo_random_splits_never_emit_invalid_utf8() {
        // Deterministic xorshift; split a rich corpus at random chunk sizes many times.
        let corpus = "α β γ 😀🥳🎂 中文 テスト <b>💡</b> \u{1F469}\u{200D}\u{1F52C}".repeat(4);
        let bytes = corpus.as_bytes();
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..500 {
            let mut s = Utf8Streamer::new();
            let mut out = String::new();
            let mut i = 0;
            while i < bytes.len() {
                let step = 1 + (next() % 5) as usize;
                let end = (i + step).min(bytes.len());
                // push() returns a String → always valid UTF-8 (the invariant tested here).
                out.push_str(&s.push(&bytes[i..end]));
                i = end;
            }
            out.push_str(&s.finish());
            assert_eq!(out, corpus, "random-split stream must reproduce the corpus exactly");
        }
    }
}
