//! `hydra-worker` — a standalone stage-worker process (BLUEPRINT §2, M2 sub-slice A).
//!
//! Reads its provisioning [`Bootstrap`] (mTLS material + role) from the file named by `argv[1]`,
//! binds a TCP+mTLS listener, and serves one long-lived [`Worker`] (the real `hydra-state` stage
//! SM + engine). Connections are accepted **sequentially** so the single-session worker keeps its
//! stage + KV state across a coordinator reconnect (spec §1.4: one active session per instance).
//!
//! Run on a **current-thread** runtime: the engine's C context is not `Send`, and a worker owns it
//! on exactly one thread.

use std::net::SocketAddr;

use hydra_transport::server_config_with_ca;
use hydra_transport::tcp_mtls::TcpMtlsListener;
use hydra_worker::bootstrap::Bootstrap;
use hydra_worker::worker::{serve_conn, Worker};

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
    let mut worker = Worker::new(boot.cfg)?;
    eprintln!(
        "hydra-worker {} rank={rank} layers=[{lf},{ll}] final={is_final} tokens={toks} engine={}",
        boot.device_name,
        worker.has_engine()
    );

    loop {
        match listener.accept().await {
            Ok(mut conn) => {
                if let Err(e) = serve_conn(&mut worker, &mut conn).await {
                    eprintln!("hydra-worker: connection ended with error: {e}");
                }
            }
            Err(e) => {
                eprintln!("hydra-worker: accept failed: {e}");
                return Err(e.into());
            }
        }
    }
}
