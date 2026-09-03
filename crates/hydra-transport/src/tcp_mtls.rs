//! TCP + mutual-TLS transport (the default; BLUEPRINT §1.3). Frames per `hydra-proto` ride on
//! top of the TLS stream via [`Conn`](crate::framed::Conn).

use std::net::SocketAddr;
use std::sync::Arc;

use rustls_pki_types::ServerName;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{client, server, TlsAcceptor, TlsConnector};

use crate::framed::Conn;
use crate::roles::{BoundPeer, RoleTable};
use crate::tls::{ClusterCa, DeviceIdentity};
use crate::{Transport, TransportError};

/// A connection accepted by [`TcpMtlsListener`].
pub type ServerConn = Conn<server::TlsStream<TcpStream>>;

/// An accepted connection **together with the role its certificate bound to** (audit C2).
///
/// `accept()` returns this rather than a bare `ServerConn` so a caller cannot serve a connection
/// without having been handed its role. The role is a property of the connection, established once
/// at the handshake, rather than a check each message handler must remember to perform.
pub struct AcceptedConn {
    pub conn: ServerConn,
    pub peer: BoundPeer,
}
/// A connection produced by [`TcpMtls::connect`].
pub type ClientConn = Conn<client::TlsStream<TcpStream>>;

/// Accepting side: binds a TCP port and completes an mTLS handshake per connection.
pub struct TcpMtlsListener {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    /// **Audit C2.** Not an `Option`: a listener cannot exist without a role table, so there is no
    /// "roles not configured" state for a caller to fall into. An empty table is itself refused at
    /// bind, with a message saying it is a configuration error rather than a policy.
    roles: Arc<RoleTable>,
}

impl TcpMtlsListener {
    pub async fn bind(
        addr: SocketAddr,
        ca: &ClusterCa,
        id: &DeviceIdentity,
        roles: RoleTable,
    ) -> Result<Self, TransportError> {
        Self::bind_with_config(addr, ca.server_config(id)?, roles).await
    }

    /// Bind from a prebuilt server config (e.g. a provisioned worker via
    /// [`server_config_with_ca`](crate::tls::server_config_with_ca)).
    pub async fn bind_with_config(
        addr: SocketAddr,
        cfg: rustls::ServerConfig,
        roles: RoleTable,
    ) -> Result<Self, TransportError> {
        // An empty role table would refuse every peer — fail-closed, but almost certainly a
        // misconfiguration rather than an intent. Catching it at BIND rather than at the first
        // rejected connection turns a puzzling runtime refusal into a startup error.
        if roles.is_empty() {
            return Err(TransportError::UnboundPeer(
                "refusing to bind a listener with an EMPTY role table: every peer would be refused. \
                 Name the peers this endpoint expects (audit C2)"
                    .into(),
            ));
        }
        // Addendum 2 §E1: refuse a wildcard bind unless it was explicitly opted into. Checked
        // BEFORE the socket exists, so there is no window in which the port is open on every
        // interface while we decide whether it should be.
        crate::check_bind_addr(addr)?;
        let acceptor = TlsAcceptor::from(Arc::new(cfg));
        let listener = TcpListener::bind(addr).await?;
        Ok(Self { listener, acceptor, roles: Arc::new(roles) })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        Ok(self.listener.local_addr()?)
    }

    /// Accept one connection, complete the mTLS handshake, and **bind the peer's certificate to a
    /// configured role** (audit C2). Errors — a client whose cert is not signed by the cluster CA,
    /// or one that binds to no role or to more than one — surface here, and the connection is
    /// dropped.
    ///
    /// **Fail closed.** There is no variant of this function that yields a connection without a
    /// role. A peer that authenticates but does not bind is refused, because *authentic* is not
    /// *authorised*: the cluster CA's signature says the peer belongs to the cluster, never that it
    /// may speak as a coordinator.
    /// **Audit H18 — the TLS handshake is bounded in time.**
    ///
    /// `accept()` performs the handshake, and callers (`serve_multi_conn` and every `pair.rs`
    /// endpoint) `await` it **sequentially**, spawning only the *post-handshake* connection. So a
    /// peer that opens a TCP socket and **sends nothing** parks the accept loop forever and every
    /// other peer — including the coordinator — is never accepted. No certificate is needed: the
    /// stall happens before any authentication, which is why C2's role binding does not help.
    ///
    /// A timeout here is not the whole fix — moving the handshake into the spawned task is the
    /// structural one, and that is a change to every call site's shape. **This bounds the damage
    /// now**: a silent peer costs one connection slot for `HANDSHAKE_TIMEOUT`, not the listener.
    /// The residual is named in §8 rather than implied by a green test.
    pub async fn accept(&self) -> Result<AcceptedConn, TransportError> {
        let (tcp, addr) = self.listener.accept().await?;
        let tls = match tokio::time::timeout(HANDSHAKE_TIMEOUT, self.acceptor.accept(tcp)).await {
            Ok(r) => r?,
            Err(_) => {
                // Logged WITH the peer address: the previous code discarded it, so a stalling peer
                // was not merely unbounded, it was unattributable.
                return Err(TransportError::HandshakeTimeout { peer: addr.to_string() });
            }
        };
        // Bind BEFORE the connection is handed up, so no frame can be read from an unbound peer.
        let peer = {
            let (_io, session) = tls.get_ref();
            self.roles.bind(session.peer_certificates())?
        };
        Ok(AcceptedConn { conn: Conn::new(tls), peer })
    }

    /// The roles this listener will accept — for tests and for logging what an endpoint expects.
    pub fn roles(&self) -> &RoleTable {
        &self.roles
    }
}

/// How long a peer may take to complete the TLS handshake (audit H18).
///
/// Long enough for a slow LAN link and a real key exchange; short enough that a silent socket is
/// not a denial of service. A cluster peer that cannot handshake in ten seconds has a problem the
/// operator needs to see as an error rather than as a hang.
pub const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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

    /// **Dial a STAGE and mint the rank the coordinator's quorum accounting may count it under.**
    ///
    /// On the accept side a rank comes from the peer certificate's bound role
    /// ([`crate::roles::BoundPeer::authenticated_rank`], audit H4). On the DIAL side the
    /// authenticated fact is the server name TLS verified against the cluster CA — the coordinator
    /// dialled `worker-s1` and the handshake proves it reached the device holding that
    /// certificate. The rank is the coordinator's own placement of that device (`name → rank`,
    /// from provisioning), bound to the connection here, BEFORE any frame is read — so H4's
    /// concern (a rank a frame chose) does not arise: no frame is ever consulted.
    ///
    /// This is the only production constructor of an [`hydra_state::AuthenticatedRank`] for a
    /// dialled peer; `hydra-node` uses it. It lives in the transport because that is where the
    /// TLS verification it rests on lives (rule 21: the right home was reachable).
    pub async fn connect_stage(
        &self,
        addr: SocketAddr,
        server_name: &str,
        rank: hydra_state::StageRank,
    ) -> Result<(ClientConn, hydra_state::AuthenticatedRank), TransportError> {
        let conn = self.connect(addr, server_name).await?;
        Ok((conn, hydra_state::AuthenticatedRank::from_authenticated_peer_role(rank)))
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
