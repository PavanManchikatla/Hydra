//! Diagnostic for the §7.24 residual (M3 gate row 8).
//!
//! Under the amendment the calibration residual is **+10.50 ms/tok at k=12 and +10.37 ms/tok at
//! k=8** — essentially **constant across split points**. That shape rules out both amended terms:
//! a per-stage cost would scale with stage count, a per-layer cost with layers. A constant
//! per-token cost must come from work the pipeline does **once per token regardless of how it is
//! split**.
//!
//! The largest named candidate is the final stage's `APPLIED_ACK` witness: `logits_digest` converts
//! the full `n_vocab` logits vector to little-endian bytes and BLAKE3-hashes it, every token. On
//! this model that is 151 936 floats → ~608 KB converted and hashed per token — and the protocol
//! microbench deliberately did **not** include it (it sent a fixed 32-byte checksum), so it is
//! genuinely unaccounted for.
//!
//! This measures that operation in isolation.

/// Local copy of the crate-private `f32_to_bytes_le` — identical body, so the diagnostic
/// measures the same work without widening the crate API for a test.
fn f32_to_bytes_le(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

#[test]
#[ignore = "diagnostic for M3 gate row 8; run explicitly"]
fn the_applied_ack_logits_digest_costs_per_token() {
    // Qwen2.5-0.5B's vocabulary — the width of the logits vector S_P digests on every token.
    const N_VOCAB: usize = 151_936;
    let logits: Vec<f32> = (0..N_VOCAB).map(|i| (i as f32) * 1e-4).collect();

    // Warm up, then measure the exact operation `worker::logits_digest` performs.
    for _ in 0..8 {
        let _ = blake3::hash(&f32_to_bytes_le(&logits));
    }
    let mut s = Vec::new();
    for _ in 0..64 {
        let t = std::time::Instant::now();
        let _ = blake3::hash(&f32_to_bytes_le(&logits));
        s.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let d = median(s);

    // Split the two halves so the report can say which dominates.
    let mut sc = Vec::new();
    for _ in 0..64 {
        let t = std::time::Instant::now();
        let b = f32_to_bytes_le(&logits);
        sc.push(t.elapsed().as_secs_f64() * 1000.0);
        std::hint::black_box(&b);
    }
    let convert = median(sc);

    eprintln!(
        "APPLIED_ACK DIGEST COST (n_vocab={N_VOCAB}, {} KB per token)\n\
         \x20 f32_to_bytes_le          {convert:.3} ms\n\
         \x20 convert + BLAKE3 (total) {d:.3} ms/token\n\
         \x20 This is paid ONCE PER TOKEN by the final stage, regardless of how the model is split \
         — the shape the calibration residual has.",
        N_VOCAB * 4 / 1024
    );

    assert!(d > 0.0, "sanity");
}
