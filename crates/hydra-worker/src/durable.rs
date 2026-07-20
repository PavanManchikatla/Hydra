//! Boundary durability on the forwarding stage (P1·1b seam A): the seam-2 R3′ retention policy
//! ([`crate::retain::R3Buffer`]) wired to the live data plane.
//!
//! In a direct-FWD pipeline the boundary tensor travels **S1→S2 directly** (worker→worker), so the
//! coordinator never sees it on the compute path. For a D1 recovery a replacement downstream is
//! rebuilt from **durable boundaries**, so the forwarding stage must also send a **`BOUNDARY_COPY`**
//! to the durability target (the coordinator's `BoundaryStore`, spec §7) — a *background-class* copy
//! alongside the direct FWD — and **retain** each boundary until it is safe to drop under R3′:
//! downstream `APPLIED_ACK ≥ p` **and**, in D1, `DURABILITY_ACK ≥ p`. A boundary above that watermark
//! is still recovery-relevant and must not be released.
//!
//! [`DurableForwarder`] is pure state + the copy-send I/O; the caller owns the durability connection
//! and feeds back the two acks. The R3′ *policy* is unchanged (`R3Buffer`); this is the wiring.

use hydra_state::Epoch;
use hydra_transport::framed::Conn;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::retain::R3Buffer;
use crate::wire::{self, SessionKeys};
use crate::worker::WorkerError;

/// Forwarding-stage boundary durability: emit `BOUNDARY_COPY` + retain under R3′ until both acks clear.
pub struct DurableForwarder {
    keys: SessionKeys,
    epoch: Epoch,
    retain: R3Buffer,
    /// Monotone id assigned to each forwarded boundary (correlates its `DURABILITY_ACK`).
    next_boundary_id: u32,
}

impl DurableForwarder {
    /// `require_durable = true` is D1 (release gates on `DURABILITY_ACK` too); `false` is D0 (release
    /// on the downstream ack alone).
    pub fn new(keys: SessionKeys, epoch: Epoch, require_durable: bool) -> DurableForwarder {
        DurableForwarder { keys, epoch, retain: R3Buffer::new(require_durable), next_boundary_id: 0 }
    }

    /// Emit a `BOUNDARY_COPY` for `boundary` on the durability connection (the background-class copy,
    /// alongside the direct FWD) and retain it under R3′. Returns the assigned `boundary_id`.
    pub async fn copy_and_retain<D>(&mut self, dur: &mut Conn<D>, input_pos: i64, boundary: &[f32]) -> Result<u32, WorkerError>
    where
        D: AsyncRead + AsyncWrite + Unpin,
    {
        let bid = self.next_boundary_id;
        self.next_boundary_id += 1;
        dur.send(0, &wire::encode_boundary_copy(&self.keys, self.epoch, bid, input_pos, 0, boundary)).await?;
        self.retain.retain(input_pos, boundary.to_vec());
        Ok(bid)
    }

    /// Record a downstream `APPLIED_ACK` (the compute path applied through this input position).
    pub fn on_applied_ack(&mut self, through_input_pos: i64) {
        self.retain.on_applied_ack(through_input_pos);
    }

    /// Record a `DURABILITY_ACK` (the durability target made durable through this input position).
    pub fn on_durability_ack(&mut self, durable_through_input_pos: i64) {
        self.retain.on_durability_ack(durable_through_input_pos);
    }

    /// Drop and return every boundary now releasable under R3′ (downstream-applied **and**, in D1,
    /// durable). A boundary still needed for recovery — applied but not yet durable — is **kept**.
    pub fn release(&mut self) -> Vec<i64> {
        self.retain.release()
    }

    /// The highest input position releasable right now (`min(applied, durable)` in D1).
    pub fn release_watermark(&self) -> i64 {
        self.retain.release_watermark()
    }

    /// Is this boundary still retained (still available for a recovery rebuild)?
    pub fn is_retained(&self, input_pos: i64) -> bool {
        self.retain.is_retained(input_pos)
    }

    /// All currently-retained input positions, ascending.
    pub fn retained(&self) -> Vec<i64> {
        self.retain.retained()
    }
}
