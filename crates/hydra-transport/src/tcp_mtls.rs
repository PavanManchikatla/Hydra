//! TCP + mutual-TLS transport (the default; BLUEPRINT §1.3). Frames per `hydra-proto` ride on
//! top of the TLS stream via [`Conn`](crate::framed::Conn).

use std::net::SocketAddr;
use std::sync::Arc;

use rustls_pki_types::ServerName;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{client, server, TlsAcceptor, TlsConnector};

use crate::framed::Conn;
use crate::tls::{ClusterCa, DeviceIdentity};
use crate::{Transport, TransportError};

/// A connection accepted by [`TcpMtlsListener`].
pub type ServerConn = Conn<server::TlsStream<TcpStream>>;
/// A connection produced by [`TcpMtls::connect`].
pub type ClientConn = Conn<client::TlsStream<TcpStream>>;

/// Accepting side: binds a TCP port and completes an mTLS handshake per connection.
pub struct TcpMtlsListener {
    listener: TcpListener,
    acceptor: TlsAcceptor,
}

impl TcpMtlsListener {
    pub async fn bind(
        addr: SocketAddr,
        ca: &ClusterCa,
        id: &DeviceIdentity,
    ) -> Result<Self, TransportError> {
        Self::bind_with_config(addr, ca.server_config(id)?).await
    }

    /// Bind from a prebuilt server config (e.g. a provisioned worker via
    /// [`server_config_with_ca`](crate::tls::server_config_with_ca)).
    pub async fn bind_with_config(
        addr: SocketAddr,
        cfg: rustls::ServerConfig,
    ) -> Result<Self, TransportError> {
        let acceptor = TlsAcceptor::from(Arc::new(cfg));
        let listener = TcpListener::bind(addr).await?;
        Ok(Self { listener, acceptor })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        Ok(self.listener.local_addr()?)
    }

    /// Accept one connection and complete the mTLS handshake. Errors (incl. a client whose cert
    /// is not signed by the cluster CA) surface here as an `Io` error.
    pub async fn accept(&self) -> Result<ServerConn, TransportError> {
        let (tcp, _peer) = self.listener.accept().await?;
        let tls = self.acceptor.accept(tcp).await?;
        Ok(Conn::new(tls))
    }
}

/// Connecting side.
pub struct TcpMtls {
    connector: TlsConnector,
}

impl TcpMtls {
    pub fn new(ca: &ClusterCa, id: &DeviceIdentity) -> Result<Self, TransportError> {
        Self::from_config(ca.client_config(id)?)
    }

    /// Build from a prebuilt client config (e.g. a provisioned worker/coordinator via
    /// [`client_config_with_ca`](crate::tls::client_config_with_ca)).
    pub fn from_config(cfg: rustls::ClientConfig) -> Result<Self, TransportError> {
        Ok(Self { connector: TlsConnector::from(Arc::new(cfg)) })
    }

    /// Connect to `addr`, verifying the server cert against the cluster CA and requiring
    /// `server_name` to match the server certificate's identity.
    pub async fn connect(
        &self,
        addr: SocketAddr,
        server_name: &str,
    ) -> Result<ClientConn, TransportError> {
        let tcp = TcpStream::connect(addr).await?;
        let sni = ServerName::try_from(server_name.to_string())
            .map_err(|_| TransportError::Dns(server_name.to_string()))?;
        let tls = self.connector.connect(sni, tcp).await?;
        Ok(Conn::new(tls))
    }
}

impl Transport for TcpMtls {
    type Conn = ClientConn;

    fn connect(
        &self,
        addr: SocketAddr,
        server_name: &str,
    ) -> impl std::future::Future<Output = Result<Self::Conn, TransportError>> + Send {
        TcpMtls::connect(self, addr, server_name)
    }
}
