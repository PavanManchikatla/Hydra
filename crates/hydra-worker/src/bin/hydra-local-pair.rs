//! `hydra-local-pair` — the dev two-worker runner (BLUEPRINT M2 sub-slice B).
//!
//! Mints a cluster CA, provisions **two real `hydra-worker` OS processes** (S1 = layers `[0,k)`,
//! S2 = layers `[k,end)`) over TCP+mTLS on localhost — the same code path as multi-machine — and
//! drives a teacher-forced NO_SAMPLE prefill through them, checking the split pipeline reproduces
//! the unsplit model's final logits **bit-exactly**. Then it exercises the kill-switch: `kill -9`
//! S2, restart it, and reconnect. The `hydra-worker` binary is located next to this one.
//!
//! Usage: `hydra-local-pair [prompt]`  (model via `HYDRA_TEST_MODEL` or the default dev GGUF).

use hydra_engine_sys::Model;
use hydra_worker::bootstrap::Bootstrap;
use hydra_worker::pair::{dev_model_path, golden_digest, run_teacher_forced_pipeline, Cluster, Endpoints, SubprocessWorker};
use hydra_worker::wire::SessionKeys;
use hydra_worker::worker::WorkerConfig;

fn der_vec(c: &hydra_transport::CertificateDer<'static>) -> Vec<u8> {
    c.as_ref().to_vec()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = std::env::args().nth(1).unwrap_or_else(|| "The capital of France is".to_string());

    let Some(model_path) = dev_model_path() else {
        eprintln!("hydra-local-pair: no engine/model (dev-environment artifacts); nothing to run.");
        return Ok(());
    };

    // Golden + tokenization (loaded once, freed before the workers come up to bound peak memory).
    let (tokens, golden, n_layer) = {
        let model = Model::load(&model_path, 0)?;
        let tokens = model.tokenize(&prompt)?.into_iter().map(|t| t as u32).collect::<Vec<_>>();
        let golden = golden_digest(&model, &tokens)?;
        (tokens, golden, model.n_layer())
    };
    let k = (n_layer / 2).max(1);
    println!("prompt={prompt:?}  tokens={}  split k={k}/{n_layer}", tokens.len());

    let cluster = Cluster::new()?;
    let s1_id = cluster.issue("worker-s1")?;
    let s2_id = cluster.issue("worker-s2")?;
    let ca_der = der_vec(&cluster.ca.ca_cert_der());
    let keys = SessionKeys::dev(0xB2);

    let binary = std::env::current_exe()?
        .parent()
        .map(|d| d.join("hydra-worker"))
        .ok_or("cannot locate sibling hydra-worker binary")?
        .to_string_lossy()
        .into_owned();

    let boot = |id: &hydra_transport::DeviceIdentity, cfg: WorkerConfig| Bootstrap {
        listen_addr: "127.0.0.1:0".to_string(),
        device_name: id.name.clone(),
        ca_cert_der: ca_der.clone(),
        cert_chain_der: id.cert_chain.iter().map(der_vec).collect(),
        key_pkcs8_der: id.key_pkcs8_der(),
        cfg,
        forwarding: None,
    };
    let base = |rank, lf, ll, is_final, receives_tokens| WorkerConfig {
        keys: keys.clone(),
        rank,
        layer_first: lf,
        layer_last: ll,
        is_final,
        receives_tokens,
        epoch: 0,
        recovery_id: 0,
        model_path: Some(model_path.clone()),
        n_gpu_layers: 0,
        n_ctx: tokens.len() as i32 + 8,
        sampler_config: None,
        recovery_start: false,
    };

    let s1 = SubprocessWorker::spawn(&binary, &boot(&s1_id, base(0, 0, k, false, true)))?;
    let mut s2 = SubprocessWorker::spawn(&binary, &boot(&s2_id, base(1, k, -1, true, false)))?;
    println!("S1 pid-endpoint {}  S2 pid-endpoint {}", s1.addr, s2.addr);

    let connector = cluster.coordinator_connector()?;
    let ep = Endpoints::new(s1.addr, "worker-s1", s2.addr, "worker-s2");
    let digest = run_teacher_forced_pipeline(&connector, &ep, &keys, &tokens)
        .await
        .map_err(|e| format!("pipeline: {e}"))?;
    let exact = digest == golden;
    println!("teacher-forced NO_SAMPLE split-vs-unsplit: {}", if exact { "BIT-EXACT ✅" } else { "MISMATCH ❌" });

    // Kill-switch demonstration: kill -9 S2, restart, and prove the fresh process re-serves over
    // mTLS with a control-plane round-trip through its stage SM. (Full D1 recovery replay — resuming
    // the *stateful* prefill without duplicated/missing tokens — is a later slice; this establishes
    // the kill-switch the DoD runs against.)
    println!("kill -9 S2 ({}) ...", s2.addr);
    s2.kill9()?;
    s2.restart()?;
    println!("S2 restarted at {}, control-plane re-serve check ...", s2.addr);
    let mut c = connector.connect(s2.addr, "worker-s2").await?;
    let tuple = hydra_state::ActivationTuple {
        kind: hydra_state::ActivationKind::Initial,
        epoch: 0,
        recovery_id: 0,
        attempt: 0,
        sampler_checkpoint_id: 0,
    };
    c.send(0, &hydra_worker::wire::encode_commit_activation(&keys, &tuple, 1)).await?;
    let reply = c.recv().await?;
    let re_serves = matches!(
        hydra_worker::wire::decode(&reply.payload, &keys).map(|(_, m)| m),
        Ok(hydra_worker::wire::Msg::ActivationCommitted(_))
    );
    println!("restarted S2 re-serves: {}", if re_serves { "✅" } else { "❌" });

    if !exact {
        return Err("split pipeline did not reproduce unsplit logits bit-exactly".into());
    }
    Ok(())
}
