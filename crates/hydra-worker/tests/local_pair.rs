//! M2 sub-slice B — **two-worker `--local-pair` pipeline**.
//!
//! 1. `two_worker_teacher_forced_no_sample_bit_exact` (engine-gated): the regression anchor. Two
//!    workers as real TCP+mTLS endpoints (S1 = layers `[0,k)`, S2 = `[k,end)`); a teacher-forced
//!    NO_SAMPLE prefill streams through them with the boundary residual **serialized as a `FWD`
//!    tensor, transmitted over mTLS, and injected** into S2. S2's final unsampled logits must match
//!    the unsplit model's **bit-exactly** (BLAKE3 digest equality). Skips without the engine/model.
//! 2. `subprocess_worker_survives_kill_9_and_restart` (no engine needed): the dev kill-switch. A
//!    **real `hydra-worker` OS process** is `kill -9`'d and restarted; a control-plane activation
//!    round-trips through its stage SM before and after. Runs everywhere (control-plane only).

use hydra_worker::pair::{dev_model_path, golden_digest, run_teacher_forced_pipeline, Cluster, SubprocessWorker};
use hydra_worker::wire::{self, Msg, SessionKeys};
use hydra_worker::worker::WorkerConfig;
use hydra_worker::Bootstrap;

fn commit_tuple() -> hydra_state::ActivationTuple {
    hydra_state::ActivationTuple {
        kind: hydra_state::ActivationKind::Initial,
        epoch: 0,
        recovery_id: 0,
        attempt: 0,
        sampler_checkpoint_id: 0,
    }
}

#[tokio::test]
async fn two_worker_teacher_forced_no_sample_bit_exact() {
    let Some(path) = dev_model_path() else {
        eprintln!("SKIP: no engine/model (dev-environment artifacts)");
        return;
    };

    // Golden (unsplit, per-position batching) + tokenization; the model is freed before the workers
    // load, bounding peak memory on the 8 GB dev box.
    let (tokens, golden, n_layer) = {
        let model = hydra_engine_sys::Model::load(&path, 0).expect("load model");
        let tokens: Vec<u32> = model.tokenize("The capital of France is").expect("tokenize").into_iter().map(|t| t as u32).collect();
        assert!(tokens.len() >= 2);
        let golden = golden_digest(&model, &tokens).expect("golden");
        (tokens, golden, model.n_layer())
    };
    let k = (n_layer / 2).max(1);
    let keys = SessionKeys::dev(0xB2);
    let n_ctx = tokens.len() as i32 + 8;

    let cluster = Cluster::new().unwrap();
    let s1_id = cluster.issue("worker-s1").unwrap();
    let s2_id = cluster.issue("worker-s2").unwrap();

    let s1_cfg = WorkerConfig {
        keys: keys.clone(), rank: 0, layer_first: 0, layer_last: k, is_final: false,
        receives_tokens: true, epoch: 0, recovery_id: 0, model_path: Some(path.clone()), n_gpu_layers: 0, n_ctx,
        sampler_config: None,
        recovery_start: false,
    };
    let s2_cfg = WorkerConfig {
        keys: keys.clone(), rank: 1, layer_first: k, layer_last: -1, is_final: true,
        receives_tokens: false, epoch: 0, recovery_id: 0, model_path: Some(path.clone()), n_gpu_layers: 0, n_ctx,
        sampler_config: None,
        recovery_start: false,
    };
    let s1_addr = hydra_worker::pair::spawn_endpoint(s1_cfg, cluster.ca.server_config(&s1_id).unwrap());
    let s2_addr = hydra_worker::pair::spawn_endpoint(s2_cfg, cluster.ca.server_config(&s2_id).unwrap());

    let connector = cluster.coordinator_connector().unwrap();
    let ep = hydra_worker::pair::Endpoints::new(s1_addr, "worker-s1", s2_addr, "worker-s2");
    let digest = run_teacher_forced_pipeline(&connector, &ep, &keys, &tokens).await.expect("pipeline");

    assert_eq!(
        digest, golden,
        "two-worker split pipeline must reproduce the unsplit final logits bit-exactly \
         (boundary serialized/transmitted/injected over mTLS; k={k}/{n_layer}, {} tokens)",
        tokens.len()
    );
}

#[tokio::test]
async fn direct_worker_to_worker_fwd_is_bit_exact() {
    // The bit-exact anchor via WORKER→WORKER direct FWD (spec §4): the coordinator talks only to S1;
    // S1 forwards each boundary straight to S2 (never relayed through the coordinator). S2's final
    // logits must still match the unsplit model bit-exactly — proving direct FWD replaces the
    // coordinator-relay interim without changing semantics.
    let Some(path) = dev_model_path() else {
        eprintln!("SKIP: no engine/model (dev-environment artifacts)");
        return;
    };
    let (tokens, golden, n_layer) = {
        let model = hydra_engine_sys::Model::load(&path, 0).expect("load model");
        let tokens: Vec<u32> = model.tokenize("The capital of France is").expect("tokenize").into_iter().map(|t| t as u32).collect();
        let golden = golden_digest(&model, &tokens).expect("golden");
        (tokens, golden, model.n_layer())
    };
    let k = (n_layer / 2).max(1);
    let keys = SessionKeys::dev(0xFD);
    let n_ctx = tokens.len() as i32 + 8;

    let cluster = Cluster::new().unwrap();
    let s1_id = cluster.issue("worker-s1").unwrap();
    let s2_id = cluster.issue("worker-s2").unwrap();
    let s1_client = cluster.issue("worker-s1-client").unwrap();

    let mk = |rank, lf, ll, is_final, toks| WorkerConfig {
        keys: keys.clone(), rank, layer_first: lf, layer_last: ll, is_final,
        receives_tokens: toks, epoch: 0, recovery_id: 0, model_path: Some(path.clone()), n_gpu_layers: 0, n_ctx,
        sampler_config: None, recovery_start: false,
    };
    // S2 first (S1 connects to it at startup).
    let s2_addr = hydra_worker::pair::spawn_endpoint(mk(1, k, -1, true, false), cluster.ca.server_config(&s2_id).unwrap());
    let s1_down = hydra_transport::tcp_mtls::TcpMtls::from_config(cluster.ca.client_config(&s1_client).unwrap()).unwrap();
    let s1_addr = hydra_worker::pair::spawn_forwarding_endpoint(
        mk(0, 0, k, false, true), cluster.ca.server_config(&s1_id).unwrap(), s1_down, s2_addr, "worker-s2".to_string(),
    );

    let connector = cluster.coordinator_connector().unwrap();
    let digest = hydra_worker::pair::run_direct_fwd_pipeline(&connector, s1_addr, "worker-s1", &keys, &tokens).await.expect("direct-fwd pipeline");
    assert_eq!(digest, golden, "worker→worker direct FWD must reproduce the unsplit logits bit-exactly (k={k}/{n_layer})");
}

#[tokio::test]
async fn subprocess_worker_survives_kill_9_and_restart() {
    let binary = env!("CARGO_BIN_EXE_hydra-worker");
    let cluster = Cluster::new().unwrap();
    let worker_id = cluster.issue("worker-1").unwrap();
    let keys = SessionKeys::dev(0x5b);

    let boot = Bootstrap {
        listen_addr: "127.0.0.1:0".to_string(),
        device_name: "worker-1".to_string(),
        ca_cert_der: cluster.ca.ca_cert_der().as_ref().to_vec(),
        cert_chain_der: worker_id.cert_chain.iter().map(|c| c.as_ref().to_vec()).collect(),
        key_pkcs8_der: worker_id.key_pkcs8_der(),
        // Control-plane only (no model) → the kill-switch is exercised everywhere, incl. CI.
        cfg: WorkerConfig {
            keys: keys.clone(), rank: 0, layer_first: 0, layer_last: -1, is_final: true,
            receives_tokens: true, epoch: 0, recovery_id: 0, model_path: None, n_gpu_layers: 0, n_ctx: 64,
            sampler_config: None,
            recovery_start: false,
        },
    };

    let mut proc = SubprocessWorker::spawn(binary, &boot).expect("spawn worker process");
    let connector = cluster.coordinator_connector().unwrap();

    // Before the kill: control-plane activation round-trips through the real stage SM.
    let mut c = connector.connect(proc.addr, "worker-1").await.expect("connect pre-kill");
    c.send(0, &wire::encode_commit_activation(&keys, &commit_tuple(), 1)).await.unwrap();
    let reply = c.recv().await.unwrap();
    assert!(
        matches!(wire::decode(&reply.payload, &keys).unwrap().1, Msg::ActivationCommitted(_)),
        "worker serves before kill"
    );
    drop(c);

    // kill -9 (SIGKILL) and restart the real process.
    proc.kill9().expect("kill -9");
    proc.restart().expect("restart");

    // After the restart: the fresh process re-serves over a new mTLS connection.
    let mut c = connector.connect(proc.addr, "worker-1").await.expect("connect post-restart");
    c.send(0, &wire::encode_commit_activation(&keys, &commit_tuple(), 1)).await.unwrap();
    let reply = c.recv().await.unwrap();
    assert!(
        matches!(wire::decode(&reply.payload, &keys).unwrap().1, Msg::ActivationCommitted(_)),
        "restarted worker re-serves"
    );
}
