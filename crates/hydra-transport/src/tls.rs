//! Cluster CA + per-device identities + rustls configs (BLUEPRINT §1.9).
//!
//! At pairing (QR/PIN in M4) the coordinator mints a **cluster CA**; each device gets an
//! Ed25519/ECDSA identity whose cert is signed by that CA. Both peers present CA-signed certs:
//! **mutual TLS**. A device whose cert is not signed by the cluster CA fails the handshake.

use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::TransportError;

fn cert_err<E: std::fmt::Display>(e: E) -> TransportError {
    TransportError::Cert(e.to_string())
}

/// Install the `ring` crypto provider once for this process (required because we build rustls
/// with only the `ring` backend, so there is no auto-selected default provider).
fn ensure_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// A per-device identity: its cert chain (`[leaf, ca]`) and private key.
pub struct DeviceIdentity {
    pub name: String,
    pub cert_chain: Vec<CertificateDer<'static>>,
    key_der: PrivateKeyDer<'static>,
}

impl DeviceIdentity {
    fn key(&self) -> PrivateKeyDer<'static> {
        self.key_der.clone_key()
    }

    /// Rebuild an identity from **provisioned DER** — the shape a worker gets in the field (its
    /// signed cert chain `[leaf, ca]` + PKCS#8 key), never the CA's private key. Pairs with
    /// [`server_config_with_ca`] / [`client_config_with_ca`], which trust a CA by its cert alone.
    pub fn from_der(
        name: impl Into<String>,
        cert_chain: Vec<CertificateDer<'static>>,
        key_pkcs8_der: Vec<u8>,
    ) -> Self {
        DeviceIdentity {
            name: name.into(),
            cert_chain,
            key_der: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pkcs8_der)),
        }
    }

    /// The PKCS#8 private-key DER (for provisioning a worker; keep it secret).
    pub fn key_pkcs8_der(&self) -> Vec<u8> {
        match &self.key_der {
            PrivateKeyDer::Pkcs8(k) => k.secret_pkcs8_der().to_vec(),
            other => other.secret_der().to_vec(),
        }
    }
}

fn roots_from(ca_cert: &CertificateDer<'static>) -> Result<RootCertStore, TransportError> {
    let mut roots = RootCertStore::empty();
    roots.add(ca_cert.clone())?;
    Ok(roots)
}

/// Server config trusting `ca_cert` as the client-auth anchor and presenting `id`'s chain —
/// built from a CA **certificate only** (no CA key), the way a provisioned worker runs.
pub fn server_config_with_ca(
    ca_cert: &CertificateDer<'static>,
    id: &DeviceIdentity,
) -> Result<ServerConfig, TransportError> {
    ensure_provider();
    let roots = Arc::new(roots_from(ca_cert)?);
    let verifier = WebPkiClientVerifier::builder(roots).build().map_err(cert_err)?;
    Ok(ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(id.cert_chain.clone(), id.key())?)
}

/// Client config trusting `ca_cert` for the server cert and presenting `id`'s chain for mTLS —
/// built from a CA **certificate only** (no CA key).
pub fn client_config_with_ca(
    ca_cert: &CertificateDer<'static>,
    id: &DeviceIdentity,
) -> Result<ClientConfig, TransportError> {
    ensure_provider();
    Ok(ClientConfig::builder()
        .with_root_certificates(roots_from(ca_cert)?)
        .with_client_auth_cert(id.cert_chain.clone(), id.key())?)
}

/// The cluster certificate authority created at pairing.
pub struct ClusterCa {
    ca_cert: rcgen::Certificate,
    ca_key: KeyPair,
}

/// Leaf certificate validity (audit M3). Long enough not to break a household cluster that is
/// powered off for a season; short enough that a leaked key is not a permanent credential.
/// Re-pairing is the rotation mechanism; a CRL/OCSP flow is M4·2 (§8).
pub const LEAF_VALIDITY_DAYS: i64 = 397; // the CA/Browser Forum's leaf maximum, as a familiar bound
/// Cluster CA validity (audit M3).
pub const CA_VALIDITY_DAYS: i64 = 3650;

fn now_utc() -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc()
}

impl ClusterCa {
    /// Mint a fresh cluster CA.
    pub fn new() -> Result<Self, TransportError> {
        ensure_provider();
        let ca_key = KeyPair::generate().map_err(cert_err)?;
        let mut params = CertificateParams::new(Vec::<String>::new()).map_err(cert_err)?;
        // **Audit M3 — the CA is path-length constrained and does not live forever.**
        //
        // `BasicConstraints::Unconstrained` lets any certificate this CA signs act as a CA itself
        // if it were ever issued with `IsCa` set — a property nothing in this project needs and
        // nobody would notice acquiring. `Constrained(0)` says what is true: this CA signs leaves.
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.not_before = now_utc();
        params.not_after = now_utc() + time::Duration::days(CA_VALIDITY_DAYS);
        params.distinguished_name.push(DnType::CommonName, "Hydra Cluster CA");
        params.key_usages =
            vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
        let ca_cert = params.self_signed(&ca_key).map_err(cert_err)?;
        Ok(Self { ca_cert, ca_key })
    }

    /// The CA's own certificate (the trust anchor to distribute at pairing).
    pub fn ca_cert_der(&self) -> CertificateDer<'static> {
        self.ca_cert.der().clone()
    }

    /// Issue a device identity with `name` as its DNS SAN + CN, usable for both client and
    /// server auth.
    pub fn issue(&self, name: &str) -> Result<DeviceIdentity, TransportError> {
        let key = KeyPair::generate().map_err(cert_err)?;
        let mut params = CertificateParams::new(vec![name.to_string()]).map_err(cert_err)?;
        params.distinguished_name.push(DnType::CommonName, name);
        // **Audit M3 — leaves expire.** `rcgen`'s defaults are 1975 → 4096, so a device key leaked
        // through H17's 0644 bootstrap file stayed valid **forever**, and with no CRL and no
        // rotation there was no mechanism by which it could ever stop being valid. A bounded
        // lifetime is not revocation, and is not claimed to be — it is the difference between a
        // leak that expires and a leak that does not.
        params.not_before = now_utc();
        params.not_after = now_utc() + time::Duration::days(LEAF_VALIDITY_DAYS);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages =
            vec![ExtendedKeyUsagePurpose::ServerAuth, ExtendedKeyUsagePurpose::ClientAuth];
        let cert = params.signed_by(&key, &self.ca_cert, &self.ca_key).map_err(cert_err)?;
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        Ok(DeviceIdentity {
            name: name.to_string(),
            cert_chain: vec![cert.der().clone(), self.ca_cert.der().clone()],
            key_der,
        })
    }

    fn roots(&self) -> Result<RootCertStore, TransportError> {
        let mut roots = RootCertStore::empty();
        roots.add(self.ca_cert.der().clone())?;
        Ok(roots)
    }

    /// Server config: presents `id`'s chain and **requires** a client cert signed by this CA.
    pub fn server_config(&self, id: &DeviceIdentity) -> Result<ServerConfig, TransportError> {
        ensure_provider();
        let roots = Arc::new(self.roots()?);
        let verifier = WebPkiClientVerifier::builder(roots).build().map_err(cert_err)?;
        let cfg = ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(id.cert_chain.clone(), id.key())?;
        Ok(cfg)
    }

    /// Client config: trusts this CA for the server cert and presents `id`'s chain for mTLS.
    /// (`id` may be signed by a *different* CA to exercise client-auth rejection.)
    pub fn client_config(&self, id: &DeviceIdentity) -> Result<ClientConfig, TransportError> {
        ensure_provider();
        let roots = self.roots()?;
        let cfg = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_client_auth_cert(id.cert_chain.clone(), id.key())?;
        Ok(cfg)
    }
}
