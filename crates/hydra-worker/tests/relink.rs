//! P1·1a seam 2 — the **direct-FWD recovery re-link** mechanism.
//!
//! In a direct-FWD pipeline the survivor forwards each boundary straight to its downstream peer. When
//! that peer is killed and the coordinator brings up a replacement (rebuilt from the durable
//! `BoundaryStore`) and updates the shared [`DownTarget`], the survivor must re-link its down-link to
//! the replacement — preserving its own KV and its upstream connection. This test drives the re-link
//! **primitive** ([`forward_with_relink`]) directly, engine-free, so it runs everywhere in CI; the
//! full engine-gated direct-FWD kill/recover/resume (byte-identical) is the P1·1a real-pair seam.

use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hydra_state::{ActivationKind, ActivationTuple};
use hydra_transport::tcp_mtls::TcpMtls;
use hydra_transport::ClusterCa;
use hydra_worker::pair::spawn_endpoint;
use hydra_worker::wire::{self, Msg, SessionKeys};
use hydra_worker::worker::{forward_with_relink, DownTarget, WorkerConfig};

const SP_NAME: &str = "s_p";
const COORD_NAME: &str = "coordinator";

fn control_plane_cfg(keys: SessionKeys) -> WorkerConfig {
    WorkerConfig {
        keys,
        rank: 1,
        layer_first: 0,
        layer_last: -1,
        is_final: true,
        receives_tokens: false,
        epoch: 0,
        recovery_id: 0,
        model_path: None, // control-plane only — the re-link primitive is engine-agnostic
        n_gpu_layers: 0,
        n_ctx: 64,
        sampler_config: None,
        recovery_start: false,
    }
}

/// A guaranteed-dead loopback address: bind a listener, take its address, drop it — subsequent
/// connects are refused (nothing listening). Stands in for a killed downstream stage.
fn dead_addr() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = l.local_addr().unwrap();
    drop(l);
    a
}

/// The survivor's downstream is dead (killed); the coordinator then brings up a replacement and
/// updates the shared `DownTarget`. `forward_with_relink` must fail against the dead target, re-read
/// the target, and re-link to the replacement — completing the forward. Proves the re-link mechanism
/// (re-read + reconnect on failure) without any engine.
#[tokio::test]
async fn forward_relinks_to_a_replacement_after_the_target_is_updated() {
    let ca = ClusterCa::new().unwrap();
    let sp_id = ca.issue(SP_NAME).unwrap();
    let coord_id = ca.issue(COORD_NAME).unwrap();
    let keys = SessionKeys::dev(21);

    // The replacement downstream (a live control-plane worker).
    let replacement = spawn_endpoint(control_plane_cfg(keys.clone()), ca.server_config(&sp_id).unwrap());

    // The down-link connector presents the survivor's (here, the coordinator's) identity, trusting CA.
    let connector = TcpMtls::from_config(ca.client_config(&coord_id).unwrap()).unwrap();

    // Start pointing at a DEAD target (the killed downstream); a background "coordinator" swaps in the
    // replacement after a short delay — exactly the recovery ordering (rebuild+activate, then re-target).
    let down: DownTarget = Arc::new(Mutex::new((dead_addr(), SP_NAME.to_string())));
    let down_updater = down.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        *down_updater.lock().unwrap() = (replacement, SP_NAME.to_string());
    });

    // Forward a control frame with re-link: dead target → retry → replacement → response.
    let tuple = ActivationTuple { kind: ActivationKind::Initial, epoch: 0, recovery_id: 0, attempt: 0, sampler_checkpoint_id: 0 };
    let frame = wire::encode_commit_activation(&keys, &tuple, 1);
    let mut dc = None;
    let resp = forward_with_relink(&mut dc, &connector, &down, &frame, 40)
        .await
        .expect("re-link to the replacement should complete the forward");
    match wire::decode(&resp, &keys).unwrap().1 {
        Msg::ActivationCommitted(t) => assert_eq!((t.epoch, t.attempt), (0, 0)),
        other => panic!("expected ActivationCommitted from the replacement, got {other:?}"),
    }

    // The link is now established; a second forward reuses it (no re-link needed) and still succeeds.
    let resp2 = forward_with_relink(&mut dc, &connector, &down, &frame, 4).await.expect("reuse the re-linked down-link");
    assert!(matches!(wire::decode(&resp2, &keys).unwrap().1, Msg::ActivationCommitted(_)));
}

/// If the target never becomes reachable, re-link gives up after its bounded retries (does not hang).
#[tokio::test]
async fn relink_gives_up_after_bounded_retries_when_no_replacement_appears() {
    let ca = ClusterCa::new().unwrap();
    let coord_id = ca.issue(COORD_NAME).unwrap();
    let keys = SessionKeys::dev(22);
    let connector = TcpMtls::from_config(ca.client_config(&coord_id).unwrap()).unwrap();
    let down: DownTarget = Arc::new(Mutex::new((dead_addr(), SP_NAME.to_string())));
    let tuple = ActivationTuple { kind: ActivationKind::Initial, epoch: 0, recovery_id: 0, attempt: 0, sampler_checkpoint_id: 0 };
    let frame = wire::encode_commit_activation(&keys, &tuple, 1);
    let mut dc = None;
    let r = tokio::time::timeout(Duration::from_secs(5), forward_with_relink(&mut dc, &connector, &down, &frame, 3)).await;
    assert!(matches!(r, Ok(Err(_))), "re-link must surface an error (not hang) when no replacement appears");
}
