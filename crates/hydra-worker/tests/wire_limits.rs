//! **M4·1 (b) — every normative wire cap is enforced BEFORE the allocation it bounds.**
//!
//! `hydra-proto.fbs` declares five hard limits and `hydra_proto::limits` implements the checks. The
//! M4·1 audit found that **only `MAX_FRAME_BYTES` was ever called** (`FrameHeader::parse`);
//! `check_tensor_len`, `check_positions`, `MAX_SNAPSHOT_BYTES` and `MAX_STRING_BYTES` existed and
//! were never invoked from anywhere in the tree. A cap that is defined and never called is
//! documentation, not enforcement — and "the frame cap already bounds it" is not an answer, because
//! it is a **different cap on a different quantity**: a 64 MiB frame could legally carry a 60 MiB
//! tensor against a declared 48 MiB tensor cap, and a sampler snapshot could be 64 MiB against a
//! declared 1 MiB one.
//!
//! # Why "before" is the whole claim
//!
//! FlatBuffers access is zero-copy: reading `t.data().bytes()` borrows the received buffer. The
//! allocation happens when the decoder copies it out (`bytes_to_f32_le`, `.to_vec()`). So the cap
//! must be checked **between** those two, and a test that only proves "an oversized frame errors"
//! would not distinguish a pre-allocation refusal from a post-allocation one. Each test below
//! therefore also asserts the *error kind* — `LimitExceeded` names the quantity and the cap, which
//! is only knowable at the point the check is made.
//!
//! See PROJECT_STATE §7.27 and `docs/SECURITY-CHECKLIST.md` (report Addendum 2 §D1).

use hydra_proto::limits::{MAX_POSITIONS_PER_FRAME, MAX_SNAPSHOT_BYTES, MAX_TENSOR_BYTES};
use hydra_proto::proto;
use hydra_worker::wire::{self, SessionKeys, WireError};

fn build_fence<'a>(
    fbb: &mut flatbuffers::FlatBufferBuilder<'a>,
    keys: &SessionKeys,
) -> flatbuffers::WIPOffset<proto::Fence<'a>> {
    let cluster_id = fbb.create_vector::<u8>(&keys.cluster_id);
    let manifest_hash = fbb.create_vector::<u8>(&keys.manifest_hash);
    let model_instance_id = fbb.create_vector::<u8>(&keys.model_instance_id);
    let session_id = fbb.create_vector::<u8>(&keys.session_id);
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
            branch_id: 0,
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

fn fwd(keys: &SessionKeys, tensor_bytes: usize, n_positions: u16) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::with_capacity(tensor_bytes + 4096);
    let fence = build_fence(&mut fbb, keys);
    let data = fbb.create_vector::<u8>(&vec![0u8; tensor_bytes]);
    let dims = fbb.create_vector::<u32>(&[1, (tensor_bytes / 4) as u32]);
    let t = proto::Tensor::create(
        &mut fbb,
        &proto::TensorArgs { dtype: proto::DType::F32, dims: Some(dims), data: Some(data), block_scales: None },
    );
    let body = proto::Fwd::create(
        &mut fbb,
        &proto::FwdArgs {
            first_input_pos: 0,
            n_positions,
            policy: proto::SamplePolicy::NO_SAMPLE,
            activations: Some(t),
            commit_up_to_output_pos: -1,
        },
    );
    finish(fbb, fence, proto::Body::Fwd, body.as_union_value())
}

fn sampled(keys: &SessionKeys, snapshot_bytes: usize) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::with_capacity(snapshot_bytes + 4096);
    let fence = build_fence(&mut fbb, keys);
    let snap = fbb.create_vector::<u8>(&vec![7u8; snapshot_bytes]);
    let digest = fbb.create_vector::<u8>(&[0u8; 32]);
    let body = proto::Sampled::create(
        &mut fbb,
        &proto::SampledArgs {
            output_pos: 0,
            token_id: 1,
            post_sample_snapshot: Some(snap),
            sampler_state_digest: Some(digest),
            topk_ids: None,
            topk_logprobs: None,
        },
    );
    finish(fbb, fence, proto::Body::Sampled, body.as_union_value())
}

/// `MAX_TENSOR_BYTES` (48 MiB) is enforced on every boundary-carrying body, before the `Vec<f32>`
/// copy. The oversized case is built just over the cap rather than at 64 MiB, so the test proves
/// the *tensor* cap and not the frame cap.
#[test]
fn an_oversized_tensor_is_refused_before_it_is_copied() {
    let keys = SessionKeys::dev(0x51);

    // Control first: a normal boundary decodes, so a refusal below is caused by the size.
    assert!(wire::decode(&fwd(&keys, 3584, 1), &keys).is_ok(), "control: an ordinary boundary decodes");

    let over = MAX_TENSOR_BYTES as usize + 4;
    let err = wire::decode(&fwd(&keys, over, 1), &keys).unwrap_err();
    assert_eq!(
        err,
        WireError::LimitExceeded { what: "Fwd activations", value: over as u64, cap: MAX_TENSOR_BYTES as u64 },
        "the refusal must name the quantity and the cap — which is only knowable at the check"
    );

    // Exactly at the cap is admitted: an off-by-one that refuses legal traffic is also a defect.
    assert!(
        wire::decode(&fwd(&keys, MAX_TENSOR_BYTES as usize, 1), &keys).is_ok(),
        "a tensor exactly at the cap is legal"
    );
}

/// `MAX_POSITIONS_PER_FRAME` (1024). `n_positions` is a `uint16`, so a peer can legally declare up
/// to 65 535 — 64× the cap — in a frame whose *bytes* are unremarkable. This is the case where the
/// frame cap gives no protection at all.
#[test]
fn an_oversized_position_count_is_refused() {
    let keys = SessionKeys::dev(0x52);
    assert!(wire::decode(&fwd(&keys, 3584, MAX_POSITIONS_PER_FRAME), &keys).is_ok(), "control: at the cap is legal");

    let err = wire::decode(&fwd(&keys, 3584, MAX_POSITIONS_PER_FRAME + 1), &keys).unwrap_err();
    assert_eq!(
        err,
        WireError::LimitExceeded {
            what: "Fwd n_positions",
            value: MAX_POSITIONS_PER_FRAME as u64 + 1,
            cap: MAX_POSITIONS_PER_FRAME as u64,
        }
    );
}

/// `MAX_SNAPSHOT_BYTES` (1 MiB). Sampler snapshots are small by design (Philox key + counter +
/// sampled position + the penalty window); a peer offering a megabytes-long "snapshot" is either
/// broken or hostile, and either way it must not be copied first and judged after.
#[test]
fn an_oversized_sampler_snapshot_is_refused() {
    let keys = SessionKeys::dev(0x53);
    assert!(wire::decode(&sampled(&keys, 128), &keys).is_ok(), "control: a real-sized snapshot decodes");

    let over = MAX_SNAPSHOT_BYTES as usize + 1;
    let err = wire::decode(&sampled(&keys, over), &keys).unwrap_err();
    assert_eq!(
        err,
        WireError::LimitExceeded { what: "post_sample_snapshot", value: over as u64, cap: MAX_SNAPSHOT_BYTES as u64 }
    );
}

/// Fixed-width digests are capped at their actual width. A 32-byte field is not a place to put a
/// megabyte, and accepting one "because it is under the frame cap" is how a fixed-width field
/// becomes an unbounded one.
#[test]
fn a_fixed_width_digest_field_is_capped_at_its_width() {
    let keys = SessionKeys::dev(0x54);
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let fence = build_fence(&mut fbb, &keys);
    let snap = fbb.create_vector::<u8>(&[7u8; 64]);
    let digest = fbb.create_vector::<u8>(&vec![0u8; 4096]); // 128× a BLAKE3 digest
    let body = proto::Sampled::create(
        &mut fbb,
        &proto::SampledArgs {
            output_pos: 0,
            token_id: 1,
            post_sample_snapshot: Some(snap),
            sampler_state_digest: Some(digest),
            topk_ids: None,
            topk_logprobs: None,
        },
    );
    let payload = finish(fbb, fence, proto::Body::Sampled, body.as_union_value());

    let err = wire::decode(&payload, &keys).unwrap_err();
    assert_eq!(err, WireError::LimitExceeded { what: "sampler_state_digest", value: 4096, cap: 32 });
}

/// The frame-level cap is enforced at the header, before the payload is read off the socket at all.
/// Included here so the checklist line "all frame/tensor/record limits enforced pre-allocation"
/// reads as one story rather than three files.
#[test]
fn the_frame_cap_is_enforced_at_the_header_before_any_payload_is_read() {
    use hydra_proto::framing::{FrameError, FrameHeader, FRAME_MAGIC, WIRE_VERSION};
    let mut hdr = Vec::new();
    hdr.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
    hdr.extend_from_slice(&WIRE_VERSION.to_le_bytes());
    hdr.extend_from_slice(&0u16.to_le_bytes());
    hdr.extend_from_slice(&(hydra_proto::limits::MAX_FRAME_BYTES + 1).to_le_bytes());

    // Note there is NO payload in `hdr` — 12 bytes total. The rejection therefore provably happens
    // before anything the header describes has been received, let alone allocated.
    assert_eq!(hdr.len(), hydra_proto::framing::HEADER_LEN);
    let err = FrameHeader::parse(&hdr).unwrap_err();
    assert!(matches!(err, FrameError::LimitExceeded { .. }), "got {err:?}");
}

/// **The regression for the defect the cap itself exposed (§7.27).**
///
/// `encode_fwd` put `activations.len()` — the number of **f32s** — into `n_positions`, a field the
/// schema declares as `<= MAX_POSITIONS_PER_FRAME`. It was harmless only while nothing read the
/// field. The moment the cap became real, the wrong value became a live defect: the dev model's
/// `n_embd` is 896 and slipped under the 1024 cap, but **every larger model would have had its
/// boundaries refused** — a 7 B has `n_embd = 4096`.
///
/// That is worth stating plainly, because it is the audit's own near-miss: enforcing a documented
/// cap against a field nobody had ever validated is exactly how a latent encoder bug becomes an
/// outage, and it would have shipped looking like a security fix.
#[test]
fn a_real_boundary_wider_than_the_position_cap_still_decodes() {
    let keys = SessionKeys::dev(0x55);
    // 4096 floats — a 7B-class `n_embd`, four times the position cap.
    let wide = vec![0.25f32; 4096];
    let payload = wire::encode_fwd(&keys, 0, 0, true, &wide);

    let (_, msg) = wire::decode(&payload, &keys).expect("a wide boundary must decode");
    match msg {
        hydra_worker::wire::Msg::Fwd { activations, .. } => assert_eq!(activations.len(), 4096),
        other => panic!("expected Fwd, got {other:?}"),
    }

    // And the field really does carry a POSITION count, not the float count.
    let frame = flatbuffers::root::<proto::Frame>(&payload).unwrap();
    assert_eq!(frame.body_as_fwd().unwrap().n_positions(), 1, "one FWD carries one position in v1");
}
