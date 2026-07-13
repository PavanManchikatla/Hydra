//! Admission hashes (spec §2.6a `INITIAL_COMMIT`): `tokenizer_hash`, `chat_template_hash`,
//! `rendered_prompt_bytes_hash`. These exist so recovery can **prove it is replaying the same
//! rendering** — a mismatch means the session cannot be reconstructed faithfully. Computed and
//! carried now even though the coordinator commit path (embedding them in `INITIAL_COMMIT`) is the
//! next slice; correctness of the hashes over whatever is rendered is what matters.
//!
//! Chat-template rendering is intentionally minimal — a fixed template per pinned model family
//! (v1: `llama`, `qwen2` → ChatML). Template generality is deferred; hash correctness is not.

use crate::tokenizer::{Tokenizer, TokenizerError};

/// One chat turn.
#[derive(Clone, Debug)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn new(role: &str, content: &str) -> Self {
        ChatMessage { role: role.to_string(), content: content.to_string() }
    }
}

/// The fixed chat templates supported in v1. Each has a **canonical string** (its stable identity,
/// hashed into `chat_template_hash`) and a renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatTemplate {
    /// ChatML (`qwen2`, and llama-family chat models that adopt it).
    ChatMl,
}

impl ChatTemplate {
    /// The canonical template identity hashed into `chat_template_hash` — a fixed, versioned string,
    /// not the rendered output.
    pub fn canonical(&self) -> &'static str {
        match self {
            ChatTemplate::ChatMl => {
                "hydra.chat_template.v1:chatml\n\
                 {% for m in messages %}<|im_start|>{role}\\n{content}<|im_end|>\\n{% endfor %}\
                 <|im_start|>assistant\\n"
            }
        }
    }

    /// Render `messages` to the exact prompt string that will be tokenized.
    pub fn render(&self, messages: &[ChatMessage]) -> String {
        match self {
            ChatTemplate::ChatMl => {
                let mut s = String::new();
                for m in messages {
                    s.push_str("<|im_start|>");
                    s.push_str(&m.role);
                    s.push('\n');
                    s.push_str(&m.content);
                    s.push_str("<|im_end|>\n");
                }
                s.push_str("<|im_start|>assistant\n");
                s
            }
        }
    }
}

/// The three admission hashes plus the rendered prompt and its tokens.
#[derive(Clone, Debug)]
pub struct Admission {
    pub tokenizer_hash: [u8; 32],
    pub chat_template_hash: [u8; 32],
    pub rendered_prompt_bytes_hash: [u8; 32],
    pub rendered_prompt: String,
    pub prompt_tokens: Vec<u32>,
}

/// Hash of the canonical template identity.
pub fn chat_template_hash(template: ChatTemplate) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"hydra.chat_template_hash.v1");
    h.update(template.canonical().as_bytes());
    *h.finalize().as_bytes()
}

/// Hash of the exact rendered prompt bytes.
pub fn rendered_prompt_bytes_hash(rendered: &str) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"hydra.rendered_prompt_bytes_hash.v1");
    h.update(rendered.as_bytes());
    *h.finalize().as_bytes()
}

impl Admission {
    /// Render `messages` with `template`, tokenize with the model's tokenizer (BOS + special
    /// parsing, so ChatML markers become their special tokens), and compute all three hashes.
    pub fn compute(
        tokenizer: &Tokenizer,
        template: ChatTemplate,
        messages: &[ChatMessage],
    ) -> Result<Admission, TokenizerError> {
        let rendered_prompt = template.render(messages);
        let prompt_tokens = tokenizer.encode(&rendered_prompt, true, true)?;
        Ok(Admission {
            tokenizer_hash: tokenizer.tokenizer_hash()?,
            chat_template_hash: chat_template_hash(template),
            rendered_prompt_bytes_hash: rendered_prompt_bytes_hash(&rendered_prompt),
            rendered_prompt,
            prompt_tokens,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chatml_renders_and_hashes_are_stable() {
        let msgs =
            [ChatMessage::new("system", "You are terse."), ChatMessage::new("user", "Capital of France?")];
        let rendered = ChatTemplate::ChatMl.render(&msgs);
        assert!(rendered.starts_with("<|im_start|>system\nYou are terse.<|im_end|>\n"));
        assert!(rendered.ends_with("<|im_start|>assistant\n"));
        // Hashes are deterministic and rendered-bytes-hash tracks the content.
        assert_eq!(chat_template_hash(ChatTemplate::ChatMl), chat_template_hash(ChatTemplate::ChatMl));
        assert_ne!(
            rendered_prompt_bytes_hash(&rendered),
            rendered_prompt_bytes_hash(&ChatTemplate::ChatMl.render(&msgs[..1])),
            "different rendering ⇒ different prompt-bytes hash"
        );
    }
}
