//! **Audit C2 — the SAN→role binding, and that it fails closed.**
//!
//! Every test here drives a **real `TcpMtlsListener` with a real `RoleTable`** over a real mTLS
//! handshake. None of them uses the in-process dev fixture (`hydra_worker::pair::dev_role_table`) —
//! that fixture exists to spare ~20 harness call sites from restating one table, and letting it
//! into these tests would make the fixture's convenience into a claim the tests do not support. The
//! §7.31 shape, avoided deliberately.
//!
//! # The finding, stated as the tests state it
//!
//! mTLS answers *"is this peer in the cluster?"*. Nothing answered *"which stage is it?"* — and
//! every authorisation decision in the protocol is about the second question. The F1 fence checks
//! *session* identity, not *sender role*, so **any certificate holder could send any message
//! family**: a stage could issue `COMMIT_ACTIVATION`, the durability target could send `SAMPLED`.
//! The cluster CA's signature was being read as an authorisation when it is only an authentication.
//!
//! Following the Wave-1a pattern the design authority made the reference for security regressions:
//! **each test demonstrates the vulnerability it closes**, so its own necessity is legible. The
//! peers below are *genuinely authentic* — correctly issued by the cluster CA, completing the
//! handshake without complaint. Only the role binding separates them.

use hydra_transport::roles::{PeerRole, RoleTable};
use hydra_transport::tcp_mtls::{TcpMtls, TcpMtlsListener};
use hydra_transport::{ClusterCa, TransportError};

/// Bind a listener with `roles`, then dial it as `client_name`. Returns what `accept()` decided.
async fn handshake_as(
    ca: &ClusterCa,
    roles: RoleTable,
    server_name: &str,
    client_name: &str,
) -> Result<hydra_transport::roles::BoundPeer, TransportError> {
    let server_id = ca.issue(server_name).unwrap();
    let client_id = ca.issue(client_name).unwrap();

    let listener = TcpMtlsListener::bind("127.0.0.1:0".parse().unwrap(), ca, &server_id, roles)
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    let accept = tokio::spawn(async move { listener.accept().await.map(|a| a.peer) });

    let connector = TcpMtls::from_config(ca.client_config(&client_id).unwrap()).unwrap();
    // The client side may or may not observe the server's refusal depending on when the server
    // drops; the SERVER's verdict is what this test is about, so the client's result is ignored.
    let _ = connector.connect(addr, server_name).await;
    accept.await.unwrap()
}

fn table() -> RoleTable {
    RoleTable::new()
        .with("coordinator", PeerRole::Coordinator)
        .with("worker-s1", PeerRole::Stage { rank: 0 })
        .with("durability", PeerRole::DurabilityTarget)
}

/// **The control.** A configured peer binds to exactly the role it was configured as — so every
/// refusal below is caused by the binding and not by a gate that refuses everything.
#[tokio::test]
async fn a_configured_peer_binds_to_its_configured_role() {
    let ca = ClusterCa::new().unwrap();
    let peer = handshake_as(&ca, table(), "worker-s1", "coordinator").await.expect("coordinator binds");
    assert_eq!(peer.role, PeerRole::Coordinator);
    assert_eq!(peer.name, "coordinator");

    let peer = handshake_as(&ca, table(), "worker-s1", "durability").await.expect("durability binds");
    assert_eq!(peer.role, PeerRole::DurabilityTarget);

    let peer = handshake_as(&ca, table(), "coordinator", "worker-s1").await.expect("stage binds");
    assert_eq!(peer.role, PeerRole::Stage { rank: 0 });
}

/// **THE FINDING.** A peer with a **perfectly valid cluster certificate** — issued by this very CA,
/// completing the mTLS handshake without complaint — is **refused at `accept()`** because its name
/// maps to no configured role.
///
/// Before the binding existed this peer was indistinguishable from any other: it had passed the only
/// question anyone asked. *Authentic is not authorised*, and this test is the difference.
#[tokio::test]
async fn an_authentic_peer_with_no_configured_role_is_refused_at_accept() {
    let ca = ClusterCa::new().unwrap();
    // "attacker-node" is issued by the REAL cluster CA. Nothing about the certificate is wrong.
    let err = handshake_as(&ca, table(), "worker-s1", "attacker-node")
        .await
        .expect_err("an authentic peer with no configured role must be REFUSED");
    match err {
        TransportError::UnboundPeer(msg) => {
            assert!(msg.contains("NONE"), "the refusal should say it matched no configured name: {msg}");
        }
        other => panic!("expected UnboundPeer, got {other:?}"),
    }
}

/// **Fail closed, not open.** An empty role table denies every peer — which is the safe direction —
/// but it is refused at **bind**, because a worker that starts and then rejects everything is
/// harder to diagnose than one that does not start. Fail closed *and* fail loudly.
#[tokio::test]
async fn an_empty_role_table_is_refused_at_bind_not_silently_permissive() {
    let ca = ClusterCa::new().unwrap();
    let id = ca.issue("worker-s1").unwrap();
    match TcpMtlsListener::bind("127.0.0.1:0".parse().unwrap(), &ca, &id, RoleTable::new()).await {
        Err(TransportError::UnboundPeer(msg)) => {
            assert!(msg.contains("EMPTY"), "the error should name the empty table: {msg}");
        }
        Err(other) => panic!("expected UnboundPeer at bind, got {other:?}"),
        Ok(_) => panic!("an empty role table must be refused at bind, not produce a listener"),
    }
}

/// A certificate that binds to **more than one** configured role is refused rather than resolved by
/// taking the first match.
///
/// This is why the binding **tests configured names against the certificate** instead of reading the
/// certificate's SANs and looking one up: a first-match lookup would let a peer holding a multi-SAN
/// certificate **choose its own role by ordering**. An ambiguous authorisation is a granted one.
#[tokio::test]
async fn a_certificate_matching_two_configured_roles_is_refused_as_ambiguous() {
    let ca = ClusterCa::new().unwrap();
    // Two table entries, both naming the SAME certificate name but as different roles — the shape a
    // multi-SAN certificate or a duplicated provisioning entry would produce.
    let ambiguous = RoleTable::new()
        .with("both-roles", PeerRole::Coordinator)
        .with("BOTH-ROLES", PeerRole::Stage { rank: 7 }); // DNS matching is case-insensitive

    let err = handshake_as(&ca, ambiguous, "worker-s1", "both-roles")
        .await
        .expect_err("a certificate matching two configured roles must be REFUSED");
    match err {
        TransportError::UnboundPeer(msg) => {
            assert!(msg.contains("AMBIGUOUS"), "the refusal should name the ambiguity: {msg}");
        }
        other => panic!("expected UnboundPeer, got {other:?}"),
    }
}

/// A configured name that is not a legal DNS name can never match a certificate. That is a
/// **configuration defect**, and it is reported rather than silently skipped — a skipped entry is a
/// role that quietly does not exist, which is exactly the class of silence this audit was about.
#[test]
fn a_configured_name_that_can_never_match_is_reported_not_skipped() {
    let ca = ClusterCa::new().unwrap();
    let id = ca.issue("worker-s1").unwrap();
    let bad = RoleTable::new().with("not a dns name!", PeerRole::Coordinator);
    let err = bad.bind(Some(&id.cert_chain)).expect_err("an unmatchable configured name is a defect");
    match err {
        TransportError::UnboundPeer(msg) => {
            assert!(msg.contains("never match"), "the error should say why: {msg}");
        }
        other => panic!("expected UnboundPeer, got {other:?}"),
    }
}

/// A peer presenting no certificate is refused rather than trusted to be impossible.
///
/// The client-auth verifier should make this unreachable. "Should be unreachable" is a claim about
/// another component, and rule 12 grants those no presumption — so the branch exists and is tested.
#[test]
fn a_peer_with_no_certificate_is_refused_rather_than_assumed_impossible() {
    let err = table().bind(None).expect_err("no certificate must be refused");
    assert!(matches!(err, TransportError::UnboundPeer(_)));
    let err = table().bind(Some(&[])).expect_err("an empty chain must be refused");
    assert!(matches!(err, TransportError::UnboundPeer(_)));
}
