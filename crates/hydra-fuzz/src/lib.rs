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
    /// `hydra_modelsvc::manifest` — the **signed shard manifest**, added by audit Wave 1.
    ///
    /// This target exists because the M4·1 fuzz arm covered the GGUF parser and the wire, and left
    /// the manifest parser — reached from the same untrusted supply chain, with the same
    /// length-prefixed counted-array shape that produced §7.28 D2 — untouched. It exercises both
    /// entry points deliberately: `verify_and_parse`, which must reject before parsing, and the
    /// raw `from_bytes`, so the parser is still fuzzed directly rather than being hidden behind a
    /// signature check that will discard almost every case.
    Manifest,
    /// `hydra_worker::bootstrap::Bootstrap::decode` — the worker's provisioning blob, which now
    /// carries the manifest **trust anchor** (audit C1). Added by the rule-17 class sweep, where it
    /// was found to be the **fourth** `Vec::with_capacity(declared)` instance (audit L2).
    Bootstrap,
    /// `hydra_wal::record::read_record` — the on-disk WAL record framing. Added by the rule-17
    /// sweep: it was already allocation-free by construction (it borrows and length-checks before
    /// slicing), but "clean by construction" is a claim, and rule 17 asks for a fuzz target rather
    /// than a claim.
    WalRecord,
    /// `hydra_coordinator::boundary_store::decode_boundary_record` — a `BOUNDARY_COPY` record read
    /// back from the boundary store for a D1 recovery replay. Added by audit C3 under rule 17: the
    /// boundary-tensor shape cross-check was fixed on the wire, and the same tensor is parsed from
    /// the **disk** by this function — a class fixed in one parser and left in another is not fixed.
    BoundaryRecord,
    /// **Audit H6 — `gguf_init_from_file`, the VENDORED C++ parser.**
    ///
    /// The distinction this target exists to make: `Target::Gguf` fuzzes
    /// `hydra_modelsvc::gguf::Gguf::parse`, which is the **offline splitter's** reader. A worker
    /// never runs it. The worker hashes the shard and then hands the path to `llama.cpp`, whose
    /// own GGUF parser has never been fuzzed by this project — so the 24-CPU-hour budget has been
    /// protecting a program that does not run on the load path, and the receipts said "the GGUF
    /// parser" without saying which.
    ///
    /// **What this target asserts is the COMPOSITION THE PRODUCT SHIPS, not the vendored parser
    /// in isolation.** In isolation it crashes: the very first sweep aborted with `SIGABRT` inside
    /// `gguf_init_from_file_ptr` (seed 1, iteration 350) — an upstream defect in `llama.cpp`,
    /// uncatchable by the shim's `catch (...)`, and not Hydra's to fix in this repo. What Hydra
    /// *can* guarantee, and what the H6 fix establishes, is that **the vendored parser is only
    /// ever handed a file the hardened parser already accepted** (`shard.rs` step 3b). So the
    /// target probes the vendored parser **only for cases `hydra_modelsvc::gguf` accepts** — which
    /// is precisely the load path — and a crash there would be a defect in the product rather than
    /// a rediscovery of the upstream one.
    ///
    /// The raw, unfiltered probe is still reachable (`--target vendored-gguf --unfiltered` in the
    /// driver) for reporting upstream; it is not in the push lane because an abort takes the whole
    /// harness with it and every other target's result becomes unobservable.
    ///
    /// Requires the real engine. When it is unavailable the target **reports that it is
    /// unavailable** rather than silently passing: an absent oracle must not read as a green one.
    VendoredGguf,
}

impl Target {
    pub const ALL: [Target; 8] = [
        Target::Gguf,
        Target::FrameHeader,
        Target::WireBody,
        Target::Manifest,
        Target::Bootstrap,
        Target::WalRecord,
        Target::BoundaryRecord,
        Target::VendoredGguf,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Target::Gguf => "gguf",
            Target::FrameHeader => "frame-header",
            Target::WireBody => "wire-body",
            Target::Manifest => "manifest",
            Target::Bootstrap => "bootstrap",
            Target::WalRecord => "wal-record",
            Target::BoundaryRecord => "boundary-record",
            Target::VendoredGguf => "vendored-gguf",
        }
    }
    pub fn parse(name: &str) -> Option<Target> {
        Target::ALL.into_iter().find(|t| t.name() == name)
    }

    /// **Why this target cannot do its job in THIS build, if it cannot.**
    ///
    /// A target structurally unable to exercise the parser it names must **say so**, not report
    /// `verdict=GREEN`. `vendored-gguf` exists to drive `gguf_init_from_file` inside llama.cpp;
    /// when the engine is stubbed it runs the **hardened Rust** parser (which the `gguf` target
    /// already covers) and then does nothing — yet the driver counted its iterations and printed
    /// GREEN, because the verdict was "no crashes" and a no-op never crashes.
    ///
    /// **In CI that green was structural**: audit L1 means CI never builds the real engine, so
    /// every fuzz leg ever run printed `target=vendored-gguf ... verdict=GREEN` for ~100M
    /// iterations of a parser it never called, and spent an eighth of the CPU-hour budget doing
    /// it. PROJECT_STATE said its CI status is "unavailable, never green" — but the LOG said
    /// GREEN, and rule 16 makes the log the thing a receipt quotes. The record's intent was
    /// right; the code did not implement it.
    ///
    /// §7.31 in the evidence layer — *a test whose name promises more than its assertion is worse
    /// than no test, because it terminates inquiry* — and rule 19's blind oracle: it could not
    /// produce the failure it guards.
    pub fn unavailable_reason(self) -> Option<&'static str> {
        match self {
            Target::VendoredGguf if !hydra_engine_sys::ENGINE_AVAILABLE => Some(
                "engine not linked (audit L1): gguf_init_from_file is never invoked, so this target proves nothing",
            ),
            _ => None,
        }
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
        Target::Manifest => gen::manifest_case(&mut rng),
        Target::Bootstrap => gen::bootstrap_case(&mut rng),
        Target::WalRecord => gen::wal_record_case(&mut rng),
        Target::BoundaryRecord => gen::boundary_record_case(&mut rng),
        // The vendored parser takes a PATH, so the same generated bytes are written to a file.
        Target::VendoredGguf => gen::gguf_case(&mut rng),
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
            let fence = hydra_worker::wire::SessionFence::dev(0x5E);
            let _ = hydra_worker::wire::decode(&input, &fence);
            let _ = hydra_worker::wire::is_fwd_frame(&input);
        }
        Target::Manifest => {
            // The production entry point: must refuse before parsing (H13). A fixed trust anchor
            // and fixed expected hash mean essentially every case is rejected — which is the point,
            // since this arm is asserting that rejection is *cheap and total*, not that it parses.
            const TRUSTED: [u8; 32] = [0x11; 32];
            const EXPECTED: [u8; 32] = [0x22; 32];
            let _ = hydra_modelsvc::manifest::Manifest::verify_and_parse(&input, &TRUSTED, &EXPECTED);
            // …and the parser directly, so the structural code is genuinely covered rather than
            // shadowed by the signature gate. Walk the accessors on success: a parser that accepts
            // a hostile manifest and then hands out an inconsistent structure has moved the crash,
            // not prevented it.
            if let Ok(m) = hydra_modelsvc::manifest::Manifest::from_bytes(&input) {
                let _ = m.canonical_bytes();
                for sh in &m.shards {
                    let _ = sh.tensors.len();
                }
            }
        }
        Target::Bootstrap => {
            // A provisioning blob is untrusted input: it arrives on the worker's filesystem from
            // whatever provisioned it, and since audit C1 it carries the manifest trust anchor —
            // so a parser defect here is upstream of the trust decision, not beside it.
            if let Ok(b) = hydra_worker::bootstrap::Bootstrap::decode_for_fuzz(&input) {
                let _ = b.listen_addr.len();
                let _ = b.cert_chain_der.len();
            }
        }
        Target::WalRecord => {
            // The on-disk record framing. `read_record` borrows rather than allocating, so the
            // oracle here is the arithmetic — `record_size`, `pad_len`, and the tag slice bounds.
            let step: hydra_wal::record::ReadStep<'_> = hydra_wal::record::read_record(&input);
            if let hydra_wal::record::ReadStep::Record { header, payload, total_len } = step {
                // A returned record must be self-consistent with the buffer it came from: if it is
                // not, the parser has handed out a slice it did not prove.
                assert!(total_len <= input.len(), "read_record returned total_len past the buffer");
                assert_eq!(payload.len(), header.payload_len as usize, "payload/header disagree");
            }
        }
        Target::VendoredGguf => {
            // `gguf_init_from_file` opens by path, so the case is materialised. Accept or reject
            // are both fine; the failure this looks for is the one that is not a return value —
            // a segfault, an abort, or an exception crossing back into Rust.
            // THE FILTER IS THE POINT: only cases the hardened parser accepts reach the vendored
            // one, because that is the order `verify_shard` now enforces on the load path.
            let hardened_accepts = hydra_modelsvc::gguf::Gguf::parse_metadata(&input, input.len() as u64).is_ok();
            if hydra_engine_sys::ENGINE_AVAILABLE && hardened_accepts {
                if let Some(path) = write_temp_case(&input, seed, iteration) {
                    // The coordinates go to disk BEFORE the call, because the failure mode here is
                    // an uncatchable abort — see `mark_in_flight`.
                    mark_in_flight(Target::VendoredGguf, seed, iteration, &path);
                    let _ = hydra_engine_sys::gguf_probe(&path);
                    clear_in_flight();
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        Target::BoundaryRecord => {
            // A record that parses must be shape-consistent by C3's rule: one position, and a
            // float count equal to the declared n_embd. A parser that returns a boundary whose
            // length disagrees with what it claimed to check has moved the defect, not fixed it.
            if let Ok(b) = hydra_coordinator::boundary_store::decode_boundary_record(&input) {
                let bc = flatbuffers_root_n_embd(&input);
                assert_eq!(Some(b.activations.len() as u64), bc, "decoded length disagrees with declared n_embd");
            }
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

/// Materialise a generated case as a file, for a target whose parser takes a path (audit H6).
/// `None` if the file cannot be written — the case is skipped rather than reported as passing.
fn write_temp_case(bytes: &[u8], seed: u64, iteration: u64) -> Option<String> {
    let dir = std::env::temp_dir().join("hydra-fuzz-vendored");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("case-{seed}-{iteration}.gguf"));
    std::fs::write(&path, bytes).ok()?;
    Some(path.to_string_lossy().into_owned())
}

/// Where the in-flight case's coordinates are recorded before a probe that may **abort** the
/// process (audit H6).
pub fn crash_marker_path() -> std::path::PathBuf {
    std::env::temp_dir().join("hydra-fuzz-vendored").join("IN-FLIGHT")
}

/// **Audit H6 — the reproduction contract, for a failure mode that cannot be caught.**
///
/// Every other target's failure is a panic, which `catch_unwind` turns into a `Crash` value
/// carrying `(seed, iteration)`. The vendored parser's failure is an **abort**: `SIGABRT` from
/// inside `libggml`, which no `catch(...)` and no `catch_unwind` will ever see — the process is
/// simply gone, and with it the coordinates. So the coordinates are written to disk *before* the
/// call and removed after it: if the file survives, it names the case that killed the run.
///
/// This is not defensive decoration. Without it the target can only report *that* something
/// aborted, and "determinism or it didn't happen" (BLUEPRINT §4.2) would be broken for the one
/// target whose finding is hardest to reproduce by hand.
fn mark_in_flight(target: Target, seed: u64, iteration: u64, case_path: &str) {
    let _ = std::fs::write(
        crash_marker_path(),
        format!("target={} seed={} iteration={} case={}\n", target.name(), seed, iteration, case_path),
    );
}

fn clear_in_flight() {
    let _ = std::fs::remove_file(crash_marker_path());
}

/// The declared `dims[1]` of an accepted boundary record (re-read independently of the decoder).
fn flatbuffers_root_n_embd(input: &[u8]) -> Option<u64> {
    let bc = flatbuffers::root::<hydra_proto::proto::BoundaryCopy>(input).ok()?;
    let dims = bc.activations().dims();
    (dims.len() == 2).then(|| dims.get(1) as u64)
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
