//! **M4·1 (a) — the reserved-hook audit, AS A TEST.**
//!
//! BLUEPRINT §3 (M4) asks for "the reserved-hook audit (every `[RESERVED]` spec field exists in
//! `hydra-proto` and is fenced)". A prose audit is not the deliverable, because a prose audit is
//! true on the day it is written and silently false afterwards. Every claim below is an assertion
//! that runs on every `cargo test`.
//!
//! # What a reserved hook has to satisfy
//!
//! Two obligations, and they pull in opposite directions:
//!
//! 1. **It EXISTS, typed, in the schema.** BLUEPRINT §1.10: *"Reserved hooks exist in the spec;
//!    leave them as typed-but-unused fields."* The wire must not change shape on the day the
//!    feature lands, so the field is carried now and encoded now.
//! 2. **It is FENCED.** A field that exists but is not validated is worse than one that does not
//!    exist: it is an accepted input on a code path nobody implemented. v1 must be unable to be
//!    driven down it, and the refusal must be **loud** — never "ignore the field and proceed",
//!    which is the silent-downgrade shape this project refuses everywhere else.
//!
//! # What this audit found (2026-08-23)
//!
//! Obligation 1 held everywhere. Obligation 2 did **not**, in three places, and all three are
//! fixed in the same commit as this file — see PROJECT_STATE §7.27:
//!
//! * **`Fence.branch_id`** — schema says *"RESERVED, must be 0 in v1"*. It was **written as 0 and
//!   never read**: a peer could set it to anything and the frame was accepted.
//! * **`Tensor.block_scales`** — schema says *"present iff `dtype == I8_BLOCKQ`"*. It was never
//!   inspected, so it was an accepted-and-ignored field on every boundary frame.
//! * **Option B (spec §1.4)** — *"one active session per model instance (Option A); Option B
//!   [RESERVED]"*. The HTTP surface minted a **new session on every POST** without a matching
//!   `Idempotency-Key`, so Option B was not absent — it was the default. That one is asserted in
//!   `hydra-coordinator/tests/session_http.rs` (the fence lives where the sessions live).
//!
//! `model_instance_id` was already correct, and is asserted here so it stays that way.

use hydra_proto::proto;
use hydra_worker::wire::{self, SessionFence, WireError};

// ---------------------------------------------------------------------------------------------
// A frame builder that can emit values the real encoder never emits. That is the point: an audit
// that can only construct well-formed frames cannot prove anything about malformed ones.
// ---------------------------------------------------------------------------------------------

/// Build an `APPLY_TOKEN` frame with a caller-chosen `branch_id`.
fn apply_token_with_branch(fence: &SessionFence, branch_id: u32) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let wf = build_fence(&mut fbb, fence, branch_id);
    let body = proto::ApplyToken::create(
        &mut fbb,
        &proto::ApplyTokenArgs {
            input_pos: 0,
            token_id: 1,
            policy: proto::SamplePolicy::NO_SAMPLE,
            commit_up_to_output_pos: -1,
        },
    );
    finish(fbb, wf, proto::Body::ApplyToken, body.as_union_value())
}

/// Build a `FWD` frame with a caller-chosen tensor dtype and optional `block_scales`.
fn fwd_with_tensor(fence: &SessionFence, dtype: proto::DType, block_scales: Option<&[u8]>) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let wf = build_fence(&mut fbb, fence, 0);
    let data = fbb.create_vector::<u8>(&[0u8; 16]);
    let dims = fbb.create_vector::<u32>(&[1, 4]);
    let scales = block_scales.map(|b| fbb.create_vector::<u8>(b));
    let t = proto::Tensor::create(
        &mut fbb,
        &proto::TensorArgs { dtype, dims: Some(dims), data: Some(data), block_scales: scales },
    );
    let body = proto::Fwd::create(
        &mut fbb,
        &proto::FwdArgs {
            first_input_pos: 0,
            n_positions: 1,
            policy: proto::SamplePolicy::NO_SAMPLE,
            activations: Some(t),
            commit_up_to_output_pos: -1,
        },
    );
    finish(fbb, wf, proto::Body::Fwd, body.as_union_value())
}

fn build_fence<'a>(
    fbb: &mut flatbuffers::FlatBufferBuilder<'a>,
    fence: &SessionFence,
    branch_id: u32,
) -> flatbuffers::WIPOffset<proto::Fence<'a>> {
    let cluster_id = fbb.create_vector::<u8>(&fence.cluster_id);
    let manifest_hash = fbb.create_vector::<u8>(&fence.manifest_hash);
    let model_instance_id = fbb.create_vector::<u8>(&fence.model_instance_id);
    let session_id = fbb.create_vector::<u8>(&fence.session_id);
    proto::Fence::create(
        fbb,
        &proto::FenceArgs {
            cluster_id: Some(cluster_id),
            manifest_hash: Some(manifest_hash),
            model_instance_id: Some(model_instance_id),
            placement_version: 0,
            session_id: Some(session_id),
            session_epoch: 0,
            recovery_id: 0,
            activation_attempt_id: 0,
            logical_context_id: 0,
            stage_context_generation: 0,
            stage_generation: 0,
            frame_attempt_id: 0,
            branch_id,
        },
    )
}

fn finish(
    mut fbb: flatbuffers::FlatBufferBuilder<'_>,
    fence: flatbuffers::WIPOffset<proto::Fence<'_>>,
    body_type: proto::Body,
    body: flatbuffers::WIPOffset<flatbuffers::UnionWIPOffset>,
) -> Vec<u8> {
    let frame =
        proto::Frame::create(&mut fbb, &proto::FrameArgs { fence: Some(fence), body_type, body: Some(body) });
    fbb.finish(frame, None);
    fbb.finished_data().to_vec()
}

// ---------------------------------------------------------------------------------------------
// Obligation 1 — the reserved fields EXIST in hydra-proto, typed.
// ---------------------------------------------------------------------------------------------

/// Every `[RESERVED]` field named in spec §1.1 / `hydra-proto.fbs` is present in the generated
/// code with the type the schema declares. Reading them here is the assertion: if a field were
/// removed or renamed, this test would not compile — which is exactly the failure mode wanted,
/// because a reserved hook that quietly disappears breaks forward compatibility silently.
#[test]
fn every_reserved_field_exists_in_hydra_proto_with_its_declared_type() {
    let fence = SessionFence::dev(0x11);
    let payload = apply_token_with_branch(&fence, 0);
    let frame = flatbuffers::root::<proto::Frame>(&payload).expect("frame");
    let wire_fence = frame.fence();

    // `model_instance_id` — [uint8], exactly 16 bytes (spec §1.4 Option B hook).
    let mid: &[u8] = wire_fence.model_instance_id().bytes();
    assert_eq!(mid.len(), 16, "model_instance_id is a 16-byte field");
    assert_eq!(mid, fence.model_instance_id, "and it is carried on the wire, not dropped");

    // `branch_id` — uint32, carried on every frame.
    let _branch: u32 = wire_fence.branch_id();

    // `DType::I8_BLOCKQ` — still in the enum (append-only schema evolution: the dtype stays so
    // that re-enabling it later is a transport change, not a protocol or engine change).
    assert_eq!(proto::DType::I8_BLOCKQ.0, 3, "I8_BLOCKQ keeps its wire value");

    // `Tensor.block_scales` — the optional companion field of I8_BLOCKQ.
    let t = frame_tensor_type_is_optional();
    assert!(t, "block_scales is an optional [uint8] on Tensor");
}

fn frame_tensor_type_is_optional() -> bool {
    // A tensor built without block_scales must read back as `None` — i.e. the field is genuinely
    // optional rather than defaulted to an empty vector, which is what "present iff" requires.
    let fence = SessionFence::dev(0x12);
    let payload = fwd_with_tensor(&fence, proto::DType::F32, None);
    let frame = flatbuffers::root::<proto::Frame>(&payload).expect("frame");
    frame.body_as_fwd().expect("fwd").activations().block_scales().is_none()
}

// ---------------------------------------------------------------------------------------------
// Obligation 2 — the reserved fields are FENCED.
// ---------------------------------------------------------------------------------------------

/// `branch_id` must be 0 in v1 (`hydra-proto.fbs`). A frame that sets it is refused **before** the
/// body is interpreted — the branch feature does not exist, so a frame asking for it must not be
/// served under v1 semantics as if it had never asked.
#[test]
fn a_nonzero_branch_id_is_refused() {
    let fence = SessionFence::dev(0x21);

    // Control: the same frame with branch_id = 0 decodes. Without this the test could pass because
    // the builder is broken rather than because the fence works (the same pairing principle as the
    // D0 zero-traffic test and the TLC cert legs).
    let ok = wire::decode(&apply_token_with_branch(&fence, 0), &fence);
    assert!(ok.is_ok(), "control: branch_id = 0 must decode, got {ok:?}");

    for branch in [1u32, 2, 0xFFFF_FFFF] {
        let err = wire::decode(&apply_token_with_branch(&fence, branch), &fence).unwrap_err();
        assert_eq!(
            err,
            WireError::ReservedInUse("branch_id"),
            "branch_id = {branch} must be refused as a reserved-hook violation, got {err:?}"
        );
    }
}

/// `I8_BLOCKQ` is **not offerable in v1** (BLUEPRINT §1.3, amended 2026-07-12: the naive per-block
/// scheme failed M2's mixed-backend tolerance for a structural, backend-invariant reason). The
/// dtype stays in the schema; a peer must not be able to put this build on that path.
#[test]
fn an_i8_blockq_boundary_is_refused_as_reserved() {
    let fence = SessionFence::dev(0x22);

    let ok = wire::decode(&fwd_with_tensor(&fence, proto::DType::F32, None), &fence);
    assert!(ok.is_ok(), "control: an F32 boundary must decode, got {ok:?}");

    let err = wire::decode(&fwd_with_tensor(&fence, proto::DType::I8_BLOCKQ, Some(&[1, 2, 3, 4])), &fence).unwrap_err();
    assert_eq!(err, WireError::ReservedInUse("DType::I8_BLOCKQ"));
}

/// The narrower float dtypes are refused too. The boundary is `f32` at this layer by construction
/// (BLUEPRINT §1.3: `f32` is mandatory for the exact-token-equality tier), and silently widening a
/// narrower payload would turn a ratified precision decision into an accident.
#[test]
fn a_non_f32_boundary_is_refused_rather_than_widened() {
    let fence = SessionFence::dev(0x23);
    for dt in [proto::DType::F16, proto::DType::BF16] {
        let err = wire::decode(&fwd_with_tensor(&fence, dt, None), &fence).unwrap_err();
        assert!(
            matches!(err, WireError::Malformed(ref m) if m.contains("F32")),
            "dtype {dt:?} must be refused, got {err:?}"
        );
    }
}

/// `block_scales` is *present iff* `dtype == I8_BLOCKQ`. Since `I8_BLOCKQ` is refused, a frame that
/// carries `block_scales` is by definition malformed — and accepting-and-ignoring it would leave a
/// field a future peer could use to smuggle state past a build that does not understand it.
#[test]
fn block_scales_on_a_non_i8_tensor_is_refused_not_ignored() {
    let fence = SessionFence::dev(0x24);
    let err = wire::decode(&fwd_with_tensor(&fence, proto::DType::F32, Some(&[9, 9, 9, 9])), &fence).unwrap_err();
    assert_eq!(err, WireError::ReservedInUse("Tensor::block_scales (valid only with I8_BLOCKQ)"));
}

/// `model_instance_id` is **validated** — a frame carrying a different one is refused at F1, before
/// any engine work.
#[test]
fn a_foreign_model_instance_id_is_refused_at_f1() {
    let ours = SessionFence::dev(0x31);
    let mut theirs = ours.clone();
    theirs.model_instance_id = [0xAB; 16];

    // A frame built with *their* instance id, decoded against *our* fence.
    let payload = apply_token_with_branch(&theirs, 0);
    let err = wire::decode(&payload, &ours).unwrap_err();
    assert_eq!(err, WireError::FenceMismatch("model_instance_id"));
}

/// …and it is **never branched on**. This is asserted structurally rather than by inspection:
/// [`wire::FenceView`] — the only fence data that reaches any decision-making code — is
/// destructured exhaustively here, so if `model_instance_id` (or `branch_id`) were ever added to
/// it, this test would **fail to compile**. Downstream code cannot branch on a value it is never
/// handed.
#[test]
fn model_instance_id_and_branch_id_never_reach_decision_making_code() {
    let fence = SessionFence::dev(0x32);
    let (view, _msg) = wire::decode(&apply_token_with_branch(&fence, 0), &fence).expect("decode");

    // Exhaustive destructure: adding a field to FenceView breaks this line.
    let wire::FenceView { epoch, recovery_id, activation_attempt_id, stage_generation } = view;
    assert_eq!((epoch, recovery_id, activation_attempt_id, stage_generation), (0, 0, 0, 0));
}

// ---------------------------------------------------------------------------------------------
// The out-of-scope hooks BLUEPRINT §1.10 says must be typed-but-unused.
// ---------------------------------------------------------------------------------------------

/// BLUEPRINT §1.10 lists what v1 must **not** build: phones as workers, WAN/NAT traversal,
/// speculative decoding, MoE, beam search, paged KV, multi-session, coordinator election, public
/// swarms. The audit obligation for these is the **absence of a wire surface** — none of them may
/// have a message type, because a typed message is a half-built feature and a reachable code path
/// for a peer that asks for it.
///
/// The `Body` union is the entire protocol surface, so enumerating it **is** the assertion. The
/// list below is spec §4's message inventory, transcribed; every union variant must appear in it
/// and vice-versa. A new variant — for speculation, branching, a second session, coordinator
/// election — fails here, which forces the addition to be a deliberate reviewed act rather than a
/// quiet one, and a *removed* variant fails here too (evolution rule: never reuse or renumber).
///
/// **Note what this does NOT assert:** that every listed message is *implemented*. Several are not
/// yet (the placement quartet, the shard-lifecycle sextet, `REJOIN`/`CANCEL`/`CLEANED`) — they are
/// in-scope v1 protocol that later slices wire up. That gap is covered by the next test, which
/// pins the behaviour that matters *today*: an unimplemented variant is **refused**, never
/// silently dropped.
#[test]
fn the_wire_body_union_is_exactly_the_spec_4_message_inventory() {
    use proto::Body;
    let spec_4 = [
        // data plane (spec §4)
        Body::ApplyToken, Body::Fwd, Body::Sampled, Body::SampleNext, Body::AppliedAck,
        Body::BoundaryCopy, Body::DurabilityAck, Body::CommitAck, Body::CommitSync,
        // recovery / reset control plane
        Body::BeginRecovery, Body::RecoveryAck, Body::ResetRecoveryAttempt, Body::ResetAck,
        // the placement quartet
        Body::PreparePlacement, Body::PlacementReady, Body::InstallPlacement, Body::PlacementInstalled,
        // the shard lifecycle sextet
        Body::AttachContextShard, Body::ContextShardAttached, Body::ContextReady,
        Body::CatchUpContext, Body::CatchUpReady, Body::DestroyContext, Body::DetachShard,
        // segment + sampler checkpoint planes
        Body::PrepareSegmentCheckpoint, Body::SegmentCheckpointReady,
        Body::InstallSamplerCheckpoint, Body::SamplerCheckpointInstalled,
        // the activation transaction
        Body::CommitActivation, Body::ActivationCommitted, Body::FinalizeActivation,
        Body::ActivationFinalized, Body::ActivationCommitAbort,
        // session lifecycle + telemetry + errors
        Body::Rejoin, Body::Cancel, Body::Cleaned, Body::Heartbeat, Body::Error,
    ];
    let schema: Vec<Body> = Body::ENUM_VALUES.iter().copied().filter(|b| *b != Body::NONE).collect();

    for b in &schema {
        assert!(
            spec_4.contains(b),
            "wire message type {b:?} is not in spec §4's inventory. If it is a deliberate v1 \
             addition, amend the spec first and add it here; if it is an out-of-scope hook \
             (BLUEPRINT §1.10 — speculation, MoE, multi-session, branching, coordinator election, \
             paged KV, beam search), it must NOT have a wire surface at all."
        );
    }
    for b in &spec_4 {
        assert!(schema.contains(b), "{b:?} is in spec §4 but no longer in the schema (evolution rule: never remove)");
    }
    assert_eq!(schema.len(), spec_4.len(), "the union and spec §4's inventory must be the same size");
}

/// The schema's evolution rule is normative: *"an unknown union variant within the same major
/// `wire_version` yields `ErrCode.ERR_UNSUPPORTED_VERSION` as a structured reply, **never a silent
/// drop**."* A variant this build has not wired up yet is exactly that case, and the property that
/// matters is that it is **refused**, because a silently-dropped control frame is indistinguishable
/// from a lost one — and the recovery machinery is built on the assumption that a peer either acts
/// on a frame or says it cannot.
#[test]
fn an_unimplemented_but_in_spec_body_is_refused_never_silently_dropped() {
    let fence = SessionFence::dev(0x41);
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let wf = build_fence(&mut fbb, &fence, 0);
    // `CANCEL` is in spec §4 and deliberately not wired up yet (the DELETE surface is a named
    // deferral in §0(c)); it stands in for every not-yet-implemented variant.
    let body = proto::Cancel::create(&mut fbb, &proto::CancelArgs {});
    let payload = finish(fbb, wf, proto::Body::Cancel, body.as_union_value());

    let err = wire::decode(&payload, &fence).unwrap_err();
    assert_eq!(
        err,
        WireError::UnsupportedBody,
        "an unimplemented body must produce a structured refusal, not Ok(()) and not a drop"
    );
}

// ---------------------------------------------------------------------------------------------
// Audit C2 — the message-family gate (the second half of the role binding).
// ---------------------------------------------------------------------------------------------

/// **The role table is an ALLOW-list, so a message family nobody has authorised has no sender.**
///
/// This is the property that makes the gate survive the protocol growing. With a deny-list, adding
/// a message family silently grants it to everyone; with an allow-list, forgetting to state a sender
/// makes it unsendable. The failure mode of forgetting is a refusal, not a grant.
#[test]
fn the_role_gate_is_an_allow_list_so_an_unstated_family_has_no_sender() {
    use hydra_proto::proto::Body;
    use hydra_transport::roles::PeerRole;
    use hydra_worker::worker::role_may_send;

    let roles = [
        PeerRole::Coordinator,
        PeerRole::Stage { rank: 0 },
        PeerRole::Stage { rank: 1 },
        PeerRole::DurabilityTarget,
    ];

    // Families v1 deliberately gives NO sender: the placement quartet, the shard-lifecycle sextet,
    // REJOIN/CLEANED, HEARTBEAT and the sampler/segment replies are either unimplemented or are
    // things a worker SENDS rather than RECEIVES. None of them may arrive at a worker from anyone.
    for body in [
        Body::PreparePlacement,
        Body::PlacementReady,
        Body::InstallPlacement,
        Body::PlacementInstalled,
        Body::AttachContextShard,
        Body::DestroyContext,
        Body::DetachShard,
        Body::Rejoin,
        Body::Cleaned,
        Body::Sampled,
        Body::SamplerCheckpointInstalled,
        Body::ActivationCommitted,
        Body::ActivationFinalized,
        Body::RecoveryAck,
    ] {
        for role in roles {
            assert!(
                !role_may_send(role, body),
                "{body:?} has no stated sender in v1, so NO role may send it — a {} was allowed to. \
                 An allow-list's whole value is that forgetting produces a refusal, not a grant.",
                role.label()
            );
        }
    }
}

/// **THE FINDING, as a table.** Before the gate, a stage could send a coordinator's frames and the
/// durability target could send a stage's. Each row below is a privilege that used to exist.
#[test]
fn a_stage_may_not_speak_as_the_coordinator_and_a_durability_target_may_not_speak_as_a_stage() {
    use hydra_proto::proto::Body;
    use hydra_transport::roles::PeerRole;
    use hydra_worker::worker::role_may_send;

    let stage = PeerRole::Stage { rank: 0 };
    let dur = PeerRole::DurabilityTarget;
    let coord = PeerRole::Coordinator;

    // The activation transaction is the coordinator's alone (spec §6.6).
    for body in [Body::CommitActivation, Body::FinalizeActivation, Body::ActivationCommitAbort] {
        assert!(role_may_send(coord, body), "control: the coordinator may send {body:?}");
        assert!(!role_may_send(stage, body), "a STAGE must not be able to drive the activation transaction ({body:?})");
        assert!(!role_may_send(dur, body), "a DURABILITY TARGET must not drive activation ({body:?})");
    }

    // Sampling is the coordinator's to request (spec §1.4's ownership boundary).
    assert!(role_may_send(coord, Body::SampleNext));
    assert!(!role_may_send(stage, Body::SampleNext), "a stage must not request a sample");
    assert!(!role_may_send(dur, Body::SampleNext), "a durability target must not request a sample");

    // Recovery is the coordinator's to open.
    for body in [Body::BeginRecovery, Body::ResetRecoveryAttempt, Body::CatchUpContext] {
        assert!(role_may_send(coord, body), "control: the coordinator opens recovery ({body:?})");
        assert!(!role_may_send(dur, body), "a durability target must not open recovery ({body:?})");
    }

    // And the durability target's own lane is exactly one frame wide.
    assert!(role_may_send(dur, Body::DurabilityAck));
    assert!(!role_may_send(dur, Body::Fwd), "a durability target must not forward boundaries");
    assert!(!role_may_send(dur, Body::ApplyToken), "a durability target must not drive the data plane");
}

/// The gate runs on a **peek**, before the body is decoded — an authorisation check that runs after
/// decoding has already let an unauthorised peer direct work (buffer allocation, tensor copies,
/// engine calls). A frame whose body does not even parse is refused too, since an unidentifiable
/// family cannot be shown to be permitted.
#[test]
fn an_unparseable_frame_is_refused_by_the_gate_rather_than_passed_to_the_decoder() {
    use hydra_transport::roles::PeerRole;
    use hydra_worker::worker::check_role;

    let err = check_role(PeerRole::Coordinator, b"not a flatbuffer at all").unwrap_err();
    assert!(
        err.to_string().contains("unparseable"),
        "an unidentifiable body must be refused by the gate, got: {err}"
    );

    // Control: a real coordinator frame passes the gate.
    let fence = SessionFence::dev(0x71);
    let frame = wire::encode_apply_token(&fence, 0, 0, 1, true);
    check_role(PeerRole::Coordinator, &frame).expect("control: a coordinator may send APPLY_TOKEN");
    // …and the same frame from a durability target does not.
    check_role(PeerRole::DurabilityTarget, &frame)
        .expect_err("a durability target may not send APPLY_TOKEN");
}
