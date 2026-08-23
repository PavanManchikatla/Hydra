//! **Audit C2 — peer identity is bound to a ROLE at `accept()`, and the binding fails closed.**
//!
//! # The finding
//!
//! mTLS answers *"is this peer in the cluster?"*. It has never answered *"which stage is it?"* —
//! and every authorisation decision in the protocol is about the second question. Before this
//! module, **any certificate holder could send any message family**: a stage worker could issue
//! `COMMIT_ACTIVATION` (a coordinator's frame), the durability target could send `SAMPLED` (S_P's
//! frame), and the F1 fence would pass all of it, because the fence checks *session identity*, not
//! *sender role*. The cluster CA's signature was being read as an authorisation when it is only an
//! authentication.
//!
//! # Why it is bound at `accept()` and not at the message
//!
//! A per-message role check is a check that can be forgotten on the next message type added. Binding
//! at `accept()` makes the role a property of the **connection**, so every frame that arrives on it
//! carries a role by construction and the only remaining decision is the table lookup.
//!
//! # Fail closed — the three refusals that are one refusal
//!
//! Per the design authority (2026-08-23): *a connection whose peer cert has no parseable SAN, or a
//! SAN that maps to no configured role, is refused at accept, never treated as "unknown but
//! allowed"*. So all three of these are [`TransportError::UnboundPeer`]:
//!
//! 1. **no peer certificate** — cannot happen with a client-auth verifier, and is refused anyway
//!    rather than trusted to be impossible;
//! 2. **matches no configured name** — the peer is authentic and has no role here;
//! 3. **matches more than one** — ambiguous, and an ambiguous authorisation is a granted one.
//!
//! The third is the reason the check is a *match* and not a *lookup*: a cert may carry several SANs,
//! and picking "the first that matches" would let a peer holding a multi-SAN certificate choose its
//! own role by ordering.
//!
//! # Why this introduces no new parser
//!
//! The binding **tests a candidate name against the certificate** (`webpki`'s subject-name
//! verification) rather than **enumerating the certificate's SANs**. Enumeration would have meant an
//! X.509 SAN parser, which under standing rule 17 is itself a parser of untrusted input and would
//! need its own fuzz target and reservation audit. Testing borrows rustls's own vetted path instead.

use std::collections::BTreeMap;
use std::sync::Arc;

use rustls_pki_types::{CertificateDer, DnsName, ServerName};

use crate::TransportError;

/// What a peer is allowed to be. v1's topology is fixed (BLUEPRINT §1.5: one coordinator, 2–3 stage
/// workers, one durability target), so the set is closed and small — and a closed set is what lets
/// the gate be a match rather than a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PeerRole {
    /// The coordinator: owns the control plane and the commit stream.
    Coordinator,
    /// A pipeline stage, identified by its rank. `is_final` is not encoded here — rank is the
    /// identity; whether a rank is S_P is placement, and placement is the coordinator's to state.
    Stage { rank: u16 },
    /// The durability target for `BOUNDARY_COPY` (spec §7).
    DurabilityTarget,
}

impl PeerRole {
    /// A short label for error messages and logs.
    pub fn label(&self) -> String {
        match self {
            PeerRole::Coordinator => "coordinator".to_string(),
            PeerRole::Stage { rank } => format!("stage#{rank}"),
            PeerRole::DurabilityTarget => "durability-target".to_string(),
        }
    }
}

/// The configured name→role table for one endpoint.
///
/// **This is deliberately not a global registry.** Each listener is told the roles *it* expects to
/// see, so a stage worker's table need not contain every peer in the cluster — and a name absent
/// from the table is refused rather than resolved elsewhere.
#[derive(Debug, Clone, Default)]
pub struct RoleTable {
    by_name: BTreeMap<String, PeerRole>,
}

impl RoleTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `name` (the certificate's DNS SAN, as issued by `ClusterCa::issue`) as `role`.
    pub fn with(mut self, name: &str, role: PeerRole) -> Self {
        self.by_name.insert(name.to_string(), role);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Bind a peer certificate chain to exactly one configured role, or refuse.
    ///
    /// Returns [`TransportError::UnboundPeer`] for **all** of: no certificate, an unparseable
    /// certificate, a match against no configured name, and a match against more than one.
    pub fn bind(&self, peer_certs: Option<&[CertificateDer<'_>]>) -> Result<BoundPeer, TransportError> {
        // An empty table would make every peer unbindable, which is a fail-closed outcome but a
        // useless one — and almost certainly a misconfiguration rather than an intent. Say so.
        if self.is_empty() {
            return Err(TransportError::UnboundPeer(
                "the role table is EMPTY: no peer can be bound to a role, so every connection would \
                 be refused. This is a configuration error, not a policy — name the peers this \
                 endpoint expects (audit C2)"
                    .into(),
            ));
        }

        let leaf = match peer_certs.and_then(|c| c.first()) {
            Some(c) => c,
            None => {
                return Err(TransportError::UnboundPeer(
                    "peer presented NO certificate. The client-auth verifier should make this \
                     unreachable — it is refused rather than trusted to be impossible (audit C2)"
                        .into(),
                ))
            }
        };
        let cert = webpki::EndEntityCert::try_from(leaf).map_err(|e| {
            TransportError::UnboundPeer(format!(
                "peer certificate does not parse as an end-entity certificate ({e:?}); refused \
                 rather than treated as unknown-but-allowed (audit C2)"
            ))
        })?;

        // Test each CONFIGURED name against the certificate. Not "read the cert's SANs and look one
        // up" — see the module docs: enumeration would need an X.509 SAN parser, and a first-match
        // lookup would let a multi-SAN peer choose its own role by ordering.
        let mut matched: Vec<(&str, PeerRole)> = Vec::new();
        for (name, role) in &self.by_name {
            let Ok(dns) = DnsName::try_from(name.as_str()) else {
                // A configured name that is not a legal DNS name can never match; that is a
                // configuration defect and is reported rather than silently skipped.
                return Err(TransportError::UnboundPeer(format!(
                    "configured role name {name:?} is not a valid DNS name, so it can never match a \
                     certificate — configuration defect (audit C2)"
                )));
            };
            if cert.verify_is_valid_for_subject_name(&ServerName::DnsName(dns)).is_ok() {
                matched.push((name.as_str(), *role));
            }
        }

        match matched.as_slice() {
            [(name, role)] => Ok(BoundPeer { name: (*name).to_string(), role: *role }),
            [] => Err(TransportError::UnboundPeer(format!(
                "peer certificate is authentic but matches NONE of this endpoint's {} configured \
                 role name(s) {:?}. Authentic is not authorised: refused (audit C2)",
                self.by_name.len(),
                self.by_name.keys().collect::<Vec<_>>()
            ))),
            many => Err(TransportError::UnboundPeer(format!(
                "peer certificate matches {} configured names {:?} — AMBIGUOUS. An ambiguous \
                 authorisation is a granted one, so this is refused rather than resolved by taking \
                 the first (audit C2)",
                many.len(),
                many.iter().map(|(n, _)| *n).collect::<Vec<_>>()
            ))),
        }
    }
}

/// A peer whose certificate has been bound to exactly one role. Holding one is the proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundPeer {
    /// The certificate name that matched.
    pub name: String,
    /// The role it is configured as.
    pub role: PeerRole,
}

/// A role table plus the mTLS acceptor, so a listener cannot be constructed without one.
pub type SharedRoleTable = Arc<RoleTable>;
