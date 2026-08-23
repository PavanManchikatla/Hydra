//! **M4·1 (c) — parser fuzzing for the two untrusted-input surfaces.**
//!
//! BLUEPRINT §3's M4 DoD: *"GGUF parser fuzzed for 24 CPU-hours without crashes."* Report Addendum
//! 2 **§D1**: *"Model files are untrusted input. GGUF parsing has had real CVEs in llama.cpp (2024
//! heap-overflow class)."* The frame parser is the same class of surface reached from the network
//! instead of the disk.
//!
//! # What this is, and what it is not — stated up front
//!
//! This is a **deterministic, structure-aware mutation fuzzer**. It is **not** coverage-guided
//! fuzzing (libFuzzer / AFL). The difference is real and is not glossed:
//!
//! * **What is given up:** no coverage feedback, so the search does not learn which mutations reach
//!   new code. A coverage-guided fuzzer will, given enough time, find deeper paths than this will.
//! * **What is gained, and why it fits this project:** every finding is reproducible from a
//!   `(seed, iteration)` pair alone — the same contract `hydra-sim` has run under since M1
//!   (BLUEPRINT §4.2, *"determinism or it didn't happen"*) — and it builds and runs on **stable**
//!   Rust, so the CI arm is one this project can actually observe running rather than one it has to
//!   take on faith (standing rule 12: verification infrastructure gets no presumption of
//!   correctness).
//! * **The upgrade is recorded, not pretended away:** adding `cargo-fuzz` targets over these same
//!   entry points is a strictly additive change and is named as an owed item in PROJECT_STATE §8.
//!
//! Structure-awareness is what makes a blind mutator worth running here. Uniformly random bytes die
//! at the four-byte GGUF magic essentially every time, so a naive fuzzer would spend 24 CPU-hours
//! re-testing one `if`. The generators below emit files and frames that are **well-formed enough to
//! get deep into the parser** and then hostile in one specific way: an enormous declared count, a
//! length that overruns the buffer, a dimension product that overflows `u64`, a string length near
//! `usize::MAX`, an offset past the end of the data section.
//!
//! # The oracle
//!
//! A parser may return `Err`. A parser may return `Ok`. What it may **not** do is panic, abort, or
//! hang — those are the crash class §D1 is about, and in Rust an arithmetic overflow panic in a
//! debug build and a wrong-length allocation in a release build come from the same defect. So the
//! oracle is: **no panic, and no unbounded allocation**, for any input whatsoever.

use std::panic::{catch_unwind, AssertUnwindSafe};

pub mod gen;

/// A tiny counter-based PRNG. Deterministic, seekable, and dependency-free — `iteration` alone
/// reproduces a case, so a crash report is `(seed, iteration)` and nothing else.
#[derive(Clone, Copy)]
pub struct Rng(pub u64);

impl Rng {
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    #[inline]
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
    #[inline]
    pub fn byte(&mut self) -> u8 {
        (self.next_u64() >> 24) as u8
    }
    /// The RNG for one case, derived from `(seed, iteration)` — so cases are independent and any
    /// one of them can be replayed without running the ones before it.
    pub fn for_case(seed: u64, iteration: u64) -> Rng {
        let mut r = Rng(seed ^ iteration.wrapping_mul(0xD1B5_4A32_D192_ED03));
        r.next_u64();
        r
    }
}

/// Which parser a case exercises.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    /// `hydra_modelsvc::gguf::Gguf::parse` — a downloaded model file (Addendum 2 §D1).
    Gguf,
    /// `hydra_proto::framing::{FrameHeader::parse, verify_frame}` — the pre-allocation header path.
    FrameHeader,
    /// `hydra_worker::wire::decode` — the FlatBuffers body + F1 fence, post-header.
    WireBody,
}

impl Target {
    pub const ALL: [Target; 3] = [Target::Gguf, Target::FrameHeader, Target::WireBody];
    pub fn name(self) -> &'static str {
        match self {
            Target::Gguf => "gguf",
            Target::FrameHeader => "frame-header",
            Target::WireBody => "wire-body",
        }
    }
    pub fn parse(name: &str) -> Option<Target> {
        Target::ALL.into_iter().find(|t| t.name() == name)
    }
}

/// A case that crashed, carrying everything needed to replay it.
#[derive(Debug)]
pub struct Crash {
    pub target: Target,
    pub seed: u64,
    pub iteration: u64,
    pub input_len: usize,
    pub message: String,
}

/// Run one case. Returns `Some(Crash)` iff the parser panicked.
///
/// The panic hook is left alone: the driver silences it once for the whole run, because a fuzzer
/// that prints a backtrace per case produces gigabytes of log and nothing else.
pub fn run_case(target: Target, seed: u64, iteration: u64) -> Option<Crash> {
    let mut rng = Rng::for_case(seed, iteration);
    let input = match target {
        Target::Gguf => gen::gguf_case(&mut rng),
        Target::FrameHeader => gen::frame_header_case(&mut rng),
        Target::WireBody => gen::wire_body_case(&mut rng),
    };
    let input_len = input.len();

    let result = catch_unwind(AssertUnwindSafe(|| match target {
        Target::Gguf => {
            // `Ok` and `Err` are both acceptable outcomes; a panic is not. On a successful parse we
            // also walk the accessors, because a parser that accepts a hostile file and then hands
            // out an out-of-range slice has moved the crash rather than prevented it.
            if let Ok(g) = hydra_modelsvc::gguf::Gguf::parse(&input) {
                for t in &g.tensors {
                    let _ = t.byte_len();
                    let _ = g.tensor_bytes(t);
                }
                let _ = g.architecture();
            }
        }
        Target::FrameHeader => {
            if let Ok(h) = hydra_proto::framing::FrameHeader::parse(&input) {
                let _ = h.frame_size();
                let _ = hydra_proto::framing::verify_frame(&input);
            }
        }
        Target::WireBody => {
            let keys = hydra_worker::wire::SessionKeys::dev(0x5E);
            let _ = hydra_worker::wire::decode(&input, &keys);
            let _ = hydra_worker::wire::is_fwd_frame(&input);
        }
    }));

    result.err().map(|payload| Crash {
        target,
        seed,
        iteration,
        input_len,
        message: panic_message(payload),
    })
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}
