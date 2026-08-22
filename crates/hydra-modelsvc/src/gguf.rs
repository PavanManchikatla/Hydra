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
    /// Number of elements (product of dims).
    pub fn n_elements(&self) -> u64 {
        self.dims.iter().product::<u64>().max(if self.dims.is_empty() { 0 } else { 1 })
    }
    /// Exact byte length of this tensor's data (from its ggml type's block layout).
    pub fn byte_len(&self) -> R<u64> {
        let (block_elems, block_bytes) = ggml_type_block(self.ggml_type)?;
        let n = self.n_elements();
        // Quantized types pack `block_elems` elements per `block_bytes`; f32/f16 are 1-per-N.
        Ok((n / block_elems) * block_bytes as u64)
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
        let n = self.u64()? as usize;
        let bytes = self.take(n)?;
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
                let n = self.u64()? as usize;
                let mut values = Vec::with_capacity(n);
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

impl Gguf {
    /// Parse a whole GGUF file from memory.
    pub fn parse(bytes: &[u8]) -> R<Gguf> {
        let mut c = Cursor { b: bytes, i: 0 };
        if c.take(4)? != MAGIC {
            return Err(GgufError::BadMagic);
        }
        let version = c.u32()?;
        if version != 2 && version != 3 {
            return Err(GgufError::BadVersion(version));
        }
        let tensor_count = c.u64()? as usize;
        let kv_count = c.u64()? as usize;

        let mut metadata = Vec::with_capacity(kv_count);
        for _ in 0..kv_count {
            let key = c.string()?;
            let ty = c.u32()?;
            let val = c.value(ty)?;
            metadata.push((key, val));
        }

        let mut tensors = Vec::with_capacity(tensor_count);
        for _ in 0..tensor_count {
            let name = c.string()?;
            let n_dims = c.u32()? as usize;
            let mut dims = Vec::with_capacity(n_dims);
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
        let data_start = align_up(c.i as u64, alignment) as usize;
        if data_start > bytes.len() {
            return Err(GgufError::Truncated { at: c.i, need: data_start - c.i, have: bytes.len() - c.i });
        }
        let data = bytes[data_start..].to_vec();

        Ok(Gguf { version, alignment, metadata, tensors, data })
    }

    /// The raw bytes of one tensor (from its offset + exact byte length).
    pub fn tensor_bytes(&self, t: &TensorInfo) -> R<&[u8]> {
        let start = t.offset as usize;
        let len = t.byte_len()? as usize;
        self.data.get(start..start + len).ok_or_else(|| GgufError::TensorOutOfRange(t.name.clone()))
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

fn align_up(x: u64, a: u64) -> u64 {
    if a == 0 {
        x
    } else {
        x.div_ceil(a) * a
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
