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
}

/// The stable part of the F1 fence tuple — one session's identity. Constant in v1 (spec §1.4).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SessionKeys {
    pub cluster_id: [u8; CLUSTER_ID_LEN],
    pub manifest_hash: [u8; HASH_LEN],
    pub model_instance_id: [u8; MODEL_INSTANCE_ID_LEN],
    pub session_id: [u8; SESSION_ID_LEN],
}

impl SessionKeys {
    /// A deterministic test/dev identity derived from a single seed byte (no RNG in this crate).
    pub fn dev(seed: u8) -> Self {
        SessionKeys {
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
    // --- control plane (maps 1:1 to StageEvent) ---
    CommitActivation(ActivationTuple),
    ActivationCommitted(ActivationTuple),
    FinalizeActivation { attempt: AttemptId },
    ActivationFinalized,
    ActivationCommitAbort { aborted_attempt: AttemptId },
    BeginRecovery { base: Epoch, target: Epoch, recovery_id: RecoveryId, truncate_to: i64 },
    RecoveryAck { applied_input_pos: i64 },
    /// `ERR_*` (e.g. `ERR_FENCED` from an F2 rejection).
    Err { code: u16 },
}

// ----------------------------- decode -----------------------------

fn fixed<const N: usize>(v: flatbuffers::Vector<'_, u8>, what: &'static str) -> Result<[u8; N], WireError> {
    let b = v.bytes();
    b.try_into().map_err(|_| WireError::Malformed(format!("{what}: expected {N} bytes, got {}", b.len())))
}

/// Parse a `Frame` payload, enforce the **F1 fence** against `keys`, and return the varying fence
/// fields + the native body. Rejects any frame whose identity tuple does not match this session —
/// **before** any boundary allocation (the fence read is O(1) and touches no payload tensors).
pub fn decode(payload: &[u8], keys: &SessionKeys) -> Result<(FenceView, Msg), WireError> {
    let frame = flatbuffers::root::<proto::Frame>(payload)
        .map_err(|e| WireError::Malformed(format!("not a Frame flatbuffer: {e}")))?;
    let fence = frame.fence();

    // F1: identity match. A stale/foreign frame is dropped here, never acted on.
    if fixed::<CLUSTER_ID_LEN>(fence.cluster_id(), "cluster_id")? != keys.cluster_id {
        return Err(WireError::FenceMismatch("cluster_id"));
    }
    if fixed::<HASH_LEN>(fence.manifest_hash(), "manifest_hash")? != keys.manifest_hash {
        return Err(WireError::FenceMismatch("manifest_hash"));
    }
    if fixed::<MODEL_INSTANCE_ID_LEN>(fence.model_instance_id(), "model_instance_id")? != keys.model_instance_id {
        return Err(WireError::FenceMismatch("model_instance_id"));
    }
    if fixed::<SESSION_ID_LEN>(fence.session_id(), "session_id")? != keys.session_id {
        return Err(WireError::FenceMismatch("session_id"));
    }

    let view = FenceView {
        epoch: fence.session_epoch(),
        recovery_id: fence.recovery_id(),
        activation_attempt_id: fence.activation_attempt_id(),
        stage_generation: fence.stage_generation(),
    };

    let msg = decode_body(&frame, view)?;
    Ok((view, msg))
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
            let t = f.activations();
            if t.dtype() != proto::DType::F32 {
                return Err(WireError::Malformed("Fwd activations must be F32 across the FFI".into()));
            }
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
                output_checksum: a.output_checksum().map(|v| v.bytes().to_vec()).unwrap_or_default(),
            })
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
    keys: &SessionKeys,
    view: FenceView,
) -> flatbuffers::WIPOffset<proto::Fence<'a>> {
    let cluster_id = Some(fbb.create_vector(&keys.cluster_id));
    let manifest_hash = Some(fbb.create_vector(&keys.manifest_hash));
    let model_instance_id = Some(fbb.create_vector(&keys.model_instance_id));
    let session_id = Some(fbb.create_vector(&keys.session_id));
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

pub fn encode_apply_token(keys: &SessionKeys, epoch: Epoch, input_pos: i64, token_id: u32, no_sample: bool) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, keys, FenceView { epoch, recovery_id: 0, activation_attempt_id: 0, stage_generation: 0 });
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

pub fn encode_fwd(keys: &SessionKeys, epoch: Epoch, first_input_pos: i64, no_sample: bool, activations: &[f32]) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, keys, FenceView { epoch, recovery_id: 0, activation_attempt_id: 0, stage_generation: 0 });
    let data = fbb.create_vector(&f32_to_bytes_le(activations));
    let dims = fbb.create_vector(&[activations.len() as u32]);
    let tensor = proto::Tensor::create(
        &mut fbb,
        &proto::TensorArgs { dtype: proto::DType::F32, dims: Some(dims), data: Some(data), block_scales: None },
    );
    let n_positions = u16::try_from(activations.len()).unwrap_or(u16::MAX); // placeholder; real n = len/n_embd
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

pub fn encode_applied_ack(keys: &SessionKeys, epoch: Epoch, cumulative_input_pos: i64, checksum: &[u8]) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, keys, FenceView { epoch, recovery_id: 0, activation_attempt_id: 0, stage_generation: 0 });
    let output_checksum = fbb.create_vector(checksum);
    let body = proto::AppliedAck::create(
        &mut fbb,
        &proto::AppliedAckArgs { cumulative_input_pos, output_checksum: Some(output_checksum) },
    );
    finish_frame(&mut fbb, fence, proto::Body::AppliedAck, body.as_union_value())
}

pub fn encode_commit_activation(keys: &SessionKeys, t: &ActivationTuple, stage_generation: u64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(
        &mut fbb,
        keys,
        FenceView { epoch: t.epoch, recovery_id: t.recovery_id, activation_attempt_id: t.attempt, stage_generation },
    );
    let tuple = build_tuple(&mut fbb, t, stage_generation);
    let body = proto::CommitActivation::create(&mut fbb, &proto::CommitActivationArgs { tuple: Some(tuple) });
    finish_frame(&mut fbb, fence, proto::Body::CommitActivation, body.as_union_value())
}

pub fn encode_activation_committed(keys: &SessionKeys, t: &ActivationTuple, stage_generation: u64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(
        &mut fbb,
        keys,
        FenceView { epoch: t.epoch, recovery_id: t.recovery_id, activation_attempt_id: t.attempt, stage_generation },
    );
    let tuple = build_tuple(&mut fbb, t, stage_generation);
    let body = proto::ActivationCommitted::create(&mut fbb, &proto::ActivationCommittedArgs { tuple: Some(tuple) });
    finish_frame(&mut fbb, fence, proto::Body::ActivationCommitted, body.as_union_value())
}

pub fn encode_finalize_activation(keys: &SessionKeys, t: &ActivationTuple, stage_generation: u64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(
        &mut fbb,
        keys,
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

pub fn encode_activation_finalized(keys: &SessionKeys, epoch: Epoch, attempt: AttemptId) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, keys, FenceView { epoch, recovery_id: 0, activation_attempt_id: attempt, stage_generation: 0 });
    let body = proto::ActivationFinalized::create(&mut fbb, &proto::ActivationFinalizedArgs { completion_id: 0 });
    finish_frame(&mut fbb, fence, proto::Body::ActivationFinalized, body.as_union_value())
}

pub fn encode_begin_recovery(keys: &SessionKeys, base: Epoch, target: Epoch, recovery_id: RecoveryId, truncate_to: i64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, keys, FenceView { epoch: target, recovery_id, activation_attempt_id: 0, stage_generation: 0 });
    let body = proto::BeginRecovery::create(
        &mut fbb,
        &proto::BeginRecoveryArgs { base_epoch: base, target_epoch: target, truncate_to_input_pos: truncate_to },
    );
    finish_frame(&mut fbb, fence, proto::Body::BeginRecovery, body.as_union_value())
}

pub fn encode_recovery_ack(keys: &SessionKeys, epoch: Epoch, recovery_id: RecoveryId, applied_input_pos: i64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, keys, FenceView { epoch, recovery_id, activation_attempt_id: 0, stage_generation: 0 });
    let body = proto::RecoveryAck::create(&mut fbb, &proto::RecoveryAckArgs { applied_input_pos });
    finish_frame(&mut fbb, fence, proto::Body::RecoveryAck, body.as_union_value())
}

pub fn encode_error(keys: &SessionKeys, epoch: Epoch, attempt: AttemptId, code: u16) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, keys, FenceView { epoch, recovery_id: 0, activation_attempt_id: attempt, stage_generation: 0 });
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
