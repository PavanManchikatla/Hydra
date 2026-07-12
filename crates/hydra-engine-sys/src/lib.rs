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
        pub fn hydra_model_free(m: *mut HydraModel);
        pub fn hydra_model_info(m: *const HydraModel) -> HydraModelInfo;
        pub fn hydra_tokenize(
            m: *const HydraModel,
            text: *const c_char,
            text_len: i32,
            out: *mut i32,
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

    /// A loaded model (full weights). Owns the C handle; freed on drop.
    pub struct Model {
        raw: *mut ffi::HydraModel,
        n_layer: i32,
        n_embd: i32,
        n_vocab: i32,
    }

    impl Model {
        /// Load a GGUF. `n_gpu_layers` 0 = CPU (deterministic DoD backend), 99 = GPU.
        pub fn load(path: &str, n_gpu_layers: i32) -> Result<Model, EngineError> {
            let c = CString::new(path).map_err(|_| EngineError { code: 8, what: "path has NUL" })?;
            let raw = unsafe { ffi::hydra_model_load(c.as_ptr(), n_gpu_layers) };
            if raw.is_null() {
                return Err(EngineError { code: 2, what: "model load failed" });
            }
            let info = unsafe { ffi::hydra_model_info(raw) };
            Ok(Model { raw, n_layer: info.n_layer, n_embd: info.n_embd, n_vocab: info.n_vocab })
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

        pub fn tokenize(&self, text: &str) -> Result<Vec<i32>, EngineError> {
            let bytes = text.as_bytes();
            // First probe for the required length, then fill.
            let need = unsafe {
                ffi::hydra_tokenize(self.raw, bytes.as_ptr() as *const _, bytes.len() as i32, std::ptr::null_mut(), 0)
            };
            let cap = if need < 0 { -need } else { need };
            if cap <= 0 {
                return Err(EngineError { code: 4, what: "tokenize failed" });
            }
            let mut out = vec![0i32; cap as usize];
            let got = unsafe {
                ffi::hydra_tokenize(self.raw, bytes.as_ptr() as *const _, bytes.len() as i32, out.as_mut_ptr(), cap)
            };
            if got < 0 {
                return Err(EngineError { code: 4, what: "tokenize failed" });
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
            Ok(Context { raw, n_embd: self.n_embd, n_vocab: self.n_vocab, _model: std::marker::PhantomData })
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
            self.apply(Some(tokens), None, pos0, tokens.len() as i32, boundary_out)
        }

        /// Apply an injected boundary residual (`n = boundary_in.len() / n_embd` positions).
        pub fn apply_boundary(
            &mut self,
            boundary_in: &[f32],
            pos0: i32,
            boundary_out: Option<&mut [f32]>,
        ) -> Result<(), EngineError> {
            let n = boundary_in.len() / self.n_embd as usize;
            self.apply(None, Some(boundary_in), pos0, n as i32, boundary_out)
        }

        fn apply(
            &mut self,
            tokens: Option<&[i32]>,
            boundary_in: Option<&[f32]>,
            pos0: i32,
            n: i32,
            boundary_out: Option<&mut [f32]>,
        ) -> Result<(), EngineError> {
            if let Some(b) = boundary_in {
                if b.len() != (n as usize) * self.n_embd as usize {
                    return Err(EngineError { code: 6, what: "boundary_in shape mismatch" });
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

    impl Model {
        pub fn load(_path: &str, _n_gpu_layers: i32) -> Result<Model, EngineError> {
            Err(EngineError::unavailable())
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
        pub fn apply_boundary(&mut self, _b: &[f32], _p: i32, _o: Option<&mut [f32]>) -> Result<(), EngineError> {
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

pub use imp::{Context, Model};

/// True when the real engine is linked (the vendored build tree was present at build time).
pub const ENGINE_AVAILABLE: bool = cfg!(not(engine_unavailable));
