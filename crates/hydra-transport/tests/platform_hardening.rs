//! **Audit Wave 3 — the platform group: H18, M1, M2, M3.**
//!
//! # Standing rule 19: what the oracles could not see
//!
//! All four are INDISTINGUISHING in the same way. Every existing transport test drove a **well-
//! behaved peer**: it completed the handshake, it sent the payload it declared, it bound to
//! `127.0.0.1`, and it finished long before any certificate could expire. Under that driver,
//! "handshake with a timeout" and "handshake without one" accept the same connections; "read the
//! payload" and "read the payload or give up" return the same frames; `is_unspecified` and
//! `is_wildcard` classify the same addresses; and a certificate valid for 397 days is
//! indistinguishable from one valid until the year 4096.
//!
//! So the tests below misbehave deliberately: connect and say nothing, declare and never deliver,
//! spell the wildcard a third way, and read the dates off the certificate.

use std::net::SocketAddr;

use hydra_transport::roles::{PeerRole, RoleTable};
use hydra_transport::tcp_mtls::{TcpMtlsListener, HANDSHAKE_TIMEOUT};
use hydra_transport::{check_bind_addr, is_wildcard, ClusterCa, TransportError};

fn table() -> RoleTable {
    RoleTable::new().with("coordinator", PeerRole::Coordinator).with("worker-s1", PeerRole::Stage { rank: 0 })
}

/// **M2 — every spelling of "every interface".**
///
/// `IpAddr::is_unspecified` is false for `::ffff:0.0.0.0`, and on Linux an `AF_INET6` socket bound
/// to it with `IPV6_V6ONLY=0` accepts on **all IPv4 interfaces** — precisely the state
/// `check_bind_addr` exists to prevent. The listen address is a runtime string from the bootstrap
/// blob, so the repository-wide grep test cannot see it either.
#[test]
fn the_ipv4_mapped_wildcard_is_refused_like_every_other_wildcard() {
    for spelling in ["0.0.0.0:8080", "[::]:8080", "[::ffff:0.0.0.0]:8080", "[::ffff:0:0]:8080"] {
        let addr: SocketAddr = spelling.parse().expect("parses");
        assert!(is_wildcard(addr.ip()), "{spelling} is a wildcard bind");
        assert!(
            matches!(check_bind_addr(addr), Err(TransportError::WildcardBind(_))),
            "{spelling} must be refused — it accepts on every interface"
        );
    }

    // Controls: real addresses are still bindable, or the check would be an outage.
    for ok in ["127.0.0.1:8080", "[::1]:8080", "192.168.1.10:8080", "[::ffff:127.0.0.1]:8080"] {
        let addr: SocketAddr = ok.parse().expect("parses");
        assert!(!is_wildcard(addr.ip()), "{ok} is not a wildcard");
        assert!(check_bind_addr(addr).is_ok(), "{ok} must be allowed");
    }
}

/// **H18 — a peer that opens a socket and says nothing must not park the listener forever.**
///
/// No certificate is needed for this: the stall happens before any authentication, which is why
/// C2's role binding does not help. The listener `await`s the handshake inline, so one silent
/// socket used to block every other peer — including the coordinator.
#[tokio::test]
async fn a_silent_peer_cannot_park_the_accept_loop_forever() {
    let ca = ClusterCa::new().unwrap();
    let server_id = ca.issue("worker-s1").unwrap();
    let listener = TcpMtlsListener::bind("127.0.0.1:0".parse().unwrap(), &ca, &server_id, table())
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();

    // A raw TCP connection that never sends a byte — held open for the duration.
    let _silent = tokio::net::TcpStream::connect(addr).await.expect("connect");

    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(HANDSHAKE_TIMEOUT * 3, listener.accept()).await;

    let elapsed = started.elapsed();
    match outcome {
        Ok(Err(TransportError::HandshakeTimeout { peer })) => {
            assert!(!peer.is_empty(), "the refusal must name the peer — an unbounded stall that is also unattributable is twice as hard to diagnose");
            assert!(elapsed < HANDSHAKE_TIMEOUT * 2, "it must give up at roughly the timeout, not later");
        }
        Ok(Err(other)) => panic!("expected a handshake timeout, got {other:?}"),
        Ok(Ok(_)) => panic!("a silent peer must not produce an accepted connection"),
        Err(_) => panic!(
            "accept() never returned: a peer that opens a socket and sends nothing still parks the \
             listener, and every other peer is never accepted (audit H18)"
        ),
    }
}

/// **M1 — a declared payload that never arrives must not pin its reservation indefinitely.**
///
/// The 64 MiB cap is enforced before the allocation (which is why this is not the D2 class); what
/// was unbounded is the *duration*. Sixteen idle connections from one certificate holder commit a
/// gigabyte. Under the honest-worker assumption that is an accident — a wedged peer, a half-open
/// NAT mapping — and the assumption excuses malice, never accidents.
#[tokio::test]
async fn a_declared_payload_that_never_arrives_times_out() {
    use hydra_transport::framed::{Conn, PAYLOAD_READ_TIMEOUT};
    use tokio::io::AsyncWriteExt;

    // A duplex pair stands in for the socket: the point is the read path, not the TLS.
    let (client, server) = tokio::io::duplex(4096);
    let mut conn = Conn::new(server);
    // The bound under test is the *existence* of a bound; waiting the production 60 s on every
    // push-lane run to observe it would be a cost paid forever for one millisecond of information.
    conn.set_payload_read_timeout(std::time::Duration::from_millis(250));

    // A well-formed 12-byte header declaring a payload, and then silence.
    let mut hdr = Vec::new();
    hdr.extend_from_slice(&hydra_proto::framing::FRAME_MAGIC.to_le_bytes());
    hdr.extend_from_slice(&hydra_proto::framing::WIRE_VERSION.to_le_bytes());
    hdr.extend_from_slice(&0u16.to_le_bytes());
    hdr.extend_from_slice(&4096u32.to_le_bytes());
    let mut client = client;
    client.write_all(&hdr).await.unwrap();
    // Deliberately no payload, and the stream is HELD OPEN (dropping it would EOF instead).

    assert_eq!(PAYLOAD_READ_TIMEOUT, std::time::Duration::from_secs(60), "the production default is the claim; this test only shortens it for itself");
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), conn.recv()).await;
    match outcome {
        Ok(Err(TransportError::PayloadReadTimeout { declared })) => assert_eq!(declared, 4096),
        Ok(other) => panic!("expected a payload read timeout, got {other:?}"),
        Err(_) => panic!("recv() never returned: the reservation is held for as long as the peer stays silent (audit M1)"),
    }
    drop(client);
}

/// **M3 — certificates expire, and the CA cannot mint sub-CAs.**
///
/// `rcgen`'s defaults are 1975 → 4096: a device key leaked through H17's world-readable bootstrap
/// file was valid **forever**, and with no CRL and no rotation nothing could ever make it invalid.
/// A bounded lifetime is not revocation and is not claimed to be — it is the difference between a
/// leak that expires and one that does not.
#[test]
fn certificates_have_bounded_lifetimes_and_the_ca_is_path_length_constrained() {
    use hydra_transport::tls::{CA_VALIDITY_DAYS, LEAF_VALIDITY_DAYS};

    let ca = ClusterCa::new().unwrap();
    let id = ca.issue("worker-s1").unwrap();

    // Parse the DER and read the validity window back out — asserting the constant is set is not
    // the same as asserting the certificate carries it.
    let leaf = id.cert_chain[0].clone();
    let (_, parsed) = x509_parser::parse_x509_certificate(&leaf).expect("leaf parses");
    let validity = parsed.validity();
    let days = (validity.not_after.timestamp() - validity.not_before.timestamp()) / 86_400;
    assert!(
        (LEAF_VALIDITY_DAYS - 2..=LEAF_VALIDITY_DAYS).contains(&days),
        "a leaf must be valid for ~{LEAF_VALIDITY_DAYS} days, not until the year 4096 (got {days})"
    );
    assert!(days < 400, "and well under the CA/Browser Forum's leaf maximum");

    let ca_der = ca.ca_cert_der();
    let (_, ca_parsed) = x509_parser::parse_x509_certificate(&ca_der).expect("CA parses");
    let ca_days = (ca_parsed.validity().not_after.timestamp() - ca_parsed.validity().not_before.timestamp()) / 86_400;
    assert!((CA_VALIDITY_DAYS - 2..=CA_VALIDITY_DAYS).contains(&ca_days), "the CA is bounded too (got {ca_days})");

    // Path length 0: this CA signs leaves, and nothing it signs may sign further.
    let bc = ca_parsed.basic_constraints().expect("basic constraints present").expect("basic constraints value");
    assert!(bc.value.ca, "the CA must be a CA");
    assert_eq!(bc.value.path_len_constraint, Some(0), "the CA must be path-length constrained to 0 (audit M3)");
}
