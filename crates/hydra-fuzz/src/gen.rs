//! Structure-aware case generators.
//!
//! Each generator builds an input that is **well-formed enough to get deep into the parser**, then
//! corrupts it in one specific, adversarial way. That is the whole reason a blind mutator is worth
//! running on these surfaces: uniformly random bytes fail the four-byte GGUF magic (or the `HYFR`
//! magic) with probability ≈ 1 − 2⁻³², so a naive fuzzer would spend its entire budget re-testing a
//! single `if`. Roughly one case in eight is still pure noise, because a parser must also survive
//! garbage — but the other seven aim at the arithmetic.
//!
//! The hostile patterns are drawn from the class report Addendum 2 §D1 names (the 2024 llama.cpp
//! heap-overflow family): **a declared count that does not match what follows**, **a length that
//! overruns the buffer**, **a product that overflows**, and **an offset past the end**.

use crate::Rng;

// ------------------------------------------------------------------------------------------
// GGUF
// ------------------------------------------------------------------------------------------

const GGUF_MAGIC: [u8; 4] = *b"GGUF";

fn put_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put_u64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}
/// A GGUF string: `u64` length then bytes.
fn put_str(v: &mut Vec<u8>, s: &str) {
    put_u64(v, s.len() as u64);
    v.extend_from_slice(s.as_bytes());
}

/// A hostile value the generator can pick where a length or a count belongs.
fn nasty_len(rng: &mut Rng) -> u64 {
    match rng.below(8) {
        0 => 0,
        1 => 1,
        2 => u64::MAX,
        3 => u64::MAX - 1,
        4 => u32::MAX as u64,
        5 => i64::MAX as u64,
        6 => 1 << 40,
        _ => rng.next_u64(),
    }
}

/// Build one GGUF case.
pub fn gguf_case(rng: &mut Rng) -> Vec<u8> {
    // One case in eight is pure noise: a parser must survive garbage too, and this keeps the
    // generator from being the only thing under test.
    if rng.below(8) == 0 {
        return noise(rng, 4096);
    }

    let mut v = Vec::with_capacity(1024);
    // Magic: mostly correct, occasionally off by one byte (so the magic check itself is exercised).
    if rng.below(16) == 0 {
        let mut m = GGUF_MAGIC;
        m[(rng.below(4)) as usize] ^= 1 << (rng.below(8));
        v.extend_from_slice(&m);
    } else {
        v.extend_from_slice(&GGUF_MAGIC);
    }

    // Version: mostly 2 or 3 (accepted), sometimes wild.
    put_u32(&mut v, if rng.below(8) == 0 { rng.next_u64() as u32 } else { 2 + (rng.below(2) as u32) });

    // The two declared counts. THIS is the §D1 shape: a count that does not match what follows, so
    // the parser is asked to allocate for a header it will never be able to fill.
    let real_tensors = rng.below(4);
    let real_kv = rng.below(4);
    let declared_tensors = if rng.below(4) == 0 { nasty_len(rng) } else { real_tensors };
    let declared_kv = if rng.below(4) == 0 { nasty_len(rng) } else { real_kv };
    put_u64(&mut v, declared_tensors);
    put_u64(&mut v, declared_kv);

    // Metadata entries.
    for i in 0..real_kv {
        // Sometimes name the alignment key, which the parser reads back and uses as a divisor/mask.
        if rng.below(4) == 0 {
            put_str(&mut v, "general.alignment");
            put_u32(&mut v, 4); // u32 value type
            put_u32(&mut v, match rng.below(4) {
                0 => 0,          // an alignment of zero — a division/mask hazard
                1 => 3,          // not a power of two
                2 => u32::MAX,   // align_up overflow bait
                _ => 32,
            });
            continue;
        }
        if rng.below(6) == 0 {
            // A string whose declared length is enormous, with nothing behind it.
            put_u64(&mut v, nasty_len(rng));
            v.push(b'x');
            continue;
        }
        put_str(&mut v, &format!("k{i}"));
        // Value type: mostly valid (0..=12), occasionally out of range.
        let ty = if rng.below(8) == 0 { rng.next_u64() as u32 } else { rng.below(13) as u32 };
        put_u32(&mut v, ty);
        put_gguf_value(&mut v, rng, ty, 0);
    }

    // Tensor infos.
    for i in 0..real_tensors {
        put_str(&mut v, &format!("blk.{i}.weight"));
        // n_dims: mostly small, sometimes a count that would allocate for billions of dims.
        let n_dims = if rng.below(6) == 0 { rng.next_u64() as u32 } else { 1 + rng.below(4) as u32 };
        put_u32(&mut v, n_dims);
        for _ in 0..n_dims.min(8) {
            // Dimensions: the product feeds `n_elements()` and then `byte_len()` — the classic
            // integer-overflow path into an allocation size.
            put_u64(&mut v, if rng.below(3) == 0 { nasty_len(rng) } else { rng.below(64) });
        }
        put_u32(&mut v, if rng.below(4) == 0 { rng.next_u64() as u32 } else { rng.below(20) as u32 });
        // Offset into the data section — frequently past its end.
        put_u64(&mut v, if rng.below(2) == 0 { nasty_len(rng) } else { rng.below(256) });
    }

    // A data section, sometimes truncated to nothing so every offset is out of range.
    let data_len = if rng.below(4) == 0 { 0 } else { rng.below(512) as usize };
    for _ in 0..data_len {
        v.push(rng.byte());
    }

    // Finally, a chance to truncate the whole file at an arbitrary point — the shape that turns a
    // "read the next 8 bytes" into a slice out of bounds.
    if rng.below(3) == 0 && !v.is_empty() {
        let cut = rng.below(v.len() as u64) as usize;
        v.truncate(cut);
    }
    v
}

/// Serialize a plausible value for GGUF type `ty`. `depth` bounds array nesting so the *generator*
/// cannot be the thing that runs out of stack.
fn put_gguf_value(v: &mut Vec<u8>, rng: &mut Rng, ty: u32, depth: u32) {
    match ty {
        0 | 1 | 7 => v.push(rng.byte()),                              // u8 / i8 / bool
        2 | 3 => v.extend_from_slice(&(rng.next_u64() as u16).to_le_bytes()),
        4..=6 => put_u32(v, rng.next_u64() as u32),                   // u32 / i32 / f32
        8 => {
            if rng.below(4) == 0 {
                put_u64(v, nasty_len(rng)); // declared string length with no bytes behind it
            } else {
                put_str(v, "value");
            }
        }
        9 => {
            // An array: element type, then a declared count. A huge count with no elements behind it
            // is the allocation-amplification case.
            let elem = if depth >= 2 { 4 } else { rng.below(13) as u32 };
            put_u32(v, elem);
            let n = if rng.below(3) == 0 { nasty_len(rng) } else { rng.below(8) };
            put_u64(v, n);
            for _ in 0..n.min(8) {
                put_gguf_value(v, rng, elem, depth + 1);
            }
        }
        10..=12 => put_u64(v, rng.next_u64()),                        // u64 / i64 / f64
        _ => put_u32(v, rng.next_u64() as u32),
    }
}

// ------------------------------------------------------------------------------------------
// Transport frame header
// ------------------------------------------------------------------------------------------

/// `HYFR` header cases: magic, version, flags, `payload_len`, then a body of unrelated length.
/// The interesting shape is `payload_len` disagreeing with what is actually present — the check
/// that has to happen *before* the payload buffer is sized.
pub fn frame_header_case(rng: &mut Rng) -> Vec<u8> {
    if rng.below(8) == 0 {
        return noise(rng, 128);
    }
    let mut v = Vec::with_capacity(256);
    let magic = if rng.below(16) == 0 { rng.next_u64() as u32 } else { hydra_proto::framing::FRAME_MAGIC };
    put_u32(&mut v, magic);
    let version =
        if rng.below(8) == 0 { rng.next_u64() as u16 } else { hydra_proto::framing::WIRE_VERSION };
    v.extend_from_slice(&version.to_le_bytes());
    v.extend_from_slice(&(rng.next_u64() as u16).to_le_bytes()); // flags
    let declared = match rng.below(6) {
        0 => u32::MAX,
        1 => hydra_proto::limits::MAX_FRAME_BYTES,
        2 => hydra_proto::limits::MAX_FRAME_BYTES + 1,
        3 => 0,
        _ => rng.below(4096) as u32,
    };
    put_u32(&mut v, declared);
    // A body whose length is deliberately unrelated to `declared`.
    let actual = rng.below(512) as usize;
    for _ in 0..actual {
        v.push(rng.byte());
    }
    if rng.below(4) != 0 {
        // Often append something tag-shaped, so `verify_frame` reaches the checksum comparison.
        for _ in 0..32 {
            v.push(rng.byte());
        }
    }
    v
}

// ------------------------------------------------------------------------------------------
// Wire body (FlatBuffers + F1 fence)
// ------------------------------------------------------------------------------------------

/// FlatBuffers payload cases. A valid buffer is grown from a real encoder and then bit-flipped:
/// mutating a *structurally valid* buffer reaches the vtable/offset arithmetic, which is where a
/// FlatBuffers verifier either holds or does not. Purely random bytes almost always fail the root
/// offset immediately and prove nothing about the rest.
pub fn wire_body_case(rng: &mut Rng) -> Vec<u8> {
    if rng.below(4) == 0 {
        return noise(rng, 256);
    }
    let keys = hydra_worker::wire::SessionKeys::dev(0x5E);
    let mut buf = match rng.below(4) {
        0 => hydra_worker::wire::encode_apply_token(&keys, 0, 0, 1, true),
        1 => hydra_worker::wire::encode_fwd(&keys, 0, 0, true, &vec![0.5f32; 64]),
        2 => hydra_worker::wire::encode_applied_ack(&keys, 0, 0, &[0u8; 32]),
        _ => hydra_worker::wire::encode_sample_next(&keys, 0, 0, &[0u8; 32], 1),
    };
    // 1..=8 bit flips: enough to break invariants, few enough that the buffer stays FlatBuffers-ish.
    let flips = 1 + rng.below(8);
    for _ in 0..flips {
        if buf.is_empty() {
            break;
        }
        let i = rng.below(buf.len() as u64) as usize;
        buf[i] ^= 1 << rng.below(8);
    }
    if rng.below(8) == 0 && !buf.is_empty() {
        let cut = rng.below(buf.len() as u64) as usize;
        buf.truncate(cut);
    }
    buf
}

// ------------------------------------------------------------------------------------------

fn noise(rng: &mut Rng, max_len: u64) -> Vec<u8> {
    let n = rng.below(max_len) as usize;
    (0..n).map(|_| rng.byte()).collect()
}
