//! **Rule 27's first product oracle: the SHIPPED `hydra-coordinator` binary produces tokens, and
//! they are the pair driver's tokens byte for byte (2026-09-02, §7.76).**
//!
//! Two in-process stages (the rule-14 harness shape, `pair::spawn_endpoint`) are provisioned from a
//! paired directory; the real coordinator PROCESS is started against them; a prompt is POSTed over
//! TLS with the minted token; the SSE stream is read to completion. The reference is
//! `hydra_worker::pair::run_generation` — the driver the rule-14 anchor already proves equal to the
//! unsplit model's argmax — run on a SECOND pair of identical stages, detokenized with the same
//! tokenizer. Byte-identical text, dense SSE ids, and the ledger on disk holding exactly the
//! sampled positions once.
//!
//! Engine-gated: CI status is *unavailable*, not green. What it cannot see: a coordinator crash
//! (§7.75 — escalated) and any topology but the coordinator-relayed two-stage one.

mod common;
use common::*;

use hydra_wire::SessionFence;
use hydra_worker::pair::{dev_model_path, run_generation, Cluster, Endpoints};
use hydra_worker::sampler::SamplingConfig;
use hydra_worker::worker::WorkerConfig;

fn stage_cfg(fence: &SessionFence, path: &str, k: i32, n_ctx: i32, rank: u16) -> WorkerConfig {
    let is_final = rank == 1;
    WorkerConfig {
        fence: fence.clone(),
        rank: rank as hydra_state::StageRank,
        layer_first: if is_final { k } else { 0 },
        layer_last: if is_final { -1 } else { k },
        is_final,
        receives_tokens: !is_final,
        epoch: 0,
        recovery_id: 0,
        model_path: Some(path.to_string()),
        n_gpu_layers: 0,
        n_ctx,
        sampler_config: if is_final { Some(SamplingConfig::greedy()) } else { None },
        recovery_start: false,
        shard_manifest: None,
    }
}

#[test]
fn the_shipped_binary_generates_the_pair_drivers_tokens_byte_for_byte() {
    let Some(model) = dev_model_path() else {
        eprintln!("SKIP: no engine/model — the product generation path is engine-gated (CI status: unavailable, not green)");
        return;
    };
    let n_layer = hydra_engine_sys::Model::load(&model, 0).expect("model").n_layer();
    let k = (n_layer / 2).max(1);
    let n_ctx = 128;
    let max_tokens = 6usize;

    // ---- pair + provision (split stated explicitly; addresses filled in after the stages bind) ----
    let dir = tempfile::tempdir().unwrap();
    let (ca, token, files) = pair_and_provision(dir.path(), &model, ["127.0.0.1:1".parse().unwrap(), "127.0.0.1:2".parse().unwrap()], k);
    let fence = files.fence.clone();

    // ---- the stages the BINARY will drive: identities from the paired CA, the provisioned fence ----
    let s1_id = ca.issue("worker-s1").unwrap();
    let s2_id = ca.issue("worker-s2").unwrap();
    let s1 = hydra_worker::pair::spawn_endpoint(stage_cfg(&fence, &model, k, n_ctx, 0), ca.server_config(&s1_id).unwrap());
    let s2 = hydra_worker::pair::spawn_endpoint(stage_cfg(&fence, &model, k, n_ctx, 1), ca.server_config(&s2_id).unwrap());

    // ---- the REFERENCE: the pair driver on a second, identical pair of stages ----
    let ref_cluster = Cluster::new().unwrap();
    let r1_id = ref_cluster.issue("worker-s1").unwrap();
    let r2_id = ref_cluster.issue("worker-s2").unwrap();
    let r1 = hydra_worker::pair::spawn_endpoint(stage_cfg(&fence, &model, k, n_ctx, 0), ref_cluster.ca.server_config(&r1_id).unwrap());
    let r2 = hydra_worker::pair::spawn_endpoint(stage_cfg(&fence, &model, k, n_ctx, 1), ref_cluster.ca.server_config(&r2_id).unwrap());

    // The prompt the binary will see, tokenized exactly as its session factory tokenizes it.
    let tokenizer = hydra_tokenizer::Tokenizer::load_vocab_only(&model).expect("tokenizer");
    let admission = hydra_tokenizer::admission::Admission::compute(&tokenizer, hydra_tokenizer::admission::ChatTemplate::ChatMl, &[hydra_tokenizer::admission::ChatMessage::new("user", "hello")]).expect("admission");
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let golden: Vec<u32> = rt.block_on(async {
        let connector = ref_cluster.coordinator_connector().unwrap();
        run_generation(&connector, &Endpoints::new(r1, "worker-s1", r2, "worker-s2"), &fence, &SamplingConfig::greedy(), &admission.prompt_tokens, max_tokens).await.expect("reference generation")
    });
    let golden_text = String::from_utf8_lossy(&tokenizer.decode_bytes(&golden).expect("detok")).into_owned();
    assert!(!golden.is_empty());

    // ---- the PRODUCT: the real binary, its stage table overriding the placeholder addresses ----
    let port = free_port();
    let data = dir.path().join("data");
    let stages = format!("worker-s1={s1},worker-s2={s2}");
    let (_proc, rx) = spawn_coordinator(
        &["--pairing-dir", dir.path().to_str().unwrap(), "--api-addr", &format!("127.0.0.1:{port}"), "--data-dir", data.to_str().unwrap(), "--stages", &stages, "--max-tokens", &max_tokens.to_string()],
        &[],
    );
    assert!(wait_listening(&rx, 30), "the binary never reported listening");

    let body = r#"{"model":"m","messages":[{"role":"user","content":"hello"}],"stream":true}"#;
    let (status, resp) = https(port, &ca.ca_cert_der(), &[("Authorization", &format!("Bearer {token}"))], body, 180);
    assert!(status.contains(" 200 "), "expected 200, got {status} / {resp}");
    let events = parse_sse(&resp);
    assert!(!events.is_empty(), "the shipped binary produced NO tokens (the stub shape) — body: {resp}");
    let ids: Vec<u64> = events.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, (1..=ids.len() as u64).collect::<Vec<_>>(), "SSE ids must be dense and stable");
    let text: String = events.iter().map(|(_, d)| d.as_str()).collect();
    assert_eq!(text, golden_text, "the binary's stream must be the pair driver's tokens byte for byte");

    // ---- disk truth: the ledger holds the prompt and each sampled position exactly once ----
    let ledger = hydra_coordinator::recovery::read(data.join("commits.wal")).expect("the ledger reads back");
    assert_eq!(ledger.prompt_tokens, admission.prompt_tokens, "INITIAL_COMMIT carries the real prompt tokens");
    assert_eq!(ledger.generated_token_ids(), golden, "every sampled token is durable, once, in order");
}
