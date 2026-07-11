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
pub mod tls;

pub use framed::Conn;
pub use tcp_mtls::TcpMtls;
pub use tls::ClusterCa;

use hydra_proto::framing::{FrameError, FrameHeader};

/// Errors from the transport layer.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
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
