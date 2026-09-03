//! **`hydra-node` — the product process's generation path (2026-09-02, §7.76; rule 27).**
//!
//! # Why a crate at the top of the graph
//!
//! The `hydra-coordinator` binary shipped for a week with a generation function that returned a
//! closed channel: *"no stages linked yet in this seam"*. It could not be otherwise where it sat —
//! the drivers that connect to stages and run a pipeline live in `hydra-worker::pair`, and
//! `hydra-worker` depends on `hydra-coordinator`, so a binary in `hydra-coordinator` linking a stage
//! would have closed a dependency cycle (rule 21: the build enforced the stub). The design
//! authority ruled the binary moves UP; this crate is where it lives now. Everything a user's
//! prompt touches — tokenization, activation, prefill, sampling, commit, SSE — runs in the process
//! this crate builds, and the oracles in `tests/` kill THAT process.
//!
//! # The topology this crate drives (v1)
//!
//! Two stages, coordinator-relayed: `C → S1 APPLY_TOKEN → S1 replies FWD(boundary) → C → S_P
//! FWD → S_P APPLIED_ACK`, then `C → S_P SAMPLE_NEXT → SAMPLED` and the sampled token is fed back
//! the same way. It is the shape `hydra_worker::pair::run_generation` proves bit-exact against the
//! unsplit model (the rule-14 anchor extended by `greedy_sample_across_pipeline_matches_unsplit_argmax`),
//! and it needs no durability target — recovery in this topology is Strategy B (token replay from
//! the commit stream, D0), which is what the ledger holds. The direct-FWD / D1 topology (S1 dials
//! S_P and a durability target) is the demo binaries' shape and is NOT wired here yet (§8).
//!
//! # What is and is not the state machine's
//!
//! Activation goes through [`hydra_coordinator::driver::ActivationDriver`] — the verified
//! `hydra_state::Coordinator` decides every step, exactly as the harnesses do. The data-plane
//! frames below carry no protocol decision (they are effects the spec names: `APPLY_TOKEN`, `FWD`,
//! `SAMPLE_NEXT`), and they ride the SAME authenticated links the activation used
//! (`ActivationDriver::send_to` / `recv_from`), so a frame never travels on a connection the SM did
//! not activate.

use std::net::SocketAddr;
use std::path::Path;

use hydra_coordinator::driver::{ActivationDriver, MtlsStageLink};
use hydra_coordinator::control_wal::ControlWal;
use hydra_coordinator::{SampledToken, WalFenceCtx};
use hydra_state::coordinator::{CoordEvent, CoordState, Coordinator};
use hydra_state::{AuthenticatedRank, SessionId};
use hydra_transport::tcp_mtls::TcpMtls;
use hydra_wire::{Msg, SessionFence};
use hydra_worker::sampler::SamplingConfig;
use hydra_worker::worker::INITIAL_CHECKPOINT_ID;

pub use hydra_cli::provision::{read_api_token, read_cluster, ClusterFiles, StageSpec};

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("driver: {0}")]
    Driver(#[from] hydra_coordinator::driver::DriverError),
    #[error("wire: {0}")]
    Wire(String),
    #[error("protocol: expected {expected}, got {got}")]
    Unexpected { expected: &'static str, got: String },
    #[error("sampler error code {0} at step {1}")]
    Sampler(u16, usize),
    #[error("{0}")]
    Config(String),
}

/// The coordinator's identity for dialling stages, and the connector built from it.
pub struct StageConnector {
    connector: TcpMtls,
}

impl StageConnector {
    /// Dial as `coordinator`, presenting an identity issued from the paired CA at `pairing_dir`.
    /// The CA key is read from the coordinator's own directory and used only to issue; it is what
    /// pairing persisted there (`ClusterCa::save_private`).
    pub fn from_pairing_dir(pairing_dir: &Path) -> Result<StageConnector, NodeError> {
        let ca = hydra_transport::ClusterCa::load_private(&pairing_dir.join("coordinator")).map_err(|e| NodeError::Transport(e.to_string()))?;
        let id = ca.issue("coordinator").map_err(|e| NodeError::Transport(e.to_string()))?;
        let cfg = hydra_transport::client_config_with_ca(&ca.ca_cert_der(), &id).map_err(|e| NodeError::Transport(e.to_string()))?;
        Ok(StageConnector { connector: TcpMtls::from_config(cfg).map_err(|e| NodeError::Transport(e.to_string()))? })
    }

    /// A connector from an already-built cluster (tests).
    pub fn from_connector(connector: TcpMtls) -> StageConnector {
        StageConnector { connector }
    }

    /// Dial every stage in rank order; each link carries the rank the transport minted for the
    /// TLS-verified name it dialled (`TcpMtls::connect_stage`).
    pub async fn connect_all(&self, stages: &[StageSpec]) -> Result<Vec<(AuthenticatedRank, MtlsStageLink)>, NodeError> {
        let mut links = Vec::with_capacity(stages.len());
        for s in stages {
            let (conn, rank) = self
                .connector
                .connect_stage(s.addr, &s.name, s.rank as hydra_state::StageRank)
                .await
                .map_err(|e| NodeError::Transport(format!("connect {} at {}: {e}", s.name, s.addr)))?;
            links.push((rank, MtlsStageLink::new(rank, conn)));
        }
        Ok(links)
    }
}

/// An activated two-stage pipeline, ready to prefill and sample.
pub struct Pipeline {
    driver: ActivationDriver<MtlsStageLink>,
    fence: SessionFence,
    s1: AuthenticatedRank,
    sp: AuthenticatedRank,
}

/// **Rule-19 crash-injection points for the restart oracle (spec §6.5a's adversarial window).**
/// Set `HYDRA_CRASH_AT=intent-durable` and the process aborts the instant the INITIAL/recovery
/// `ACTIVATION_COMMIT_INTENT` is durable and BEFORE `COMMIT_ACTIVATION` is sent — the window in
/// which a restart must fence forward and never resume the intent. Opt-in by name, announced on
/// stderr, never the silent cause of anything.
fn crash_point(name: &str) {
    if std::env::var("HYDRA_CRASH_AT").map(|v| v == name).unwrap_or(false) {
        eprintln!("hydra-coordinator: HYDRA_CRASH_AT={name} — aborting the process HERE, deliberately (restart oracle)");
        std::process::abort();
    }
}

impl Pipeline {
    /// Connect to the stages and run the **INITIAL activation through the state machine** (spec
    /// §6.6, one mechanism): intent → commit → acks → complete → finalize → acks → serviceable.
    /// The control WAL at `control_wal_path` records every decision before it goes on the wire.
    pub async fn activate(
        connector: &StageConnector,
        stages: &[StageSpec],
        fence: &SessionFence,
        control_wal_path: &Path,
    ) -> Result<Pipeline, NodeError> {
        let links = Self::links(connector, stages).await?;
        let (s1, sp) = (links[0].0, links[1].0);
        let wal = ControlWal::create(control_wal_path, fence.cluster_id, fence.session_id).map_err(|e| NodeError::Config(format!("control wal: {e}")))?;
        let coord = Coordinator::new_initial(SessionId(fence.session_id), 2, INITIAL_CHECKPOINT_ID);
        let driver = ActivationDriver::new(coord, wal, wal_fence(fence), fence.clone(), links.into_iter().map(|(_, l)| l).collect());
        let mut p = Pipeline { driver, fence: fence.clone(), s1, sp };
        p.activation_transaction().await?;
        Ok(p)
    }

    async fn links(connector: &StageConnector, stages: &[StageSpec]) -> Result<Vec<(AuthenticatedRank, MtlsStageLink)>, NodeError> {
        if stages.len() != 2 {
            return Err(NodeError::Config(format!("v1 drives exactly two stages; the stage table has {}", stages.len())));
        }
        connector.connect_all(stages).await
    }

    /// §6.6 from `StagesReconstructed` to `Serviceable` — one mechanism for INITIAL and RECOVERY.
    async fn activation_transaction(&mut self) -> Result<(), NodeError> {
        let (s1, sp) = (self.s1, self.sp);
        self.driver.step(CoordEvent::StagesReconstructed).await?;
        self.driver.step(CoordEvent::ProceedWriteIntent).await?;
        crash_point("intent-durable");
        self.driver.step(CoordEvent::ProceedSendCommit).await?;
        for rank in [s1, sp] {
            let reply = self.driver.recv_from(rank).await?;
            self.driver.on_frame(rank, &reply).await?;
        }
        self.driver.step(CoordEvent::ProceedWriteComplete).await?;
        self.driver.step(CoordEvent::ProceedSendFinalize).await?;
        for rank in [s1, sp] {
            let reply = self.driver.recv_from(rank).await?;
            self.driver.on_frame(rank, &reply).await?;
        }
        self.driver.step(CoordEvent::ProceedBecomeServiceable).await?;
        if self.driver.state() != CoordState::Serviceable {
            return Err(NodeError::Config(format!("activation did not reach SERVICEABLE: {:?}", self.driver.state())));
        }
        Ok(())
    }

    /// A bounded wait on a stage during recovery: a stage that DROPS a control frame (the stage
    /// SM's Case C answers nothing on the wire) must surface as a named failure, not a process
    /// that waits forever — the third restart-oracle window found exactly that hang (2026-09-03).
    async fn recv_bounded(driver: &mut ActivationDriver<MtlsStageLink>, rank: AuthenticatedRank, what: &'static str) -> Result<Vec<u8>, NodeError> {
        match tokio::time::timeout(std::time::Duration::from_secs(60), driver.recv_from(rank)).await {
            Ok(r) => r.map_err(NodeError::from),
            Err(_) => Err(NodeError::Config(format!("recovery: no {what} from {rank:?} within 60 s — the stage dropped the frame (Case C) or is gone"))),
        }
    }

    /// **Restart (spec §6.5a): reconstruct the coordinator from its durable control log, fence
    /// forward, and drive the recovery of both stages from the commit stream — Strategy B.**
    ///
    /// 1. `Coordinator::restart_from(records)` + `Restart` → the SM derives its state and either
    ///    resumes a decided activation or (the common case) writes a `BEGIN_RECOVERY` at
    ///    `(target+1, rid+1)`; the driver makes it durable.
    /// 2. `BEGIN_RECOVERY` to both stages (Case A: they freeze at the base and truncate to
    ///    `truncate_to`); their `RECOVERY_ACK`s are collected.
    /// 3. Strategy B rebuild, relayed: every ledger token (`prompt ⧺ generated`) as `REBUILD_APPLY`
    ///    through S1, its boundary forwarded to S_P — positions the stages already hold are no-ops
    ///    (M9 idempotency), so the replay is safe from 0.
    /// 4. `CATCH_UP_CONTEXT{goal}` to both; `INSTALL_SAMPLER_CHECKPOINT` (the ledger's last durable
    ///    checkpoint) to S_P (I17: before activation).
    /// 5. The activation transaction at the new epoch → SERVICEABLE.
    pub async fn recover(
        connector: &StageConnector,
        stages: &[StageSpec],
        fence: &SessionFence,
        control_wal_path: &Path,
        ledger: &hydra_coordinator::RecoveryState,
    ) -> Result<Pipeline, NodeError> {
        let links = Self::links(connector, stages).await?;
        let (s1, sp) = (links[0].0, links[1].0);
        let (wal, records) = ControlWal::open(control_wal_path, &fence.cluster_id, &fence.session_id).map_err(|e| NodeError::Config(format!("control wal (reopen): {e}")))?;
        eprintln!("hydra-coordinator: restart — {} durable control records reread", records.len());
        // The ledger is the service witness (spec §6.5a refinement): any durable generation
        // commit means the activation served, and a crash after service fences forward.
        let served = ledger.generation_durable_pos >= 0;
        let coord = Coordinator::restart_from(SessionId(fence.session_id), 2, INITIAL_CHECKPOINT_ID, records, served);
        let driver = ActivationDriver::new(coord, wal, wal_fence(fence), fence.clone(), links.into_iter().map(|(_, l)| l).collect());
        let mut p = Pipeline { driver, fence: fence.clone(), s1, sp };

        // 1. The SM classifies and (normally) fences forward: BEGIN written, made durable.
        p.driver.step(CoordEvent::Restart).await?;
        let state = p.driver.state();
        eprintln!("hydra-coordinator: restart classified as {state:?} (epoch {} rid {} attempt {})", p.driver.coordinator().epoch(), p.driver.coordinator().recovery_id(), p.driver.coordinator().attempt());
        match state {
            CoordState::RecoveryStarted => {}
            other => {
                // A decided activation resumes finalization (I22); a recorded UNSERVABLE supersedes.
                // Both are §6.5's own branches; neither is the fence-forward path this seam wires
                // end to end, and neither is silently treated as it.
                return Err(NodeError::Config(format!("restart classified as {other:?}: only the fence-forward path (RecoveryStarted) is wired in this seam — recorded in PROJECT_STATE §8")));
            }
        }
        // 2. BEGIN_RECOVERY to both stages; collect the acks. Each ack carries the stage's OWN
        //    applied frontier (`RECOVERY_ACK{applied_input_pos}`): a stage that survived the
        //    coordinator's crash still holds the prompt and everything applied since, and its
        //    data-plane idempotency refuses any position at or below that frontier (`ERR_GAP`,
        //    spec §2.3d). The first product run of the restart oracle produced exactly that
        //    refusal (2026-09-03): the replay started at position 0 against stages holding the
        //    whole prefix. The resume rule is the spec's: replay from `applied_pos + 1`.
        p.driver.step(CoordEvent::ProceedSendBeginRecovery).await?;
        let mut applied = [-1i64; 2];
        for (i, rank) in [s1, sp].into_iter().enumerate() {
            let reply = Self::recv_bounded(&mut p.driver, rank, "RECOVERY_ACK").await?;
            match decode(&reply, &p.fence)? {
                Msg::RecoveryAck { applied_input_pos } => applied[i] = applied_input_pos,
                other => return Err(NodeError::Unexpected { expected: "RECOVERY_ACK", got: format!("{other:?}") }),
            }
        }
        // 3. Strategy B, relayed through the topology, from the acknowledged frontier. The relayed
        //    rebuild can only source an activation for S_P from S1's forward, so the two frontiers
        //    must agree; a pair that disagrees (one stage replaced, one intact) needs the partial
        //    rebuild this seam does not wire — refused by name, recorded in PROJECT_STATE §8.
        let tokens = ledger.replay_tokens();
        if applied[0] != applied[1] {
            return Err(NodeError::Config(format!(
                "the stages acknowledge different applied frontiers (S1 {}, S_P {}): a partial relayed rebuild is not wired in this seam — recorded in PROJECT_STATE §8",
                applied[0], applied[1]
            )));
        }
        let from = (applied[0] + 1).max(0) as usize;
        eprintln!("hydra-coordinator: restart — both stages hold applied frontier {}; replaying {} of {} ledger tokens", applied[0], tokens.len().saturating_sub(from), tokens.len());
        for (pos, &tok) in tokens.iter().enumerate().skip(from) {
            p.apply_relayed(pos as i64, tok, true).await?;
        }
        // 4. Catch-up frontier, then the sampler checkpoint (I17), then
        let epoch = p.epoch();
        let goal = ledger.input_frontier();
        for rank in [s1, sp] {
            p.driver.send_to(rank, hydra_wire::encode_catch_up_context(&p.fence, epoch, p.driver.coordinator().recovery_id(), goal)).await?;
            let ready = Self::recv_bounded(&mut p.driver, rank, "CATCH_UP_READY").await?;
            if !matches!(decode(&ready, &p.fence)?, Msg::CatchUpReady { .. }) {
                return Err(NodeError::Unexpected { expected: "CATCH_UP_READY", got: "something else".into() });
            }
        }
        p.driver.send_to(sp, hydra_wire::encode_install_sampler_checkpoint(&p.fence, epoch, ledger.checkpoint_id, &ledger.last_checkpoint)).await?;
        let installed = Self::recv_bounded(&mut p.driver, sp, "SAMPLER_CHECKPOINT_INSTALLED").await?;
        if !matches!(decode(&installed, &p.fence)?, Msg::SamplerCheckpointInstalled { .. }) {
            return Err(NodeError::Unexpected { expected: "SAMPLER_CHECKPOINT_INSTALLED", got: "something else".into() });
        }
        // 5. the activation transaction at the new epoch.
        p.activation_transaction().await?;
        Ok(p)
    }

    /// Continue a generation after a restart: sample from `next_output_pos` with the input frontier
    /// at `input_pos`, for at most `remaining` steps. `checkpoint_id` is the installed checkpoint
    /// the ledger names (the fence `SAMPLE_NEXT` carries).
    pub async fn resume_generate(
        &mut self,
        next_output_pos: i64,
        mut input_pos: i64,
        checkpoint_id: u64,
        config: &SamplingConfig,
        remaining: usize,
        mut emit: impl FnMut(&SampledToken) -> bool,
    ) -> Result<Vec<u32>, NodeError> {
        let mut out = Vec::new();
        for i in 0..remaining {
            let output_pos = next_output_pos + i as i64;
            let epoch = self.epoch();
            self.driver
                .send_to(self.sp, hydra_wire::encode_sample_next(&self.fence, epoch, output_pos, &config.hash(), checkpoint_id))
                .await?;
            let reply = self.driver.recv_from(self.sp).await?;
            let s = match decode(&reply, &self.fence)? {
                Msg::Sampled { output_pos, token_id, post_sample_snapshot, .. } => SampledToken { output_pos, token_id, snapshot: post_sample_snapshot },
                Msg::Err { code } => return Err(NodeError::Sampler(code, output_pos as usize)),
                other => return Err(NodeError::Unexpected { expected: "SAMPLED from S_P", got: format!("{other:?}") }),
            };
            out.push(s.token_id);
            if !emit(&s) {
                break;
            }
            if i + 1 < remaining {
                self.feed_back(input_pos, s.token_id).await?;
                input_pos += 1;
            }
        }
        Ok(out)
    }

    pub fn state(&self) -> CoordState {
        self.driver.state()
    }

    fn epoch(&self) -> hydra_state::Epoch {
        self.driver.coordinator().epoch()
    }

    /// One relayed apply: `APPLY_TOKEN` to S1, its boundary forwarded to S_P, S_P's ack drained.
    async fn apply_relayed(&mut self, input_pos: i64, token: u32, no_sample: bool) -> Result<(), NodeError> {
        let epoch = self.epoch();
        self.driver.send_to(self.s1, hydra_wire::encode_apply_token(&self.fence, epoch, input_pos, token, no_sample)).await?;
        let fwd = self.driver.recv_from(self.s1).await?;
        let activations = match decode(&fwd, &self.fence)? {
            Msg::Fwd { activations, .. } => activations,
            other => return Err(NodeError::Unexpected { expected: "FWD from S1", got: format!("{other:?}") }),
        };
        self.driver.send_to(self.sp, hydra_wire::encode_fwd(&self.fence, epoch, input_pos, no_sample, &activations)).await?;
        let ack = self.driver.recv_from(self.sp).await?;
        match decode(&ack, &self.fence)? {
            Msg::AppliedAck { .. } => Ok(()),
            other => Err(NodeError::Unexpected { expected: "APPLIED_ACK from S_P", got: format!("{other:?}") }),
        }
    }

    /// Teacher-forced prefill (`NO_SAMPLE`) of `tokens` starting at input position `from`.
    pub async fn prefill(&mut self, tokens: &[u32], from: i64) -> Result<(), NodeError> {
        for (i, &tok) in tokens.iter().enumerate() {
            self.apply_relayed(from + i as i64, tok, true).await?;
        }
        Ok(())
    }

    /// Ask S_P for the next token at `output_pos` (I14: sampled from retained logits; the sampler
    /// and every snapshot live on S_P, spec §1.4).
    pub async fn sample_next(&mut self, output_pos: i64, config: &SamplingConfig) -> Result<SampledToken, NodeError> {
        let epoch = self.epoch();
        self.driver
            .send_to(self.sp, hydra_wire::encode_sample_next(&self.fence, epoch, output_pos, &config.hash(), INITIAL_CHECKPOINT_ID))
            .await?;
        let reply = self.driver.recv_from(self.sp).await?;
        match decode(&reply, &self.fence)? {
            Msg::Sampled { output_pos, token_id, post_sample_snapshot, .. } => Ok(SampledToken { output_pos, token_id, snapshot: post_sample_snapshot }),
            Msg::Err { code } => Err(NodeError::Sampler(code, output_pos as usize)),
            other => Err(NodeError::Unexpected { expected: "SAMPLED from S_P", got: format!("{other:?}") }),
        }
    }

    /// Feed a sampled token back autoregressively at `input_pos`.
    pub async fn feed_back(&mut self, input_pos: i64, token: u32) -> Result<(), NodeError> {
        self.apply_relayed(input_pos, token, false).await
    }

    /// The whole generation: prefill, then `max_steps` sample/feed-back rounds, each token handed
    /// to `emit` as it is sampled (the API's session commits and streams it). Stops early if
    /// `emit` reports the consumer is gone. Returns the sampled tokens.
    pub async fn generate(
        &mut self,
        prompt_tokens: &[u32],
        config: &SamplingConfig,
        max_steps: usize,
        mut emit: impl FnMut(&SampledToken) -> bool,
    ) -> Result<Vec<u32>, NodeError> {
        self.prefill(prompt_tokens, 0).await?;
        let mut out = Vec::with_capacity(max_steps);
        let mut input_pos = prompt_tokens.len() as i64;
        for step in 0..max_steps {
            let s = self.sample_next(step as i64, config).await?;
            out.push(s.token_id);
            if !emit(&s) {
                break;
            }
            if step + 1 < max_steps {
                self.feed_back(input_pos, s.token_id).await?;
                input_pos += 1;
            }
        }
        Ok(out)
    }
}

fn decode(frame: &[u8], fence: &SessionFence) -> Result<Msg, NodeError> {
    hydra_wire::decode(frame, fence).map(|(_v, m)| m).map_err(|e| NodeError::Wire(e.to_string()))
}

/// The WAL fence context every record in this session carries (epoch 0, the initial attempt space).
pub fn wal_fence(fence: &SessionFence) -> WalFenceCtx {
    WalFenceCtx {
        cluster_id: fence.cluster_id,
        manifest_hash: fence.manifest_hash,
        model_instance_id: fence.model_instance_id,
        session_id: fence.session_id,
        epoch: 0,
        recovery_id: 0,
        activation_attempt_id: 0,
    }
}

/// A stage table from `name=addr` pairs in rank order (the `--stages` override).
pub fn parse_stages(arg: &str) -> Result<Vec<StageSpec>, NodeError> {
    let mut out = Vec::new();
    for (rank, part) in arg.split(',').enumerate() {
        let (name, addr) = part.split_once('=').ok_or_else(|| NodeError::Config(format!("--stages: expected name=addr, got {part:?}")))?;
        let addr: SocketAddr = addr.trim().parse().map_err(|e| NodeError::Config(format!("--stages {name}: {e}")))?;
        out.push(StageSpec { name: name.trim().to_string(), rank: rank as u16, addr });
    }
    Ok(out)
}
