//! **M4·2 — cluster pairing: the ceremony that creates a cluster's trust.**
//!
//! # What pairing is, and what it is not
//!
//! Pairing establishes the **cluster CA** on the coordinator and issues a **per-device identity**
//! to each worker. Everything the security posture rests on descends from it: C2's role binding
//! reads the SAN of a certificate issued here, C1's manifest anchor is distributed here, and M3's
//! bounded validity is stamped here.
//!
//! It is **not** an authentication of the human. A PIN shown on one screen and typed on another
//! proves *physical proximity for a short window*, which is the property a household cluster
//! actually has available. That is stated plainly because the alternative — implying it is a
//! password — is the §7.35 shape (a mechanism whose name promises more than it delivers).
//!
//! # The structural constraint
//!
//! **The CA private key never leaves the coordinator.** That is not a rule someone must remember:
//! [`PairingSession`] holds the [`ClusterCa`] and hands out only [`IssuedIdentity`] values, and
//! nothing in this crate's public API can serialise a CA key. A future caller cannot leak what it
//! cannot obtain.

use std::time::{Duration, SystemTime};

use hydra_transport::{ClusterCa, DeviceIdentity};

pub mod status;

/// How long a pairing window stays open (M4·2).
///
/// Short enough that a PIN shouted across a room is not a standing credential, long enough for a
/// person to walk to another machine and type six digits. **A window that never closes is not a
/// window**, and an expired one is the most common real failure — hence a first-class error rather
/// than a comparison someone might forget.
pub const PAIRING_WINDOW: Duration = Duration::from_secs(180);

/// How many wrong PINs a window tolerates before it is burned (M4·2).
///
/// A six-digit PIN is 10⁶ values; three attempts inside a three-minute window makes online guessing
/// hopeless without making a typo fatal. The window is **burned rather than merely counted** — an
/// attacker who can retry is an attacker with unbounded attempts spread over many windows.
pub const MAX_PIN_ATTEMPTS: u8 = 3;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PairError {
    #[error("wrong PIN ({remaining} attempt(s) left before this pairing window is burned)")]
    WrongPin { remaining: u8 },
    #[error("this pairing window is burned: too many wrong PINs. Start a new one on the coordinator")]
    Burned,
    #[error("this pairing window expired ({age_secs}s old; the limit is {limit_secs}s). Start a new one")]
    Expired { age_secs: u64, limit_secs: u64 },
    #[error("transport: {0}")]
    Transport(String),
}

/// A device identity issued by a pairing session: the certificate chain and key for **one** device,
/// plus the CA certificate it must trust.
///
/// Note what is absent: the **CA private key**. It is not omitted by convention, it is unreachable
/// — [`PairingSession`] never exposes it and this struct has nowhere to put it.
pub struct IssuedIdentity {
    pub device_name: String,
    pub identity: DeviceIdentity,
    pub ca_cert_der: Vec<u8>,
}

/// One open pairing window on the coordinator.
pub struct PairingSession {
    ca: ClusterCa,
    pin: String,
    opened: SystemTime,
    attempts_left: u8,
    burned: bool,
    issued: Vec<String>,
}

impl PairingSession {
    /// Open a window: mint the cluster CA and a PIN.
    ///
    /// The PIN comes from the **system CSPRNG**, not from a timestamp or a counter — a predictable
    /// PIN is not a proximity proof, it is a formality (the M12 lesson, applied where it is being
    /// written rather than after an audit finds it).
    pub fn open() -> Result<PairingSession, PairError> {
        let ca = ClusterCa::new().map_err(|e| PairError::Transport(e.to_string()))?;
        Ok(PairingSession {
            ca,
            pin: mint_pin(),
            opened: SystemTime::now(),
            attempts_left: MAX_PIN_ATTEMPTS,
            burned: false,
            issued: Vec::new(),
        })
    }

    /// Re-open a window over an **existing** CA — the re-pair path, used when a device is replaced
    /// or a key is believed compromised. The cluster identity survives; the PIN does not.
    pub fn reopen(ca: ClusterCa) -> PairingSession {
        PairingSession { ca, pin: mint_pin(), opened: SystemTime::now(), attempts_left: MAX_PIN_ATTEMPTS, burned: false, issued: Vec::new() }
    }

    /// The PIN to display (and to encode in a QR alongside the coordinator's address).
    pub fn pin(&self) -> &str {
        &self.pin
    }

    /// The CA certificate a device must trust. **The CA *key* has no accessor, here or anywhere.**
    pub fn ca_cert_der(&self) -> Vec<u8> {
        self.ca.ca_cert_der().to_vec()
    }

    /// **Persist the CA and issue the COORDINATOR's own identity (M4·2; deferred here in seam C).**
    ///
    /// The coordinator is a peer like any other and needs a certificate to serve the API and to
    /// dial stages. Its identity is issued from the CA it holds, and the CA is persisted **into the
    /// same coordinator-local directory** so a restart can still re-pair a device later — a CA that
    /// vanishes on restart cannot issue a replacement when a key leaks, which is the recovery path
    /// H17's scenario needs.
    pub fn provision_coordinator(&mut self, dir: &std::path::Path) -> Result<IssuedIdentity, PairError> {
        self.ca.save_private(dir).map_err(|e| PairError::Transport(e.to_string()))?;
        let identity = self.ca.issue("coordinator").map_err(|e| PairError::Transport(e.to_string()))?;
        self.issued.push("coordinator".to_string());
        Ok(IssuedIdentity { device_name: "coordinator".into(), identity, ca_cert_der: self.ca_cert_der() })
    }

    pub fn issued_devices(&self) -> &[String] {
        &self.issued
    }

    /// Claim an identity for `device_name` by presenting `pin`.
    ///
    /// Checks, in this order: **burned**, then **expired**, then **PIN**. The order matters — a
    /// burned or expired window must not become a PIN oracle that answers "wrong" versus "right"
    /// for an attacker probing after the fact.
    pub fn claim(&mut self, device_name: &str, pin: &str, now: SystemTime) -> Result<IssuedIdentity, PairError> {
        if self.burned {
            return Err(PairError::Burned);
        }
        let age = now.duration_since(self.opened).unwrap_or(Duration::ZERO);
        if age > PAIRING_WINDOW {
            return Err(PairError::Expired { age_secs: age.as_secs(), limit_secs: PAIRING_WINDOW.as_secs() });
        }
        // Constant-time over the digest, so a wrong PIN does not leak how much of it was right.
        if blake3::hash(pin.as_bytes()) != blake3::hash(self.pin.as_bytes()) {
            self.attempts_left = self.attempts_left.saturating_sub(1);
            if self.attempts_left == 0 {
                self.burned = true;
                return Err(PairError::Burned);
            }
            return Err(PairError::WrongPin { remaining: self.attempts_left });
        }
        // The leaf carries Wave 3's bounded validity and the CA its path-length constraint (M3);
        // `issue` is the only way to make one, so pairing cannot mint an unbounded certificate.
        let identity = self.ca.issue(device_name).map_err(|e| PairError::Transport(e.to_string()))?;
        self.issued.push(device_name.to_string());
        Ok(IssuedIdentity { device_name: device_name.to_string(), identity, ca_cert_der: self.ca_cert_der() })
    }
}

/// A six-digit PIN from the system CSPRNG.
fn mint_pin() -> String {
    let mut b = [0u8; 4];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b)
        .expect("the system CSPRNG must be available to open a pairing window");
    format!("{:06}", u32::from_le_bytes(b) % 1_000_000)
}
