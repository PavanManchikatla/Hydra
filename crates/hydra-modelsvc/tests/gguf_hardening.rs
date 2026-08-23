//! **Directed regressions for the two defects the M4·1 fuzzer found in the GGUF parser.**
//!
//! Report Addendum 2 §D1: *"Model files are untrusted input. GGUF parsing has had real CVEs in
//! llama.cpp (2024 heap-overflow class). A cluster that auto-downloads community quantizations and
//! streams shards to every device multiplies the blast radius."*
//!
//! The fuzzer (`hydra-fuzz`, target `gguf`) found both of these within a second of first running.
//! Each is reproduced here as a **directed test with a hand-built hostile file**, because that is
//! this project's standing discipline: a fuzz finding becomes a named regression that runs on every
//! push, not a corpus entry that only a weekly job would notice regressing.
//!
//! Both defects share a shape worth naming: a **count or a shape declared in the header, before the
//! bytes that would justify it**. That is the whole reason a length-prefixed binary format is
//! dangerous to parse naively, and it is why the fixes are about *what may be trusted before it is
//! read*, not about adding a size limit somewhere.

use hydra_modelsvc::gguf::{Gguf, GgufError, TensorInfo};

const MAGIC: &[u8; 4] = b"GGUF";

fn put_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put_u64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put_str(v: &mut Vec<u8>, s: &str) {
    put_u64(v, s.len() as u64);
    v.extend_from_slice(s.as_bytes());
}

fn header(tensor_count: u64, kv_count: u64) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(MAGIC);
    put_u32(&mut v, 3);
    put_u64(&mut v, tensor_count);
    put_u64(&mut v, kv_count);
    v
}

/// **Defect 1 — allocation amplification from a declared count.**
///
/// The parser reserved `Vec::with_capacity(kv_count)` / `with_capacity(tensor_count)` / and, worst,
/// `with_capacity(n)` for a declared array length — all read from the file **before** a single
/// element was parsed. A ~40-byte file could therefore ask for terabytes. The fuzzer's first hit
/// was a request for **70 368 744 177 664 bytes (64 TiB)**, which does not panic — allocation
/// failure *aborts* the process, so it is not even catchable — it just kills the worker.
///
/// The fix is not a magic cap: it is that a declared count may never reserve more than the
/// remaining input could possibly justify (`Cursor::reserve_for`). The file below stays tiny and
/// must therefore parse-and-fail in bounded memory rather than dying on the reservation.
#[test]
fn a_declared_count_far_beyond_the_file_does_not_allocate_for_it() {
    // 2^40 metadata entries, with nothing behind them.
    let big = Gguf::parse(&header(0, 1u64 << 40));
    assert!(matches!(big, Err(GgufError::Truncated { .. })), "expected a truncation, got {big:?}");

    // 2^40 tensor infos, likewise.
    let big = Gguf::parse(&header(1u64 << 40, 0));
    assert!(matches!(big, Err(GgufError::Truncated { .. })), "expected a truncation, got {big:?}");

    // u64::MAX, the extreme the fuzzer also reaches.
    let big = Gguf::parse(&header(u64::MAX, u64::MAX));
    assert!(matches!(big, Err(GgufError::Truncated { .. })), "expected a truncation, got {big:?}");

    // A declared ARRAY length of 2^40 inside one metadata value — the case that actually aborted,
    // because the element type made the reservation 64 bytes per element rather than one.
    let mut v = header(0, 1);
    put_str(&mut v, "k");
    put_u32(&mut v, 9); // array
    put_u32(&mut v, 10); // element type u64
    put_u64(&mut v, 1u64 << 40); // declared element count
    // …and no elements.
    let arr = Gguf::parse(&v);
    assert!(matches!(arr, Err(GgufError::Truncated { .. })), "expected a truncation, got {arr:?}");

    // A declared STRING length near u64::MAX with one byte behind it.
    let mut v = header(0, 1);
    put_u64(&mut v, u64::MAX);
    v.push(b'x');
    let s = Gguf::parse(&v);
    assert!(matches!(s, Err(GgufError::Truncated { .. })), "expected a truncation, got {s:?}");
}

/// **Defect 2 — unchecked 64-bit arithmetic on a declared shape.**
///
/// `n_elements()` multiplied the declared dimensions with no overflow check, and `byte_len()` then
/// multiplied that by the type's block size. In a **debug** build this panicked (`attempt to
/// multiply with overflow`) — which is how the fuzzer saw it. In a **release** build it would have
/// wrapped **silently** to a small number, and that number would then have sized a read against a
/// real buffer.
///
/// The release behaviour is the actual vulnerability, and it is the reason the fix is checked
/// arithmetic reported as an error rather than a `saturating_mul`: a saturated length is still a
/// *number*, and a caller that does not look at it carefully will use it.
#[test]
fn an_overflowing_declared_shape_is_an_error_not_a_wrap() {
    let t = TensorInfo {
        name: "blk.0.weight".to_string(),
        dims: vec![1 << 32, 1 << 32, 4], // product overflows u64
        ggml_type: 0,                    // f32
        offset: 0,
    };
    assert!(t.checked_n_elements().is_none(), "the product must be reported as overflowing");
    match t.byte_len() {
        Err(GgufError::ShapeOverflow { what, .. }) => assert_eq!(what, "element count"),
        other => panic!("expected ShapeOverflow, got {other:?}"),
    }

    // Control: an ordinary shape computes normally, so the guard is not simply refusing everything.
    let ok = TensorInfo { name: "t".into(), dims: vec![896, 4864], ggml_type: 0, offset: 0 };
    assert_eq!(ok.checked_n_elements(), Some(896 * 4864));
    assert_eq!(ok.byte_len().unwrap(), 896 * 4864 * 4);
}

/// `offset + byte_len` is checked on `u64` **before** either side is narrowed to `usize`. A hostile
/// offset that wraps could otherwise land back inside the buffer and read the wrong bytes with no
/// error at all — the quietest possible version of this bug.
#[test]
fn a_wrapping_tensor_offset_is_refused_rather_than_wrapped_into_range() {
    let mut v = header(0, 0);
    // Pad to a data section so `Gguf` has something to slice.
    v.extend_from_slice(&[0u8; 64]);
    let g = Gguf::parse(&v).expect("an empty-but-valid GGUF parses");

    let t = TensorInfo { name: "t".into(), dims: vec![4], ggml_type: 0, offset: u64::MAX - 8 };
    match g.tensor_bytes(&t) {
        Err(GgufError::ShapeOverflow { what, .. }) => assert_eq!(what, "offset + length"),
        Err(GgufError::TensorOutOfRange(_)) => {} // also a correct refusal
        other => panic!("a wrapping offset must be refused, got {other:?}"),
    }

    // An in-range-but-too-long read is refused as out of range, not truncated to what fits.
    let t = TensorInfo { name: "t".into(), dims: vec![1000], ggml_type: 0, offset: 0 };
    assert!(matches!(g.tensor_bytes(&t), Err(GgufError::TensorOutOfRange(_))));
}

/// `general.alignment` is read from the file and used as a divisor and a multiplier. Zero, a
/// non-power-of-two, and `u32::MAX` all have to be survivable — the last one overflows `div_ceil(a)
/// * a` unless the multiply saturates.
#[test]
fn a_hostile_alignment_value_does_not_overflow_the_data_offset() {
    for alignment in [0u32, 3, 7, u32::MAX] {
        let mut v = header(0, 1);
        put_str(&mut v, "general.alignment");
        put_u32(&mut v, 4); // u32 value type
        put_u32(&mut v, alignment);
        v.extend_from_slice(&[0u8; 32]);
        // Either outcome is fine; aborting or panicking is not.
        let _ = Gguf::parse(&v);
    }
}

// ---------------------------------------------------------------------------------------------
// **Audit H12 — nested arrays recursed without a depth bound.**
//
// # Standing rule 19, SILENT degree — and the auditor named the line
//
// `gen.rs` read `let elem = if depth >= 2 { 4 } else { … }`, so **no case the fuzzer could ever
// generate nested deeper than two levels**, while the parser's unbounded recursion needs thousands
// to matter. Twenty-four CPU-hours of green verdicts *structurally excluded* the bug the target
// existed to find — the blindness was in the generator, not the assertion, which is why no amount
// of additional fuzzing would have helped.
//
// The auditor marked this "CONFIRMED BY READING, NOT EXECUTED". It is now executed: **600 053
// bytes at depth 50 000 overflowed the main thread's 8 MiB stack** — `fatal runtime error: stack
// overflow, aborting`. That is SIGSEGV/abort, the same uncatchable class as the `reserve_for`
// amplification this file was written to close; `catch_unwind` never sees it.
// ---------------------------------------------------------------------------------------------

/// Build a GGUF whose single KV value is an array nested `depth` levels deep. Each level costs 12
/// wire bytes, which is the whole problem: a tiny file buys unbounded recursion.
fn nested_array_gguf(depth: usize) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"GGUF");
    v.extend_from_slice(&3u32.to_le_bytes()); // version
    v.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
    v.extend_from_slice(&1u64.to_le_bytes()); // kv_count
    v.extend_from_slice(&1u64.to_le_bytes()); // key length
    v.push(b'k');
    v.extend_from_slice(&9u32.to_le_bytes()); // value type = ARRAY
    for _ in 0..depth {
        v.extend_from_slice(&9u32.to_le_bytes()); // elem_type = ARRAY
        v.extend_from_slice(&1u64.to_le_bytes()); // n = 1
    }
    v.extend_from_slice(&4u32.to_le_bytes()); // innermost elem_type = u32
    v.extend_from_slice(&1u64.to_le_bytes()); // n = 1
    v.extend_from_slice(&7u32.to_le_bytes()); // the value
    v
}

#[test]
fn a_nested_array_is_refused_rather_than_recursed_into() {
    // The depth that actually aborted the process before the fix.
    let deep = nested_array_gguf(50_000);
    assert!(deep.len() < 700_000, "the attack costs almost nothing to build: {} bytes", deep.len());
    let err = Gguf::parse(&deep).unwrap_err();
    assert!(
        matches!(err, GgufError::NestedArray { .. }),
        "50 000 levels must be refused structurally, not recursed into: {err:?}"
    );

    // One level of nesting is refused too — the bound is "no nesting", not "not too much nesting",
    // because upstream GGUF has no nested-array support and no real model file contains one.
    assert!(matches!(Gguf::parse(&nested_array_gguf(1)).unwrap_err(), GgufError::NestedArray { .. }));

    // The metadata-only path (audit H6, the worker's pre-flight) refuses it as well — the worker
    // must not be the one component that still recurses.
    let d = nested_array_gguf(50_000);
    assert!(matches!(Gguf::parse_metadata(&d, d.len() as u64).unwrap_err(), GgufError::NestedArray { .. }));
}

/// **The control: an ordinary (unnested) array still parses.** `tokenizer.ggml.tokens` is one, so a
/// fix that refused all arrays would refuse every real model — a worse failure than the one fixed.
#[test]
fn an_ordinary_array_value_still_parses() {
    let mut v = Vec::new();
    v.extend_from_slice(b"GGUF");
    v.extend_from_slice(&3u32.to_le_bytes());
    v.extend_from_slice(&0u64.to_le_bytes());
    v.extend_from_slice(&1u64.to_le_bytes());
    v.extend_from_slice(&6u64.to_le_bytes());
    v.extend_from_slice(b"tokens");
    v.extend_from_slice(&9u32.to_le_bytes()); // ARRAY
    v.extend_from_slice(&4u32.to_le_bytes()); // of u32
    v.extend_from_slice(&3u64.to_le_bytes()); // three of them
    for x in [1u32, 2, 3] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    // Pad to the default alignment: a real GGUF's data section starts on that boundary, and the
    // parser (correctly) refuses a file whose declared data start lies past its end.
    while v.len() % 32 != 0 {
        v.push(0);
    }
    let g = Gguf::parse(&v).expect("a flat array is ordinary GGUF and must parse");
    assert_eq!(g.metadata.len(), 1);
}

// ---------------------------------------------------------------------------------------------
// **Audit H11 — `general.architecture` from the input file became an output path.**
//
// # Standing rule 19: what the oracle could not see — INDISTINGUISHING
//
// Every splitter test used a fixture whose architecture was `llama` or `qwen2`. Under those inputs
// `format!("{arch}-stage{stage}...")` and `sanitize(arch)?` produce **identical output for every
// call**, so no test could tell the sanitised implementation from the unsanitised one. The defect
// is only visible for an input no fixture contained — and the value comes from a *community model
// file*, which is exactly the input the project treats as untrusted everywhere else.
//
// The harm is not a stray file: `Path::join` **discards the directory when the component is
// absolute**, and the CLI writes `{arch}.signing.pkcs8` — the **private signing key** — through
// that join.
// ---------------------------------------------------------------------------------------------
#[test]
fn a_hostile_architecture_string_is_refused_rather_than_used_as_a_path() {
    use hydra_modelsvc::split::{sanitize_arch, SplitError};

    for hostile in [
        "../../tmp/x",              // traversal
        "/etc/cron.d/x",            // absolute — Path::join discards out_dir entirely
        ".hidden",                  // leading dot
        "a/b",                      // separator
        "a\\b",                     // separator on another platform
        "",                         // empty: format! would yield "-stage0-..."
        "with space",
        "sémantique",               // non-ASCII look-alikes
        "x\0y",                     // NUL
    ] {
        assert!(
            matches!(sanitize_arch(hostile), Err(SplitError::UnsafeArchitecture { .. })),
            "{hostile:?} must be refused as a path component"
        );
    }

    // A very long name is refused too — a 4 KB "architecture" is not an architecture.
    assert!(sanitize_arch(&"a".repeat(65)).is_err());

    // Controls: the names real models actually carry still work, or the fix would be an outage.
    for ok in ["llama", "qwen2", "gemma3", "phi-3", "deepseek_v2", "Qwen2.5"] {
        assert_eq!(sanitize_arch(ok).unwrap(), ok, "{ok} is a legitimate architecture name");
    }
}
