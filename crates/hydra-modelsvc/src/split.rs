//! The splitter: partition a GGUF's tensors by layer range into per-stage shard GGUFs, and emit the
//! signed manifest. **Deterministic by construction** — tensor order is inherited from the source
//! GGUF, offsets are computed by [`crate::gguf::GgufWriter`], and the manifest is canonical + Ed25519
//! (itself deterministic), so the same GGUF + key ⇒ byte-identical shards and manifest, twice.

use ring::signature::Ed25519KeyPair;

use crate::gguf::{tensor_layer, Gguf, GgufValue, GgufWriter};
use crate::manifest::{chat_template_hash, inference_config_hash, tokenizer_hash, Manifest, ShardEntry};

#[derive(Debug, thiserror::Error)]
pub enum SplitError {
    #[error("gguf: {0}")]
    Gguf(#[from] crate::gguf::GgufError),
    #[error("empty stage range list")]
    NoStages,
    #[error("stage ranges must be contiguous and cover [0,{n_layer}): got {got:?}")]
    NonContiguous { n_layer: u32, got: Vec<(u32, u32)> },
    #[error("a stage range is empty or inverted: {0:?}")]
    EmptyRange((u32, u32)),
}

type R<T> = Result<T, SplitError>;

/// One produced shard: its file name and full GGUF bytes.
pub struct Shard {
    pub file_name: String,
    pub bytes: Vec<u8>,
}

/// The result of a split: the shard files (in stage order) + the signed manifest.
pub struct SplitOutput {
    pub shards: Vec<Shard>,
    pub manifest: Manifest,
}

/// Split `g` into per-stage shards for the given contiguous `ranges` (`[first,last)` per stage,
/// together covering `[0, n_layer_total)`), signing the manifest with `keypair`.
///
/// Tensor assignment: `blk.{l}.*` → the stage whose range contains `l`; `token_embd*` → the first
/// stage (embeddings); `output*` / `output_norm*` → the last stage (final norm + lm-head); any other
/// non-block global (e.g. `rope_freqs`) → **replicated to every stage** (it is tiny and some stages
/// reference it), so no shard is ever missing a tensor its compute needs.
pub fn split(g: &Gguf, ranges: &[(u32, u32)], keypair: &Ed25519KeyPair) -> R<SplitOutput> {
    if ranges.is_empty() {
        return Err(SplitError::NoStages);
    }
    let n_layer = ranges.iter().map(|(_, b)| *b).max().unwrap_or(0);
    // Validate contiguity + coverage of [0, n_layer).
    let mut expect = 0u32;
    for &(a, b) in ranges {
        if b <= a {
            return Err(SplitError::EmptyRange((a, b)));
        }
        if a != expect {
            return Err(SplitError::NonContiguous { n_layer, got: ranges.to_vec() });
        }
        expect = b;
    }

    let arch = g.architecture().unwrap_or("").to_string();
    let first_stage = 0usize;
    let last_stage = ranges.len() - 1;

    let mut shards = Vec::with_capacity(ranges.len());
    let mut shard_entries = Vec::with_capacity(ranges.len());

    for (stage, &(first, last)) in ranges.iter().enumerate() {
        // Collect this stage's tensors, in source order (deterministic).
        let mut sel: Vec<(&str, &[u8], &[u64], u32)> = Vec::new();
        for t in &g.tensors {
            let bytes = g.tensor_bytes(t)?;
            let keep = match tensor_layer(&t.name) {
                Some(l) => l >= first && l < last,
                None => {
                    if t.name.starts_with("token_embd") {
                        stage == first_stage
                    } else if t.name == "output.weight" || t.name.starts_with("output_norm") || t.name.starts_with("output.") {
                        stage == last_stage
                    } else {
                        true // other global tensor → replicate to every stage
                    }
                }
            };
            if keep {
                sel.push((&t.name, bytes, &t.dims, t.ggml_type));
            }
        }

        // Build the shard GGUF: original metadata verbatim + the hydra.split.* role KVs.
        let mut metadata = g.metadata.clone();
        metadata.push(("hydra.split.stage".into(), GgufValue::U32(stage as u32)));
        metadata.push(("hydra.split.layer_first".into(), GgufValue::U32(first)));
        metadata.push(("hydra.split.layer_last".into(), GgufValue::U32(last)));
        metadata.push(("hydra.split.n_layer_total".into(), GgufValue::U32(n_layer)));

        let writer = GgufWriter {
            version: g.version,
            alignment: g.alignment,
            metadata,
            tensors: sel.iter().map(|(n, b, d, ty)| (n.to_string(), d.to_vec(), *ty, b.to_vec())).collect(),
        };
        let bytes = writer.to_bytes();
        let file_name = format!("{arch}-stage{stage}-L{first}_{last}.gguf");

        // Per-tensor BLAKE3 (sorted by name) + whole-shard BLAKE3.
        let mut tensors: Vec<(String, [u8; 32])> =
            sel.iter().map(|(n, b, _, _)| (n.to_string(), *blake3::hash(b).as_bytes())).collect();
        tensors.sort_by(|a, b| a.0.cmp(&b.0));
        let shard_blake3 = *blake3::hash(&bytes).as_bytes();

        shard_entries.push(ShardEntry {
            stage: stage as u32,
            layer_first: first,
            layer_last: last,
            file_name: file_name.clone(),
            shard_blake3,
            tensors,
        });
        shards.push(Shard { file_name, bytes });
    }

    let mut manifest = Manifest {
        architecture: arch,
        n_layer_total: n_layer,
        tokenizer_hash: tokenizer_hash(g),
        chat_template_hash: chat_template_hash(g),
        inference_config_hash: inference_config_hash(g),
        shards: shard_entries,
        signer_pubkey: [0; 32],
        signature: None,
    };
    manifest.sign(keypair);

    Ok(SplitOutput { shards, manifest })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gguf::GgufWriter;
    use crate::manifest::generate_keypair;

    /// A synthetic 4-layer GGUF: token_embd + blk.0..3 + output_norm + output.
    fn synthetic_model() -> Vec<u8> {
        let mut tensors: Vec<(String, Vec<u64>, u32, Vec<u8>)> = Vec::new();
        tensors.push(("token_embd.weight".into(), vec![8], 1, vec![0xAA; 16]));
        for l in 0..4u32 {
            tensors.push((format!("blk.{l}.attn_q.weight"), vec![4], 1, vec![l as u8; 8]));
            tensors.push((format!("blk.{l}.ffn_up.weight"), vec![4], 1, vec![(l + 10) as u8; 8]));
        }
        tensors.push(("output_norm.weight".into(), vec![4], 0, vec![0xBB; 16]));
        tensors.push(("output.weight".into(), vec![8], 1, vec![0xCC; 16]));
        GgufWriter {
            version: 3,
            alignment: 32,
            metadata: vec![
                ("general.architecture".into(), GgufValue::Str("llama".into())),
                ("llama.block_count".into(), GgufValue::U32(4)),
                ("tokenizer.ggml.model".into(), GgufValue::Str("gpt2".into())),
                ("tokenizer.chat_template".into(), GgufValue::Str("{{msg}}".into())),
            ],
            tensors,
        }
        .to_bytes()
    }

    #[test]
    fn split_partitions_by_layer_and_places_globals() {
        let bytes = synthetic_model();
        let g = Gguf::parse(&bytes).unwrap();
        let (_pk, kp) = generate_keypair().unwrap();
        // Two stages: [0,2) and [2,4).
        let out = split(&g, &[(0, 2), (2, 4)], &kp).unwrap();
        assert_eq!(out.shards.len(), 2);

        // Stage 0 has token_embd + blk.0/1, NOT output*.
        let s0 = Gguf::parse(&out.shards[0].bytes).unwrap();
        let n0: Vec<&str> = s0.tensors.iter().map(|t| t.name.as_str()).collect();
        assert!(n0.contains(&"token_embd.weight"));
        assert!(n0.iter().any(|n| n.starts_with("blk.0.")) && n0.iter().any(|n| n.starts_with("blk.1.")));
        assert!(!n0.iter().any(|n| n.starts_with("blk.2.")));
        assert!(!n0.contains(&"output.weight"));
        // The role metadata is present.
        assert_eq!(s0.meta("hydra.split.layer_first"), Some(&GgufValue::U32(0)));
        assert_eq!(s0.meta("hydra.split.layer_last"), Some(&GgufValue::U32(2)));

        // Stage 1 (last) has blk.2/3 + output_norm + output, NOT token_embd.
        let s1 = Gguf::parse(&out.shards[1].bytes).unwrap();
        let n1: Vec<&str> = s1.tensors.iter().map(|t| t.name.as_str()).collect();
        assert!(n1.contains(&"output.weight") && n1.contains(&"output_norm.weight"));
        assert!(!n1.contains(&"token_embd.weight"));
        assert!(n1.iter().any(|n| n.starts_with("blk.3.")));

        // Manifest: signed, verifies **against the key this test generated** (C1 — there is no
        // argument-less `verify()`; the trust anchor is always the caller's), covers both shards,
        // records the right layer ranges.
        let trusted = crate::manifest::public_key_of(&kp);
        out.manifest.verify_against(&trusted).expect("manifest verifies against the trusted signer");
        assert_eq!(out.manifest.n_layer_total, 4);
        assert_eq!(out.manifest.shards[0].layer_first, 0);
        assert_eq!(out.manifest.shards[1].layer_last, 4);

        // A shard's BLAKE3 in the manifest matches the actual shard bytes.
        assert_eq!(out.manifest.shards[0].shard_blake3, *blake3::hash(&out.shards[0].bytes).as_bytes());
    }

    #[test]
    fn split_is_deterministic_bytewise() {
        let bytes = synthetic_model();
        let g = Gguf::parse(&bytes).unwrap();
        // Same fixed key both times (Ed25519 signing is deterministic → identical manifest bytes).
        let (pk8, _) = generate_keypair().unwrap();
        let kp1 = crate::manifest::keypair_from_pkcs8(&pk8).unwrap();
        let kp2 = crate::manifest::keypair_from_pkcs8(&pk8).unwrap();
        let a = split(&g, &[(0, 2), (2, 4)], &kp1).unwrap();
        let b = split(&g, &[(0, 2), (2, 4)], &kp2).unwrap();
        assert_eq!(a.shards.len(), b.shards.len());
        for (x, y) in a.shards.iter().zip(&b.shards) {
            assert_eq!(x.file_name, y.file_name);
            assert_eq!(x.bytes, y.bytes, "shard bytes must be byte-identical across splits");
        }
        assert_eq!(a.manifest.to_bytes().unwrap(), b.manifest.to_bytes().unwrap(), "manifest byte-identical");
    }

    #[test]
    fn non_contiguous_ranges_rejected() {
        let g = Gguf::parse(&synthetic_model()).unwrap();
        let (_pk, kp) = generate_keypair().unwrap();
        assert!(matches!(split(&g, &[(0, 2), (3, 4)], &kp), Err(SplitError::NonContiguous { .. })));
    }
}
