//! `hydra-worker` — a standalone stage-worker process (BLUEPRINT §2, M2 sub-slice A).
//!
//! Reads its provisioning [`Bootstrap`] (mTLS material + role) from the file named by `argv[1]`,
//! binds a TCP+mTLS listener, and serves one long-lived [`Worker`] (the real `hydra-state` stage
//! SM + engine) with the **multi-connection serve loop** (`serve_multi_conn`): concurrent inbound
//! connections share the one `Worker`, so a stage can serve its upstream `FWD` **and** the
//! coordinator's control/`SAMPLE_NEXT` at once (P1·1a — the seam-3 requirement for a direct-FWD
//! pipeline). The single-session stage + KV state is preserved across a coordinator reconnect (spec
//! §1.4: one active session per instance) because every connection shares the same `Worker`.
//!
//! Run on a **current-thread** runtime inside a `LocalSet`: the engine's C context is not `Send`, and
//! a worker owns it on exactly one thread; the per-connection tasks are `spawn_local`.

use std::net::SocketAddr;

use hydra_transport::tcp_mtls::{TcpMtls, TcpMtlsListener};
use hydra_transport::{client_config_with_ca, server_config_with_ca};
use hydra_worker::bootstrap::Bootstrap;
use hydra_worker::worker::{
    serve_multi_conn, serve_multi_conn_forwarding_durable, shared, shared_down, DownstreamState, Worker,
};
use hydra_worker::DurableForwarder;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: hydra-worker <bootstrap-file>")?;
    let boot = Bootstrap::read_from(&path)?;

    let addr: SocketAddr = boot.listen_addr.parse()?;
    let cfg = server_config_with_ca(&boot.ca_cert(), &boot.identity())?;
    let listener = TcpMtlsListener::bind_with_config(addr, cfg).await?;
    let bound = listener.local_addr()?;
    // Advertise the actually-bound address (port may have been 0) on stdout for the runner.
    println!("HYDRA_WORKER_LISTENING {bound}");

    let (rank, lf, ll, is_final, toks) =
        (boot.cfg.rank, boot.cfg.layer_first, boot.cfg.layer_last, boot.cfg.is_final, boot.cfg.receives_tokens);
    let keys = boot.cfg.keys.clone();
    let epoch = boot.cfg.epoch;
    // A forwarding stage dials its downstream + durability target as a client (presenting its own
    // identity, trusting the cluster CA). Capture the mTLS material as owned values BEFORE moving
    // `boot.cfg` into the (non-Send) Worker.
    let forwarding = boot.forwarding.clone();
    let ca_cert = boot.ca_cert();
    let identity = boot.identity();
    let client_cfg = move || client_config_with_ca(&ca_cert, &identity);

    let device_name = boot.device_name.clone();
    let worker = Worker::new(boot.cfg)?;
    eprintln!(
        "hydra-worker {device_name} rank={rank} layers=[{lf},{ll}] final={is_final} tokens={toks} engine={} forwarding={}",
        worker.has_engine(),
        forwarding.is_some(),
    );

    // Concurrent connections share the one (non-Send) Worker on this thread via a LocalSet.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let r = match forwarding {
                // Forwarding stage (S1/S2): multi-conn + direct worker→worker FWD + durable copy.
                Some(f) => {
                    let down_conn = TcpMtls::from_config(client_cfg()?)?;
                    let down = down_conn.connect(f.down_addr.parse()?, &f.down_name).await?;
                    let dur_conn = TcpMtls::from_config(client_cfg()?)?;
                    let dur = dur_conn.connect(f.dur_addr.parse()?, &f.dur_name).await?;
                    eprintln!("hydra-worker: forwarding down→{} durability→{}", f.down_addr, f.dur_addr);
                    let forwarder = DurableForwarder::new(keys.clone(), epoch, f.require_durable, f.capacity as usize);
                    let down_state = shared_down(DownstreamState { down, dur, forwarder });
                    serve_multi_conn_forwarding_durable(shared(worker), down_state, keys, listener).await
                }
                // Final stage (S_P): multi-conn (samples; never forwards).
                None => serve_multi_conn(shared(worker), listener).await,
            };
            if let Err(e) = r {
                eprintln!("hydra-worker: serve loop ended with error: {e}");
                return Err::<(), Box<dyn std::error::Error>>(e.into());
            }
            Ok(())
        })
        .await
}
