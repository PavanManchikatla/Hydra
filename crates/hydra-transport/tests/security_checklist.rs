//! **M4·1 (b) — the security checklist, AS TESTS.**
//!
//! BLUEPRINT §3 (M4 DoD): *"security checklist from report Addendum 2 §E1/D1 passes (no 0.0.0.0
//! binds, API auth enforced, GGUF parser fuzzed for 24 CPU-hours without crashes)."*
//!
//! `docs/SECURITY-CHECKLIST.md` maps every checklist line to the assertion that proves it. This
//! file holds the ones that are properties of the **transport and the repository as a whole**; the
//! API-auth half lives with the API (`hydra-coordinator/tests/session_http.rs`) and the parser half
//! with the parsers.
//!
//! The repository-wide tests below read the source tree. That is unusual for a unit test and it is
//! deliberate: a policy like *"nothing binds `0.0.0.0`"* is a property of the **whole tree**, and
//! the only way to keep it true next month is to assert it over the whole tree. A reviewer's
//! promise decays; a test does not.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use hydra_transport::roles::{PeerRole, RoleTable};
use hydra_transport::{check_bind_addr, ClusterCa, TransportError, ALLOW_WILDCARD_BIND_ENV};

/// A minimal role table for the checklist's own listeners (audit C2 makes one mandatory). These
/// tests are about binds and handshakes, not authorisation, so the table names only what they dial.
fn checklist_roles() -> RoleTable {
    RoleTable::new()
        .with("worker-client", PeerRole::Coordinator)
        .with("checklist-node", PeerRole::Stage { rank: 0 })
}

fn repo_root() -> PathBuf {
    // crates/hydra-transport/ -> ../..
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").canonicalize().expect("repo root")
}

/// Every `.rs` file under `crates/`, with its repo-relative path.
fn rust_sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                // Generated FlatBuffers code is not hand-written policy surface.
                if p.file_name().is_some_and(|n| n == "generated" || n == "target") {
                    continue;
                }
                walk(&p, root, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                if let Ok(src) = std::fs::read_to_string(&p) {
                    let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().into_owned();
                    out.push((rel, src));
                }
            }
        }
    }
    let root = repo_root();
    let mut out = Vec::new();
    walk(&root.join("crates"), &root, &mut out);
    assert!(out.len() > 20, "source walk found only {} files — the walk is broken, not the tree", out.len());
    out
}

// ---------------------------------------------------------------------------------------------
// §E1 — "must not bind 0.0.0.0 by default"
// ---------------------------------------------------------------------------------------------

/// The bind policy itself: an unspecified address is **refused**, a real interface address is not.
///
/// This is checked before the socket is created, so there is no window in which the port is open on
/// every interface while the decision is being made.
#[test]
fn a_wildcard_bind_is_refused_and_an_explicit_interface_is_not() {
    // Guard against a stray opt-in in the ambient environment making this test vacuous.
    assert!(
        std::env::var(ALLOW_WILDCARD_BIND_ENV).is_err(),
        "{ALLOW_WILDCARD_BIND_ENV} is set in this environment; the refusal test would be vacuous"
    );

    for wildcard in ["0.0.0.0:8080", "[::]:8080", "0.0.0.0:0"] {
        let addr: SocketAddr = wildcard.parse().unwrap();
        let err = check_bind_addr(addr).unwrap_err();
        assert!(
            matches!(err, TransportError::WildcardBind(a) if a == addr),
            "{wildcard} must be refused, got {err:?}"
        );
    }

    // Control: explicit addresses pass, so the refusals above are caused by the wildcard and not by
    // a check that refuses everything.
    for ok in ["127.0.0.1:8080", "192.168.1.10:9000", "[::1]:8080"] {
        check_bind_addr(ok.parse().unwrap()).unwrap_or_else(|e| panic!("{ok} must be allowed, got {e}"));
    }
}

/// A real listener refuses a wildcard bind — the policy is on the path every listener takes, not
/// merely available to be called.
#[tokio::test]
async fn the_real_listener_refuses_a_wildcard_bind() {
    let ca = ClusterCa::new().unwrap();
    let id = ca.issue("checklist-node").unwrap();

    match hydra_transport::tcp_mtls::TcpMtlsListener::bind("0.0.0.0:0".parse().unwrap(), &ca, &id, checklist_roles()).await {
        Err(TransportError::WildcardBind(_)) => {}
        Err(other) => panic!("wrong refusal for a wildcard bind: {other:?}"),
        Ok(_) => panic!("a wildcard bind must not produce a listener"),
    }

    // Control: loopback binds fine with the same CA and identity.
    let l = hydra_transport::tcp_mtls::TcpMtlsListener::bind("127.0.0.1:0".parse().unwrap(), &ca, &id, checklist_roles())
        .await
        .expect("loopback must bind");
    assert!(l.local_addr().unwrap().ip().is_loopback());
}

/// **Repository-wide:** exactly one place in the tree opts into a wildcard bind, and it is the
/// containerised CI runner, whose network namespace *is* the isolation boundary and whose port is
/// published only on `127.0.0.1`.
///
/// The value of this assertion is not today's count — it is that adding a second opt-in becomes a
/// deliberate act that has to edit this list and explain itself.
#[test]
fn only_the_container_ci_runner_opts_into_a_wildcard_bind() {
    const EXPECTED: &str = "crates/hydra-worker/src/bin/hydra-2node-ci.rs";
    // Two needles, because there are two ways to opt in: naming the constant, or writing the raw
    // environment-variable string. Searching for only the first would miss the second, and a
    // security assertion that misses the obvious bypass is not one.
    let opts_in = |src: &str| src.contains("ALLOW_WILDCARD_BIND_ENV") || src.contains(ALLOW_WILDCARD_BIND_ENV);
    // The crate that *defines* the constant and the test that *asserts on it* naturally mention it;
    // an opt-in is a use of the name anywhere else.
    const DEFINES_OR_ASSERTS: [&str; 2] =
        ["crates/hydra-transport/src/lib.rs", "crates/hydra-transport/tests/security_checklist.rs"];
    let offenders: Vec<String> = rust_sources()
        .into_iter()
        .filter(|(path, src)| {
            path != EXPECTED
                && !DEFINES_OR_ASSERTS.contains(&path.as_str())
                && opts_in(src)
        })
        .map(|(path, _)| path)
        .collect();
    assert!(
        offenders.is_empty(),
        "unexpected wildcard-bind opt-in(s): {offenders:?}. A wildcard bind is the precondition of \
         the DNS-rebinding attack in report Addendum 2 §E1. If a new one is genuinely needed, it \
         must be added here with its justification."
    );

    let has_it = rust_sources().into_iter().any(|(p, s)| p == EXPECTED && opts_in(&s));
    assert!(has_it, "{EXPECTED} no longer carries the opt-in — if the wildcard bind was removed, remove this test too");
}

/// **Repository-wide:** no source file hard-codes a `0.0.0.0` listen address except the container
/// runner (which is covered by the opt-in test above). Comments and doc-lines *about* the policy
/// are fine — they are how the rule stays legible.
#[test]
fn no_source_file_hardcodes_a_wildcard_listen_address() {
    const ALLOWED: [&str; 5] = [
        "crates/hydra-worker/src/bin/hydra-2node-ci.rs", // namespaced container, published on 127.0.0.1
        "crates/hydra-transport/src/lib.rs",             // defines and documents the policy
        "crates/hydra-transport/tests/security_checklist.rs", // this file
        "crates/hydra-worker/src/bin/hydra-wan.rs",      // comments only: "bind here ONLY, never 0.0.0.0"
        // Audit M2: the test that asserts EVERY spelling of the wildcard is refused — including
        // `::ffff:0.0.0.0`, which this grep would not have caught and `is_unspecified` did not.
        // It must name the addresses to refuse them; being flagged here is the guard working.
        "crates/hydra-transport/tests/platform_hardening.rs",
    ];
    let mut offenders = Vec::new();
    for (path, src) in rust_sources() {
        if ALLOWED.contains(&path.as_str()) {
            continue;
        }
        for (i, line) in src.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            // Audit M2: the grep now also looks for the IPv4-mapped spelling, since that is the
            // one `is_unspecified` missed and therefore the one a source file could carry while
            // every existing check called it safe.
            if code.contains("0.0.0.0") || code.contains("[::]:") || code.contains("::ffff:0:0") {
                offenders.push(format!("{path}:{}", i + 1));
            }
        }
    }
    assert!(offenders.is_empty(), "wildcard listen address in non-allow-listed code: {offenders:?}");
}

// ---------------------------------------------------------------------------------------------
// mTLS on every link
// ---------------------------------------------------------------------------------------------

/// **Every link is mTLS, and there is no plaintext transport to fall back to.**
///
/// The checklist item is not "TLS is available" but "there is no way to talk to a worker without
/// it". A plaintext path that exists for testing is a plaintext path that exists.
#[test]
fn the_transport_exposes_no_plaintext_path() {
    let offenders: Vec<String> = rust_sources()
        .into_iter()
        .filter(|(path, _)| path.starts_with("crates/hydra-transport/src/"))
        .flat_map(|(path, src)| {
            src.lines()
                .enumerate()
                .filter(|(_, l)| {
                    let code = l.split("//").next().unwrap_or("");
                    // A raw `TcpStream::connect` or `TcpListener::bind` is fine only where it is
                    // immediately wrapped by the TLS connector/acceptor in `tcp_mtls.rs`.
                    code.contains("TcpStream::connect") || code.contains("TcpListener::bind")
                })
                .map(|(i, _)| format!("{path}:{}", i + 1))
                .collect::<Vec<_>>()
        })
        .filter(|loc| !loc.starts_with("crates/hydra-transport/src/tcp_mtls.rs"))
        .collect();
    assert!(
        offenders.is_empty(),
        "raw TCP outside the mTLS module: {offenders:?}. Every link in the cluster is mutually \
         authenticated; a bare socket elsewhere in the transport crate is a bypass."
    );
}

/// A peer whose certificate is not signed by **this** cluster's CA is rejected at the handshake —
/// the property that makes mTLS the trust boundary rather than decoration.
#[tokio::test]
async fn a_peer_from_a_foreign_ca_is_rejected_at_the_handshake() {
    let ours = ClusterCa::new().unwrap();
    let theirs = ClusterCa::new().unwrap();
    let server_id = ours.issue("worker").unwrap();
    let rogue_id = theirs.issue("worker-client").unwrap();

    let listener =
        hydra_transport::tcp_mtls::TcpMtlsListener::bind("127.0.0.1:0".parse().unwrap(), &ours, &server_id, checklist_roles())
            .await
            .unwrap();
    let addr = listener.local_addr().unwrap();
    let accept = tokio::spawn(async move { listener.accept().await.map(|_| ()) });

    // A client holding a cert from a DIFFERENT cluster CA.
    let rogue = hydra_transport::tcp_mtls::TcpMtls::from_config(theirs.client_config(&rogue_id).unwrap()).unwrap();
    let client = rogue.connect(addr, "worker").await;

    // One side or the other must fail; a successful handshake on both sides is the failure.
    let server = accept.await.unwrap();
    assert!(
        client.is_err() || server.is_err(),
        "a rogue-CA peer completed the handshake — the cluster CA is not the trust boundary"
    );
}
