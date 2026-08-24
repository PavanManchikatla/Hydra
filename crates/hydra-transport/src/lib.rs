//! # hydra-transport
//!
//! The control/data-plane transport (BLUEPRINT §1.3, spec §4). A [`Transport`] trait sits in
//! front of concrete impls — [`tcp_mtls`] (TCP + mTLS, built first; the default) and, later,
//! QUIC. All impls speak the same wire framing from `hydra-proto` (`HYFR` header + BLAKE3),
//! and every frame's header is validated against the hard caps **before** the payload is read
//! or the flatbuffer is parsed.
//!
//! Security boundary = one trusted household (BLUEPRINT §1.9): per-device identity + a cluster
//! CA created at pairing; both peers present certs signed by that CA (mutual TLS).

pub mod framed;
pub mod tcp_mtls;
pub mod roles;
pub mod tls;

pub use framed::Conn;
pub use tcp_mtls::TcpMtls;
pub use tls::{client_config_with_ca, server_config_with_ca, ClusterCa, DeviceIdentity};

/// Re-exported so downstream crates can build cert values without pinning `rustls-pki-types`.
pub use rustls_pki_types::CertificateDer;

use hydra_proto::framing::{FrameError, FrameHeader};

/// Errors from the transport layer.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// **Audit M1.** A peer declared a payload length and then did not deliver it within
    /// [`crate::framed::PAYLOAD_READ_TIMEOUT`]. The reservation it was holding is released.
    #[error("payload read timed out after a declared length of {declared} bytes (audit M1)")]
    PayloadReadTimeout { declared: u32 },
    /// **Audit H18.** A peer opened a TCP connection and did not complete the TLS handshake within
    /// [`crate::tcp_mtls::HANDSHAKE_TIMEOUT`]. Carries the peer address, which the previous code
    /// discarded — an unbounded stall that is also unattributable is twice as hard to diagnose.
    #[error("TLS handshake timed out for peer {peer} (audit H18)")]
    HandshakeTimeout { peer: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Framing/limit/checksum rejection (from `hydra-proto`); several map to structured `ErrCode`.
    #[error("frame: {0}")]
    Frame(#[from] FrameError),
    #[error("tls: {0}")]
    Tls(#[from] rustls::Error),
    #[error("certificate: {0}")]
    Cert(String),
    #[error("invalid dns name: {0}")]
    Dns(String),
    /// **Audit C2.** The peer completed the mTLS handshake but its certificate could not be bound
    /// to a configured role. **Fail-closed**: an unparseable certificate, a certificate matching no
    /// configured name, or one matching more than one are all this error — never "unknown but
    /// allowed".
    #[error("peer REFUSED at accept: {0}")]
    UnboundPeer(String),
    /// A wildcard bind (`0.0.0.0` / `[::]`) was attempted without the explicit opt-in.
    /// See [`check_bind_addr`].
    #[error("refusing wildcard bind {0}: v1's trust boundary is one household LAN; bind an explicit \
             interface address, or set HYDRA_ALLOW_WILDCARD_BIND=1 if this really is a namespaced \
             environment (report Addendum 2 §E1)")]
    WildcardBind(std::net::SocketAddr),
}

/// The environment variable that makes a wildcard bind an explicit, visible decision.
pub const ALLOW_WILDCARD_BIND_ENV: &str = "HYDRA_ALLOW_WILDCARD_BIND";

/// **Audit M2 — every spelling of "every interface", not just the two obvious ones.**
///
/// `IpAddr::is_unspecified` is true for `0.0.0.0` and `::` and **false for `::ffff:0.0.0.0`** — the
/// IPv4-mapped wildcard. On Linux an `AF_INET6` socket bound to that address with `IPV6_V6ONLY=0`
/// accepts on **all IPv4 interfaces**, which is exactly the state this check exists to prevent.
///
/// The listen address is a runtime string from the bootstrap blob, so the repository-wide grep
/// test cannot cover it and the unit test only knew the two spellings it was written with — a
/// third spelling was, to both oracles, indistinguishable from a loopback bind.
pub fn is_wildcard(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_unspecified(),
        std::net::IpAddr::V6(v6) => {
            v6.is_unspecified() || v6.to_ipv4_mapped().is_some_and(|m| m.is_unspecified())
        }
    }
}

/// **Report Addendum 2 §E1, enforced rather than documented:** *"must not bind 0.0.0.0 by
/// default."*
///
/// Local inference servers of the Ollama class have been exploited by browsers on the same LAN
/// reaching an unauthenticated API bound to every interface, so a wildcard bind is not a
/// configuration preference here — it is the precondition of that attack. This makes it impossible
/// to reach **by accident**: every listener in the project goes through this check, an unspecified
/// address is refused, and the one environment that legitimately needs it (a container, whose
/// network namespace *is* the isolation boundary) has to say so out loud.
///
/// It is deliberately an opt-**in**, not an opt-out. An opt-out is a flag someone forgets to set;
/// an opt-in is a decision someone had to make.
pub fn check_bind_addr(addr: std::net::SocketAddr) -> Result<(), TransportError> {
    if !is_wildcard(addr.ip()) {
        return Ok(());
    }
    match std::env::var(ALLOW_WILDCARD_BIND_ENV).as_deref() {
        Ok("1") => Ok(()),
        _ => Err(TransportError::WildcardBind(addr)),
    }
}

/// A bidirectional, authenticated, framed connection to one peer.
pub trait Transport {
    /// The connection type produced by this transport.
    type Conn;
    /// Connect to `addr`, authenticating the peer via the cluster CA. `server_name` is the
    /// identity expected in the peer's certificate.
    fn connect(
        &self,
        addr: std::net::SocketAddr,
        server_name: &str,
    ) -> impl std::future::Future<Output = Result<Self::Conn, TransportError>> + Send;
}

/// One received frame: the validated header plus the (already tag-verified) payload bytes.
#[derive(Debug, Clone)]
pub struct Frame {
    pub header: FrameHeader,
    pub payload: Vec<u8>,
}
