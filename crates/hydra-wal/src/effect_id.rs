//! Effect IDs (WAL-FORMAT.md §4): replay-deterministic identity for state-machine effects.
//!
//! `blake3(session_id || session_epoch || recovery_id || activation_attempt_id || effect_kind
//! || monotonic_seq)` truncated to u64. Identical (state, event) inputs yield identical ids, so
//! the runtime can deduplicate effect execution across coordinator restarts.

/// Compute a stable effect id. `monotonic_seq` is per-(session, epoch) and part of the state
/// machine's replayed state.
pub fn effect_id(
    session_id: &[u8; 16],
    session_epoch: u32,
    recovery_id: u32,
    activation_attempt_id: u32,
    effect_kind: u16,
    monotonic_seq: u64,
) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(session_id);
    h.update(&session_epoch.to_le_bytes());
    h.update(&recovery_id.to_le_bytes());
    h.update(&activation_attempt_id.to_le_bytes());
    h.update(&effect_kind.to_le_bytes());
    h.update(&monotonic_seq.to_le_bytes());
    let digest = h.finalize();
    u64::from_le_bytes(digest.as_bytes()[..8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_sensitive() {
        let sid = [1u8; 16];
        let a = effect_id(&sid, 2, 3, 4, 5, 6);
        let b = effect_id(&sid, 2, 3, 4, 5, 6);
        assert_eq!(a, b, "identical inputs => identical id");
        // any input change perturbs the id
        assert_ne!(a, effect_id(&sid, 2, 3, 4, 5, 7));
        assert_ne!(a, effect_id(&sid, 2, 3, 4, 6, 6));
        assert_ne!(a, effect_id(&[2u8; 16], 2, 3, 4, 5, 6));
    }
}
