//! **The fast lane of the M4·1 (c) fuzzing arm.**
//!
//! The 24-CPU-hour DoD is accumulated by the CI arm (`.github/workflows/fuzz.yml`, dispatch +
//! weekly). This file is the part that runs on **every push**, because a weekly job is a slow
//! feedback loop for a defect introduced on a Monday: a fixed, seeded slice of the same generator
//! and the same oracle, sized to a few seconds.
//!
//! **The seeds are fixed on purpose.** A test that fuzzed from a random seed would be flaky —
//! sometimes catching a regression and sometimes not — and a flaky security test is one that gets
//! muted. Fixed seeds give a deterministic corpus that either passes or does not, and the CI arm
//! does the exploratory search.

use hydra_fuzz::{run_case, Target};

/// Iterations per (target, seed). Chosen so the whole file runs in a couple of seconds in debug —
/// the budget of a push-time test, not of a fuzzing campaign.
const ITERATIONS: u64 = 20_000;
const SEEDS: [u64; 3] = [1, 0xC0FFEE, 0xDEADBEEF];

fn sweep(target: Target) {
    let mut crashes = Vec::new();
    for seed in SEEDS {
        for iteration in 0..ITERATIONS {
            if let Some(c) = run_case(target, seed, iteration) {
                crashes.push(format!(
                    "seed={} iteration={} input_len={} message={:?} (replay: cargo run -p hydra-fuzz \
                     --bin hydra-fuzz -- --target {} --seed {} --replay {})",
                    c.seed, c.iteration, c.input_len, c.message, target.name(), c.seed, c.iteration
                ));
                if crashes.len() >= 5 {
                    break;
                }
            }
        }
    }
    assert!(
        crashes.is_empty(),
        "{} parser crashed on {} of {} seeded cases:\n  {}",
        target.name(),
        crashes.len(),
        SEEDS.len() as u64 * ITERATIONS,
        crashes.join("\n  ")
    );
}

/// The GGUF parser — a **downloaded model file** is the untrusted input of report Addendum 2 §D1,
/// and this is the surface that had both of the M4·1 defects (allocation amplification from a
/// declared count; unchecked 64-bit shape arithmetic).
#[test]
fn the_gguf_parser_survives_a_seeded_hostile_corpus() {
    // Suppress per-case backtraces; a crash is reported by the assertion, with its replay command.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let r = std::panic::catch_unwind(|| sweep(Target::Gguf));
    std::panic::set_hook(prev);
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}

/// The transport frame header — the **pre-allocation** path. Everything here runs before a payload
/// buffer is sized, which is exactly why it must not be able to panic on a hostile header.
#[test]
fn the_frame_header_parser_survives_a_seeded_hostile_corpus() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let r = std::panic::catch_unwind(|| sweep(Target::FrameHeader));
    std::panic::set_hook(prev);
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}

/// The wire body — FlatBuffers decoding plus the F1 fence, reached from the network after the
/// header check. Cases are bit-flips of *structurally valid* frames, so they exercise the vtable
/// and offset arithmetic rather than dying at the root offset.
#[test]
fn the_wire_body_parser_survives_a_seeded_hostile_corpus() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let r = std::panic::catch_unwind(|| sweep(Target::WireBody));
    std::panic::set_hook(prev);
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}

/// The signed shard manifest — the third length-prefixed counted-array format in the supply chain,
/// and the one M4·1's arm did not cover (audit Wave 1). Its `verify_and_parse` must reject an
/// unsigned case *before* the parser runs, and the raw parser must survive the case anyway.
#[test]
fn the_manifest_parser_survives_a_seeded_hostile_corpus() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let r = std::panic::catch_unwind(|| sweep(Target::Manifest));
    std::panic::set_hook(prev);
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}

/// The **bootstrap provisioning blob** (rule-17 sweep). Untrusted input that, since audit C1,
/// carries the manifest trust anchor — so a parser defect here is upstream of the trust decision.
/// This is where audit **L2**, the fourth `Vec::with_capacity(declared)`, was found.
#[test]
fn the_bootstrap_parser_survives_a_seeded_hostile_corpus() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let r = std::panic::catch_unwind(|| sweep(Target::Bootstrap));
    std::panic::set_hook(prev);
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}

/// **Audit H6 — the VENDORED parser, the one the worker actually loads through.**
///
/// Skipped (loudly) when the engine is not linked, which is the CI case today: **`build.rs`
/// degrades to a stub and CI never builds the real engine at all** (audit L1). So this target is
/// exercised locally and its CI status is *unavailable*, not *green* — recorded here so the
/// distinction survives into whoever reads the receipts.
#[test]
fn the_vendored_gguf_parser_survives_a_seeded_hostile_corpus() {
    if !hydra_engine_sys::ENGINE_AVAILABLE {
        eprintln!("SKIP: the vendored engine is not linked — this target is UNAVAILABLE, not passing (audit L1)");
        return;
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let r = std::panic::catch_unwind(|| sweep(Target::VendoredGguf));
    std::panic::set_hook(prev);
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}

/// The **on-disk WAL record framing** (rule-17 sweep). It was already allocation-free by
/// construction — but "clean by construction" is a claim, and rule 17 asks for a target rather than
/// a claim. The oracle is the arithmetic and the self-consistency of any record it hands back.
#[test]
fn the_boundary_record_parser_survives_a_seeded_hostile_corpus() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let r = std::panic::catch_unwind(|| sweep(Target::BoundaryRecord));
    std::panic::set_hook(prev);
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}

#[test]
fn the_wal_record_parser_survives_a_seeded_hostile_corpus() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let r = std::panic::catch_unwind(|| sweep(Target::WalRecord));
    std::panic::set_hook(prev);
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}

/// The generator itself must produce **varied, non-trivial** inputs. Without this, every test above
/// could pass because the fuzzer emits nothing but empty buffers — a green that means nothing. The
/// same reason the D0 zero-traffic test is paired with a D1 control.
#[test]
fn the_generator_produces_varied_nonempty_inputs() {
    for target in Target::ALL {
        let mut lens = std::collections::HashSet::new();
        let mut total = 0usize;
        for iteration in 0..2_000u64 {
            let mut rng = hydra_fuzz::Rng::for_case(7, iteration);
            let input = match target {
                Target::Gguf => hydra_fuzz::gen::gguf_case(&mut rng),
                Target::FrameHeader => hydra_fuzz::gen::frame_header_case(&mut rng),
                Target::WireBody => hydra_fuzz::gen::wire_body_case(&mut rng),
                Target::Manifest => hydra_fuzz::gen::manifest_case(&mut rng),
                Target::Bootstrap => hydra_fuzz::gen::bootstrap_case(&mut rng),
                Target::WalRecord => hydra_fuzz::gen::wal_record_case(&mut rng),
                Target::BoundaryRecord => hydra_fuzz::gen::boundary_record_case(&mut rng),
                Target::VendoredGguf => hydra_fuzz::gen::gguf_case(&mut rng),
            };
            total += input.len();
            lens.insert(input.len());
        }
        assert!(lens.len() > 50, "{} generator produced only {} distinct lengths", target.name(), lens.len());
        assert!(total / 2_000 > 8, "{} generator's mean input is {} bytes — too small to reach anything", target.name(), total / 2_000);
    }
}

/// A case is reproducible from `(seed, iteration)` alone — the contract a crash report rests on
/// (BLUEPRINT §4.2, *"determinism or it didn't happen"*). If this were not true, every `CRASH` line
/// the driver prints would be unactionable.
#[test]
fn a_case_is_reproducible_from_seed_and_iteration_alone() {
    for target in Target::ALL {
        for iteration in [0u64, 1, 999, 12_345] {
            let build = |i: u64| {
                let mut rng = hydra_fuzz::Rng::for_case(42, i);
                match target {
                    Target::Gguf => hydra_fuzz::gen::gguf_case(&mut rng),
                    Target::FrameHeader => hydra_fuzz::gen::frame_header_case(&mut rng),
                    Target::WireBody => hydra_fuzz::gen::wire_body_case(&mut rng),
                    Target::Manifest => hydra_fuzz::gen::manifest_case(&mut rng),
                    Target::Bootstrap => hydra_fuzz::gen::bootstrap_case(&mut rng),
                    Target::WalRecord => hydra_fuzz::gen::wal_record_case(&mut rng),
                    Target::BoundaryRecord => hydra_fuzz::gen::boundary_record_case(&mut rng),
                    Target::VendoredGguf => hydra_fuzz::gen::gguf_case(&mut rng),
                }
            };
            assert_eq!(build(iteration), build(iteration), "{} case {iteration} is not reproducible", target.name());
            assert_ne!(
                build(iteration),
                build(iteration + 1),
                "{} case {iteration} equals its successor — the iteration is not varying the case",
                target.name()
            );
        }
    }
}

/// **Audit H12 — the generator can now reach nesting the parser refuses.**
///
/// The oracle half of the fix. `gen.rs` read `if depth >= 2 { 4 }`, so **no case the fuzzer could
/// ever generate nested deeper than two levels**, while the parser's unbounded recursion needed
/// thousands to matter: 24 CPU-hours of green verdicts *structurally excluded* the bug this target
/// exists to find. A generator that stops where the parser stops can only confirm the parser
/// agrees with itself — so this asserts the generator produces the shape that is now refused.
#[test]
fn the_generator_reaches_array_nesting_deeper_than_the_old_cap() {
    let mut deepest = 0usize;
    for iteration in 0..4_000u64 {
        let mut rng = hydra_fuzz::Rng::for_case(11, iteration);
        let case = hydra_fuzz::gen::gguf_case(&mut rng);
        // Consecutive `elem_type = 9` markers: a lower bound on the nesting emitted.
        let (mut d, mut i) = (0usize, 0usize);
        while i + 12 <= case.len() {
            if u32::from_le_bytes([case[i], case[i + 1], case[i + 2], case[i + 3]]) == 9 {
                d += 1;
                i += 12;
            } else {
                i += 1;
            }
        }
        deepest = deepest.max(d);
    }
    assert!(
        deepest > 2,
        "the generator must nest deeper than the old `depth >= 2` cap, or it structurally excludes \
         H12 all over again (deepest seen: {deepest})"
    );
}
