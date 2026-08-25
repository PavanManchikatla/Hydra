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
use hydra_worker::shard::{verify_shard, ShardRefused, TrustedSigner};
use hydra_worker::wire::SessionFence;
use hydra_worker::worker::{ShardManifestConfig, WorkerConfig};

/// `(stage0_shard, stage1_shard, manifest)` for the 2-stage `[0,12)` / `[12,24)` dev split.
fn shard_set() -> Option<(String, String, String)> {
    let dir = std::env::var("HYDRA_TEST_SHARDS")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../models/shards2").to_string());
    let s0 = format!("{dir}/qwen2-stage0-L0_12.gguf");
    let s1 = format!("{dir}/qwen2-stage1-L12_24.gguf");
    let mf = format!("{dir}/qwen2.manifest");
    [&s0, &s1, &mf].iter().all(|p| std::path::Path::new(p).exists()).then_some((s0, s1, mf))
}

/// The fixture's signing key, as the cluster would have it after pairing.
///
/// **Audit C1.** Every call below must name a trust anchor; there is no argument-less verification
/// to fall back on. The fixture ships the dev signing key next to the shards, so the "cluster's
/// pinned key" here is derived from it — the same value a real provisioner would hand a worker.
fn trusted_signer() -> TrustedSigner {
    let dir = std::env::var("HYDRA_TEST_SHARDS")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../models/shards2").to_string());
    let pkcs8 = std::fs::read(format!("{dir}/qwen2.signing.pkcs8")).expect("fixture signing key");
    TrustedSigner(hydra_modelsvc::manifest::pubkey_from_pkcs8(&pkcs8).expect("fixture pubkey"))
}

/// `blake3` of the manifest file — the value the session fence tuple's `manifest_hash` must carry
/// for the **H14** identity binding to admit it.
fn manifest_hash(manifest_path: &str) -> [u8; 32] {
    *blake3::hash(&std::fs::read(manifest_path).expect("read manifest")).as_bytes()
}

/// Session fence whose fence tuple actually names this manifest (H14). Building them any other way
/// is now a refusal, which is the point: a worker's session and its weights must agree on identity.
fn keys_for(manifest_path: &str, seed: u8) -> SessionFence {
    let mut k = SessionFence::dev(seed);
    k.manifest_hash = manifest_hash(manifest_path);
    k
}

#[tokio::test]
async fn two_worker_anchor_is_bit_exact_with_shard_loaded_weights() {
    // Same shape as `single_worker.rs`'s guard: the message named the engine, the guard did not
    // check it (rule 17 — the class is audited, not just the instance that failed).
    let (Some(full), Some((s0, s1, manifest)), true) =
        (dev_model_path(), shard_set(), hydra_engine_sys::ENGINE_AVAILABLE)
    else {
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
    // H14: the fence tuple names this manifest. `SessionFence::dev(0xB2)` alone would now be
    // REFUSED — a valid signature says "one of ours", never "the one this session agreed on".
    let fence = keys_for(&manifest, 0xB2);
    let signer = trusted_signer();
    let n_ctx = tokens.len() as i32 + 8;

    let cluster = Cluster::new().unwrap();
    let s1_id = cluster.issue("worker-s1").unwrap();
    let s2_id = cluster.issue("worker-s2").unwrap();

    // The ONLY difference from the standing anchor: each worker points at its own shard file plus
    // the signed manifest. Same ranks, same layer ranges, same everything else.
    let s1_cfg = WorkerConfig {
        fence: fence.clone(), rank: 0, layer_first: 0, layer_last: k, is_final: false,
        receives_tokens: true, epoch: 0, recovery_id: 0, model_path: Some(s0.clone()), n_gpu_layers: 0, n_ctx,
        sampler_config: None,
        recovery_start: false,
        shard_manifest: Some(ShardManifestConfig { path: manifest.clone(), trusted_signer: signer.0 }),
    };
    let s2_cfg = WorkerConfig {
        fence: fence.clone(), rank: 1, layer_first: k, layer_last: -1, is_final: true,
        receives_tokens: false, epoch: 0, recovery_id: 0, model_path: Some(s1.clone()), n_gpu_layers: 0, n_ctx,
        sampler_config: None,
        recovery_start: false,
        shard_manifest: Some(ShardManifestConfig { path: manifest.clone(), trusted_signer: signer.0 }),
    };
    let s1_addr = hydra_worker::pair::spawn_endpoint(s1_cfg, cluster.ca.server_config(&s1_id).unwrap());
    let s2_addr = hydra_worker::pair::spawn_endpoint(s2_cfg, cluster.ca.server_config(&s2_id).unwrap());

    let connector = cluster.coordinator_connector().unwrap();
    let ep = hydra_worker::pair::Endpoints::new(s1_addr, "worker-s1", s2_addr, "worker-s2");
    let digest = run_teacher_forced_pipeline(&connector, &ep, &fence, &tokens).await.expect("pipeline");

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

    let err = verify_shard(&manifest, tampered.to_str().unwrap(), 0, 12, &trusted_signer(), &manifest_hash(&manifest)).unwrap_err();
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

    let err = verify_shard(bad.to_str().unwrap(), &s0, 0, 12, &trusted_signer(), &manifest_hash(bad.to_str().unwrap())).unwrap_err();
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
    let err = verify_shard(&manifest, &s0, 12, 24, &trusted_signer(), &manifest_hash(&manifest)).unwrap_err();
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

    let err = verify_shard(&manifest, renamed.to_str().unwrap(), 0, 12, &trusted_signer(), &manifest_hash(&manifest)).unwrap_err();
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
    let v0 = verify_shard(&manifest, &s0, 0, 12, &trusted_signer(), &manifest_hash(&manifest)).expect("stage-0 shard verifies");
    assert_eq!((v0.layer_first, v0.layer_last, v0.n_layer_total), (0, 12, 24));
    // `-1` in the config means "to the model's last layer" and must resolve against the manifest.
    let v1 = verify_shard(&manifest, &s1, 12, -1, &trusted_signer(), &manifest_hash(&manifest)).expect("stage-1 shard verifies with layer_last=-1");
    assert_eq!((v1.layer_first, v1.layer_last, v1.n_layer_total), (12, 24, 24));
}

// ---------------------------------------------------------------------------------------------
// Audit Wave 1 — C1 / H13 / H14 regressions.
// ---------------------------------------------------------------------------------------------

/// **C1 — a manifest signed by a key the cluster does not trust is REFUSED.**
///
/// This is the finding, stated as a test. The previous `verify()` checked the signature against
/// `manifest.signer_pubkey` — the key *carried inside the manifest*. An attacker generates a
/// keypair, re-signs a manifest describing whatever shard bytes they like, and it verifies: the
/// artifact carries its own answer key. Every later check — per-tensor BLAKE3, the layer-range map,
/// the admission hashes — then validates against the attacker's numbers and passes cleanly.
///
/// Here the manifest is **genuinely, correctly signed** — just by the wrong key. Nothing about it
/// is malformed. Only the trust anchor distinguishes it from the real one, which is exactly why an
/// anchor that comes from the artifact is not an anchor.
#[test]
fn a_manifest_signed_by_an_untrusted_key_is_refused() {
    let Some((s0, _s1, manifest)) = shard_set() else {
        eprintln!("SKIP: no shards (dev artifact)");
        return;
    };
    // Re-sign the real manifest's contents with an ATTACKER key. Structurally perfect, self-
    // consistent, and verifiable against its own embedded pubkey — the old check's happy path.
    let raw = std::fs::read(&manifest).unwrap();
    let mut m = hydra_modelsvc::manifest::Manifest::from_bytes(&raw).expect("parse fixture manifest");
    let (attacker_pkcs8, _) = hydra_modelsvc::manifest::generate_keypair().unwrap();
    let attacker = hydra_modelsvc::manifest::keypair_from_pkcs8(&attacker_pkcs8).unwrap();
    m.sign(&attacker);

    let attacker_pub = hydra_modelsvc::manifest::public_key_of(&attacker);
    // The self-attestation the old code performed still succeeds — proving the forgery is a good
    // one and that this test's setup is real, not a broken manifest that would fail anything.
    m.verify_against(&TrustedSigner(attacker_pub).0).expect("the forgery is internally consistent");

    let dir = std::env::temp_dir().join("hydra-audit-c1");
    std::fs::create_dir_all(&dir).unwrap();
    let forged = dir.join("qwen2.forged.manifest");
    let forged_bytes = m.to_bytes().unwrap();
    std::fs::write(&forged, &forged_bytes).unwrap();
    let forged_hash = *blake3::hash(&forged_bytes).as_bytes();

    // …and against the CLUSTER's key it is refused. Note the fence tuple is given the forgery's own
    // hash, so H14 cannot be what rejects it — this isolates C1.
    let err = verify_shard(forged.to_str().unwrap(), &s0, 0, 12, &trusted_signer(), &forged_hash)
        .expect_err("a manifest signed by an untrusted key must be REFUSED");
    assert!(matches!(err, ShardRefused::Signature { .. }), "expected a signature refusal, got: {err}");

    // Control: the genuine manifest, same call shape, is accepted — so the refusal above is caused
    // by the signer and not by a check that refuses everything.
    verify_shard(&manifest, &s0, 0, 12, &trusted_signer(), &manifest_hash(&manifest))
        .expect("control: the genuine, cluster-signed manifest verifies");

    let _ = std::fs::remove_file(&forged);
}

/// **H14 — a genuine, cluster-signed manifest for a DIFFERENT model is refused.**
///
/// The cluster's key legitimately signs every model it publishes, so a valid signature means "this
/// is one of ours" and never "this is the one this session agreed on". Without the identity
/// binding, an attacker who can influence which manifest file a worker reads substitutes one
/// genuine artifact for another — no forgery required.
///
/// Simulated here by presenting the real manifest against a fence tuple that names something else,
/// which is precisely the observable the worker has.
#[test]
fn a_genuine_manifest_that_the_fence_tuple_does_not_name_is_refused() {
    let Some((s0, _s1, manifest)) = shard_set() else {
        eprintln!("SKIP: no shards (dev artifact)");
        return;
    };
    let other_model = [0xAB; 32]; // some other model the cluster also signed

    let err = verify_shard(&manifest, &s0, 0, 12, &trusted_signer(), &other_model)
        .expect_err("a manifest the session's fence tuple does not name must be REFUSED");
    assert!(
        matches!(err, ShardRefused::Signature { .. }),
        "expected an identity refusal (reported through the signature/verification gate), got: {err}"
    );

    // Control: the same manifest, same key, bound to the hash the fence tuple should carry.
    verify_shard(&manifest, &s0, 0, 12, &trusted_signer(), &manifest_hash(&manifest))
        .expect("control: bound to its own hash, the genuine manifest verifies");
}

/// **H13 — the signature is checked before the structure is parsed.**
///
/// Asserted by consequence rather than by instrumentation: a manifest whose *structure* is
/// catastrophically hostile — a declared shard count of `u32::MAX` with nothing behind it — is
/// refused on the **signature**, not on a parse error, because the parser never runs. If the order
/// were reversed this would surface as `ManifestMalformed` and the attacker-directed parse would
/// already have happened.
#[test]
fn an_unsigned_but_structurally_hostile_manifest_never_reaches_the_parser() {
    let Some((s0, _s1, _mf)) = shard_set() else {
        eprintln!("SKIP: no shards (dev artifact)");
        return;
    };
    // Header magic + a plausible prefix, then an enormous declared count, then a 64-byte trailer so
    // the shape is "a manifest" rather than "obviously truncated".
    let mut raw = Vec::new();
    raw.extend_from_slice(b"HYDRAMF1");
    raw.extend_from_slice(&4u32.to_le_bytes());
    raw.extend_from_slice(b"qwen");
    raw.extend_from_slice(&24u32.to_le_bytes());
    raw.extend_from_slice(&[0u8; 32 * 4]); // three admission hashes + signer pubkey
    raw.extend_from_slice(&u32::MAX.to_le_bytes()); // n_shards — the amplification lever
    raw.extend_from_slice(&[0u8; 64]); // signature trailer

    let dir = std::env::temp_dir().join("hydra-audit-h13");
    std::fs::create_dir_all(&dir).unwrap();
    let hostile = dir.join("qwen2.hostile.manifest");
    std::fs::write(&hostile, &raw).unwrap();
    let hostile_hash = *blake3::hash(&raw).as_bytes();

    let err = verify_shard(hostile.to_str().unwrap(), &s0, 0, 12, &trusted_signer(), &hostile_hash)
        .expect_err("an unsigned manifest must be refused");
    let msg = err.to_string();
    assert!(
        matches!(err, ShardRefused::Signature { .. }),
        "H13: the refusal must come from the SIGNATURE gate, proving the parser never ran on \
         unauthenticated bytes. Got: {msg}"
    );
    let _ = std::fs::remove_file(&hostile);
}

// ---------------------------------------------------------------------------------------------
// **Audit H6 — the worker never ran the hardened GGUF parser, and the two opens were unrelated.**
//
// # Standing rule 19: what the oracle could not see, in two of the three degrees
//
// **SILENT.** No test in this project has ever driven the parser the worker actually loads
// through. `Target::Gguf` fuzzes `hydra_modelsvc::gguf` — the *offline splitter's* reader — for
// 24 CPU-hours; the worker hashed the bytes and handed the **path** to `llama.cpp`. Two different
// programs, and the receipts said "the GGUF parser" without saying which. The new
// `vendored-gguf` fuzz target found a **SIGABRT inside `gguf_init_from_file_ptr` on its first
// run** (seed 1, iteration 350) — an abort, so the shim's `catch (...)` cannot see it either.
//
// **INDISTINGUISHING.** The hash and the load are two separate `open()`s of the same *name*, and
// every test fed them a file nobody was modifying — under which "hash the bytes you load" and
// "hash some bytes, then load whatever that name resolves to now" return the same answer for
// every input the harness could produce.
// ---------------------------------------------------------------------------------------------

/// **H6 — a symlinked shard path is refused outright (`O_NOFOLLOW`).**
///
/// A symlink is a name that can point somewhere else a microsecond later, which is precisely the
/// window the hash is supposed to close.
#[test]
fn a_symlinked_shard_path_is_refused_before_it_is_hashed() {
    let Some((s0, _s1, mf)) = shard_set() else {
        eprintln!("skip: shard fixture unavailable");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join("qwen2-stage0-L0_12.gguf");
    std::os::unix::fs::symlink(&s0, &link).expect("symlink");

    let err = verify_shard(&mf, link.to_str().unwrap(), 0, 12, &trusted_signer(), &manifest_hash(&mf)).unwrap_err();
    assert!(
        matches!(err, ShardRefused::NotARegularFile { .. }),
        "a symlinked shard must be refused, not resolved: {err:?}"
    );
}

/// **H6 — the file that gets loaded must be the file that was hashed.**
///
/// The verification is performed, and *then* the file is replaced with different bytes before the
/// load — the TOCTOU the finding names. The identity check must catch it and the model must not
/// be produced.
#[test]
fn a_shard_replaced_between_verification_and_load_is_refused() {
    let Some((s0, s1, mf)) = shard_set() else {
        eprintln!("skip: shard fixture unavailable");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("qwen2-stage0-L0_12.gguf");
    std::fs::copy(&s0, &path).expect("stage the real shard");

    let v = verify_shard(&mf, path.to_str().unwrap(), 0, 12, &trusted_signer(), &manifest_hash(&mf))
        .expect("the staged copy verifies");

    // Now swap the file for a different one (the other stage's shard: real GGUF, wrong bytes).
    std::fs::remove_file(&path).unwrap();
    std::fs::copy(&s1, &path).expect("swap in different bytes");

    match hydra_worker::shard::load_verified_shard(&v, 0) {
        Err(ShardRefused::ChangedUnderneath { .. }) => {}
        Err(other) => panic!("expected ChangedUnderneath, got {other:?}"),
        Ok(_) => panic!(
            "loading after a swap was ACCEPTED — the verification proved nothing about the bytes \
             that got mapped, which is the TOCTOU H6 names"
        ),
    }
}

/// **H6 — the hardened parser now runs on the worker, and it refuses what it should.**
///
/// The control matters as much as the refusal: a real shard must still parse, or the pre-flight
/// would simply be an outage.
#[test]
fn the_hardened_parser_runs_on_the_load_path_and_refuses_a_hostile_metadata_region() {
    use hydra_modelsvc::gguf::Gguf;

    let Some((s0, _s1, _mf)) = shard_set() else {
        eprintln!("skip: shard fixture unavailable");
        return;
    };
    // Control: a real shard's metadata region parses from a bounded prefix, against the real size.
    let len = std::fs::metadata(&s0).unwrap().len();
    let mut prefix = vec![0u8; (64 * 1024 * 1024u64).min(len) as usize];
    {
        use std::io::Read;
        let mut f = std::fs::File::open(&s0).unwrap();
        let n = f.read(&mut prefix).unwrap();
        prefix.truncate(n);
    }
    let meta = Gguf::parse_metadata(&prefix, len).expect("a real shard's metadata parses from a prefix");
    assert!(!meta.tensors.is_empty(), "and it actually found the tensor table");
    assert_eq!(meta.architecture(), Some("qwen2"));

    // The case that aborts the VENDORED parser (hydra-fuzz seed 1, iteration 350) is refused by
    // the hardened one — cheaply, with a structured error, and without touching the engine.
    let mut rng = hydra_fuzz::Rng::for_case(1, 350);
    let hostile = hydra_fuzz::gen::gguf_case(&mut rng);
    let out = Gguf::parse_metadata(&hostile, hostile.len() as u64);
    assert!(
        out.is_err(),
        "the hardened parser must refuse the case that SIGABRTs llama.cpp's parser — that refusal \
         is what keeps the vendored parser from ever seeing it"
    );

    // A tensor table pointing past the end of the file is refused against the REAL length, which
    // a prefix alone could never establish.
    assert!(
        Gguf::parse_metadata(&prefix, 1024).is_err(),
        "a tensor extent beyond the file length must be refused"
    );
}
