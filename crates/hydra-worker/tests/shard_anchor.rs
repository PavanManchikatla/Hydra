//! P2·10b — **the real DoD: the rule-14 bit-exact anchor, green with shard-loaded weights.**
//!
//! `local_pair.rs::two_worker_teacher_forced_no_sample_bit_exact` is the standing regression anchor
//! (standing rule 14): two workers as real TCP+mTLS endpoints, a teacher-forced NO_SAMPLE prefill,
//! the boundary residual serialized → transmitted → injected, and S2's final unsampled logits equal
//! to the unsplit model's **bit-exactly** (BLAKE3 digest equality).
//!
//! This file runs that **same anchor, unchanged in every respect except one**: each worker loads
//! **its own per-stage shard file** (verified against the Ed25519-signed manifest) instead of the
//! whole model. Passing means shard-load is **semantically invisible** — the memory saving costs
//! nothing. That is binding point 3, and it is the gate the caveat removal depends on.
//!
//! The refusal tests below are binding point 1: a worker that cannot prove what weights it is about
//! to run must refuse, with a structured error and no fallback.
//!
//! Skips cleanly without the engine/model/shards (dev-environment artifacts). To produce them:
//! ```text
//! cargo run --release -p hydra-modelsvc --bin hydra-modelsvc -- \
//!     split models/qwen2.5-0.5b-instruct-fp16.gguf models/shards2 --stages 0-12,12-24
//! ```

use hydra_worker::pair::{dev_model_path, golden_digest, run_teacher_forced_pipeline, Cluster};
use hydra_worker::shard::{verify_shard, ShardRefused};
use hydra_worker::wire::SessionKeys;
use hydra_worker::worker::WorkerConfig;

/// `(stage0_shard, stage1_shard, manifest)` for the 2-stage `[0,12)` / `[12,24)` dev split.
fn shard_set() -> Option<(String, String, String)> {
    let dir = std::env::var("HYDRA_TEST_SHARDS")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../models/shards2").to_string());
    let s0 = format!("{dir}/qwen2-stage0-L0_12.gguf");
    let s1 = format!("{dir}/qwen2-stage1-L12_24.gguf");
    let mf = format!("{dir}/qwen2.manifest");
    [&s0, &s1, &mf].iter().all(|p| std::path::Path::new(p).exists()).then_some((s0, s1, mf))
}

#[tokio::test]
async fn two_worker_anchor_is_bit_exact_with_shard_loaded_weights() {
    let (Some(full), Some((s0, s1, manifest))) = (dev_model_path(), shard_set()) else {
        eprintln!("SKIP: no engine/model/shards (dev-environment artifacts)");
        return;
    };

    // The golden is the UNSPLIT full model, exactly as the rule-14 anchor computes it. The model is
    // freed before the workers load, bounding peak memory on the 8 GB dev box.
    let (tokens, golden, n_layer) = {
        let model = hydra_engine_sys::Model::load(&full, 0).expect("load model");
        let tokens: Vec<u32> =
            model.tokenize("The capital of France is").expect("tokenize").into_iter().map(|t| t as u32).collect();
        assert!(tokens.len() >= 2);
        let golden = golden_digest(&model, &tokens).expect("golden");
        (tokens, golden, model.n_layer())
    };
    let k = (n_layer / 2).max(1);
    assert_eq!(k, 12, "the committed shard fixture is the 24-layer dev model split at 12");
    let keys = SessionKeys::dev(0xB2);
    let n_ctx = tokens.len() as i32 + 8;

    let cluster = Cluster::new().unwrap();
    let s1_id = cluster.issue("worker-s1").unwrap();
    let s2_id = cluster.issue("worker-s2").unwrap();

    // The ONLY difference from the standing anchor: each worker points at its own shard file plus
    // the signed manifest. Same ranks, same layer ranges, same everything else.
    let s1_cfg = WorkerConfig {
        keys: keys.clone(), rank: 0, layer_first: 0, layer_last: k, is_final: false,
        receives_tokens: true, epoch: 0, recovery_id: 0, model_path: Some(s0.clone()), n_gpu_layers: 0, n_ctx,
        sampler_config: None,
        recovery_start: false, shard_manifest: Some(manifest.clone()),
    };
    let s2_cfg = WorkerConfig {
        keys: keys.clone(), rank: 1, layer_first: k, layer_last: -1, is_final: true,
        receives_tokens: false, epoch: 0, recovery_id: 0, model_path: Some(s1.clone()), n_gpu_layers: 0, n_ctx,
        sampler_config: None,
        recovery_start: false, shard_manifest: Some(manifest.clone()),
    };
    let s1_addr = hydra_worker::pair::spawn_endpoint(s1_cfg, cluster.ca.server_config(&s1_id).unwrap());
    let s2_addr = hydra_worker::pair::spawn_endpoint(s2_cfg, cluster.ca.server_config(&s2_id).unwrap());

    let connector = cluster.coordinator_connector().unwrap();
    let ep = hydra_worker::pair::Endpoints::new(s1_addr, "worker-s1", s2_addr, "worker-s2");
    let digest = run_teacher_forced_pipeline(&connector, &ep, &keys, &tokens).await.expect("pipeline");

    assert_eq!(
        digest, golden,
        "the rule-14 anchor must stay bit-exact with SHARD-LOADED weights — each worker loaded only \
         its own layers' weights from its own shard file (k={k}/{n_layer}, {} tokens). Shard-load is \
         a memory optimization and must be semantically invisible.",
        tokens.len()
    );
}

// ----------------------------- refusal: binding point 1 -----------------------------

#[test]
fn a_shard_whose_bytes_do_not_match_the_manifest_is_refused() {
    let Some((s0, _s1, manifest)) = shard_set() else {
        eprintln!("SKIP: no shards (dev artifact)");
        return;
    };
    // Copy the stage-0 shard under its own name, flip one byte deep in the weights, and verify.
    let dir = std::env::temp_dir().join("hydra-p210b-tamper");
    std::fs::create_dir_all(&dir).unwrap();
    let tampered = dir.join("qwen2-stage0-L0_12.gguf");
    let mut bytes = std::fs::read(&s0).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&tampered, &bytes).unwrap();

    let err = verify_shard(&manifest, tampered.to_str().unwrap(), 0, 12).unwrap_err();
    assert!(
        matches!(err, ShardRefused::Blake3Mismatch { .. }),
        "a tampered shard must be REFUSED on its BLAKE3, got: {err}"
    );
    let _ = std::fs::remove_file(&tampered);
}

#[test]
fn a_tampered_manifest_is_refused_on_its_signature() {
    let Some((s0, _s1, manifest)) = shard_set() else {
        eprintln!("SKIP: no shards (dev artifact)");
        return;
    };
    // Flip a byte in the SIGNED region of the manifest. The signature must catch it before any of
    // the manifest's own claims (file names, hashes, ranges) are believed.
    let dir = std::env::temp_dir().join("hydra-p210b-tamper");
    std::fs::create_dir_all(&dir).unwrap();
    let bad = dir.join("qwen2.tampered.manifest");
    let mut raw = std::fs::read(&manifest).unwrap();
    let mid = raw.len() / 2;
    raw[mid] ^= 0x01;
    std::fs::write(&bad, &raw).unwrap();

    let err = verify_shard(bad.to_str().unwrap(), &s0, 0, 12).unwrap_err();
    assert!(
        matches!(err, ShardRefused::Signature { .. } | ShardRefused::ManifestMalformed { .. }),
        "a tampered manifest must be REFUSED (signature or structure), got: {err}"
    );
    let _ = std::fs::remove_file(&bad);
}

#[test]
fn a_shard_for_the_wrong_stage_is_refused() {
    let Some((s0, _s1, manifest)) = shard_set() else {
        eprintln!("SKIP: no shards (dev artifact)");
        return;
    };
    // The stage-0 shard is genuine and its manifest verifies — but this worker was configured to be
    // stage 1. Placement and configuration disagree; never silently follow one of them.
    let err = verify_shard(&manifest, &s0, 12, 24).unwrap_err();
    assert!(
        matches!(err, ShardRefused::RangeMismatch { have_first: 0, have_last: 12, want_first: 12, want_last: 24, .. }),
        "a genuine shard for the WRONG stage must be REFUSED, got: {err}"
    );
}

#[test]
fn an_unlisted_shard_file_is_refused() {
    let Some((s0, _s1, manifest)) = shard_set() else {
        eprintln!("SKIP: no shards (dev artifact)");
        return;
    };
    // Same genuine bytes, a name the manifest never signed. Content-addressing is only as good as
    // the binding between name and hash, so an unlisted name is a refusal, not a lookup miss.
    let dir = std::env::temp_dir().join("hydra-p210b-tamper");
    std::fs::create_dir_all(&dir).unwrap();
    let renamed = dir.join("qwen2-stage9-L0_12.gguf");
    std::fs::copy(&s0, &renamed).unwrap();

    let err = verify_shard(&manifest, renamed.to_str().unwrap(), 0, 12).unwrap_err();
    assert!(
        matches!(err, ShardRefused::NotInManifest { .. }),
        "a shard file the manifest does not list must be REFUSED, got: {err}"
    );
    let _ = std::fs::remove_file(&renamed);
}

#[test]
fn a_verified_shard_reports_the_manifests_layer_range() {
    let Some((s0, s1, manifest)) = shard_set() else {
        eprintln!("SKIP: no shards (dev artifact)");
        return;
    };
    // The manifest's layer-range map — not the worker's config — is what drives the load window.
    let v0 = verify_shard(&manifest, &s0, 0, 12).expect("stage-0 shard verifies");
    assert_eq!((v0.layer_first, v0.layer_last, v0.n_layer_total), (0, 12, 24));
    // `-1` in the config means "to the model's last layer" and must resolve against the manifest.
    let v1 = verify_shard(&manifest, &s1, 12, -1).expect("stage-1 shard verifies with layer_last=-1");
    assert_eq!((v1.layer_first, v1.layer_last, v1.n_layer_total), (12, 24, 24));
}
