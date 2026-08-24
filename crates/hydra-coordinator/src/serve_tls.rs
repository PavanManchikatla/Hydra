//! **Audit H21 — serving the client API over TLS, with the cluster's own material (M4·0).**
//!
//! `axum::serve` speaks plain HTTP; the router itself has no TLS layer. So the one link carrying
//! **user prompts and the API bearer token** was cleartext, on a LAN, while the project's own
//! checklist said *"every cluster link is mutually authenticated"* — a claim that excluded the one
//! link a person actually uses.
//!
//! This accepts TCP, completes a TLS handshake with the coordinator's identity, and hands the
//! stream to hyper with the axum router as its service. The client is **not** asked for a
//! certificate (see `hydra_transport::api_server_config`): its credential is the bearer token.
//!
//! The handshake is bounded and happens **in the spawned task**, not in the accept loop — the H18
//! lesson applied at the point it is being written rather than after an audit finds it again.

use std::net::SocketAddr;
use std::sync::Arc;

use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use tokio_rustls::TlsAcceptor;

/// How long a client may take to complete the TLS handshake (audit H18's shape, applied here).
const API_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Serve `router` over TLS on `addr` until the process ends.
pub async fn serve_tls(
    addr: SocketAddr,
    tls: tokio_rustls::rustls::ServerConfig,
    router: axum::Router,
) -> std::io::Result<()> {
    // Addendum 2 §E1 / audit M2: refuse a wildcard bind before the socket exists.
    hydra_transport::check_bind_addr(addr).map_err(|e| std::io::Error::other(e.to_string()))?;
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("hydra-coordinator: API listening on https://{}", listener.local_addr()?);

    loop {
        let (tcp, peer) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let router = router.clone();
        tokio::spawn(async move {
            let stream = match tokio::time::timeout(API_HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    eprintln!("hydra-coordinator: TLS handshake failed for {peer}: {e}");
                    return;
                }
                Err(_) => {
                    eprintln!("hydra-coordinator: TLS handshake timed out for {peer}");
                    return;
                }
            };
            let service = hyper_util::service::TowerToHyperService::new(router);
            if let Err(e) = ConnBuilder::new(TokioExecutor::new()).serve_connection(TokioIo::new(stream), service).await {
                eprintln!("hydra-coordinator: connection from {peer} ended: {e}");
            }
        });
    }
}
