//! Wire codec: the `hydra_proto` `Frame` (`fence` + `Body` union, spec §4) <-> native types,
//! plus the **F1 fence** identity check.
//!
//! The transport envelope (`HYFR` header + BLAKE3 tag + `payload_len ≤ MAX_FRAME_BYTES`) is
//! validated by `hydra-transport`'s `Conn::recv` **before** the payload is allocated
//! (`hydra_proto::framing::verify_frame`). This module runs one layer up: it parses the payload
//! as a FlatBuffer `Frame`, checks the **F1 fence tuple** (cluster / manifest / model-instance /
//! session identity) **before** any engine work or boundary-buffer allocation, and maps the body
//! union to a native [`Msg`]. Activation-attempt (F2) and epoch/recovery fencing is the
//! `hydra-state` stage SM's job, not this codec's — the codec never branches on protocol state.
//!
//! Generated FlatBuffers code is the source of truth (BLUEPRINT §2 item 4); there are no shadow
//! structs — every accessor here goes through `hydra_proto::proto::*`.

use flatbuffers::FlatBufferBuilder;
use hydra_proto::proto;
use hydra_state::{ActivationKind, ActivationTuple, AttemptId, Epoch, RecoveryId};

pub const CLUSTER_ID_LEN: usize = 16;
pub const HASH_LEN: usize = 32;
pub const MODEL_INSTANCE_ID_LEN: usize = 16;
pub const SESSION_ID_LEN: usize = 16;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WireError {
    #[error("malformed frame: {0}")]
    Malformed(String),
    #[error("F1 fence mismatch: {0}")]
    FenceMismatch(&'static str),
    #[error("unsupported body for this endpoint")]
    UnsupportedBody,
    /// A **[RESERVED] forward-compatibility hook** carried a non-v1 value. Reserved fields are
    /// typed so the wire never changes shape when the feature lands, and **fenced** so that until
    /// it lands v1 can never be driven down a path it does not implement. A peer that sets one is
    /// speaking a protocol this build does not serve — refuse loudly, never ignore the field and
    /// proceed (that is the silent-downgrade shape this project refuses everywhere else).
    #[error("RESERVED field carries a non-v1 value: {0}")]
    ReservedInUse(&'static str),
    /// A declared length exceeded a normative cap from `hydra-proto.fbs`. Raised **before** the
    /// payload is copied into an owned buffer, so an oversized record is refused rather than
    /// allocated. Maps to `ErrCode::ERR_LIMIT_EXCEEDED`.
    #[error("{what} length {value} exceeds cap {cap}")]
    LimitExceeded { what: &'static str, value: u64, cap: u64 },
}

/// The stable part of the F1 fence tuple — one session's identity. Constant in v1 (spec §1.4).
///
/// # ⚠️ THIS IS NOT A SECRET, AND IT WAS NAMED AS IF IT WERE
///
/// This type was called `SessionKeys` until 2026-08-23. **It has never held a key.** Every field
/// travels **in cleartext inside every frame** — that is the whole point of a fence tuple — so
/// anyone who can read one frame can reproduce all four values and forge the tuple perfectly.
///
/// The name mattered because of what it suppressed. "Session keys, checked on every frame" reads
/// like an authentication mechanism, so **nobody asked where authentication actually lived** — and
/// the answer, until audit C2, was *nowhere*: mTLS proved a peer was in the cluster and nothing
/// bound it to a role. The old name did not merely mislead, it made the real gap uninteresting to
/// look for. That is PROJECT_STATE §7.31 (*a name that promises more than the construction delivers
/// terminates inquiry*) at the **design** layer rather than the test layer, and it is recorded as
/// §7.35.
///
/// # What it actually does, which is worth keeping
///
/// **Misrouting prevention (I4/F1), an accident-class property.** It answers *"is this frame for
/// THIS session?"* — rejecting a stale frame from a previous session, a crossed wire between two
/// clusters, a replay from a dead epoch. Those are real failures and this really prevents them.
///
/// | Field | What it does now |
/// |---|---|
/// | `cluster_id` | Nothing security-relevant — mTLS proves cluster membership cryptographically |
/// | `manifest_hash` | **More** than before: audit H14's model-identity binding — but as a *public* identity, not a secret |
/// | `model_instance_id` | Nothing — `[RESERVED]`, constant in v1, validated-never-branched (§7.27) |
/// | `session_id` | Rejects stale traffic from a previous session — the correctness job |
///
/// **Authentication is the peer certificate; authorisation is `hydra_transport::roles::PeerRole`.**
/// If you are reaching for this type to answer "may this peer do that?", you want the role table.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SessionFence {
    pub cluster_id: [u8; CLUSTER_ID_LEN],
    pub manifest_hash: [u8; HASH_LEN],
    pub model_instance_id: [u8; MODEL_INSTANCE_ID_LEN],
    pub session_id: [u8; SESSION_ID_LEN],
}

impl SessionFence {
    /// A deterministic test/dev identity derived from a single seed byte (no RNG in this crate).
    pub fn dev(seed: u8) -> Self {
        SessionFence {
            cluster_id: [seed; CLUSTER_ID_LEN],
            manifest_hash: [seed ^ 0x5a; HASH_LEN],
            model_instance_id: [seed ^ 0x11; MODEL_INSTANCE_ID_LEN],
            session_id: [seed ^ 0x77; SESSION_ID_LEN],
        }
    }
}

/// The per-frame varying fence fields the caller may need after an F1-passing decode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FenceView {
    pub epoch: Epoch,
    pub recovery_id: RecoveryId,
    pub activation_attempt_id: AttemptId,
    pub stage_generation: u64,
}

/// A decoded body (only the variants slice-2 exercises; extend per later M2 slices).
#[derive(Clone, PartialEq, Debug)]
pub enum Msg {
    // --- data plane ---
    /// `APPLY_TOKEN` (C -> S1). `no_sample` teacher-forces (NO_SAMPLE) per spec §3.
    ApplyToken { input_pos: i64, token_id: u32, no_sample: bool },
    /// `FWD` (Si -> Si+1): the boundary residual for `n_positions` positions, f32 across the FFI.
    Fwd { first_input_pos: i64, no_sample: bool, activations: Vec<f32> },
    /// `APPLIED_ACK` — `output_checksum` carries the final-stage logits digest for the anchor.
    AppliedAck { cumulative_input_pos: i64, output_checksum: Vec<u8> },
    /// `BOUNDARY_COPY` (Si -> durability target): a boundary residual chunk copied for durable D1
    /// recovery. `boundary_id` is the edge index i (between Si and Si+1); `chunk_id` sequences chunks.
    BoundaryCopy { boundary_id: u32, first_input_pos: i64, chunk_id: u32, activations: Vec<f32> },
    /// `DURABILITY_ACK` — the durability target has made boundary `boundary_id` durable through
    /// `durable_through_input_pos` (the R3′ release condition, spec §5).
    DurabilityAck { boundary_id: u32, durable_through_input_pos: i64, storage_generation: u64 },
    /// `COMMIT_ACK` — downstream has committed through `committed_through_output_pos`.
    CommitAck { committed_through_output_pos: i64 },
    /// `COMMIT_SYNC` — a piggybacked commit watermark to `commit_up_to_output_pos`.
    CommitSync { commit_up_to_output_pos: i64 },
    // --- control plane (maps 1:1 to StageEvent) ---
    CommitActivation(ActivationTuple),
    ActivationCommitted(ActivationTuple),
    FinalizeActivation { attempt: AttemptId },
    ActivationFinalized,
    ActivationCommitAbort { aborted_attempt: AttemptId },
    BeginRecovery { base: Epoch, target: Epoch, recovery_id: RecoveryId, truncate_to: i64 },
    RecoveryAck { applied_input_pos: i64 },
    /// `CATCH_UP_CONTEXT{goal}` — drive the stage's `RebuildStep` to `goal` (recovery catch-up).
    CatchUpContext { goal_input_pos: i64 },
    /// `CATCH_UP_READY{applied}` — the stage reached `FROZEN_READY` (catch-up complete).
    CatchUpReady { applied_input_pos: i64 },
    // --- sampler plane (spec §2.6a/§2.6b, M2 slice 3) ---
    /// `SAMPLE_NEXT{output_pos, sampling_config_hash, expected_sampler_checkpoint_id}` (I14).
    SampleNext { output_pos: i64, sampling_config_hash: Vec<u8>, expected_sampler_checkpoint_id: u64 },
    /// `SAMPLED{q}` carrying `post_sample_state_snapshot(q)` (spec §2.6a).
    Sampled { output_pos: i64, token_id: u32, post_sample_snapshot: Vec<u8>, sampler_state_digest: Vec<u8> },
    /// `INSTALL_SAMPLER_CHECKPOINT{checkpoint_id, snapshot}` (I17).
    InstallSamplerCheckpoint { checkpoint_id: u64, snapshot: Vec<u8> },
    /// `SAMPLER_CHECKPOINT_INSTALLED{checkpoint_id, resulting_state_digest, sampled_output_pos}`.
    SamplerCheckpointInstalled { checkpoint_id: u64, sampled_output_pos: i64, resulting_state_digest: Vec<u8> },
    /// `ERR_*` (e.g. `ERR_FENCED` from an F2 rejection, `ERR_CHECKPOINT_MISMATCH` from sampler drift).
    Err { code: u16 },
}

// ----------------------------- decode -----------------------------

fn fixed<const N: usize>(v: flatbuffers::Vector<'_, u8>, what: &'static str) -> Result<[u8; N], WireError> {
    let b = v.bytes();
    b.try_into().map_err(|_| WireError::Malformed(format!("{what}: expected {N} bytes, got {}", b.len())))
}

/// Parse a `Frame` payload, enforce the **F1 fence** against `fence`, and return the varying fence
/// fields + the native body. Rejects any frame whose identity tuple does not match this session —
/// **before** any boundary allocation (the fence read is O(1) and touches no payload tensors).
pub fn decode(payload: &[u8], fence: &SessionFence) -> Result<(FenceView, Msg), WireError> {
    let frame = flatbuffers::root::<proto::Frame>(payload)
        .map_err(|e| WireError::Malformed(format!("not a Frame flatbuffer: {e}")))?;
    let wire_fence = frame.fence();

    // F1: identity match. A stale/foreign frame is dropped here, never acted on.
    if fixed::<CLUSTER_ID_LEN>(wire_fence.cluster_id(), "cluster_id")? != fence.cluster_id {
        return Err(WireError::FenceMismatch("cluster_id"));
    }
    if fixed::<HASH_LEN>(wire_fence.manifest_hash(), "manifest_hash")? != fence.manifest_hash {
        return Err(WireError::FenceMismatch("manifest_hash"));
    }
    if fixed::<MODEL_INSTANCE_ID_LEN>(wire_fence.model_instance_id(), "model_instance_id")? != fence.model_instance_id {
        return Err(WireError::FenceMismatch("model_instance_id"));
    }
    if fixed::<SESSION_ID_LEN>(wire_fence.session_id(), "session_id")? != fence.session_id {
        return Err(WireError::FenceMismatch("session_id"));
    }

    // RESERVED hooks (spec §1.1, `hydra-proto.fbs`). `model_instance_id` is fenced by the identity
    // check above and is **never branched on** — note it is deliberately absent from [`FenceView`],
    // so no downstream code *can* branch on it. `branch_id` has no identity to check against: the
    // schema says "must be 0 in v1", and this is where that becomes true rather than aspirational.
    if wire_fence.branch_id() != 0 {
        return Err(WireError::ReservedInUse("branch_id"));
    }

    let view = FenceView {
        epoch: wire_fence.session_epoch(),
        recovery_id: wire_fence.recovery_id(),
        activation_attempt_id: wire_fence.activation_attempt_id(),
        stage_generation: wire_fence.stage_generation(),
    };

    let msg = decode_body(&frame, view)?;
    Ok((view, msg))
}

/// Cheap peek at a frame's body type — roots the FlatBuffer and reads `body_type` only, touching no
/// payload. Used by the **audit-C2 role gate**, which must decide whether a peer may send a message
/// family *before* the body is interpreted: an authorisation check that runs after decoding has
/// already let the unauthorised peer direct work.
pub fn peek_body(payload: &[u8]) -> Option<proto::Body> {
    flatbuffers::root::<proto::Frame>(payload).ok().map(|f| f.body_type())
}

/// Cheap peek: is this frame a `FWD`? (roots the flatbuffer, reads `body_type`, touches no tensor).
/// Used by the forwarding serve loop to route a boundary directly to the downstream peer.
pub fn is_fwd_frame(payload: &[u8]) -> bool {
    flatbuffers::root::<proto::Frame>(payload).map(|f| f.body_type() == proto::Body::Fwd).unwrap_or(false)
}

fn tuple_from_wire(t: proto::ActivationTuple<'_>) -> ActivationTuple {
    ActivationTuple {
        kind: match t.kind() {
            proto::ActivationKind::RECOVERY => ActivationKind::Recovery,
            _ => ActivationKind::Initial,
        },
        epoch: t.epoch(),
        recovery_id: t.recovery_id(),
        attempt: t.activation_attempt_id(),
        sampler_checkpoint_id: t.sampler_checkpoint_id(),
    }
}

/// Copy a wire byte-vector into an owned `Vec<u8>` **only after** checking it against its normative
/// cap (`hydra-proto.fbs`). The order is the whole point: the FlatBuffer itself is zero-copy, so
/// the allocation happens *here*, and a cap checked after the copy is not a cap.
fn capped_bytes(v: flatbuffers::Vector<'_, u8>, what: &'static str, cap: u32) -> Result<Vec<u8>, WireError> {
    let len = v.bytes().len() as u64;
    if len > cap as u64 {
        return Err(WireError::LimitExceeded { what, value: len, cap: cap as u64 });
    }
    Ok(v.bytes().to_vec())
}

/// Admission checks for a boundary tensor, run **before** the payload is copied into a `Vec<f32>`.
///
/// Two of the three are RESERVED-hook fences (BLUEPRINT §1.3, amended 2026-07-12):
///
/// * **`I8_BLOCKQ` is not offerable in v1.** The dtype stays in the schema (append-only evolution),
///   but the naive per-block characterization failed M2's mixed-backend tolerance for a *structural*
///   reason — outlier-dominated error — so a peer must not be able to put v1 on that path. Only
///   `F32` crosses the FFI, and `F16`/`BF16` are refused here too: the boundary is `f32` at this
///   layer by construction, and silently widening a narrower payload is exactly the kind of
///   accommodation that turns a precision decision into an accident.
/// * **`block_scales` is present iff `dtype == I8_BLOCKQ`** (schema comment, normative). Since
///   `I8_BLOCKQ` is refused, a frame carrying `block_scales` is *by definition* malformed — and
///   accepting-and-ignoring it would leave a field a future peer could use to smuggle state past a
///   build that does not understand it.
fn check_boundary_tensor(t: proto::Tensor<'_>, what: &'static str) -> Result<(), WireError> {
    if t.dtype() == proto::DType::I8_BLOCKQ {
        return Err(WireError::ReservedInUse("DType::I8_BLOCKQ"));
    }
    if t.dtype() != proto::DType::F32 {
        return Err(WireError::Malformed(format!("{what} must be F32 across the FFI")));
    }
    if t.block_scales().is_some() {
        return Err(WireError::ReservedInUse("Tensor::block_scales (valid only with I8_BLOCKQ)"));
    }
    // `MAX_TENSOR_BYTES` (48 MiB), checked **before** `bytes_to_f32_le` allocates. The frame cap
    // (64 MiB) already bounds the transport read, but it is a *different* cap on a *different*
    // quantity, and the schema declares this one as normative. A cap that exists in `limits.rs` and
    // is never called is documentation, not enforcement.
    let n = t.data().bytes().len() as u64;
    if !hydra_proto::limits::check_tensor_len(n).is_ok() {
        return Err(WireError::LimitExceeded {
            what,
            value: n,
            cap: hydra_proto::limits::MAX_TENSOR_BYTES as u64,
        });
    }
    Ok(())
}

fn decode_body(frame: &proto::Frame<'_>, view: FenceView) -> Result<Msg, WireError> {
    use proto::Body;
    match frame.body_type() {
        Body::ApplyToken => {
            let a = frame.body_as_apply_token().ok_or(WireError::Malformed("ApplyToken".into()))?;
            Ok(Msg::ApplyToken {
                input_pos: a.input_pos(),
                token_id: a.token_id(),
                no_sample: a.policy() == proto::SamplePolicy::NO_SAMPLE,
            })
        }
        Body::Fwd => {
            let f = frame.body_as_fwd().ok_or(WireError::Malformed("Fwd".into()))?;
            check_boundary_tensor(f.activations(), "Fwd activations")?;
            if !hydra_proto::limits::check_positions(f.n_positions() as u32).is_ok() {
                return Err(WireError::LimitExceeded {
                    what: "Fwd n_positions",
                    value: f.n_positions() as u64,
                    cap: hydra_proto::limits::MAX_POSITIONS_PER_FRAME as u64,
                });
            }
            let t = f.activations();
            Ok(Msg::Fwd {
                first_input_pos: f.first_input_pos(),
                no_sample: f.policy() == proto::SamplePolicy::NO_SAMPLE,
                activations: bytes_to_f32_le(t.data().bytes()),
            })
        }
        Body::AppliedAck => {
            let a = frame.body_as_applied_ack().ok_or(WireError::Malformed("AppliedAck".into()))?;
            Ok(Msg::AppliedAck {
                cumulative_input_pos: a.cumulative_input_pos(),
                output_checksum: match a.output_checksum() {
                    Some(v) => capped_bytes(v, "APPLIED_ACK output_checksum", HASH_LEN as u32)?,
                    None => Vec::new(),
                },
            })
        }
        Body::BoundaryCopy => {
            let b = frame.body_as_boundary_copy().ok_or(WireError::Malformed("BoundaryCopy".into()))?;
            check_boundary_tensor(b.activations(), "BoundaryCopy activations")?;
            let t = b.activations();
            Ok(Msg::BoundaryCopy {
                boundary_id: b.boundary_id(),
                first_input_pos: b.first_input_pos(),
                chunk_id: b.chunk_id(),
                activations: bytes_to_f32_le(t.data().bytes()),
            })
        }
        Body::DurabilityAck => {
            let d = frame.body_as_durability_ack().ok_or(WireError::Malformed("DurabilityAck".into()))?;
            Ok(Msg::DurabilityAck {
                boundary_id: d.boundary_id(),
                durable_through_input_pos: d.durable_through_input_pos(),
                storage_generation: d.storage_generation(),
            })
        }
        Body::CommitAck => {
            let c = frame.body_as_commit_ack().ok_or(WireError::Malformed("CommitAck".into()))?;
            Ok(Msg::CommitAck { committed_through_output_pos: c.committed_through_output_pos() })
        }
        Body::CommitSync => {
            let c = frame.body_as_commit_sync().ok_or(WireError::Malformed("CommitSync".into()))?;
            Ok(Msg::CommitSync { commit_up_to_output_pos: c.commit_up_to_output_pos() })
        }
        Body::CommitActivation => {
            let c = frame.body_as_commit_activation().ok_or(WireError::Malformed("CommitActivation".into()))?;
            Ok(Msg::CommitActivation(tuple_from_wire(c.tuple())))
        }
        Body::ActivationCommitted => {
            let c = frame.body_as_activation_committed().ok_or(WireError::Malformed("ActivationCommitted".into()))?;
            Ok(Msg::ActivationCommitted(tuple_from_wire(c.tuple())))
        }
        Body::FinalizeActivation => {
            let f = frame.body_as_finalize_activation().ok_or(WireError::Malformed("FinalizeActivation".into()))?;
            Ok(Msg::FinalizeActivation { attempt: f.tuple().activation_attempt_id() })
        }
        Body::ActivationFinalized => Ok(Msg::ActivationFinalized),
        Body::ActivationCommitAbort => {
            let a = frame.body_as_activation_commit_abort().ok_or(WireError::Malformed("ActivationCommitAbort".into()))?;
            Ok(Msg::ActivationCommitAbort { aborted_attempt: a.aborted_attempt_id() })
        }
        Body::BeginRecovery => {
            let b = frame.body_as_begin_recovery().ok_or(WireError::Malformed("BeginRecovery".into()))?;
            Ok(Msg::BeginRecovery {
                base: b.base_epoch(),
                target: b.target_epoch(),
                recovery_id: view.recovery_id,
                truncate_to: b.truncate_to_input_pos(),
            })
        }
        Body::RecoveryAck => {
            let r = frame.body_as_recovery_ack().ok_or(WireError::Malformed("RecoveryAck".into()))?;
            Ok(Msg::RecoveryAck { applied_input_pos: r.applied_input_pos() })
        }
        Body::CatchUpContext => {
            let c = frame.body_as_catch_up_context().ok_or(WireError::Malformed("CatchUpContext".into()))?;
            Ok(Msg::CatchUpContext { goal_input_pos: c.goal_input_pos() })
        }
        Body::CatchUpReady => {
            let c = frame.body_as_catch_up_ready().ok_or(WireError::Malformed("CatchUpReady".into()))?;
            Ok(Msg::CatchUpReady { applied_input_pos: c.applied_input_pos() })
        }
        Body::SampleNext => {
            let s = frame.body_as_sample_next().ok_or(WireError::Malformed("SampleNext".into()))?;
            Ok(Msg::SampleNext {
                output_pos: s.output_pos(),
                sampling_config_hash: capped_bytes(s.sampling_config_hash(), "sampling_config_hash", HASH_LEN as u32)?,
                expected_sampler_checkpoint_id: s.expected_sampler_checkpoint_id(),
            })
        }
        Body::Sampled => {
            let s = frame.body_as_sampled().ok_or(WireError::Malformed("Sampled".into()))?;
            Ok(Msg::Sampled {
                output_pos: s.output_pos(),
                token_id: s.token_id(),
                post_sample_snapshot: capped_bytes(s.post_sample_snapshot(), "post_sample_snapshot", hydra_proto::limits::MAX_SNAPSHOT_BYTES)?,
                sampler_state_digest: capped_bytes(s.sampler_state_digest(), "sampler_state_digest", HASH_LEN as u32)?,
            })
        }
        Body::InstallSamplerCheckpoint => {
            let i = frame.body_as_install_sampler_checkpoint().ok_or(WireError::Malformed("InstallSamplerCheckpoint".into()))?;
            Ok(Msg::InstallSamplerCheckpoint {
                checkpoint_id: i.checkpoint_id(),
                snapshot: capped_bytes(i.snapshot(), "sampler snapshot", hydra_proto::limits::MAX_SNAPSHOT_BYTES)?,
            })
        }
        Body::SamplerCheckpointInstalled => {
            let i = frame.body_as_sampler_checkpoint_installed().ok_or(WireError::Malformed("SamplerCheckpointInstalled".into()))?;
            Ok(Msg::SamplerCheckpointInstalled {
                checkpoint_id: i.checkpoint_id(),
                sampled_output_pos: i.sampled_output_pos(),
                resulting_state_digest: capped_bytes(i.resulting_state_digest(), "resulting_state_digest", HASH_LEN as u32)?,
            })
        }
        Body::Error => {
            let e = frame.body_as_error().ok_or(WireError::Malformed("Error".into()))?;
            Ok(Msg::Err { code: e.code().0 })
        }
        _ => Err(WireError::UnsupportedBody),
    }
}

// ----------------------------- encode -----------------------------

fn build_fence<'a>(
    fbb: &mut FlatBufferBuilder<'a>,
    fence: &SessionFence,
    view: FenceView,
) -> flatbuffers::WIPOffset<proto::Fence<'a>> {
    let cluster_id = Some(fbb.create_vector(&fence.cluster_id));
    let manifest_hash = Some(fbb.create_vector(&fence.manifest_hash));
    let model_instance_id = Some(fbb.create_vector(&fence.model_instance_id));
    let session_id = Some(fbb.create_vector(&fence.session_id));
    proto::Fence::create(
        fbb,
        &proto::FenceArgs {
            cluster_id,
            manifest_hash,
            model_instance_id,
            placement_version: 0,
            session_id,
            session_epoch: view.epoch,
            recovery_id: view.recovery_id,
            activation_attempt_id: view.activation_attempt_id,
            logical_context_id: 0,
            stage_context_generation: 0,
            stage_generation: view.stage_generation,
            frame_attempt_id: 0,
            branch_id: 0,
        },
    )
}

fn finish_frame(
    fbb: &mut FlatBufferBuilder<'_>,
    fence: flatbuffers::WIPOffset<proto::Fence>,
    body_type: proto::Body,
    body: flatbuffers::WIPOffset<flatbuffers::UnionWIPOffset>,
) -> Vec<u8> {
    let frame = proto::Frame::create(fbb, &proto::FrameArgs { fence: Some(fence), body_type, body: Some(body) });
    fbb.finish(frame, None);
    fbb.finished_data().to_vec()
}

/// Build a wire tuple from the reduced `hydra-state` tuple (single-stage placeholders for the
/// required vectors — full placement plumbing lands with the scheduler in M3).
fn build_tuple<'a>(
    fbb: &mut FlatBufferBuilder<'a>,
    t: &ActivationTuple,
    stage_generation: u64,
) -> flatbuffers::WIPOffset<proto::ActivationTuple<'a>> {
    let shard_generations = Some(fbb.create_vector(&[stage_generation]));
    let expected_applied_input_pos = Some(fbb.create_vector(&[0i64]));
    let sampler_state_checksum = Some(fbb.create_vector(&[0u8; HASH_LEN]));
    proto::ActivationTuple::create(
        fbb,
        &proto::ActivationTupleArgs {
            kind: match t.kind {
                ActivationKind::Recovery => proto::ActivationKind::RECOVERY,
                ActivationKind::Initial => proto::ActivationKind::INITIAL,
            },
            epoch: t.epoch,
            recovery_id: t.recovery_id,
            activation_attempt_id: t.attempt,
            placement_version: 0,
            logical_context_id: 0,
            shard_generations,
            recovery_goal_input_pos: 0,
            expected_applied_input_pos,
            expected_next_output_pos: 0,
            sampler_checkpoint_id: t.sampler_checkpoint_id,
            sampler_state_checksum,
        },
    )
}

pub fn encode_apply_token(fence: &SessionFence, epoch: Epoch, input_pos: i64, token_id: u32, no_sample: bool) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, fence, FenceView { epoch, recovery_id: 0, activation_attempt_id: 0, stage_generation: 0 });
    let body = proto::ApplyToken::create(
        &mut fbb,
        &proto::ApplyTokenArgs {
            input_pos,
            token_id,
            policy: if no_sample { proto::SamplePolicy::NO_SAMPLE } else { proto::SamplePolicy::SAMPLE },
            commit_up_to_output_pos: 0,
        },
    );
    finish_frame(&mut fbb, fence, proto::Body::ApplyToken, body.as_union_value())
}

pub fn encode_fwd(fence: &SessionFence, epoch: Epoch, first_input_pos: i64, no_sample: bool, activations: &[f32]) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, fence, FenceView { epoch, recovery_id: 0, activation_attempt_id: 0, stage_generation: 0 });
    let data = fbb.create_vector(&f32_to_bytes_le(activations));
    let dims = fbb.create_vector(&[activations.len() as u32]);
    let tensor = proto::Tensor::create(
        &mut fbb,
        &proto::TensorArgs { dtype: proto::DType::F32, dims: Some(dims), data: Some(data), block_scales: None },
    );
    // `n_positions` is a POSITION count, not a float count. It carried `activations.len()` — the
    // number of f32s — which was harmless only because nothing read it. Once M4·1 made the schema's
    // `<= MAX_POSITIONS_PER_FRAME` cap real, the wrong value became a live defect: every model with
    // `n_embd > 1024` (i.e. every model above the 0.5 B dev one — a 7 B has 4096) would have had its
    // boundaries REFUSED. v1's pipeline is strictly position-at-a-time, so a `FWD` carries exactly
    // one position — the same value `encode_boundary_copy` has always used. See PROJECT_STATE §7.27.
    let n_positions: u16 = 1;
    let body = proto::Fwd::create(
        &mut fbb,
        &proto::FwdArgs {
            first_input_pos,
            n_positions,
            policy: if no_sample { proto::SamplePolicy::NO_SAMPLE } else { proto::SamplePolicy::SAMPLE },
            commit_up_to_output_pos: 0,
            activations: Some(tensor),
        },
    );
    finish_frame(&mut fbb, fence, proto::Body::Fwd, body.as_union_value())
}

pub fn encode_applied_ack(fence: &SessionFence, epoch: Epoch, cumulative_input_pos: i64, checksum: &[u8]) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, fence, FenceView { epoch, recovery_id: 0, activation_attempt_id: 0, stage_generation: 0 });
    let output_checksum = fbb.create_vector(checksum);
    let body = proto::AppliedAck::create(
        &mut fbb,
        &proto::AppliedAckArgs { cumulative_input_pos, output_checksum: Some(output_checksum) },
    );
    finish_frame(&mut fbb, fence, proto::Body::AppliedAck, body.as_union_value())
}

pub fn encode_boundary_copy(fence: &SessionFence, epoch: Epoch, boundary_id: u32, first_input_pos: i64, chunk_id: u32, activations: &[f32]) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, fence, FenceView { epoch, recovery_id: 0, activation_attempt_id: 0, stage_generation: 0 });
    let data = fbb.create_vector(&f32_to_bytes_le(activations));
    let dims = fbb.create_vector(&[activations.len() as u32]);
    let tensor = proto::Tensor::create(
        &mut fbb,
        &proto::TensorArgs { dtype: proto::DType::F32, dims: Some(dims), data: Some(data), block_scales: None },
    );
    let body = proto::BoundaryCopy::create(
        &mut fbb,
        &proto::BoundaryCopyArgs { boundary_id, first_input_pos, n_positions: 1, chunk_id, activations: Some(tensor) },
    );
    finish_frame(&mut fbb, fence, proto::Body::BoundaryCopy, body.as_union_value())
}

pub fn encode_durability_ack(fence: &SessionFence, epoch: Epoch, boundary_id: u32, durable_through_input_pos: i64, storage_generation: u64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, fence, FenceView { epoch, recovery_id: 0, activation_attempt_id: 0, stage_generation: 0 });
    let body = proto::DurabilityAck::create(
        &mut fbb,
        &proto::DurabilityAckArgs { boundary_id, durable_through_input_pos, storage_generation },
    );
    finish_frame(&mut fbb, fence, proto::Body::DurabilityAck, body.as_union_value())
}

pub fn encode_commit_ack(fence: &SessionFence, epoch: Epoch, committed_through_output_pos: i64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, fence, FenceView { epoch, recovery_id: 0, activation_attempt_id: 0, stage_generation: 0 });
    let body = proto::CommitAck::create(&mut fbb, &proto::CommitAckArgs { committed_through_output_pos });
    finish_frame(&mut fbb, fence, proto::Body::CommitAck, body.as_union_value())
}

pub fn encode_commit_sync(fence: &SessionFence, epoch: Epoch, commit_up_to_output_pos: i64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, fence, FenceView { epoch, recovery_id: 0, activation_attempt_id: 0, stage_generation: 0 });
    let body = proto::CommitSync::create(&mut fbb, &proto::CommitSyncArgs { commit_up_to_output_pos });
    finish_frame(&mut fbb, fence, proto::Body::CommitSync, body.as_union_value())
}

pub fn encode_commit_activation(fence: &SessionFence, t: &ActivationTuple, stage_generation: u64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(
        &mut fbb,
        fence,
        FenceView { epoch: t.epoch, recovery_id: t.recovery_id, activation_attempt_id: t.attempt, stage_generation },
    );
    let tuple = build_tuple(&mut fbb, t, stage_generation);
    let body = proto::CommitActivation::create(&mut fbb, &proto::CommitActivationArgs { tuple: Some(tuple) });
    finish_frame(&mut fbb, fence, proto::Body::CommitActivation, body.as_union_value())
}

pub fn encode_activation_committed(fence: &SessionFence, t: &ActivationTuple, stage_generation: u64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(
        &mut fbb,
        fence,
        FenceView { epoch: t.epoch, recovery_id: t.recovery_id, activation_attempt_id: t.attempt, stage_generation },
    );
    let tuple = build_tuple(&mut fbb, t, stage_generation);
    let body = proto::ActivationCommitted::create(&mut fbb, &proto::ActivationCommittedArgs { tuple: Some(tuple) });
    finish_frame(&mut fbb, fence, proto::Body::ActivationCommitted, body.as_union_value())
}

pub fn encode_finalize_activation(fence: &SessionFence, t: &ActivationTuple, stage_generation: u64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(
        &mut fbb,
        fence,
        FenceView { epoch: t.epoch, recovery_id: t.recovery_id, activation_attempt_id: t.attempt, stage_generation },
    );
    let tuple = build_tuple(&mut fbb, t, stage_generation);
    let complete_record_hash = fbb.create_vector(&[0u8; HASH_LEN]);
    let body = proto::FinalizeActivation::create(
        &mut fbb,
        &proto::FinalizeActivationArgs { completion_id: 0, tuple: Some(tuple), complete_record_hash: Some(complete_record_hash) },
    );
    finish_frame(&mut fbb, fence, proto::Body::FinalizeActivation, body.as_union_value())
}

pub fn encode_activation_finalized(fence: &SessionFence, epoch: Epoch, attempt: AttemptId) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, fence, FenceView { epoch, recovery_id: 0, activation_attempt_id: attempt, stage_generation: 0 });
    let body = proto::ActivationFinalized::create(&mut fbb, &proto::ActivationFinalizedArgs { completion_id: 0 });
    finish_frame(&mut fbb, fence, proto::Body::ActivationFinalized, body.as_union_value())
}

pub fn encode_begin_recovery(fence: &SessionFence, base: Epoch, target: Epoch, recovery_id: RecoveryId, truncate_to: i64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, fence, FenceView { epoch: target, recovery_id, activation_attempt_id: 0, stage_generation: 0 });
    let body = proto::BeginRecovery::create(
        &mut fbb,
        &proto::BeginRecoveryArgs { base_epoch: base, target_epoch: target, truncate_to_input_pos: truncate_to },
    );
    finish_frame(&mut fbb, fence, proto::Body::BeginRecovery, body.as_union_value())
}

pub fn encode_recovery_ack(fence: &SessionFence, epoch: Epoch, recovery_id: RecoveryId, applied_input_pos: i64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, fence, FenceView { epoch, recovery_id, activation_attempt_id: 0, stage_generation: 0 });
    let body = proto::RecoveryAck::create(&mut fbb, &proto::RecoveryAckArgs { applied_input_pos });
    finish_frame(&mut fbb, fence, proto::Body::RecoveryAck, body.as_union_value())
}

pub fn encode_sample_next(fence: &SessionFence, epoch: Epoch, output_pos: i64, sampling_config_hash: &[u8], expected_sampler_checkpoint_id: u64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, fence, FenceView { epoch, recovery_id: 0, activation_attempt_id: 0, stage_generation: 0 });
    let cfg = fbb.create_vector(sampling_config_hash);
    let body = proto::SampleNext::create(
        &mut fbb,
        &proto::SampleNextArgs { output_pos, sampling_config_hash: Some(cfg), expected_sampler_checkpoint_id },
    );
    finish_frame(&mut fbb, fence, proto::Body::SampleNext, body.as_union_value())
}

pub fn encode_sampled(fence: &SessionFence, epoch: Epoch, output_pos: i64, token_id: u32, snapshot: &[u8], state_digest: &[u8]) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, fence, FenceView { epoch, recovery_id: 0, activation_attempt_id: 0, stage_generation: 0 });
    let snap = fbb.create_vector(snapshot);
    let dig = fbb.create_vector(state_digest);
    let body = proto::Sampled::create(
        &mut fbb,
        &proto::SampledArgs {
            output_pos,
            token_id,
            topk_ids: None,
            topk_logprobs: None,
            post_sample_snapshot: Some(snap),
            sampler_state_digest: Some(dig),
        },
    );
    finish_frame(&mut fbb, fence, proto::Body::Sampled, body.as_union_value())
}

pub fn encode_install_sampler_checkpoint(fence: &SessionFence, epoch: Epoch, checkpoint_id: u64, snapshot: &[u8]) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, fence, FenceView { epoch, recovery_id: 0, activation_attempt_id: 0, stage_generation: 0 });
    let snap = fbb.create_vector(snapshot);
    let body = proto::InstallSamplerCheckpoint::create(&mut fbb, &proto::InstallSamplerCheckpointArgs { checkpoint_id, snapshot: Some(snap) });
    finish_frame(&mut fbb, fence, proto::Body::InstallSamplerCheckpoint, body.as_union_value())
}

pub fn encode_sampler_checkpoint_installed(fence: &SessionFence, epoch: Epoch, checkpoint_id: u64, sampled_output_pos: i64, state_digest: &[u8]) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, fence, FenceView { epoch, recovery_id: 0, activation_attempt_id: 0, stage_generation: 0 });
    let dig = fbb.create_vector(state_digest);
    let body = proto::SamplerCheckpointInstalled::create(
        &mut fbb,
        &proto::SamplerCheckpointInstalledArgs { checkpoint_id, resulting_state_digest: Some(dig), sampled_output_pos },
    );
    finish_frame(&mut fbb, fence, proto::Body::SamplerCheckpointInstalled, body.as_union_value())
}

pub fn encode_catch_up_context(fence: &SessionFence, epoch: Epoch, recovery_id: RecoveryId, goal_input_pos: i64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, fence, FenceView { epoch, recovery_id, activation_attempt_id: 0, stage_generation: 0 });
    let body = proto::CatchUpContext::create(&mut fbb, &proto::CatchUpContextArgs { goal_input_pos });
    finish_frame(&mut fbb, fence, proto::Body::CatchUpContext, body.as_union_value())
}

pub fn encode_catch_up_ready(fence: &SessionFence, epoch: Epoch, recovery_id: RecoveryId, applied_input_pos: i64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, fence, FenceView { epoch, recovery_id, activation_attempt_id: 0, stage_generation: 0 });
    let body = proto::CatchUpReady::create(&mut fbb, &proto::CatchUpReadyArgs { applied_input_pos });
    finish_frame(&mut fbb, fence, proto::Body::CatchUpReady, body.as_union_value())
}

pub fn encode_error(fence: &SessionFence, epoch: Epoch, attempt: AttemptId, code: u16) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, fence, FenceView { epoch, recovery_id: 0, activation_attempt_id: attempt, stage_generation: 0 });
    let body = proto::Error::create(&mut fbb, &proto::ErrorArgs { code: proto::ErrCode(code), state: None, detail: None });
    finish_frame(&mut fbb, fence, proto::Body::Error, body.as_union_value())
}

// ----------------------------- f32 <-> bytes (little-endian, host==host over the wire) -----------------------------

pub(crate) fn f32_to_bytes_le(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for &x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

pub(crate) fn bytes_to_f32_le(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}
