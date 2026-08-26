//! # hydra-engine-sys
//!
//! The **only** place `unsafe`/C touches Hydra: a narrow FFI over the vendored, patched
//! `llama.cpp`/`ggml`. It **computes** — loads a shard's contiguous layer range, applies token or
//! boundary-residual ranges, returns logits, truncates KV. It holds **no protocol concept**
//! (sessions, epochs, attempts, WAL, fencing, position bookkeeping) — all of that stays in
//! `hydra-state` (BLUEPRINT §1.4). Any retry/fence/position logic added here is a defect.
//!
//! **Boundaries cross the FFI as `f32` only** (the accepted `hydra-engine-sys` sketch); wire
//! precision (`f16` default / `f32` exact / `int8_blockq` reserved) is the Rust transport's job,
//! not the engine's.
//!
//! **Dev-environment assumptions** (see `build.rs`): linking uses the vendored `llama.cpp`
//! *build tree* (`vendor/llama.cpp/build/bin`), produced by the M-1 spike's `cmake` build; and
//! the smoke test loads a small git-ignored GGUF. On an 8 GB `--local-pair` box these are real
//! constraints — small model only, lazy shard loading — and are **dev-mode artifacts, not runtime
//! properties**. If the build tree is absent the crate compiles a stub (see `engine_unavailable`).

use std::fmt;

/// An FFI-layer error. Carries the C status code and a short static label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError {
    pub code: i32,
    pub what: &'static str,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "engine error {}: {}", self.code, self.what)
    }
}
impl std::error::Error for EngineError {}

impl EngineError {
    #[cfg_attr(not(engine_unavailable), allow(dead_code))]
    fn unavailable() -> Self {
        EngineError { code: -1, what: "engine unavailable: vendored llama.cpp build tree not built" }
    }
}

// ============================ real implementation ============================
#[cfg(not(engine_unavailable))]
mod ffi {
    use std::os::raw::{c_char, c_int};
    #[repr(C)]
    pub struct HydraModel {
        _private: [u8; 0],
    }
    #[repr(C)]
    pub struct HydraContext {
        _private: [u8; 0],
    }
    #[repr(C)]
    pub struct HydraModelInfo {
        pub n_layer: i32,
        pub n_embd: i32,
        pub n_vocab: i32,
    }
    extern "C" {
        pub fn hydra_model_load(path: *const c_char, n_gpu_layers: i32) -> *mut HydraModel;
        pub fn hydra_model_load_vocab_only(path: *const c_char) -> *mut HydraModel;
        pub fn hydra_model_load_shard(path: *const c_char, l0: i32, l1: i32, n_gpu_layers: i32) -> *mut HydraModel;
        pub fn hydra_model_load_window(m: *const HydraModel, l0: *mut i32, l1: *mut i32);
        pub fn hydra_model_free(m: *mut HydraModel);
        pub fn hydra_model_info(m: *const HydraModel) -> HydraModelInfo;
        pub fn hydra_tokenize_ex(
            m: *const HydraModel,
            text: *const c_char,
            text_len: i32,
            add_special: i32,
            parse_special: i32,
            out: *mut i32,
            cap: i32,
        ) -> i32;
        pub fn hydra_token_to_piece(
            m: *const HydraModel,
            token: i32,
            special: i32,
            out: *mut u8,
            cap: i32,
        ) -> i32;
        pub fn hydra_context_new(
            m: *mut HydraModel,
            l0: i32,
            l1: i32,
            embeddings: i32,
            n_ctx: i32,
            n_batch: i32,
        ) -> *mut HydraContext;
        pub fn hydra_context_free(c: *mut HydraContext);
        pub fn hydra_apply(
            c: *mut HydraContext,
            tokens: *const i32,
            boundary_in: *const f32,
            pos0: i32,
            n: i32,
            boundary_out: *mut f32,
        ) -> i32;
        pub fn hydra_logits(c: *mut HydraContext, at_pos: i32, out: *mut f32, out_cap: i32) -> i32;
        pub fn hydra_kv_truncate(c: *mut HydraContext, pos: i32) -> c_int;
        pub fn hydra_gguf_probe(path: *const c_char) -> i32;
    }
}

#[cfg(not(engine_unavailable))]
mod imp {
    use super::{ffi, EngineError};
    use std::ffi::CString;

    fn check(code: i32) -> Result<(), EngineError> {
        if code == 0 {
            Ok(())
        } else {
            Err(EngineError { code, what: "hydra FFI call failed" })
        }
    }

    /// **Audit H6 — drive the VENDORED GGUF parser over `path`.**
    ///
    /// `Ok(true)` = the vendored parser accepted the file, `Ok(false)` = it rejected it. **Both are
    /// fine.** The only failure this exists to detect is the one that is not a return value at all:
    /// a segfault, an abort, or a C++ exception crossing back into Rust.
    ///
    /// The 24-CPU-hour budget has been fuzzing `hydra-modelsvc`'s Rust reader — the **offline
    /// splitter's** parser. The worker loads through *this* one. They are different programs.
    pub fn gguf_probe(path: &str) -> Result<bool, EngineError> {
        let c = CString::new(path).map_err(|_| EngineError { code: 8, what: "path contains a NUL" })?;
        let rc = unsafe { ffi::hydra_gguf_probe(c.as_ptr()) };
        Ok(rc == 0)
    }

    /// A loaded model (full weights). Owns the C handle; freed on drop.
    pub struct Model {
        raw: *mut ffi::HydraModel,
        n_layer: i32,
        n_embd: i32,
        n_vocab: i32,
    }

    /// **`HYDRA_TEST_NGL` — the verification lever the M4 GPU gate row needs (owner-ruled 2026-08-25).**
    ///
    /// **Why it exists.** All 31 engine-test configurations in this workspace hard-code
    /// `n_gpu_layers: 0`, so every byte-identity claim the project makes — the rule-14 anchors, the
    /// shard anchor, `d1_recovery`, three-node recovery, the M3 calibration — is **CPU-only
    /// evidence**, and Metal is exercised by nothing in the suite (§7.63). Meanwhile the M−1 sweep
    /// shows Metal's KV truncate+replay is **not** bit-exact in at least one case. The gate asks
    /// whether the byte-identity assertions survive on the GPU, and answering it needs a lever.
    ///
    /// **Editing the 31 literals was rejected**: it would have to be undone, and a lever that has to
    /// be reverted is one a future session cannot re-pull. Intercepting at the single load boundary
    /// covers every caller, binaries included, and leaves **0 (the DoD backend) as the default**.
    ///
    /// **It is opt-in by explicit environment variable and announces itself on stderr**, so it can
    /// never silently be the reason a result differs — the same contract as
    /// `HYDRA_FORCE_ENGINE_STUB` in `build.rs`.
    fn effective_ngl(requested: i32) -> i32 {
        match std::env::var("HYDRA_TEST_NGL").ok().and_then(|v| v.parse::<i32>().ok()) {
            Some(n) if n != requested => {
                eprintln!(
                    "hydra-engine-sys: HYDRA_TEST_NGL={n} overrides n_gpu_layers={requested} \
                     (verification lever — the default DoD backend is 0/CPU)"
                );
                n
            }
            Some(n) => n,
            None => requested,
        }
    }

    impl Model {
        /// Load a GGUF. `n_gpu_layers` 0 = CPU (deterministic DoD backend), 99 = GPU.
        pub fn load(path: &str, n_gpu_layers: i32) -> Result<Model, EngineError> {
            let c = CString::new(path).map_err(|_| EngineError { code: 8, what: "path has NUL" })?;
            let raw = unsafe { ffi::hydra_model_load(c.as_ptr(), effective_ngl(n_gpu_layers)) };
            Self::wrap(raw)
        }

        /// Load only the tokenizer/vocab (no weights) — the low-memory coordinator path. Contexts
        /// cannot be created from a vocab-only model (tokenize/detokenize only).
        pub fn load_vocab_only(path: &str) -> Result<Model, EngineError> {
            let c = CString::new(path).map_err(|_| EngineError { code: 8, what: "path has NUL" })?;
            let raw = unsafe { ffi::hydra_model_load_vocab_only(c.as_ptr()) };
            Self::wrap(raw)
        }

        /// Load a per-stage **shard** GGUF (`hydra-modelsvc split` output), allocating only layers
        /// `[l0, l1)`. `n_layer()` still reports the FULL model's layer count — the shard carries
        /// the architecture's own `block_count` verbatim, so positions and layer indices keep their
        /// global meaning and nothing downstream has to re-base them.
        ///
        /// This is the memory payoff of P2·10: `load` maps every worker's copy of the whole model,
        /// `load_shard` maps one stage's share. Requesting a [`Model::context`] outside `[l0, l1)`
        /// is refused by the engine rather than null-dereferencing.
        pub fn load_shard(path: &str, l0: i32, l1: i32, n_gpu_layers: i32) -> Result<Model, EngineError> {
            let c = CString::new(path).map_err(|_| EngineError { code: 8, what: "path has NUL" })?;
            let raw = unsafe { ffi::hydra_model_load_shard(c.as_ptr(), l0, l1, effective_ngl(n_gpu_layers)) };
            Self::wrap(raw)
        }

        fn wrap(raw: *mut ffi::HydraModel) -> Result<Model, EngineError> {
            if raw.is_null() {
                return Err(EngineError { code: 2, what: "model load failed" });
            }
            let info = unsafe { ffi::hydra_model_info(raw) };
            Ok(Model { raw, n_layer: info.n_layer, n_embd: info.n_embd, n_vocab: info.n_vocab })
        }

        /// The shard layer window this model was loaded with, or `None` for a full load.
        pub fn load_window(&self) -> Option<(i32, i32)> {
            let (mut l0, mut l1) = (0i32, -1i32);
            unsafe { ffi::hydra_model_load_window(self.raw, &mut l0, &mut l1) };
            if l1 >= 0 {
                Some((l0, l1))
            } else {
                None
            }
        }

        pub fn n_layer(&self) -> i32 {
            self.n_layer
        }
        pub fn n_embd(&self) -> i32 {
            self.n_embd
        }
        pub fn n_vocab(&self) -> i32 {
            self.n_vocab
        }

        /// Tokenize with BOS + special-token parsing (the default session path).
        pub fn tokenize(&self, text: &str) -> Result<Vec<i32>, EngineError> {
            self.tokenize_ex(text, true, true)
        }

        /// Tokenize with explicit `add_special` (BOS/EOS) and `parse_special` control.
        /// **Audit M5, second half — `text_len` was narrowed `usize as i32`.**
        ///
        /// The directive's gloss of M5 was "the 14 shim entry points", and those were wrapped in
        /// Wave 1c. The auditor's M5 has a second clause the gloss dropped: *"`text_len` narrowed
        /// `usize` → `i32`"*. A prompt at or above 2 GiB wraps negative, and `llama_tokenize` then
        /// throws `std::length_error` on the negative length. Nothing capped prompt bytes at
        /// ingress. Checked, not cast (standing rule 20 found this).
        pub fn tokenize_ex(&self, text: &str, add_special: bool, parse_special: bool) -> Result<Vec<i32>, EngineError> {
            let bytes = text.as_bytes();
            let (add, parse) = (add_special as i32, parse_special as i32);
            let text_len = i32::try_from(text.len())
                .map_err(|_| EngineError { code: 8, what: "prompt does not fit i32 bytes (audit M5)" })?;
            let need = unsafe {
                ffi::hydra_tokenize_ex(self.raw, bytes.as_ptr() as *const _, text_len, add, parse, std::ptr::null_mut(), 0)
            };
            let cap = if need < 0 { -need } else { need };
            if cap < 0 {
                return Err(EngineError { code: 4, what: "tokenize failed" });
            }
            let mut out = vec![0i32; cap as usize];
            let got = unsafe {
                ffi::hydra_tokenize_ex(self.raw, bytes.as_ptr() as *const _, text_len, add, parse, out.as_mut_ptr(), cap)
            };
            if got < 0 {
                return Err(EngineError { code: 4, what: "tokenize failed" });
            }
            out.truncate(got as usize);
            Ok(out)
        }

        /// Render one token to its raw display bytes (`special=false` renders special tokens empty).
        /// The detokenizer substrate: pieces are **bytes**, not strings (I6 UTF-8 safety).
        pub fn token_to_piece(&self, token: i32, special: bool) -> Result<Vec<u8>, EngineError> {
            let need = unsafe { ffi::hydra_token_to_piece(self.raw, token, special as i32, std::ptr::null_mut(), 0) };
            let cap = if need < 0 { -need } else { need };
            if cap < 0 {
                return Err(EngineError { code: 4, what: "token_to_piece failed" });
            }
            let mut out = vec![0u8; cap as usize];
            let got = unsafe { ffi::hydra_token_to_piece(self.raw, token, special as i32, out.as_mut_ptr(), cap) };
            if got < 0 {
                return Err(EngineError { code: 4, what: "token_to_piece failed" });
            }
            out.truncate(got as usize);
            Ok(out)
        }

        /// New context windowed to layers `[l0, l1)` (`l1 == -1` => to the last layer).
        /// `embeddings` makes a boundary-emitting context; otherwise a logits context.
        pub fn context(
            &self,
            l0: i32,
            l1: i32,
            embeddings: bool,
            n_ctx: i32,
            n_batch: i32,
        ) -> Result<Context<'_>, EngineError> {
            let raw = unsafe {
                ffi::hydra_context_new(self.raw, l0, l1, embeddings as i32, n_ctx, n_batch)
            };
            if raw.is_null() {
                return Err(EngineError { code: 3, what: "context init failed" });
            }
            Ok(Context { raw, n_embd: self.n_embd, n_vocab: self.n_vocab, n_ctx, n_batch, _model: std::marker::PhantomData })
        }
    }

    impl Drop for Model {
        fn drop(&mut self) {
            unsafe { ffi::hydra_model_free(self.raw) };
        }
    }

    /// A live inference context over a [`Model`]'s layer window. Borrows the model.
    pub struct Context<'m> {
        raw: *mut ffi::HydraContext,
        n_embd: i32,
        n_vocab: i32,
        n_ctx: i32,
        n_batch: i32,
        _model: std::marker::PhantomData<&'m Model>,
    }

    impl<'m> Context<'m> {
        /// Apply `tokens` starting at `pos0`. For an embeddings context, `boundary_out` (if given,
        /// length `tokens.len() * n_embd`) receives the residual leaving the window.
        pub fn apply_tokens(
            &mut self,
            tokens: &[i32],
            pos0: i32,
            boundary_out: Option<&mut [f32]>,
        ) -> Result<(), EngineError> {
            let n = i32::try_from(tokens.len()).map_err(|_| EngineError { code: 8, what: "token count does not fit i32" })?;
            // M5 (engine side): a token id outside the vocabulary never crosses the FFI.
            if let Some(&bad) = tokens.iter().find(|&&t| t < 0 || t >= self.n_vocab) {
                let _ = bad;
                return Err(EngineError { code: 8, what: "token id outside [0, n_vocab)" });
            }
            self.apply(Some(tokens), None, pos0, n, boundary_out)
        }

        /// Apply an injected boundary residual of **exactly** `n_positions` positions.
        ///
        /// **Audit C3:** `n_positions` is an explicit argument, never derived from
        /// `boundary_in.len()`. Before C3 this computed `n = len / n_embd`, so the byte count
        /// *defined* the position count and a frame that lied about its shape was applied as
        /// whatever its length happened to divide into. The caller now states the shape it was
        /// told, and the engine refuses the call if the data does not match it.
        pub fn apply_boundary(
            &mut self,
            boundary_in: &[f32],
            n_positions: i32,
            pos0: i32,
            boundary_out: Option<&mut [f32]>,
        ) -> Result<(), EngineError> {
            self.apply(None, Some(boundary_in), pos0, n_positions, boundary_out)
        }

        /// The bounds every apply is held to **before** the FFI (audit C3/H7): a position count in
        /// `[1, n_batch]`, a position range inside `[0, n_ctx)`, and buffers of exactly
        /// `n × n_embd`. These are the engine's own facts (`n_batch`, `n_ctx`, `n_embd` came from
        /// the context/model), so this is where a network-declared shape finally meets ground
        /// truth — the shim is never the first thing to find out.
        fn apply(
            &mut self,
            tokens: Option<&[i32]>,
            boundary_in: Option<&[f32]>,
            pos0: i32,
            n: i32,
            boundary_out: Option<&mut [f32]>,
        ) -> Result<(), EngineError> {
            if n < 1 {
                return Err(EngineError { code: 8, what: "n_positions must be >= 1" });
            }
            if n > self.n_batch {
                return Err(EngineError { code: 8, what: "n_positions exceeds n_batch (audit H7)" });
            }
            if pos0 < 0 || pos0.checked_add(n).map_or(true, |end| end > self.n_ctx) {
                return Err(EngineError { code: 8, what: "position range escapes [0, n_ctx)" });
            }
            if let Some(t) = tokens {
                if t.len() != n as usize {
                    return Err(EngineError { code: 6, what: "tokens length != n_positions" });
                }
            }
            if let Some(b) = boundary_in {
                if b.len() != (n as usize) * self.n_embd as usize {
                    return Err(EngineError { code: 6, what: "boundary_in shape mismatch (len != n_positions × n_embd)" });
                }
            }
            let out_ptr = match &boundary_out {
                Some(o) => {
                    if o.len() != (n as usize) * self.n_embd as usize {
                        return Err(EngineError { code: 6, what: "boundary_out shape mismatch" });
                    }
                    o.as_ptr() as *mut f32
                }
                None => std::ptr::null_mut(),
            };
            let code = unsafe {
                ffi::hydra_apply(
                    self.raw,
                    tokens.map_or(std::ptr::null(), |t| t.as_ptr()),
                    boundary_in.map_or(std::ptr::null(), |b| b.as_ptr()),
                    pos0,
                    n,
                    out_ptr,
                )
            };
            check(code)
        }

        pub fn n_embd(&self) -> i32 {
            self.n_embd
        }
        pub fn n_vocab(&self) -> i32 {
            self.n_vocab
        }
        pub fn n_ctx(&self) -> i32 {
            self.n_ctx
        }
        pub fn n_batch(&self) -> i32 {
            self.n_batch
        }

        /// Retained (unsampled) logits at `at_pos`. Sampling is the caller's job (I14).
        pub fn logits(&mut self, at_pos: i32) -> Result<Vec<f32>, EngineError> {
            let mut out = vec![0f32; self.n_vocab as usize];
            let code = unsafe { ffi::hydra_logits(self.raw, at_pos, out.as_mut_ptr(), self.n_vocab) };
            check(code).map(|_| out)
        }

        /// Drop cached KV for positions >= `pos` (recovery truncate; I7a).
        pub fn kv_truncate(&mut self, pos: i32) -> Result<(), EngineError> {
            check(unsafe { ffi::hydra_kv_truncate(self.raw, pos) })
        }
    }

    impl Drop for Context<'_> {
        fn drop(&mut self) {
            unsafe { ffi::hydra_context_free(self.raw) };
        }
    }
}

// ============================ unavailable stub ============================
#[cfg(engine_unavailable)]
mod imp {
    use super::EngineError;

    /// Stub — the vendored llama.cpp build tree was not found at build time (see `build.rs`).
    pub struct Model;
    pub struct Context<'m>(std::marker::PhantomData<&'m ()>);

    // The real arm's `Model` and `Context` own C handles and free them on drop. The stub owns
    // nothing, but it must still be *substitutable* for the real type, and `Drop` is part of a
    // type's observable shape: without these, callers' deliberate `drop(ctx)` — the RSS discipline
    // the shard-load and bit-exact tests depend on — trips `clippy::drop_non_drop` under the stub
    // and is clean under the real engine. Same class as the `gguf_probe` re-export break: a stub
    // that differs structurally from what it stands in for fails only on the machines that use it.
    impl Drop for Model {
        fn drop(&mut self) {}
    }
    impl Drop for Context<'_> {
        fn drop(&mut self) {}
    }

    /// Stub twin of the real `imp::gguf_probe`. **It must be a FREE function, exactly as the
    /// real arm is** — `pub use imp::{gguf_probe, ..}` below resolves against this module, not
    /// against `Model`. When this lived inside `impl Model` the re-export did not resolve and the
    /// whole workspace failed to compile on any machine without a built, patched `vendor/llama.cpp`
    /// — which is every machine except the developer's and is what a clean checkout gets.
    pub fn gguf_probe(_path: &str) -> Result<bool, EngineError> {
        Err(EngineError::unavailable())
    }

    impl Model {
        pub fn load(_path: &str, _n_gpu_layers: i32) -> Result<Model, EngineError> {
            Err(EngineError::unavailable())
        }
        pub fn load_vocab_only(_path: &str) -> Result<Model, EngineError> {
            Err(EngineError::unavailable())
        }
        pub fn load_shard(_path: &str, _l0: i32, _l1: i32, _n_gpu_layers: i32) -> Result<Model, EngineError> {
            Err(EngineError::unavailable())
        }
        pub fn load_window(&self) -> Option<(i32, i32)> {
            None
        }
        pub fn n_layer(&self) -> i32 {
            0
        }
        pub fn n_embd(&self) -> i32 {
            0
        }
        pub fn n_vocab(&self) -> i32 {
            0
        }
        pub fn tokenize(&self, _text: &str) -> Result<Vec<i32>, EngineError> {
            Err(EngineError::unavailable())
        }
        pub fn tokenize_ex(&self, _text: &str, _add_special: bool, _parse_special: bool) -> Result<Vec<i32>, EngineError> {
            Err(EngineError::unavailable())
        }
        pub fn token_to_piece(&self, _token: i32, _special: bool) -> Result<Vec<u8>, EngineError> {
            Err(EngineError::unavailable())
        }
        pub fn context(
            &self,
            _l0: i32,
            _l1: i32,
            _embeddings: bool,
            _n_ctx: i32,
            _n_batch: i32,
        ) -> Result<Context<'_>, EngineError> {
            Err(EngineError::unavailable())
        }
    }

    impl<'m> Context<'m> {
        pub fn apply_tokens(&mut self, _t: &[i32], _p: i32, _o: Option<&mut [f32]>) -> Result<(), EngineError> {
            Err(EngineError::unavailable())
        }
        pub fn n_embd(&self) -> i32 {
            0
        }
        pub fn n_vocab(&self) -> i32 {
            0
        }
        pub fn n_ctx(&self) -> i32 {
            0
        }
        pub fn n_batch(&self) -> i32 {
            0
        }
        pub fn apply_boundary(&mut self, _b: &[f32], _n: i32, _p: i32, _o: Option<&mut [f32]>) -> Result<(), EngineError> {
            Err(EngineError::unavailable())
        }
        pub fn logits(&mut self, _at: i32) -> Result<Vec<f32>, EngineError> {
            Err(EngineError::unavailable())
        }
        pub fn kv_truncate(&mut self, _pos: i32) -> Result<(), EngineError> {
            Err(EngineError::unavailable())
        }
    }
}

pub use imp::{gguf_probe, Context, Model};

/// True when the real engine is linked (the vendored build tree was present at build time).
pub const ENGINE_AVAILABLE: bool = cfg!(not(engine_unavailable));
