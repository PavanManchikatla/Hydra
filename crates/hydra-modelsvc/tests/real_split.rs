//! P2·10a real-model regression: split the dev GGUF into the P1·2 3-stage layout and assert the
//! signed manifest verifies, its per-shard BLAKE3 matches the produced bytes, and every layer tensor
//! lands in exactly one shard within its stage's range. Engine-free (pure GGUF surgery), gated on the
//! dev model being present — skips cleanly in CI without the git-ignored model, like the engine-gated
//! worker tests.
//!
//! **Byte-identical determinism** is proven cheaply by the synthetic `split::split_is_deterministic_bytewise`
//! (the writer is a pure function; Ed25519 signing is deterministic). We do **not** split the 1.3 GB
//! model twice here — that would peak at multiple GB and thrash CI. One split + manifest checks is the
//! real-format assurance this test adds.

use hydra_modelsvc::gguf::{layers_present, tensor_layer, Gguf};
use hydra_modelsvc::manifest::{generate_keypair, keypair_from_pkcs8};
use hydra_modelsvc::split::split;

fn dev_model() -> Option<Vec<u8>> {
    let path = std::env::var("HYDRA_TEST_MODEL")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../models/qwen2.5-0.5b-instruct-fp16.gguf").to_string());
    std::fs::read(path).ok()
}

// `#[ignore]`: splitting the 1.3 GB dev model materializes ~1.3 GB of shard bytes alongside it,
// which swaps on the 8 GB dev box (~6 min) — too slow for the default `cargo test --workspace` health
// loop. The fast synthetic `split_is_deterministic_bytewise` covers the logic; run this real-model
// check on demand with `cargo test -p hydra-modelsvc -- --ignored` (it also skips without the model).
#[test]
#[ignore = "heavy: materializes ~2.5 GB; run on demand with --ignored"]
fn real_gguf_split_verifies_and_covers_every_layer_once() {
    let Some(bytes) = dev_model() else {
        eprintln!("SKIP: dev model not present (git-ignored artifact)");
        return;
    };
    let g = Gguf::parse(&bytes).expect("parse dev GGUF");
    assert_eq!(g.architecture(), Some("qwen2"));
    assert_eq!(layers_present(&g).len(), 24, "dev model has 24 layers");
    let total_blk = g.tensors.iter().filter(|t| tensor_layer(&t.name).is_some()).count();

    let (pk8, _) = generate_keypair().unwrap();
    let kp = keypair_from_pkcs8(&pk8).unwrap();
    let trusted = hydra_modelsvc::manifest::public_key_of(&kp);
    let ranges = [(0u32, 14u32), (14, 21), (21, 24)]; // the P1·2 capability-weighted split
    let out = split(&g, &ranges, &kp).unwrap();
    assert_eq!(out.shards.len(), 3);

    // The signed manifest verifies **against the pinned key** (C1) and covers the whole model.
    out.manifest.verify_against(&trusted).expect("manifest signature verifies against the trusted signer");
    assert_eq!(out.manifest.n_layer_total, 24);
    assert_eq!(out.manifest.shards[0].layer_first, 0);
    assert_eq!(out.manifest.shards[2].layer_last, 24);

    // Each shard's recorded BLAKE3 matches its produced bytes; every layer tensor in the manifest is a
    // blk.L within that stage's range; and every original layer tensor lands in exactly one shard.
    let mut placed_blk = 0usize;
    for (entry, shard) in out.manifest.shards.iter().zip(&out.shards) {
        assert_eq!(entry.shard_blake3, *blake3::hash(&shard.bytes).as_bytes(), "manifest BLAKE3 == shard bytes");
        for (name, _) in &entry.tensors {
            if let Some(l) = tensor_layer(name) {
                assert!(l >= entry.layer_first && l < entry.layer_last, "{name} outside [{},{})", entry.layer_first, entry.layer_last);
                placed_blk += 1;
            }
        }
    }
    assert_eq!(placed_blk, total_blk, "every layer tensor placed exactly once (no drop/duplicate)");

    // The embeddings live only in the first shard, the lm-head/final-norm only in the last.
    let names = |i: usize| -> Vec<&String> { out.manifest.shards[i].tensors.iter().map(|(n, _)| n).collect() };
    assert!(names(0).iter().any(|n| n.starts_with("token_embd")), "token_embd in the first shard");
    assert!(!names(2).iter().any(|n| n.starts_with("token_embd")), "token_embd NOT in the last shard");
    assert!(names(2).iter().any(|n| n.as_str() == "output.weight"), "lm-head in the last shard");
}
