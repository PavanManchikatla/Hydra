//! M0 transport DoD: two peers exchange authenticated framed messages over TCP+mTLS with a
//! shared cluster CA; a peer whose cert is not signed by that CA is rejected at the handshake.

use std::net::SocketAddr;

use hydra_transport::tcp_mtls::TcpMtlsListener;
use hydra_transport::{ClusterCa, TcpMtls};

#[tokio::test]
async fn two_peers_exchange_authenticated_frames() {
    let ca = ClusterCa::new().unwrap();
    let server_id = ca.issue("hydra-server").unwrap();
    let client_id = ca.issue("hydra-client").unwrap();

    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener = TcpMtlsListener::bind(addr, &ca, &server_id).await.unwrap();
    let bound = listener.local_addr().unwrap();

    // Server: accept one mTLS conn, echo a control frame back with a flag set.
    let server = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        let frame = conn.recv().await.unwrap();
        assert_eq!(frame.payload, b"BEGIN_RECOVERY tuple bytes");
        conn.send(0x0001, b"RECOVERY_ACK").await.unwrap();
    });

    let client = TcpMtls::new(&ca, &client_id).unwrap();
    let mut conn = client.connect(bound, "hydra-server").await.unwrap();
    conn.send(0x0000, b"BEGIN_RECOVERY tuple bytes").await.unwrap();
    let reply = conn.recv().await.unwrap();
    assert_eq!(reply.payload, b"RECOVERY_ACK");
    assert_eq!(reply.header.flags, 0x0001);

    server.await.unwrap();
}

#[tokio::test]
async fn client_cert_not_signed_by_cluster_ca_is_rejected() {
    let ca = ClusterCa::new().unwrap();
    let rogue_ca = ClusterCa::new().unwrap();
    let server_id = ca.issue("hydra-server").unwrap();
    // A client that trusts the real CA (so it accepts the server) but presents a cert signed by
    // a *different* CA — isolates mutual-TLS client-auth rejection.
    let rogue_id = rogue_ca.issue("hydra-client").unwrap();

    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener = TcpMtlsListener::bind(addr, &ca, &server_id).await.unwrap();
    let bound = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        // The handshake must fail; accept() returns an error and no frame is ever exchanged.
        listener.accept().await.err().is_some()
    });

    // client_config: trust the real CA's roots, but present the rogue (wrong-CA) identity.
    let rogue = TcpMtls::new(&ca, &rogue_id).unwrap();
    let result = async {
        let mut conn = rogue.connect(bound, "hydra-server").await?;
        conn.send(0x0000, b"should not arrive").await?;
        conn.recv().await
    }
    .await;
    assert!(result.is_err(), "rogue client must not complete an authenticated exchange");

    assert!(server.await.unwrap(), "server must reject the rogue handshake");
}
