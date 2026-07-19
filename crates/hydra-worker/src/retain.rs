//! R3′ boundary retention (spec §5, retries & retention).
//!
//! A stage that forwards boundaries downstream must **retain** each one until it may be safely
//! released. Release requires the downstream `APPLIED_ACK ≥ p` **and**, in a durability-required
//! mode (D1), the `DURABILITY_ACK ≥ p` — the boundary is the substrate a replacement S_P is rebuilt
//! from, so it may not be dropped while it is still recovery-relevant. A boundary above the release
//! watermark is **still needed for recovery and MUST NOT be dropped**.
//!
//! This is a pure retention policy — no I/O. The serve loop retains each forwarded boundary, feeds
//! back the two acks, and asks [`R3Buffer::release`] what may be dropped.

use std::collections::BTreeMap;

/// The R3′ retain buffer for one stage edge.
#[derive(Debug)]
pub struct R3Buffer {
    /// D1 (durability-required) needs `DURABILITY_ACK`; D0 releases on the downstream ack alone.
    require_durable: bool,
    /// Retained boundaries keyed by input position (ordered, so release is a prefix drain).
    buffered: BTreeMap<i64, Vec<f32>>,
    /// Downstream has applied through this input position (`APPLIED_ACK`).
    applied_through: i64,
    /// The durability target has made durable through this input position (`DURABILITY_ACK`).
    durable_through: i64,
}

impl R3Buffer {
    pub fn new(require_durable: bool) -> Self {
        R3Buffer { require_durable, buffered: BTreeMap::new(), applied_through: -1, durable_through: -1 }
    }

    /// Retain a forwarded boundary (it has been sent downstream but may not yet be released).
    pub fn retain(&mut self, input_pos: i64, boundary: Vec<f32>) {
        self.buffered.insert(input_pos, boundary);
    }

    /// Record a downstream `APPLIED_ACK` (monotone).
    pub fn on_applied_ack(&mut self, through_input_pos: i64) {
        self.applied_through = self.applied_through.max(through_input_pos);
    }

    /// Record a `DURABILITY_ACK` from the durability target (monotone).
    pub fn on_durability_ack(&mut self, through_input_pos: i64) {
        self.durable_through = self.durable_through.max(through_input_pos);
    }

    /// The highest input position that may be released under R3′: the downstream ack, further gated
    /// by durability when the mode requires it (`min` — BOTH conditions must hold).
    pub fn release_watermark(&self) -> i64 {
        if self.require_durable {
            self.applied_through.min(self.durable_through)
        } else {
            self.applied_through
        }
    }

    /// Drop and return the input positions of all boundaries at or below the release watermark. A
    /// boundary above the watermark (still recovery-relevant) is kept.
    pub fn release(&mut self) -> Vec<i64> {
        let w = self.release_watermark();
        let released: Vec<i64> = self.buffered.range(..=w).map(|(&p, _)| p).collect();
        for p in &released {
            self.buffered.remove(p);
        }
        released
    }

    /// Is this boundary still retained (i.e., still available for a recovery rebuild)?
    pub fn is_retained(&self, input_pos: i64) -> bool {
        self.buffered.contains_key(&input_pos)
    }

    /// The retained boundary at `input_pos`, if any (for a recovery replay).
    pub fn get(&self, input_pos: i64) -> Option<&[f32]> {
        self.buffered.get(&input_pos).map(|v| v.as_slice())
    }

    /// All currently-retained input positions, ascending.
    pub fn retained(&self) -> Vec<i64> {
        self.buffered.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d1_never_releases_a_boundary_before_its_durability_ack() {
        // The core R3′ guarantee: in D1, a boundary the downstream has already applied is STILL
        // retained until it is durable — because it is the substrate a replacement S_P rebuilds from.
        let mut b = R3Buffer::new(true);
        for p in 0..3 {
            b.retain(p, vec![p as f32; 4]);
        }
        // Downstream applied everything, but nothing is durable yet.
        b.on_applied_ack(2);
        assert_eq!(b.release_watermark(), -1, "durability gates release: min(2, -1) = -1");
        assert!(b.release().is_empty(), "no boundary may be released before its DURABILITY_ACK");
        assert!(b.is_retained(0) && b.is_retained(2), "all still retained for recovery");

        // Durability catches up to position 1: 0 and 1 may release; 2 stays (not yet durable).
        b.on_durability_ack(1);
        assert_eq!(b.release_watermark(), 1);
        assert_eq!(b.release(), vec![0, 1]);
        assert!(!b.is_retained(0) && b.is_retained(2), "position 2 is still needed for recovery");

        b.on_durability_ack(2);
        assert_eq!(b.release(), vec![2]);
        assert!(b.retained().is_empty());
    }

    #[test]
    fn d0_releases_on_the_downstream_ack_alone() {
        let mut b = R3Buffer::new(false);
        b.retain(0, vec![1.0]);
        b.on_applied_ack(0); // no durability in D0
        assert_eq!(b.release(), vec![0]);
    }
}
