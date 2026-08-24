//! **M4·0 — the coordinator activation driver: a thin effect executor.**
//!
//! # What this closes
//!
//! BLUEPRINT §2: *"Any protocol behavior implemented outside `hydra-state` is a defect."* The
//! **stage** side has always honoured that — `Worker::on_frame` decodes, steps the real `Stage` SM,
//! and executes what it returns. The **coordinator** side did not: `hydra_state::Coordinator` was
//! constructed in exactly one place in the workspace (`hydra-sim`), no shipping binary owned one,
//! and every non-test activation was a hand-rolled `COMMIT_ACTIVATION` → `FINALIZE_ACTIVATION`
//! pair with `activation_attempt_id` hard-coded to `1`, no ack collection, no intent record, no
//! completion record, no abort path and no unservable path.
//!
//! So the coordinator SM was **verified but not deployed**, and TLC's and the DST sim's guarantees
//! did not reach the shipping path. This module is the missing layer, and it is deliberately thin:
//!
//! ```text
//!   effects  →  frames   (execute what the SM decided)
//!   frames   →  events   (tell the SM what happened)
//! ```
//!
//! **No protocol decision is taken here.** There is no branch in this file that chooses an attempt
//! id, decides whether a quorum is reached, or decides what a restart means. Those live in
//! `hydra_state::Coordinator`, which TLC checks. If a decision creeps in here, that is the same
//! defect this slice exists to remove, wearing a newer coat.
//!
//! # The two orderings that are load-bearing
//!
//! 1. **WAL-before-wire.** An `Effect::WriteWal` is appended and `fdatasync`'d, and only then is
//!    `CoordEvent::WalDurable` fed back. The window between them is a real crash window, and §6.5's
//!    restart classification is what reads the log to decide what was in flight.
//! 2. **The rank comes from the CONNECTION, never from the frame** (audit H4).
//!    `ACTIVATION_COMMITTED` and `ACTIVATION_FINALIZED` carry **no rank on the wire**, so a
//!    coordinator keying its ack set on anything the frame says is keying it on nothing.
//!    [`StageLink::rank`] is minted by `hydra-transport` from the peer's certificate role.

use std::collections::BTreeMap;

use hydra_state::coordinator::{CoordEvent, CoordState, Coordinator, WalKindTag};
use hydra_state::{AuthenticatedRank, ControlMsg, Effect, StageRank, WalRecord};
use hydra_wire::SessionFence;

use crate::commit_stream::WalFenceCtx;
use crate::control_wal::{tuple_hash, ControlWal, ControlWalError};

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("control wal: {0}")]
    Wal(#[from] ControlWalError),
    #[error("transport: {0}")]
    Transport(String),
    #[error("stage rank {0} has no link")]
    NoLink(StageRank),
    #[error("wire: {0}")]
    Wire(String),
}

/// One stage's link, and the **authenticated** identity that makes its acks countable.
pub trait StageLink {
    /// The rank this peer authenticated as. Minted by `hydra-transport` from the certificate's
    /// role, never read from a frame (audit H4).
    fn rank(&self) -> AuthenticatedRank;
    /// Send one already-encoded frame.
    fn send(&mut self, frame: Vec<u8>) -> Result<(), DriverError>;
}

/// What the driver did, for the caller (and for tests) to observe. Deliberately a record of
/// **executed effects**, not of decisions: the decisions are the SM's.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Executed {
    pub wal_records: Vec<&'static str>,
    pub frames_sent: usize,
}

/// The activation driver: one `hydra_state::Coordinator`, its durable control log, and the links.
pub struct ActivationDriver<L: StageLink> {
    coord: Coordinator,
    wal: ControlWal,
    wal_fence: WalFenceCtx,
    fence: SessionFence,
    links: BTreeMap<StageRank, L>,
}

impl<L: StageLink> ActivationDriver<L> {
    pub fn new(coord: Coordinator, wal: ControlWal, wal_fence: WalFenceCtx, fence: SessionFence, links: Vec<L>) -> Self {
        let links = links.into_iter().map(|l| (l.rank().rank(), l)).collect();
        ActivationDriver { coord, wal, wal_fence, fence, links }
    }

    pub fn state(&self) -> CoordState {
        self.coord.state()
    }

    pub fn coordinator(&self) -> &Coordinator {
        &self.coord
    }

    /// Step the SM and execute what it returns.
    ///
    /// **`durable` is the switch that makes the crash window reachable.** With `durable = true` the
    /// driver appends the record, waits for the `fdatasync`, and feeds `WalDurable` back — the
    /// normal path. With `durable = false` it appends and **stops there**, which is exactly the
    /// state a machine is in when it dies between the write and the acknowledgement. §6.5's
    /// classification is defined for that state and, until this driver existed, **nothing could
    /// produce it** (rule 19).
    pub fn step(&mut self, event: CoordEvent) -> Result<Executed, DriverError> {
        let effects = self.coord.step(event);
        self.execute(effects, true)
    }

    /// [`Self::step`], but the WAL write is left **un-acknowledged**: the record is durable on disk
    /// and the SM has not been told. Models a crash in the write→`WalDurable` window.
    pub fn step_without_acknowledging_durability(&mut self, event: CoordEvent) -> Result<Executed, DriverError> {
        let effects = self.coord.step(event);
        self.execute(effects, false)
    }

    fn execute(&mut self, effects: Vec<Effect>, acknowledge: bool) -> Result<Executed, DriverError> {
        let mut out = Executed::default();
        let mut durable_tags = Vec::new();
        for eff in effects {
            match eff {
                Effect::WriteWal { record, .. } => {
                    // WAL-before-wire: durable when `append` returns.
                    self.wal.append(&self.wal_fence, &record)?;
                    out.wal_records.push(label(&record));
                    if let Some(tag) = tag_of(&record) {
                        durable_tags.push(tag);
                    }
                }
                Effect::Send { msg, .. } => {
                    let frame = self.encode(&msg);
                    // A control message goes to every stage: the SM decides WHAT is sent, the
                    // driver only decides that "the stages" means every link it holds.
                    for link in self.links.values_mut() {
                        link.send(frame.clone())?;
                        out.frames_sent += 1;
                    }
                }
            }
        }
        if acknowledge {
            for tag in durable_tags {
                // Feeding this is what "the write is durable" MEANS to the SM. It happens after the
                // fdatasync above returned, never before.
                let more = self.coord.step(CoordEvent::WalDurable(tag));
                let nested = self.execute(more, acknowledge)?;
                out.wal_records.extend(nested.wal_records);
                out.frames_sent += nested.frames_sent;
            }
        }
        Ok(out)
    }

    fn encode(&self, msg: &ControlMsg) -> Vec<u8> {
        match msg {
            ControlMsg::CommitActivation { tuple } => hydra_wire::encode_commit_activation(&self.fence, tuple, 0),
            ControlMsg::FinalizeActivation { tuple, completion_id } => {
                // **Audit H2 — the finalize carries its completion evidence.**
                //
                // `completion_id` and `complete_record_hash` were dropped at decode on the stage
                // side because nothing produced them: there was no coordinator writing an
                // `ACTIVATION_COMPLETE` record for them to refer to. There is now, and the hash is
                // the one committed to that record, so both sides compare the same value rather
                // than each computing its own.
                hydra_wire::encode_finalize_activation_with_evidence(&self.fence, tuple, 0, *completion_id, &tuple_hash(tuple))
            }
            ControlMsg::ActivationCommitAbort { attempt, .. } => hydra_wire::encode_activation_commit_abort(&self.fence, *attempt),
        }
    }

    /// Feed an inbound frame from a **specific, authenticated** peer.
    ///
    /// The `rank` argument is an [`AuthenticatedRank`], which only `hydra-transport` can mint from
    /// a certificate role — so a caller cannot pass a rank a frame claimed (audit H4).
    pub fn on_frame(&mut self, rank: AuthenticatedRank, payload: &[u8]) -> Result<Executed, DriverError> {
        let (_view, msg) = hydra_wire::decode(payload, &self.fence).map_err(|e| DriverError::Wire(e.to_string()))?;
        let event = match msg {
            hydra_wire::Msg::ActivationCommitted(t) => Some(CoordEvent::StageCommitted { rank, attempt: t.attempt }),
            hydra_wire::Msg::ActivationFinalized => {
                Some(CoordEvent::StageFinalized { rank, attempt: self.coord.attempt() })
            }
            // Everything else is either a data-plane frame or an ack this SM does not model.
            _ => None,
        };
        match event {
            Some(e) => self.step(e),
            None => Ok(Executed::default()),
        }
    }
}

fn label(r: &WalRecord) -> &'static str {
    match r {
        WalRecord::ActivationCommitIntent { .. } => "INTENT",
        WalRecord::ActivationComplete { .. } => "COMPLETE",
        WalRecord::ActivationAbort { .. } => "ABORT",
        WalRecord::ActivationUnservable { .. } => "UNSERVABLE",
        WalRecord::SessionTerminate => "TERMINATE",
    }
}

fn tag_of(r: &WalRecord) -> Option<WalKindTag> {
    Some(match r {
        WalRecord::ActivationCommitIntent { .. } => WalKindTag::Intent,
        WalRecord::ActivationComplete { .. } => WalKindTag::Complete,
        WalRecord::ActivationAbort { .. } => WalKindTag::Abort,
        WalRecord::ActivationUnservable { .. } => WalKindTag::Unservable,
        WalRecord::SessionTerminate => WalKindTag::Terminate,
    })
}
