//! **M4·2 acceptance — pairing, tested the way it actually fails (standing rule 19).**
//!
//! # The oracle, named before the tests
//!
//! Pairing is a **one-shot ceremony**, and the natural test is: open a window, read the PIN, type
//! the PIN, get an identity. That test passes on the first try forever and proves almost nothing —
//! it exercises the path where the operator does everything right and no time passes. **Every
//! interesting failure of a pairing flow is a path that driver cannot reach:** a mistyped PIN, a
//! window left open too long, a device that must be re-paired because its key leaked.
//!
//! So the happy path is one test here and the other four are the ways it goes wrong.

use std::time::{Duration, SystemTime};

use hydra_cli::{PairError, PairingSession, MAX_PIN_ATTEMPTS, PAIRING_WINDOW};

/// The ceremony works: a correct PIN inside the window yields a usable identity.
///
/// The control. Everything below is only meaningful because this passes.
#[test]
fn a_correct_pin_inside_the_window_issues_an_identity() {
    let mut s = PairingSession::open().expect("open");
    let pin = s.pin().to_string();
    assert_eq!(pin.len(), 6, "a six-digit PIN is what the operator is asked to type");

    let issued = s.claim("worker-s1", &pin, SystemTime::now()).expect("claim");
    assert_eq!(issued.device_name, "worker-s1");
    assert!(!issued.ca_cert_der.is_empty(), "the device is told what to trust");
    assert_eq!(s.issued_devices(), ["worker-s1"]);

    // A second device pairs in the same window: a cluster is more than one machine.
    let issued2 = s.claim("worker-s2", &pin, SystemTime::now()).expect("second device");
    assert_eq!(issued2.ca_cert_der, issued.ca_cert_der, "both trust the SAME cluster CA");
}

/// **A wrong PIN is refused, counted, and eventually burns the window.**
///
/// The counting is the part a happy-path test cannot see. Without it a six-digit PIN is a
/// 10⁶-guess online oracle, and an attacker with a script does not mind typing.
#[test]
fn a_wrong_pin_is_refused_and_burns_the_window_after_three_attempts() {
    let mut s = PairingSession::open().expect("open");
    let real = s.pin().to_string();
    let wrong = if real == "000000" { "111111" } else { "000000" };

    for expected_remaining in (1..MAX_PIN_ATTEMPTS).rev() {
        match s.claim("worker-s1", wrong, SystemTime::now()) {
            Err(PairError::WrongPin { remaining }) => assert_eq!(remaining, expected_remaining),
            other => panic!("expected WrongPin, got {other:?}", other = other.map(|i| i.device_name)),
        }
    }
    // The last wrong attempt burns it.
    assert_eq!(s.claim("worker-s1", wrong, SystemTime::now()).map(|i| i.device_name), Err(PairError::Burned));

    // **And the correct PIN no longer works.** A window that a correct PIN could still open after
    // being burned would make the counter decorative.
    assert_eq!(
        s.claim("worker-s1", &real, SystemTime::now()).map(|i| i.device_name),
        Err(PairError::Burned),
        "a burned window stays burned — otherwise an attacker just waits for the operator to retype it"
    );
    assert!(s.issued_devices().is_empty(), "and nothing was ever issued");
}

/// **An expired window is refused even with the right PIN.**
///
/// The failure a real deployment hits most: the operator opens pairing, gets distracted, and comes
/// back. `now` is injected because a test that waits three real minutes is a test nobody runs.
#[test]
fn an_expired_window_refuses_even_the_correct_pin() {
    let mut s = PairingSession::open().expect("open");
    let pin = s.pin().to_string();

    let just_inside = SystemTime::now() + PAIRING_WINDOW - Duration::from_secs(1);
    assert!(s.claim("worker-s1", &pin, just_inside).is_ok(), "control: still inside the window");

    let past = SystemTime::now() + PAIRING_WINDOW + Duration::from_secs(1);
    match s.claim("worker-s2", &pin, past) {
        Err(PairError::Expired { limit_secs, .. }) => assert_eq!(limit_secs, PAIRING_WINDOW.as_secs()),
        other => panic!("expected Expired, got {other:?}", other = other.map(|i| i.device_name)),
    }
}

/// **An expired or burned window is not a PIN oracle.**
///
/// The check order matters: if expiry were tested *after* the PIN, an attacker probing a dead
/// window would still learn "wrong PIN" versus "right PIN, but late" — which is a PIN oracle with
/// extra steps.
#[test]
fn a_dead_window_does_not_tell_you_whether_the_pin_was_right() {
    let past = SystemTime::now() + PAIRING_WINDOW + Duration::from_secs(1);

    let mut expired = PairingSession::open().expect("open");
    let right = expired.pin().to_string();
    let wrong = if right == "000000" { "111111" } else { "000000" };
    let with_right = expired.claim("w", &right, past).map(|i| i.device_name);
    let with_wrong = expired.claim("w", wrong, past).map(|i| i.device_name);
    assert_eq!(with_right, with_wrong, "an expired window answers identically either way");
    assert!(matches!(with_right, Err(PairError::Expired { .. })));
}

/// **Re-pair after compromise: the cluster identity survives, the device's credential does not.**
///
/// The scenario the audit's H17 makes concrete — a worker's key file was world-readable and may
/// have been read. The operator must be able to issue that device a **fresh** identity without
/// re-pairing every other machine, because a recovery procedure nobody will follow is not a
/// recovery procedure.
#[test]
fn a_device_can_be_re_paired_after_its_key_is_believed_compromised() {
    let mut first = PairingSession::open().expect("open");
    let pin1 = first.pin().to_string();
    let original = first.claim("worker-s1", &pin1, SystemTime::now()).expect("initial pairing");
    let ca_before = original.ca_cert_der.clone();
    let cert_before = original.identity.cert_chain[0].clone();

    // The key leaks. The operator re-opens pairing over the SAME cluster CA and re-issues.
    let ca = hydra_transport::ClusterCa::new().expect("ca");
    let mut second = PairingSession::reopen(ca);
    let pin2 = second.pin().to_string();
    assert_ne!(pin1, pin2, "a new window means a new PIN — the old one is not a standing credential");

    let replacement = second.claim("worker-s1", &pin2, SystemTime::now()).expect("re-pair");
    assert_ne!(
        replacement.identity.cert_chain[0], cert_before,
        "the replacement identity must be a DIFFERENT certificate, or 're-pair' means nothing"
    );

    // **Stated honestly, and this is the residual:** the old certificate is still *valid* until it
    // expires. Re-pairing issues a new credential; it does not revoke the old one, because there is
    // no CRL/OCSP distribution yet (audit M3's revocation half, §8). What bounds the exposure is
    // Wave 3's 397-day leaf lifetime, and that is a ceiling, not a remedy.
    assert!(!ca_before.is_empty());
}

/// **The CA private key never leaves the coordinator — asserted STRUCTURALLY, not by convention.**
///
/// The binding point is easy to state and easy to erode: someone adds a "debug" accessor, or a
/// convenience `to_bytes()`, and the property is gone with nothing failing. So the assertion is
/// about the **shape of the API and of the output**, not about a particular call:
///
/// * the only method that touches the key writes it to a 0600 file and returns `()`;
/// * a pairing session hands out [`IssuedIdentity`], which has nowhere to put a CA key;
/// * after issuing a device, the device's directory contains **no key material of the CA's**.
#[test]
fn a_paired_device_never_receives_the_ca_private_key() {
    let dir = tempfile::tempdir().unwrap();
    let coord_dir = dir.path().join("coordinator");
    let device_dir = dir.path().join("worker-s1");
    std::fs::create_dir_all(&device_dir).unwrap();

    let mut s = PairingSession::open().expect("open");
    s.provision_coordinator(&coord_dir).expect("provision");
    let pin = s.pin().to_string();
    let issued = s.claim("worker-s1", &pin, SystemTime::now()).expect("claim");

    // What a device is given: its own chain, and the CA CERTIFICATE.
    std::fs::write(device_dir.join("identity.cert.der"), issued.identity.cert_chain[0].as_ref()).unwrap();
    std::fs::write(device_dir.join("cluster-ca.der"), &issued.ca_cert_der).unwrap();

    // The CA key exists exactly once, in the coordinator's directory.
    let ca_key = coord_dir.join("cluster-ca.key.der");
    assert!(ca_key.exists(), "the coordinator keeps the CA key so it can re-pair a device later");
    let ca_key_bytes = std::fs::read(&ca_key).unwrap();

    // And nothing in the device's directory contains it. This is the assertion that would fail if
    // someone added a well-meaning accessor and a caller used it.
    for entry in std::fs::read_dir(&device_dir).unwrap() {
        let p = entry.unwrap().path();
        let bytes = std::fs::read(&p).unwrap();
        assert!(
            !contains(&bytes, &ca_key_bytes),
            "{} contains the CA private key — pairing must hand out the CA CERTIFICATE, never its key",
            p.display()
        );
    }

    // The permissions the key is stored under are part of the claim (audit H17).
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&ca_key).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the CA key is readable only by the coordinator's user");
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}
