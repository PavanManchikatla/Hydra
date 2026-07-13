//! M2 slice 4 — tokenizer/detokenizer against the **real llama.cpp vocab** (engine-gated; skips
//! cleanly without the vendored build tree + GGUF, like the engine-sys bit-exact test).
//!
//! (a) `incremental_detok_is_byte_identical_to_batch` — streamed token-by-token output equals batch
//!     detokenization, over text with multi-byte codepoints (emoji) split across tokens.
//! (b) UTF-8 validity of every emitted chunk is structural (each `push` returns a `String`) and is
//!     fuzzed purely in `src/utf8.rs`; here we confirm it holds through the *real* vocab too.
//! (c) `round_trip_tokenize_detokenize_matches_reference` — encode→decode reproduces the input over
//!     a fixed prompt set (the llama.cpp reference is its own token_to_piece).

use hydra_tokenizer::{Admission, ChatMessage, ChatTemplate, Tokenizer};

fn model_path() -> Option<String> {
    if !hydra_engine_sys::ENGINE_AVAILABLE {
        return None;
    }
    let default = concat!(env!("CARGO_MANIFEST_DIR"), "/../../models/qwen2.5-0.5b-instruct-fp16.gguf");
    std::env::var("HYDRA_TEST_MODEL")
        .ok()
        .filter(|p| std::path::Path::new(p).exists())
        .or_else(|| std::path::Path::new(default).exists().then(|| default.to_string()))
}

/// Prefer the low-memory vocab-only path; if this model can't load vocab-only, fall back to the
/// full model (a documented dev artifact) so the test still exercises the real vocab.
fn load_tokenizer(path: &str) -> Tokenizer {
    match Tokenizer::load_vocab_only(path) {
        Ok(t) => t,
        Err(_) => {
            eprintln!("NOTE: vocab-only load unavailable for this model; using full-model load (dev artifact)");
            Tokenizer::from_model(hydra_engine_sys::Model::load(path, 0).expect("full model load"))
        }
    }
}

const CORPUS: &[&str] = &[
    "The capital of France is Paris.",
    "café ☕ naïve Ω ∑ — smart quotes “x”",
    "emoji: 😀🚀🎉🥳 and family 👩‍💻👨‍👩‍👧‍👦",
    "中文 テスト 한국어 العربية",
    "code: fn main() { println!(\"hi\\n\"); }",
];

#[test]
fn incremental_detok_is_byte_identical_to_batch() {
    let Some(path) = model_path() else {
        eprintln!("SKIP: no engine/model");
        return;
    };
    let tok = load_tokenizer(&path);

    for &text in CORPUS {
        let tokens = tok.encode(text, false, false).expect("encode");
        // Batch reference.
        let batch = String::from_utf8(tok.decode_bytes(&tokens).expect("decode")).expect("batch is valid utf8");
        // Incremental (token by token), then flush.
        let mut inc = tok.incremental();
        let mut streamed = String::new();
        for &t in &tokens {
            streamed.push_str(&inc.push_token(t).expect("push_token"));
        }
        streamed.push_str(&inc.finish());
        assert_eq!(streamed, batch, "incremental detok must equal batch for {text:?}");
    }
}

#[test]
fn round_trip_tokenize_detokenize_matches_reference() {
    let Some(path) = model_path() else {
        eprintln!("SKIP: no engine/model");
        return;
    };
    let tok = load_tokenizer(&path);
    for &text in CORPUS {
        let tokens = tok.encode(text, /*add_special=*/ false, /*parse_special=*/ false).expect("encode");
        let back = String::from_utf8(tok.decode_bytes(&tokens).expect("decode")).expect("valid utf8");
        assert_eq!(back, text, "round-trip tokenize→detokenize must reproduce {text:?}");
    }
}

#[test]
fn tokenizer_hash_is_stable_and_admission_hashes_are_wired() {
    let Some(path) = model_path() else {
        eprintln!("SKIP: no engine/model");
        return;
    };
    let tok = load_tokenizer(&path);
    let h1 = tok.tokenizer_hash().expect("hash");
    let h2 = tok.tokenizer_hash().expect("hash");
    assert_eq!(h1, h2, "tokenizer_hash is deterministic");
    assert!(h1.iter().any(|&b| b != 0));

    let msgs = [ChatMessage::new("user", "Capital of France? 🇫🇷")];
    let adm = Admission::compute(&tok, ChatTemplate::ChatMl, &msgs).expect("admission");
    assert_eq!(adm.tokenizer_hash, h1);
    assert!(!adm.prompt_tokens.is_empty(), "rendered prompt tokenizes");
    assert!(adm.rendered_prompt.contains("<|im_start|>assistant"));
    // The three hashes are distinct, non-trivial, and the rendered-bytes hash matches a recompute.
    assert_eq!(adm.rendered_prompt_bytes_hash, hydra_tokenizer::rendered_prompt_bytes_hash(&adm.rendered_prompt));
    assert_ne!(adm.tokenizer_hash, adm.chat_template_hash);
    assert_ne!(adm.chat_template_hash, adm.rendered_prompt_bytes_hash);
}
