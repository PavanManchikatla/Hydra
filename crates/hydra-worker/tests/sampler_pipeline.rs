//! M2 slice 3 — **sampler @ S_P** determinism, over the two-worker pipeline (spec §2.6a/§2.6b;
//! invariants I14/I15/I17/I19-producer). All engine-gated (skip cleanly without the model).
//!
//! (a) `greedy_sample_across_pipeline_matches_unsplit_argmax` — the anchor extended one step:
//!     greedy decoding after a teacher-forced prefill is bit-exact vs the unsplit model's argmax.
//! (b) `seeded_sampling_is_reproducible_across_two_full_runs` — same seed/config/prompt ⇒ identical
//!     token sequence across two independent runs of the two-process pipeline (Philox determinism).
//! (c) `duplicate_sample_next_is_idempotent_and_does_not_advance_rng` — a repeated `SAMPLE_NEXT` is
//!     served from the SAMPLED cache byte-for-byte; the RNG never re-advances (I14 retention half).
//! (d) `install_sampler_checkpoint_round_trips_over_mtls` — INSTALL → INSTALLED at S_P (I17).

use hydra_worker::pair::{
    dev_model_path, golden_next_token, run_generation, sample_next_twice, Cluster, Endpoints,
};
use hydra_worker::sampler::{initial_checkpoint_bytes, SamplingConfig};
use hydra_worker::wire::{self, Msg, SessionKeys};
use hydra_worker::worker::{WorkerConfig, INITIAL_CHECKPOINT_ID};

const S1: &str = "worker-s1";
const S2: &str = "worker-s2";

struct Pipe {
    cluster: Cluster,
    ep: Endpoints,
    keys: SessionKeys,
}

/// Stand up a two-worker pipeline (S1 embeddings `[0,k)`, S2 logits+sampler `[k,end)`) with the
/// given sampler config on S2.
fn spin_up(path: &str, n_layer: i32, n_ctx: i32, seed_byte: u8, sampler: SamplingConfig) -> Pipe {
    let keys = SessionKeys::dev(seed_byte);
    let cluster = Cluster::new().unwrap();
    let s1_id = cluster.issue(S1).unwrap();
    let s2_id = cluster.issue(S2).unwrap();
    let k = (n_layer / 2).max(1);
    let s1_cfg = WorkerConfig {
        keys: keys.clone(), rank: 0, layer_first: 0, layer_last: k, is_final: false,
        receives_tokens: true, epoch: 0, recovery_id: 0, model_path: Some(path.to_string()),
        n_gpu_layers: 0, n_ctx, sampler_config: None,
    };
    let s2_cfg = WorkerConfig {
        keys: keys.clone(), rank: 1, layer_first: k, layer_last: -1, is_final: true,
        receives_tokens: false, epoch: 0, recovery_id: 0, model_path: Some(path.to_string()),
        n_gpu_layers: 0, n_ctx, sampler_config: Some(sampler),
    };
    let s1_addr = hydra_worker::pair::spawn_endpoint(s1_cfg, cluster.ca.server_config(&s1_id).unwrap());
    let s2_addr = hydra_worker::pair::spawn_endpoint(s2_cfg, cluster.ca.server_config(&s2_id).unwrap());
    Pipe { cluster, ep: Endpoints::new(s1_addr, S1, s2_addr, S2), keys }
}

fn setup() -> Option<(String, Vec<u32>, i32)> {
    let path = dev_model_path()?;
    let model = hydra_engine_sys::Model::load(&path, 0).ok()?;
    let tokens: Vec<u32> = model.tokenize("The capital of France is").ok()?.into_iter().map(|t| t as u32).collect();
    let n_layer = model.n_layer();
    Some((path, tokens, n_layer))
}

#[tokio::test]
async fn greedy_sample_across_pipeline_matches_unsplit_argmax() {
    let Some((path, tokens, n_layer)) = setup() else {
        eprintln!("SKIP: no engine/model");
        return;
    };
    // Golden argmax from the unsplit model (model freed before the workers load).
    let golden = {
        let model = hydra_engine_sys::Model::load(&path, 0).unwrap();
        golden_next_token(&model, &tokens).unwrap()
    };
    let n_ctx = tokens.len() as i32 + 8;
    let pipe = spin_up(&path, n_layer, n_ctx, 0xC1, SamplingConfig::greedy());
    let connector = pipe.cluster.coordinator_connector().unwrap();
    let seq = run_generation(&connector, &pipe.ep, &pipe.keys, &SamplingConfig::greedy(), &tokens, 1)
        .await
        .expect("generation");
    assert_eq!(seq, vec![golden], "greedy sampling across the pipeline must equal the unsplit argmax (bit-exact)");
}

#[tokio::test]
async fn seeded_sampling_is_reproducible_across_two_full_runs() {
    let Some((path, tokens, n_layer)) = setup() else {
        eprintln!("SKIP: no engine/model");
        return;
    };
    let n_ctx = tokens.len() as i32 + 16;
    let cfg = SamplingConfig { temperature: 0.8, top_p: 0.95, repeat_penalty: 1.1, penalty_last_n: 16, seed: 0xDEADBEEF };

    // Two fully independent runs (fresh workers, fresh samplers) with the same seed/config/prompt.
    let a = {
        let p = spin_up(&path, n_layer, n_ctx, 0xA1, cfg.clone());
        let conn = p.cluster.coordinator_connector().unwrap();
        run_generation(&conn, &p.ep, &p.keys, &cfg, &tokens, 8).await.expect("run a")
    };
    let b = {
        let p = spin_up(&path, n_layer, n_ctx, 0xA2, cfg.clone());
        let conn = p.cluster.coordinator_connector().unwrap();
        run_generation(&conn, &p.ep, &p.keys, &cfg, &tokens, 8).await.expect("run b")
    };
    assert_eq!(a.len(), 8);
    assert_eq!(a, b, "same seed/config/prompt must yield an identical token sequence across two runs");
}

#[tokio::test]
async fn duplicate_sample_next_is_idempotent_and_does_not_advance_rng() {
    let Some((path, tokens, n_layer)) = setup() else {
        eprintln!("SKIP: no engine/model");
        return;
    };
    let n_ctx = tokens.len() as i32 + 8;
    // Stochastic config: if the RNG advanced, a re-sample would (almost surely) differ.
    let cfg = SamplingConfig { temperature: 1.0, top_p: 1.0, repeat_penalty: 1.0, penalty_last_n: 0, seed: 7 };
    let pipe = spin_up(&path, n_layer, n_ctx, 0xC3, cfg.clone());
    let connector = pipe.cluster.coordinator_connector().unwrap();

    let (first, second) = sample_next_twice(&connector, &pipe.ep, &pipe.keys, &cfg, &tokens)
        .await
        .expect("sample twice");
    match (first, second) {
        (
            Msg::Sampled { token_id: t1, post_sample_snapshot: s1, .. },
            Msg::Sampled { token_id: t2, post_sample_snapshot: s2, .. },
        ) => {
            assert_eq!(t1, t2, "duplicate SAMPLE_NEXT returns the same token (I14)");
            assert_eq!(s1, s2, "duplicate returns the identical snapshot — RNG did not re-advance");
        }
        other => panic!("expected two SAMPLED, got {other:?}"),
    }
}

#[tokio::test]
async fn install_sampler_checkpoint_round_trips_over_mtls() {
    let Some((path, tokens, n_layer)) = setup() else {
        eprintln!("SKIP: no engine/model");
        return;
    };
    let n_ctx = tokens.len() as i32 + 8;
    let cfg = SamplingConfig { temperature: 0.7, top_p: 0.9, repeat_penalty: 1.05, penalty_last_n: 8, seed: 42 };
    let pipe = spin_up(&path, n_layer, n_ctx, 0xC4, cfg.clone());
    let connector = pipe.cluster.coordinator_connector().unwrap();

    // The coordinator constructs ONLY the config-defined initial checkpoint (spec §1.4) and installs
    // it into S_P; the worker acks the exact checkpoint (I17).
    let snapshot = initial_checkpoint_bytes(INITIAL_CHECKPOINT_ID, &cfg);
    let mut c2 = connector.connect(pipe.ep.s2_addr, S2).await.expect("connect s2");
    c2.send(0, &wire::encode_install_sampler_checkpoint(&pipe.keys, 0, INITIAL_CHECKPOINT_ID, &snapshot)).await.unwrap();
    let reply = c2.recv().await.unwrap();
    match wire::decode(&reply.payload, &pipe.keys).unwrap().1 {
        Msg::SamplerCheckpointInstalled { checkpoint_id, .. } => {
            assert_eq!(checkpoint_id, INITIAL_CHECKPOINT_ID, "installed the exact checkpoint (I17)");
        }
        other => panic!("expected SAMPLER_CHECKPOINT_INSTALLED, got {other:?}"),
    }
    let _ = tokens; // prompt not needed here
}
