//! A minimal, faithful GGUF v2/v3 reader + deterministic writer (enough to split a model into
//! per-stage shards and re-emit valid GGUF containers). No engine dependency — this parses the
//! documented on-disk format directly.
//!
//! Layout (little-endian): magic `GGUF` · `version:u32` · `tensor_count:u64` ·
//! `metadata_kv_count:u64` · metadata KVs · tensor infos · padding to `general.alignment` (default
//! 32) · tensor data (each tensor at its `offset` from the start of the data section). We keep the
//! metadata **in file order** and re-emit it verbatim, so a round-trip is structure-preserving and a
//! split is a deterministic function of its inputs.

use std::collections::BTreeMap;

/// The GGUF metadata magic bytes.
pub const MAGIC: &[u8; 4] = b"GGUF";
/// Default tensor-data alignment when `general.alignment` is absent.
pub const DEFAULT_ALIGNMENT: u64 = 32;

#[derive(Debug, thiserror::Error)]
pub enum GgufError {
    /// **Audit H12.** An array whose elements are themselves arrays. The parser used to recurse
    /// for each one, unbounded — 12 wire bytes per level, so a small file overflows the stack and
    /// aborts. Upstream GGUF has no nested-array support, so this shape never occurs in a real
    /// model file.
    #[error("nested array at offset {at}: GGUF arrays may not contain arrays")]
    NestedArray { at: usize },
    #[error("not a GGUF file (bad magic)")]
    BadMagic,
    #[error("unsupported GGUF version {0} (expected 2 or 3)")]
    BadVersion(u32),
    #[error("truncated: needed {need} bytes at offset {at}, have {have}")]
    Truncated { at: usize, need: usize, have: usize },
    #[error("unknown metadata value type {0}")]
    BadValueType(u32),
    #[error("unknown ggml tensor type {0} (extend the size table)")]
    BadTensorType(u32),
    #[error("invalid utf-8 in a GGUF string")]
    BadUtf8,
    #[error("tensor {0:?} data out of range")]
    TensorOutOfRange(String),
    /// A declared shape/offset overflows 64-bit arithmetic. Found by the M4·1 fuzzer: in a debug
    /// build this was a panic, and in a **release** build it would have wrapped silently to a
    /// small length — a wrong-size read against a real buffer, which is precisely the 2024
    /// llama.cpp heap-overflow class report Addendum 2 §D1 names.
    #[error("tensor {name:?}: {what} overflows 64-bit arithmetic")]
    ShapeOverflow { name: String, what: &'static str },
}

type R<T> = Result<T, GgufError>;

/// A GGUF metadata value (the 13 documented types). Arrays are typed + length-prefixed.
#[derive(Debug, Clone, PartialEq)]
pub enum GgufValue {
    U8(u8),
    I8(i8),
    U16(u16),
    I16(i16),
    U32(u32),
    I32(i32),
    U64(u64),
    I64(i64),
    F32(f32),
    F64(f64),
    Bool(bool),
    Str(String),
    Array(GgufArray),
}

/// A typed GGUF array. `elem_type` is the wire type-id; values are the parsed elements.
#[derive(Debug, Clone, PartialEq)]
pub struct GgufArray {
    pub elem_type: u32,
    pub values: Vec<GgufValue>,
}

/// One tensor's descriptor (name, shape, ggml type, and byte offset within the data section).
#[derive(Debug, Clone)]
pub struct TensorInfo {
    pub name: String,
    pub dims: Vec<u64>,
    pub ggml_type: u32,
    /// Offset (bytes) from the start of the tensor-**data** section.
    pub offset: u64,
}

impl TensorInfo {
    /// Number of elements (product of dims), or `None` if the declared shape overflows.
    ///
    /// **Every arithmetic step on an attacker-declared shape is checked.** A GGUF file is untrusted
    /// input (report Addendum 2 §D1), and `dims` comes straight off the wire: four dimensions of
    /// 2⁴⁸ overflow `u64` in the product. Unchecked, that is a debug panic and — far worse — a
    /// **release-build silent wrap to a small number**, which then sizes a read against a real
    /// buffer. That is the heap-overflow shape, not a cosmetic overflow.
    pub fn checked_n_elements(&self) -> Option<u64> {
        if self.dims.is_empty() {
            return Some(0);
        }
        self.dims.iter().try_fold(1u64, |acc, d| acc.checked_mul(*d))
    }

    /// Number of elements, saturating on overflow. Retained for callers that only want a magnitude;
    /// anything that sizes an allocation or a slice must use [`TensorInfo::checked_n_elements`] or
    /// [`TensorInfo::byte_len`], both of which report the overflow instead of hiding it.
    pub fn n_elements(&self) -> u64 {
        self.checked_n_elements().unwrap_or(u64::MAX)
    }

    /// Exact byte length of this tensor's data (from its ggml type's block layout).
    pub fn byte_len(&self) -> R<u64> {
        let (block_elems, block_bytes) = ggml_type_block(self.ggml_type)?;
        let n = self
            .checked_n_elements()
            .ok_or_else(|| GgufError::ShapeOverflow { name: self.name.clone(), what: "element count" })?;
        // Quantized types pack `block_elems` elements per `block_bytes`; f32/f16 are 1-per-N.
        (n / block_elems.max(1))
            .checked_mul(block_bytes as u64)
            .ok_or_else(|| GgufError::ShapeOverflow { name: self.name.clone(), what: "byte length" })
    }
}

/// A parsed GGUF file, holding metadata (in file order), tensor infos, and the raw data section.
#[derive(Debug, Clone)]
pub struct Gguf {
    pub version: u32,
    pub alignment: u64,
    /// Metadata key/value pairs, **in file order** (order is preserved on re-emit).
    pub metadata: Vec<(String, GgufValue)>,
    pub tensors: Vec<TensorInfo>,
    /// The tensor-data section (each tensor's bytes live at `[offset, offset+byte_len)`).
    pub data: Vec<u8>,
}

// ------------------------------- parsing -------------------------------

struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}
impl<'a> Cursor<'a> {
    /// Bytes still unread. **Every reservation made from an attacker-controlled count is clamped by
    /// this** — see [`Cursor::reserve_for`].
    fn remaining(&self) -> usize {
        self.b.len().saturating_sub(self.i)
    }

    /// A capacity to pre-allocate for `declared` items of at least `min_bytes_each` on the wire.
    ///
    /// **Report Addendum 2 §D1 — model files are untrusted input.** A GGUF header declares its
    /// counts *before* the items follow, so `Vec::with_capacity(declared)` hands an attacker a
    /// multiplier: a 40-byte file declaring 2⁴⁰ array elements asked for **70 TB** and aborted the
    /// process. That is not a panic — allocation failure aborts, so it is not even catchable — and
    /// the M4·1 fuzzer found it in under a second (`hydra-fuzz`, target `gguf`).
    ///
    /// The invariant is simple and total: **a declared count may never reserve more memory than the
    /// remaining input could possibly justify.** Each item costs at least `min_bytes_each` on the
    /// wire, so `remaining / min_bytes_each` is a hard ceiling on how many can really be there. A
    /// count above it is not rejected here — the parse loop will fail on the truncation, with a
    /// better error — it is simply not *pre-allocated* for.
    fn reserve_for(&self, declared: u64, min_bytes_each: usize) -> usize {
        let justified = self.remaining() / min_bytes_each.max(1);
        (declared as usize).min(justified)
    }

    fn take(&mut self, n: usize) -> R<&'a [u8]> {
        let end = self.i.checked_add(n).ok_or(GgufError::Truncated { at: self.i, need: n, have: 0 })?;
        if end > self.b.len() {
            return Err(GgufError::Truncated { at: self.i, need: n, have: self.b.len() - self.i.min(self.b.len()) });
        }
        let s = &self.b[self.i..end];
        self.i = end;
        Ok(s)
    }
    fn u32(&mut self) -> R<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> R<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn string(&mut self) -> R<String> {
        let n = self.u64()?;
        // `take` bounds-checks before slicing, so a huge declared length is a `Truncated` error and
        // never an allocation. The `as usize` cast is done after that check, not before it.
        if n > self.remaining() as u64 {
            return Err(GgufError::Truncated { at: self.i, need: usize::MAX, have: self.remaining() });
        }
        let bytes = self.take(n as usize)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| GgufError::BadUtf8)
    }
    fn value(&mut self, ty: u32) -> R<GgufValue> {
        Ok(match ty {
            0 => GgufValue::U8(self.take(1)?[0]),
            1 => GgufValue::I8(self.take(1)?[0] as i8),
            2 => GgufValue::U16(u16::from_le_bytes(self.take(2)?.try_into().unwrap())),
            3 => GgufValue::I16(i16::from_le_bytes(self.take(2)?.try_into().unwrap())),
            4 => GgufValue::U32(self.u32()?),
            5 => GgufValue::I32(self.u32()? as i32),
            6 => GgufValue::F32(f32::from_bits(self.u32()?)),
            7 => GgufValue::Bool(self.take(1)?[0] != 0),
            8 => GgufValue::Str(self.string()?),
            9 => {
                let elem_type = self.u32()?;
                // **Audit H12 — nested arrays are refused, not recursed into.**
                //
                // `value()` called itself for each element with no depth bound, and each nesting
                // level costs **12 wire bytes** (`elem_type = 9`, `n = 1`). Observed rather than
                // reasoned about: a **600 KB** input at depth 50 000 overflows the main thread's
                // 8 MiB stack and aborts — `fatal runtime error: stack overflow`. A stack overflow
                // is SIGSEGV/abort, the same **uncatchable** class as the `reserve_for`
                // amplification this parser was hardened against; no `catch_unwind` sees it.
                //
                // The bound is a refusal rather than a depth cap because **upstream GGUF does not
                // support nested arrays at all** (`llama.cpp`'s reader has no case for it), so no
                // real model file contains one: a cap would preserve a shape nothing produces,
                // while a refusal removes the recursion outright. The array itself (depth 1) is
                // ordinary and stays — `tokenizer.ggml.tokens` is one.
                if elem_type == 9 {
                    return Err(GgufError::NestedArray { at: self.i });
                }
                let n = self.u64()?;
                // Smallest element on the wire is 1 byte (u8/i8/bool), so `remaining` is the ceiling.
                let mut values = Vec::with_capacity(self.reserve_for(n, 1));
                for _ in 0..n {
                    values.push(self.value(elem_type)?);
                }
                GgufValue::Array(GgufArray { elem_type, values })
            }
            10 => GgufValue::U64(self.u64()?),
            11 => GgufValue::I64(self.u64()? as i64),
            12 => GgufValue::F64(f64::from_bits(self.u64()?)),
            other => return Err(GgufError::BadValueType(other)),
        })
    }
}

/// The metadata half of a GGUF: everything but the tensor data (audit H6).
#[derive(Debug, Clone)]
pub struct GgufMeta {
    pub version: u32,
    pub alignment: u64,
    pub metadata: Vec<(String, GgufValue)>,
    pub tensors: Vec<TensorInfo>,
    pub data_start: u64,
}

impl GgufMeta {
    /// The architecture string (`general.architecture`), if present.
    pub fn architecture(&self) -> Option<&str> {
        match self.metadata.iter().find(|(k, _)| k == "general.architecture") {
            Some((_, GgufValue::Str(s))) => Some(s.as_str()),
            _ => None,
        }
    }
}

impl Gguf {
    /// Parse a whole GGUF file from memory.
    /// **Audit H6 — parse the metadata region only, without copying the tensor data.**
    ///
    /// [`Gguf::parse`] ends with `bytes[data_start..].to_vec()`, which is fine for the offline
    /// splitter (it wants the data) and impossible on a worker's load path, where a shard is
    /// gigabytes. That is a large part of *why* the worker never ran this parser and handed the
    /// path straight to `llama.cpp` instead — the hardened reader was shaped for the wrong caller.
    ///
    /// This entry point parses the header, the KV block and the tensor-info table — **the region
    /// every GGUF parsing defect lives in** — from a bounded prefix, and validates each tensor's
    /// extent against `file_len` (the real length on disk, which the caller knows and the prefix
    /// does not). It copies nothing.
    ///
    /// `bytes` may be a prefix of the file; `file_len` must be the whole file's length.
    pub fn parse_metadata(bytes: &[u8], file_len: u64) -> R<GgufMeta> {
        let (metadata, tensors, data_start, version, alignment) = Self::parse_head(bytes)?;
        if data_start > file_len {
            return Err(GgufError::Truncated { at: data_start as usize, need: 0, have: 0 });
        }
        // Every tensor must lie inside the file. The splitter's `tensor_bytes` checks this against
        // an in-memory copy; on the load path the file itself is the bound, and a tensor table
        // that points past the end is exactly the shape that makes a mapping parser read garbage.
        for t in &tensors {
            let len = t.byte_len()?;
            let end = data_start
                .checked_add(t.offset)
                .and_then(|o| o.checked_add(len))
                .ok_or_else(|| GgufError::ShapeOverflow { name: t.name.clone(), what: "data_start + offset + length" })?;
            if end > file_len {
                return Err(GgufError::TensorOutOfRange(t.name.clone()));
            }
        }
        Ok(GgufMeta { version, alignment, metadata, tensors, data_start })
    }

    /// Shared head parse for [`Gguf::parse`] and [`Gguf::parse_metadata`].
    #[allow(clippy::type_complexity)]
    fn parse_head(bytes: &[u8]) -> R<(Vec<(String, GgufValue)>, Vec<TensorInfo>, u64, u32, u64)> {
        let mut c = Cursor { b: bytes, i: 0 };
        if c.take(4)? != MAGIC {
            return Err(GgufError::BadMagic);
        }
        let version = c.u32()?;
        if version != 2 && version != 3 {
            return Err(GgufError::BadVersion(version));
        }
        let tensor_count = c.u64()?;
        let kv_count = c.u64()?;

        // A metadata entry is at least a u64 key length + a u32 value type + 1 byte of value.
        let mut metadata = Vec::with_capacity(c.reserve_for(kv_count, 13));
        for _ in 0..kv_count {
            let key = c.string()?;
            let ty = c.u32()?;
            let val = c.value(ty)?;
            metadata.push((key, val));
        }

        // A tensor info is at least a u64 name length + a u32 n_dims + a u32 type + a u64 offset.
        let mut tensors = Vec::with_capacity(c.reserve_for(tensor_count, 24));
        for _ in 0..tensor_count {
            let name = c.string()?;
            let n_dims = c.u32()?;
            let mut dims = Vec::with_capacity(c.reserve_for(n_dims as u64, 8));
            for _ in 0..n_dims {
                dims.push(c.u64()?);
            }
            let ggml_type = c.u32()?;
            let offset = c.u64()?;
            tensors.push(TensorInfo { name, dims, ggml_type, offset });
        }

        let alignment = metadata
            .iter()
            .find(|(k, _)| k == "general.alignment")
            .map(|(_, v)| match v {
                GgufValue::U32(a) => *a as u64,
                GgufValue::U64(a) => *a,
                _ => DEFAULT_ALIGNMENT,
            })
            .unwrap_or(DEFAULT_ALIGNMENT);

        // The data section begins at the next `alignment` boundary after the tensor infos.
        let data_start_u64 = align_up(c.i as u64, alignment);
        Ok((metadata, tensors, data_start_u64, version, alignment))
    }

    pub fn parse(bytes: &[u8]) -> R<Gguf> {
        let (metadata, tensors, data_start_u64, version, alignment) = Self::parse_head(bytes)?;
        let c_i = data_start_u64 as usize;
        // Compare in u64 first: on any target, narrowing a saturated value to `usize` before the
        // bounds check is how an out-of-range offset becomes an in-range one.
        if data_start_u64 > bytes.len() as u64 {
            return Err(GgufError::Truncated {
                at: c_i.min(bytes.len()),
                need: usize::MAX,
                have: bytes.len().saturating_sub(c_i.min(bytes.len())),
            });
        }
        let data_start = data_start_u64 as usize;
        let data = bytes[data_start..].to_vec();

        Ok(Gguf { version, alignment, metadata, tensors, data })
    }

    /// The raw bytes of one tensor (from its offset + exact byte length).
    pub fn tensor_bytes(&self, t: &TensorInfo) -> R<&[u8]> {
        // `offset + len` is computed on `u64` with a checked add BEFORE either is narrowed to
        // `usize`: on a 32-bit target the cast alone would truncate, and on any target the add can
        // wrap a hostile offset back into a range that looks valid.
        let len = t.byte_len()?;
        let end = t
            .offset
            .checked_add(len)
            .ok_or_else(|| GgufError::ShapeOverflow { name: t.name.clone(), what: "offset + length" })?;
        if end > self.data.len() as u64 {
            return Err(GgufError::TensorOutOfRange(t.name.clone()));
        }
        Ok(&self.data[t.offset as usize..end as usize])
    }

    /// A metadata value by key.
    pub fn meta(&self, key: &str) -> Option<&GgufValue> {
        self.metadata.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// The architecture string (`general.architecture`), if present.
    pub fn architecture(&self) -> Option<&str> {
        match self.meta("general.architecture") {
            Some(GgufValue::Str(s)) => Some(s.as_str()),
            _ => None,
        }
    }
}

// ------------------------------- writing -------------------------------

/// A deterministic GGUF builder: given metadata (in a fixed order) and a set of (info, bytes)
/// tensors, it lays them out with aligned offsets and serializes to bytes. Same inputs ⇒ identical
/// output (the splitter's determinism rests on this).
pub struct GgufWriter {
    pub version: u32,
    pub alignment: u64,
    pub metadata: Vec<(String, GgufValue)>,
    /// (name, dims, ggml_type, data). Written in the order given.
    pub tensors: Vec<(String, Vec<u64>, u32, Vec<u8>)>,
}

impl GgufWriter {
    /// Serialize to a complete GGUF byte vector.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        put_u32(&mut out, self.version);
        put_u64(&mut out, self.tensors.len() as u64);
        put_u64(&mut out, self.metadata.len() as u64);
        for (k, v) in &self.metadata {
            put_string(&mut out, k);
            put_value(&mut out, v);
        }
        // Tensor infos need offsets; compute them first (aligned, in tensor order).
        let mut offsets = Vec::with_capacity(self.tensors.len());
        let mut cursor = 0u64;
        for (_, _, _, data) in &self.tensors {
            cursor = align_up(cursor, self.alignment);
            offsets.push(cursor);
            cursor += data.len() as u64;
        }
        for ((name, dims, ty, _), off) in self.tensors.iter().zip(&offsets) {
            put_string(&mut out, name);
            put_u32(&mut out, dims.len() as u32);
            for d in dims {
                put_u64(&mut out, *d);
            }
            put_u32(&mut out, *ty);
            put_u64(&mut out, *off);
        }
        // Pad to alignment, then the data section (each tensor at its aligned offset).
        pad_to(&mut out, self.alignment);
        let data_start = out.len() as u64;
        for ((_, _, _, data), off) in self.tensors.iter().zip(&offsets) {
            let want = data_start + off;
            while (out.len() as u64) < want {
                out.push(0);
            }
            out.extend_from_slice(data);
        }
        out
    }
}

/// The ggml block layout for a tensor type: `(elements_per_block, bytes_per_block)`.
/// Covers the F16/F32 dev types and the Q4_K_M-family reference quants (BLUEPRINT §1.7).
pub fn ggml_type_block(ty: u32) -> R<(u64, usize)> {
    // QK_K = 256 for the k-quants.
    Ok(match ty {
        0 => (1, 4),      // F32
        1 => (1, 2),      // F16
        2 => (32, 18),    // Q4_0
        3 => (32, 20),    // Q4_1
        6 => (32, 22),    // Q5_0
        7 => (32, 24),    // Q5_1
        8 => (32, 34),    // Q8_0
        9 => (32, 36),    // Q8_1
        10 => (256, 84),  // Q2_K
        11 => (256, 110), // Q3_K
        12 => (256, 144), // Q4_K
        13 => (256, 176), // Q5_K
        14 => (256, 210), // Q6_K
        15 => (256, 292), // Q8_K
        28 => (1, 8),     // I64 (rare, metadata tensors)
        30 => (1, 2),     // BF16
        other => return Err(GgufError::BadTensorType(other)),
    })
}

/// Round `x` up to a multiple of `a`, saturating instead of overflowing.
///
/// `a` comes from the file's own `general.alignment` metadata, so it is attacker-controlled: `a = 0`
/// is a division hazard (handled) and a large `a` makes `div_ceil(a) * a` overflow. Saturating is
/// the right behaviour here rather than an error, because the caller immediately compares the result
/// against the buffer length and reports a truncation — `u64::MAX` fails that comparison, which is
/// exactly the outcome a hostile alignment deserves.
fn align_up(x: u64, a: u64) -> u64 {
    if a == 0 {
        x
    } else {
        x.div_ceil(a).saturating_mul(a)
    }
}
fn pad_to(out: &mut Vec<u8>, a: u64) {
    let target = align_up(out.len() as u64, a) as usize;
    out.resize(target, 0);
}
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_string(out: &mut Vec<u8>, s: &str) {
    put_u64(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}
fn put_value(out: &mut Vec<u8>, v: &GgufValue) {
    // Writes the type-id then the payload (mirrors the reader).
    let ty = value_type_id(v);
    // Note: metadata KVs carry their own type-id inline (the reader read it before dispatching);
    // callers that write a bare value inside an array must NOT re-emit the type — see put_value_bare.
    put_u32(out, ty);
    put_value_bare(out, v);
}
fn put_value_bare(out: &mut Vec<u8>, v: &GgufValue) {
    match v {
        GgufValue::U8(x) => out.push(*x),
        GgufValue::I8(x) => out.push(*x as u8),
        GgufValue::U16(x) => out.extend_from_slice(&x.to_le_bytes()),
        GgufValue::I16(x) => out.extend_from_slice(&x.to_le_bytes()),
        GgufValue::U32(x) => put_u32(out, *x),
        GgufValue::I32(x) => put_u32(out, *x as u32),
        GgufValue::F32(x) => put_u32(out, x.to_bits()),
        GgufValue::Bool(x) => out.push(*x as u8),
        GgufValue::Str(s) => put_string(out, s),
        GgufValue::U64(x) => put_u64(out, *x),
        GgufValue::I64(x) => put_u64(out, *x as u64),
        GgufValue::F64(x) => put_u64(out, x.to_bits()),
        GgufValue::Array(a) => {
            put_u32(out, a.elem_type);
            put_u64(out, a.values.len() as u64);
            for e in &a.values {
                put_value_bare(out, e);
            }
        }
    }
}
fn value_type_id(v: &GgufValue) -> u32 {
    match v {
        GgufValue::U8(_) => 0,
        GgufValue::I8(_) => 1,
        GgufValue::U16(_) => 2,
        GgufValue::I16(_) => 3,
        GgufValue::U32(_) => 4,
        GgufValue::I32(_) => 5,
        GgufValue::F32(_) => 6,
        GgufValue::Bool(_) => 7,
        GgufValue::Str(_) => 8,
        GgufValue::Array(_) => 9,
        GgufValue::U64(_) => 10,
        GgufValue::I64(_) => 11,
        GgufValue::F64(_) => 12,
    }
}

/// The per-layer index parsed from a tensor name like `blk.14.attn_q.weight` → `Some(14)`.
/// Non-block tensors (`token_embd.weight`, `output.weight`, `output_norm.weight`, …) → `None`.
pub fn tensor_layer(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("blk.")?;
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    num.parse().ok()
}

/// Group tensor names by layer for a quick shape summary (used in tests/CLI).
pub fn layers_present(g: &Gguf) -> BTreeMap<u32, usize> {
    let mut m = BTreeMap::new();
    for t in &g.tensors {
        if let Some(l) = tensor_layer(&t.name) {
            *m.entry(l).or_insert(0) += 1;
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_round_trips_a_small_synthetic_gguf() {
        // Build a tiny GGUF in-memory, write it, parse it back, and confirm structure + tensor bytes.
        let w = GgufWriter {
            version: 3,
            alignment: 32,
            metadata: vec![
                ("general.architecture".into(), GgufValue::Str("qwen2".into())),
                ("qwen2.block_count".into(), GgufValue::U32(2)),
                ("general.alignment".into(), GgufValue::U32(32)),
            ],
            tensors: vec![
                ("token_embd.weight".into(), vec![4, 2], 0, vec![1u8; 32]), // F32, 8 elems * 4 = 32B
                ("blk.0.attn_q.weight".into(), vec![2, 2], 1, vec![2u8; 8]), // F16, 4 elems * 2 = 8B
            ],
        };
        let bytes = w.to_bytes();
        let g = Gguf::parse(&bytes).unwrap();
        assert_eq!(g.version, 3);
        assert_eq!(g.alignment, 32);
        assert_eq!(g.architecture(), Some("qwen2"));
        assert_eq!(g.tensors.len(), 2);
        assert_eq!(g.tensor_bytes(&g.tensors[0]).unwrap(), &[1u8; 32]);
        assert_eq!(g.tensor_bytes(&g.tensors[1]).unwrap(), &[2u8; 8]);
        assert_eq!(tensor_layer(&g.tensors[1].name), Some(0));
        assert_eq!(tensor_layer(&g.tensors[0].name), None);
    }

    #[test]
    fn writer_is_deterministic() {
        let mk = || GgufWriter {
            version: 3,
            alignment: 32,
            metadata: vec![("general.architecture".into(), GgufValue::Str("llama".into()))],
            tensors: vec![("blk.5.ffn_up.weight".into(), vec![3], 1, vec![7u8; 6])],
        };
        assert_eq!(mk().to_bytes(), mk().to_bytes(), "same inputs ⇒ byte-identical GGUF");
    }
}
