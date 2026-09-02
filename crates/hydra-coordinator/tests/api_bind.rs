//! **M4 DoD row 3, clause (a) — "no 0.0.0.0 binds" — the oracle for the API LISTENER.**
//!
//! `hydra-transport` refuses a wildcard address (`check_bind_addr`) and its own mTLS listener is
//! tested to call it. The client API is a different listener (`serve_tls`, plain TLS, no client
//! cert), and until 2026-09-02 nothing asserted that IT refuses `0.0.0.0` — a grep is a reading,
//! not an oracle (rule 19). This test FAILS if `serve_tls` ever binds a wildcard address.

use std::net::SocketAddr;

fn api_tls() -> tokio_rustls::rustls::ServerConfig {
    let ca = hydra_transport::ClusterCa::new().expect("ca");
    let id = ca.issue_api("coordinator", &["127.0.0.1".to_string()]).expect("identity");
    hydra_transport::api_server_config(&id).expect("server config")
}

#[tokio::test]
async fn the_api_listener_refuses_a_wildcard_bind_and_accepts_an_explicit_one() {
    assert!(
        std::env::var(hydra_transport::ALLOW_WILDCARD_BIND_ENV).is_err(),
        "{} is set; the refusal test would be vacuous",
        hydra_transport::ALLOW_WILDCARD_BIND_ENV
    );
    for wildcard in ["0.0.0.0:0", "[::]:0"] {
        let addr: SocketAddr = wildcard.parse().unwrap();
        // Bounded: a wildcard that was NOT refused would serve forever, and the timeout would then
        // be the signal that the refusal did not happen.
        let r = tokio::time::timeout(std::time::Duration::from_secs(2), hydra_coordinator::serve_tls::serve_tls(addr, api_tls(), axum::Router::new())).await;
        match r {
            Ok(Err(e)) => assert!(e.to_string().contains("wildcard"), "{wildcard}: refused, but not as a wildcard bind: {e}"),
            Ok(Ok(())) => panic!("{wildcard}: serve_tls returned Ok — it bound the wildcard and stopped?"),
            Err(_) => panic!("{wildcard}: serve_tls is SERVING on a wildcard address (it did not refuse)"),
        }
    }
    // Control: an explicit loopback address gets past the check and serves (the timeout fires
    // because the accept loop never returns) — so the refusals above are the wildcard's doing.
    let ok: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let r = tokio::time::timeout(std::time::Duration::from_millis(800), hydra_coordinator::serve_tls::serve_tls(ok, api_tls(), axum::Router::new())).await;
    assert!(r.is_err(), "127.0.0.1:0 must bind and serve; instead: {r:?}");
}
