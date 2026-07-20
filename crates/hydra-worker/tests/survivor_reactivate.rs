//! §7.19 — reproduce + fix the **active-sampler survivor re-activation** hang (worker/orchestration
//! layer per the owner ruling). A survivor S_P that was `ACTIVE_FINAL` with a live sampler must, on a
//! Case-A freeze, be re-installable + re-activatable under the recovery epoch (the spec's survivor
//! freeze is load-bearing — I10b/I7b/I15). This is **control-plane only** (a sampler is built without
//! the engine — `Sampler::initial` needs no model), so the exact §7.19 interleaving runs in CI.
//!
//! The first test is diagnostic: each recovery step is timeout-wrapped so a hang points at the layer
//! instead of blocking. The second loops the full freeze→reinstall→reactivate ~100× (§7.19 (a)).

use std::net::SocketAddr;
use std::time::Duration;

use hydra_state::{ActivationKind, ActivationTuple};
use hydra_transport::tcp_mtls::{ClientConn, TcpMtls};
use hydra_transport::ClusterCa;
use hydra_worker::pair::spawn_multiconn_endpoint;
use hydra_worker::sampler::{initial_checkpoint_bytes, SamplingConfig};
use hydra_worker::wire::{self, Msg, SessionKeys};
use hydra_worker::worker::{WorkerConfig, INITIAL_CHECKPOINT_ID};

const SP: &str = "s_p";
const COORD: &str = "coordinator";

fn greedy() -> SamplingConfig {
    SamplingConfig { temperature: 0.0, top_p: 1.0, repeat_penalty: 1.0, penalty_last_n: 0, seed: 5 }
}

/// A final sampler stage with NO engine (control-plane): `is_final` + `sampler_config` gives a real
/// `Sampler` (built without a model), so the freeze/install/activate control path is exercised exactly
/// as on a real S_P survivor — without needing the engine.
fn sp_cfg(keys: SessionKeys) -> WorkerConfig {
    WorkerConfig {
        keys, rank: 2, layer_first: 0, layer_last: -1, is_final: true, receives_tokens: false,
        epoch: 0, recovery_id: 0, model_path: None, n_gpu_layers: 0, n_ctx: 64,
        sampler_config: Some(greedy()), recovery_start: false,
    }
}

fn tuple(kind: ActivationKind, epoch: u32, rid: u32) -> ActivationTuple {
    ActivationTuple { kind, epoch, recovery_id: rid, attempt: 0, sampler_checkpoint_id: if matches!(kind, ActivationKind::Recovery) { INITIAL_CHECKPOINT_ID } else { 0 } }
}

async fn send_recv(conn: &mut ClientConn, keys: &SessionKeys, frame: Vec<u8>, label: &str) -> Msg {
    conn.send(0, &frame).await.unwrap_or_else(|e| panic!("send {label}: {e}"));
    let f = tokio::time::timeout(Duration::from_secs(8), conn.recv())
        .await
        .unwrap_or_else(|_| panic!("§7.19 HANG localized at step: {label} (recv timed out 8s)"))
        .unwrap_or_else(|e| panic!("recv {label}: {e}"));
    wire::decode(&f.payload, keys).unwrap_or_else(|e| panic!("decode {label}: {e}")).1
}

async fn activate(conn: &mut ClientConn, keys: &SessionKeys, kind: ActivationKind, epoch: u32, rid: u32) {
    let t = tuple(kind, epoch, rid);
    match send_recv(conn, keys, wire::encode_commit_activation(keys, &t, 1), "COMMIT_ACTIVATION").await {
        Msg::ActivationCommitted(_) => {}
        o => panic!("expected ACTIVATION_COMMITTED, got {o:?}"),
    }
    match send_recv(conn, keys, wire::encode_finalize_activation(keys, &t, 1), "FINALIZE_ACTIVATION").await {
        Msg::ActivationFinalized => {}
        o => panic!("expected ACTIVATION_FINALIZED, got {o:?}"),
    }
}

fn setup() -> (SocketAddr, TcpMtls, SessionKeys) {
    let ca = ClusterCa::new().unwrap();
    let sp_id = ca.issue(SP).unwrap();
    let coord_id = ca.issue(COORD).unwrap();
    let keys = SessionKeys::dev(0x79);
    let addr = spawn_multiconn_endpoint(sp_cfg(keys.clone()), ca.server_config(&sp_id).unwrap());
    let connector = TcpMtls::from_config(ca.client_config(&coord_id).unwrap()).unwrap();
    (addr, connector, keys)
}

/// DIAGNOSTIC: an active-sampler survivor S_P, then freeze → reinstall → reactivate, each step
/// timeout-wrapped so a hang names the exact call (rule-10 trace). This is the §7.19 reproduction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_survivor_freeze_reinstall_reactivate_converges() {
    let (addr, connector, keys) = setup();
    let mut c = connector.connect(addr, SP).await.unwrap();

    // Bring the survivor to ACTIVE_FINAL with a live sampler (the state a real S_P is in at kill time).
    activate(&mut c, &keys, ActivationKind::Initial, 0, 0).await;

    // Freeze (Case A): ACTIVE_FINAL → FROZEN. truncate_to is the durable frontier.
    match send_recv(&mut c, &keys, wire::encode_begin_recovery(&keys, 0, 1, 1, 0), "BEGIN_RECOVERY").await {
        Msg::RecoveryAck { .. } => {}
        o => panic!("expected RECOVERY_ACK, got {o:?}"),
    }
    // ROOT-CAUSE FIX (§7.19): a Case-A freeze leaves the stage in FROZEN; `RecvCommit` is only
    // accepted in FROZEN_READY (stage.rs). The survivor must **catch up to its durable frontier**
    // (RebuildStep → FROZEN_READY) before re-activation — even though its KV is intact, so the
    // catch-up re-applies nothing (applied ≥ goal → immediate FROZEN_READY). Skipping this is what
    // hung COMMIT_ACTIVATION forever. The SM is correct (M1-proven); the orchestration omitted it.
    match send_recv(&mut c, &keys, wire::encode_catch_up_context(&keys, 0, 1, 0), "CATCH_UP_CONTEXT").await {
        Msg::CatchUpReady { .. } => {}
        o => panic!("expected CATCH_UP_READY, got {o:?}"),
    }
    // Re-install the sampler checkpoint (I17: install before activation).
    match send_recv(&mut c, &keys, wire::encode_install_sampler_checkpoint(&keys, 0, INITIAL_CHECKPOINT_ID, &initial_checkpoint_bytes(INITIAL_CHECKPOINT_ID, &greedy())), "INSTALL_SAMPLER_CHECKPOINT").await {
        Msg::SamplerCheckpointInstalled { .. } => {}
        o => panic!("expected SAMPLER_CHECKPOINT_INSTALLED, got {o:?}"),
    }
    // Re-activate under the recovery epoch.
    activate(&mut c, &keys, ActivationKind::Recovery, 1, 1).await;
}

/// §7.19 regression (a): the full freeze → reinstall → reactivate cycle converges **~100×** on the
/// same live survivor (each cycle advances the recovery epoch), proving the hang is gone and does not
/// reappear under repetition.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_survivor_reactivation_converges_100x() {
    let (addr, connector, keys) = setup();
    let mut c = connector.connect(addr, SP).await.unwrap();
    activate(&mut c, &keys, ActivationKind::Initial, 0, 0).await;

    // Each cycle advances the epoch: the survivor is ACTIVE_FINAL at epoch i-1, so the next Case-A
    // freeze is base=i-1 → target=i (Case A requires self.epoch == base).
    for i in 1..=100u32 {
        match send_recv(&mut c, &keys, wire::encode_begin_recovery(&keys, i - 1, i, i, 0), "BEGIN_RECOVERY").await {
            Msg::RecoveryAck { .. } => {}
            o => panic!("iter {i}: expected RECOVERY_ACK, got {o:?}"),
        }
        // Catch up to FROZEN_READY before re-activation (the §7.19 fix — see the diagnostic).
        match send_recv(&mut c, &keys, wire::encode_catch_up_context(&keys, i, i, 0), "CATCH_UP").await {
            Msg::CatchUpReady { .. } => {}
            o => panic!("iter {i}: expected CATCH_UP_READY, got {o:?}"),
        }
        match send_recv(&mut c, &keys, wire::encode_install_sampler_checkpoint(&keys, 0, INITIAL_CHECKPOINT_ID, &initial_checkpoint_bytes(INITIAL_CHECKPOINT_ID, &greedy())), "INSTALL").await {
            Msg::SamplerCheckpointInstalled { .. } => {}
            o => panic!("iter {i}: expected INSTALLED, got {o:?}"),
        }
        activate(&mut c, &keys, ActivationKind::Recovery, i, i).await;
    }
    eprintln!("§7.19 (a): active-survivor freeze→reinstall→reactivate converged 100×");
}
