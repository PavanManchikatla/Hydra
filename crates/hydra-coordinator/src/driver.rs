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
use crate::control_wal::{ControlWal, ControlWalError};

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
    ///
    /// Async because the production link is a real mTLS `Conn`. The trait is used generically
    /// (never as `dyn`), so `async fn` in a trait is exactly right here.
    fn send(&mut self, frame: Vec<u8>) -> impl std::future::Future<Output = Result<(), DriverError>> + Send;
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
    pub async fn step(&mut self, event: CoordEvent) -> Result<Executed, DriverError> {
        let effects = self.coord.step(event);
        self.execute(effects, true).await
    }

    /// [`Self::step`], but the WAL write is left **un-acknowledged**: the record is durable on disk
    /// and the SM has not been told. Models a crash in the write→`WalDurable` window.
    pub async fn step_without_acknowledging_durability(&mut self, event: CoordEvent) -> Result<Executed, DriverError> {
        let effects = self.coord.step(event);
        self.execute(effects, false).await
    }

    /// Execute effects, and the effects those produce, until the SM is quiet.
    ///
    /// A **worklist**, not recursion: feeding `WalDurable` back can itself yield effects, and an
    /// async recursive function would need boxing at every level for no benefit. The loop also
    /// makes the ordering explicit, which matters — every WAL write in a batch is made durable
    /// before any of the resulting events is fed back.
    async fn execute(&mut self, effects: Vec<Effect>, acknowledge: bool) -> Result<Executed, DriverError> {
        let mut out = Executed::default();
        let mut queue = std::collections::VecDeque::from(effects);
        loop {
            let mut durable_tags = Vec::new();
            while let Some(eff) = queue.pop_front() {
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
                        // A control message goes to every stage: the SM decides WHAT is sent; the
                        // driver only decides that "the stages" means every link it holds.
                        for link in self.links.values_mut() {
                            link.send(frame.clone()).await?;
                            out.frames_sent += 1;
                        }
                    }
                }
            }
            if !acknowledge || durable_tags.is_empty() {
                return Ok(out);
            }
            for tag in durable_tags {
                // Feeding this is what "the write is durable" MEANS to the SM. It happens after the
                // fdatasync above returned, never before.
                queue.extend(self.coord.step(CoordEvent::WalDurable(tag)));
            }
        }
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
                hydra_wire::encode_finalize_activation_with_evidence(&self.fence, tuple, 0, *completion_id, &tuple.completion_hash())
            }
            ControlMsg::ActivationCommitAbort { attempt, .. } => hydra_wire::encode_activation_commit_abort(&self.fence, *attempt),
            // M4·0b — the recovery plane. The encoders have existed since M2 (and M13's decode arm
            // since Wave 4); what did not exist was anything that *decided* to send one.
            ControlMsg::BeginRecovery { base, target, recovery_id, truncate_to } => {
                hydra_wire::encode_begin_recovery(&self.fence, *base, *target, *recovery_id, *truncate_to)
            }
            ControlMsg::ResetRecoveryAttempt { target, new_recovery_id, truncate_to } => hydra_wire::encode_reset_recovery_attempt(
                &self.fence,
                *target,
                new_recovery_id.saturating_sub(1),
                *new_recovery_id,
                *truncate_to,
                0,
            ),
        }
    }

    /// Replace the link for a rank — the **re-link** a recovery needs after a stage is killed and
    /// a replacement comes up on a fresh port.
    ///
    /// The rank is taken from the new link's own authenticated identity, so a replacement cannot
    /// quietly take over a different stage's slot (audit H4 again, at the one moment when a peer
    /// legitimately changes address).
    pub fn replace_link(&mut self, link: L) {
        self.links.insert(link.rank().rank(), link);
    }

    /// Receive one frame from the link belonging to `rank`.
    ///
    /// Paired with [`Self::on_frame`] so the rank a reply is attributed to is **the rank of the
    /// link it arrived on** — the driver never has an opportunity to read one out of the frame,
    /// which is the whole of audit H4.
    pub async fn recv_from(&mut self, rank: AuthenticatedRank) -> Result<Vec<u8>, DriverError>
    where
        L: Receivable,
    {
        let link = self.links.get_mut(&rank.rank()).ok_or(DriverError::NoLink(rank.rank()))?;
        link.recv().await
    }

    /// **M4·0c — reconstruct a stage's context: the strategy path, driven by the coordinator.**
    ///
    /// This is everything between `BEGIN_RECOVERY` and re-activation, and until now **every line of
    /// it lived in a demo binary or a test**: `hydra-wan`, `hydra-3node-kill`, `hydra-2node-ci` and
    /// the recovery test files each hand-sent `CATCH_UP_CONTEXT` and
    /// `INSTALL_SAMPLER_CHECKPOINT` in the right order. So the *sequence* was demonstrated many
    /// times and was **never owned by the product** — which is the same shape as the activation
    /// transaction before M4·0, one layer further in.
    ///
    /// The order is the spec's (§6.2/§6.3) and is not negotiable:
    /// 1. **rebuild the KV** with the strategy's frames (`REBUILD_APPLY` / boundary replay),
    /// 2. **drive the control-plane frontier** with `CATCH_UP_CONTEXT{goal}` and wait for
    ///    `CATCH_UP_READY` — the SM's `applied` must reach the goal or activation cannot commit,
    /// 3. **install the sampler checkpoint** (I17: installation precedes activation, always).
    ///
    /// Returns when the stage is reconstructed. The caller then feeds `StagesReconstructed`, and
    /// the activation transaction proceeds exactly as it does for a fresh session — which is the
    /// point of §6.6 being *one* mechanism for INITIAL and RECOVERY.
    pub async fn reconstruct(
        &mut self,
        rank: AuthenticatedRank,
        strategy: &RecoveryStrategy,
        goal_input_pos: i64,
        checkpoint_id: u64,
        checkpoint_snapshot: &[u8],
    ) -> Result<Executed, DriverError>
    where
        L: Receivable,
    {
        let mut out = Executed::default();
        let epoch = self.coord.epoch();

        // 1. Rebuild the KV. These are data-plane frames; the stage applies them with NO_SAMPLE,
        //    so nothing is sampled and no output position is produced (I14 is untouched).
        match strategy {
            RecoveryStrategy::TokenReplay { tokens } => {
                for (pos, tok) in tokens.iter().enumerate() {
                    let frame = hydra_wire::encode_apply_token(&self.fence, epoch, pos as i64, *tok, true);
                    self.send_to(rank, frame).await?;
                    out.frames_sent += 1;
                    // Each apply is acked; draining keeps the connection in step and surfaces a
                    // refusal (audit M10) instead of leaving it in the socket.
                    let _ = self.recv_from(rank).await?;
                }
            }
            RecoveryStrategy::BoundaryReplay { boundaries } => {
                for (pos, activations) in boundaries {
                    let frame = hydra_wire::encode_fwd(&self.fence, epoch, *pos, true, activations);
                    self.send_to(rank, frame).await?;
                    out.frames_sent += 1;
                    let _ = self.recv_from(rank).await?;
                }
            }
        }

        // 2. Drive the SM's control-plane frontier to the goal.
        let frame = hydra_wire::encode_catch_up_context(&self.fence, epoch, self.coord.recovery_id(), goal_input_pos);
        self.send_to(rank, frame).await?;
        out.frames_sent += 1;
        let ready = self.recv_from(rank).await?;
        let (_v, msg) = hydra_wire::decode(&ready, &self.fence).map_err(|e| DriverError::Wire(e.to_string()))?;
        if !matches!(msg, hydra_wire::Msg::CatchUpReady { .. }) {
            return Err(DriverError::Wire(format!("expected CATCH_UP_READY, got {msg:?}")));
        }

        // 3. Install the sampler checkpoint BEFORE activation (I17). A stage that activated first
        //    could serve a token from a sampler state the ledger never committed to.
        let frame = hydra_wire::encode_install_sampler_checkpoint(&self.fence, epoch, checkpoint_id, checkpoint_snapshot);
        self.send_to(rank, frame).await?;
        out.frames_sent += 1;
        let installed = self.recv_from(rank).await?;
        let (_v, msg) = hydra_wire::decode(&installed, &self.fence).map_err(|e| DriverError::Wire(e.to_string()))?;
        if !matches!(msg, hydra_wire::Msg::SamplerCheckpointInstalled { .. }) {
            return Err(DriverError::Wire(format!("expected SAMPLER_CHECKPOINT_INSTALLED, got {msg:?}")));
        }
        Ok(out)
    }

    /// Send one frame to a specific authenticated peer (M4·0c). Public since 2026-09-02 so the
    /// product's data plane (`hydra-node`) rides the SAME authenticated links the activation used —
    /// a data-plane frame never travels on a connection the SM did not activate.
    pub async fn send_to(&mut self, rank: AuthenticatedRank, frame: Vec<u8>) -> Result<(), DriverError> {
        let link = self.links.get_mut(&rank.rank()).ok_or(DriverError::NoLink(rank.rank()))?;
        link.send(frame).await
    }

    /// Feed an inbound frame from a **specific, authenticated** peer.
    ///
    /// The `rank` argument is an [`AuthenticatedRank`], which only `hydra-transport` can mint from
    /// a certificate role — so a caller cannot pass a rank a frame claimed (audit H4).
    pub async fn on_frame(&mut self, rank: AuthenticatedRank, payload: &[u8]) -> Result<Executed, DriverError> {
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
            Some(e) => self.step(e).await,
            None => Ok(Executed::default()),
        }
    }
}

/// **M4·0c — how a rebuilt stage's context is reconstructed (spec §6.2 / §6.3).**
///
/// The choice is a property of the session's **durability mode**, fixed at admission, not a
/// per-recovery decision: D1 (the default) keeps boundary logs outside the protected failure
/// domain and therefore replays *boundaries*; D0 has only the commit stream and the prompt and
/// therefore replays *tokens*. The state machine decides **when** each phase happens; this decides
/// **what the rebuild frames carry**, which is an effect, not a transition.
pub enum RecoveryStrategy {
    /// **Strategy B (spec §6.3, D0):** replay the committed tokens as `REBUILD_APPLY`
    /// (`APPLY_TOKEN` with `NO_SAMPLE`). Correct anywhere, and the slowest — it recomputes the
    /// whole KV from the prompt forward.
    TokenReplay { tokens: Vec<u32> },
    /// **Strategy A (spec §6.2, D1 default):** replay the durable `BOUNDARY_COPY` records into the
    /// replacement, which is what makes D1 faster than a full recompute — the boundaries were
    /// already paid for once.
    BoundaryReplay { boundaries: Vec<(i64, Vec<f32>)> },
}

impl RecoveryStrategy {
    pub fn label(&self) -> &'static str {
        match self {
            RecoveryStrategy::TokenReplay { .. } => "B/token-replay",
            RecoveryStrategy::BoundaryReplay { .. } => "A/boundary-replay",
        }
    }
}

/// A link that can also be read from. Separate from [`StageLink`] because a test double may only
/// need to record what it was sent, and requiring a `recv` it will never serve would be a trait
/// bound describing the test rather than the protocol.
pub trait Receivable {
    fn recv(&mut self) -> impl std::future::Future<Output = Result<Vec<u8>, DriverError>> + Send;
}

/// **The production link: a real mTLS connection to one stage.**
///
/// The rank is supplied at construction from the peer's **authenticated** role — the caller gets it
/// from `BoundPeer::authenticated_rank()` or from the role it dialled — and never from a frame
/// (audit H4). `ACTIVATION_COMMITTED` carries no rank on the wire at all, so a coordinator that
/// read one from a frame would be counting a number the sender chose.
pub struct MtlsStageLink {
    rank: AuthenticatedRank,
    conn: hydra_transport::tcp_mtls::ClientConn,
}

impl MtlsStageLink {
    pub fn new(rank: AuthenticatedRank, conn: hydra_transport::tcp_mtls::ClientConn) -> Self {
        MtlsStageLink { rank, conn }
    }

}

impl Receivable for MtlsStageLink {
    /// Receive one frame from this stage. The caller pairs it with [`MtlsStageLink::rank`] when
    /// feeding the driver, which is the only way a rank enters the ack set.
    async fn recv(&mut self) -> Result<Vec<u8>, DriverError> {
        let frame = self.conn.recv().await.map_err(|e| DriverError::Transport(e.to_string()))?;
        Ok(frame.payload)
    }
}

impl StageLink for MtlsStageLink {
    fn rank(&self) -> AuthenticatedRank {
        self.rank
    }
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), DriverError> {
        self.conn.send(0, &frame).await.map_err(|e| DriverError::Transport(e.to_string()))
    }
}

fn label(r: &WalRecord) -> &'static str {
    match r {
        WalRecord::ActivationCommitIntent { .. } => "INTENT",
        WalRecord::ActivationComplete { .. } => "COMPLETE",
        WalRecord::ActivationAbort { .. } => "ABORT",
        WalRecord::ActivationUnservable { .. } => "UNSERVABLE",
        WalRecord::BeginRecovery { .. } => "BEGIN_RECOVERY",
        WalRecord::ResetRecoveryAttempt { .. } => "RESET",
        WalRecord::SessionTerminate => "TERMINATE",
    }
}

fn tag_of(r: &WalRecord) -> Option<WalKindTag> {
    Some(match r {
        WalRecord::ActivationCommitIntent { .. } => WalKindTag::Intent,
        WalRecord::ActivationComplete { .. } => WalKindTag::Complete,
        WalRecord::ActivationAbort { .. } => WalKindTag::Abort,
        WalRecord::ActivationUnservable { .. } => WalKindTag::Unservable,
        WalRecord::BeginRecovery { .. } => WalKindTag::BeginRecovery,
        WalRecord::ResetRecoveryAttempt { .. } => WalKindTag::Reset,
        WalRecord::SessionTerminate => WalKindTag::Terminate,
    })
}
