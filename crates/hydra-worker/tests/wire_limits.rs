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
use hydra_worker::wire::{self, SessionFence, WireError};

fn build_fence<'a>(
    fbb: &mut flatbuffers::FlatBufferBuilder<'a>,
    fence: &SessionFence,
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

/// The dev model's boundary width, the one every engine-gated test uses.
const N_EMBD: usize = 896;

/// A **self-consistent** FWD: `n_positions` positions of `n_embd` floats, `dims = [n_positions,
/// n_embd]`, `data = n_positions × 4 × n_embd` bytes. The helper builds the only shape C3 admits
/// so each test below changes exactly one thing; the raw builder [`fwd_raw`] exists for the
/// tests whose *point* is an inconsistent frame.
fn fwd(fence: &SessionFence, n_positions: u16, n_embd: usize) -> Vec<u8> {
    let bytes = n_positions as usize * 4 * n_embd;
    fwd_raw(fence, bytes, &[n_positions as u32, n_embd as u32], n_positions)
}

/// A FWD with caller-chosen byte count, `dims`, and `n_positions` — the three independent claims a
/// frame makes about its own shape, each settable on its own.
fn fwd_raw(fence: &SessionFence, tensor_bytes: usize, dims: &[u32], n_positions: u16) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::with_capacity(tensor_bytes + 4096);
    let wf = build_fence(&mut fbb, fence);
    let data = fbb.create_vector::<u8>(&vec![0u8; tensor_bytes]);
    let dims = fbb.create_vector::<u32>(dims);
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
    finish(fbb, wf, proto::Body::Fwd, body.as_union_value())
}

fn sampled(fence: &SessionFence, snapshot_bytes: usize) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::with_capacity(snapshot_bytes + 4096);
    let wf = build_fence(&mut fbb, fence);
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
    finish(fbb, wf, proto::Body::Sampled, body.as_union_value())
}

/// `MAX_TENSOR_BYTES` (48 MiB) is enforced on every boundary-carrying body, before the `Vec<f32>`
/// copy. The oversized case is built just over the cap rather than at 64 MiB, so the test proves
/// the *tensor* cap and not the frame cap.
#[test]
fn an_oversized_tensor_is_refused_before_it_is_copied() {
    let fence = SessionFence::dev(0x51);

    // Control first: a normal boundary decodes, so a refusal below is caused by the size.
    assert!(wire::decode(&fwd(&fence, 1, N_EMBD), &fence).is_ok(), "control: an ordinary boundary decodes");

    // One position, one float over the cap — the frame is shape-consistent (dims agree with the
    // bytes), so the ONLY thing wrong with it is its size, and the cap must be what refuses it.
    let over_floats = MAX_TENSOR_BYTES as usize / 4 + 1;
    let over = over_floats * 4;
    let err = wire::decode(&fwd(&fence, 1, over_floats), &fence).unwrap_err();
    assert_eq!(
        err,
        WireError::LimitExceeded { what: "Fwd activations", value: over as u64, cap: MAX_TENSOR_BYTES as u64 },
        "the refusal must name the quantity and the cap — which is only knowable at the check"
    );

    // Exactly at the cap is admitted: an off-by-one that refuses legal traffic is also a defect.
    assert!(
        wire::decode(&fwd(&fence, 1, MAX_TENSOR_BYTES as usize / 4), &fence).is_ok(),
        "a tensor exactly at the cap is legal"
    );
}

/// `MAX_POSITIONS_PER_FRAME` (1024). `n_positions` is a `uint16`, so a peer can legally declare up
/// to 65 535 — 64× the cap — in a frame whose *bytes* are unremarkable. This is the case where the
/// frame cap gives no protection at all.
///
/// # ⚑ THE BLIND ORACLE THIS TEST USED TO BE (audit C3, fixed 2026-08-23)
///
/// Until C3 this test's **control** asserted that a frame with **3 584 tensor bytes** (one
/// position of the dev model's 896 floats) declaring **`n_positions = 1024`** decodes OK —
/// "at the cap is legal". It *enshrined* the exact length/shape inconsistency C3 forbids: the
/// bytes said one position, the declaration said a thousand and twenty-four, and the codec was
/// asserted correct for accepting both at once. The control did not merely fail to catch the
/// defect; it would have **failed the fix** — any codec that cross-checked length against shape
/// would have turned this test red, and the natural reading of a red control is "the fix broke
/// decoding", not "the control was wrong".
///
/// **Which oracle was blind, and why:** the oracle here was "the cap check" and it was asked one
/// question — *is `n_positions` ≤ 1024?* — with a fixture whose other two shape claims (bytes,
/// `dims`) were chosen for convenience rather than consistency. An oracle that checks one of three
/// mutually-constraining claims in isolation cannot express a violation of their *relationship*,
/// so it cannot guard it, so its green meant nothing about it. This is the same shape as the
/// checklist rows (§7.31) and M6's hard-coded simulator fsync: **a harness that cannot produce
/// the failure it nominally guards.** The control now builds a consistent 1024-position frame
/// (which M4 then refuses for being more than one position — see the next test — so the cap is
/// asserted through the *order* of checks: caps before shape), and the inconsistency itself has
/// its own tests below.
#[test]
fn an_oversized_position_count_is_refused() {
    let fence = SessionFence::dev(0x52);

    // The over-cap frame is self-consistent in every respect but the cap — 1025 positions of
    // 896 floats, dims and bytes agreeing — so the cap, and only the cap, is what refuses it.
    let err = wire::decode(&fwd(&fence, MAX_POSITIONS_PER_FRAME + 1, N_EMBD), &fence).unwrap_err();
    assert_eq!(
        err,
        WireError::LimitExceeded {
            what: "n_positions",
            value: MAX_POSITIONS_PER_FRAME as u64 + 1,
            cap: MAX_POSITIONS_PER_FRAME as u64,
        }
    );

    // At the cap the cap is satisfied, and the refusal that remains is M4's (one position in v1),
    // NOT a cap refusal. This pins the check ORDER: a frame is refused as oversized before it is
    // refused as mis-shaped, so an attacker learns the cheaper fact first and the codec never
    // reasons about the shape of something it should not have admitted.
    let err = wire::decode(&fwd(&fence, MAX_POSITIONS_PER_FRAME, N_EMBD), &fence).unwrap_err();
    assert!(matches!(err, WireError::Shape { expected: 1, got: 1024, .. }), "got {err:?}");
}

/// **M4 — `n_positions == 1` in v1.** The pipeline is position-at-a-time by construction; every
/// encoder in `hydra-worker` writes `1`. A peer declaring two positions — even with perfectly
/// consistent bytes — is speaking a protocol this build does not run.
#[test]
fn more_than_one_position_per_boundary_is_refused_in_v1() {
    let fence = SessionFence::dev(0x56);
    assert!(wire::decode(&fwd(&fence, 1, N_EMBD), &fence).is_ok(), "control: one position decodes");
    let err = wire::decode(&fwd(&fence, 2, N_EMBD), &fence).unwrap_err();
    assert_eq!(err, WireError::Shape { what: "n_positions (v1 carries exactly one position per boundary)", expected: 1, got: 2 });
    let err = wire::decode(&fwd(&fence, 0, N_EMBD), &fence).unwrap_err();
    assert_eq!(err, WireError::Shape { what: "n_positions (v1 carries exactly one position per boundary)", expected: 1, got: 0 });
}

/// **C3 — `data.len() == n_positions × 4 × n_embd`, cross-checked at decode.** Each case changes
/// exactly one of the three shape claims (bytes, `dims`, `n_positions`) against the control, so
/// the refusal is attributable. Before C3 every one of these decoded, and the byte count alone
/// decided how many positions the engine applied.
#[test]
fn a_boundary_whose_bytes_disagree_with_its_declared_shape_is_refused() {
    let fence = SessionFence::dev(0x57);
    let one = 4 * N_EMBD;
    assert!(wire::decode(&fwd_raw(&fence, one, &[1, N_EMBD as u32], 1), &fence).is_ok(), "control");

    // Two positions' worth of bytes under a one-position declaration: the pre-C3 silent
    // "apply as two positions" case.
    let err = wire::decode(&fwd_raw(&fence, 2 * one, &[1, N_EMBD as u32], 1), &fence).unwrap_err();
    assert_eq!(
        err,
        WireError::Shape { what: "tensor bytes vs declared n_positions × 4 × n_embd", expected: one as u64, got: 2 * one as u64 }
    );

    // One byte short: the pre-C3 `chunks_exact` silent-truncation case.
    let err = wire::decode(&fwd_raw(&fence, one - 1, &[1, N_EMBD as u32], 1), &fence).unwrap_err();
    assert_eq!(
        err,
        WireError::Shape { what: "tensor bytes vs declared n_positions × 4 × n_embd", expected: one as u64, got: one as u64 - 1 }
    );

    // One byte over (not a multiple of four).
    let err = wire::decode(&fwd_raw(&fence, one + 1, &[1, N_EMBD as u32], 1), &fence).unwrap_err();
    assert!(matches!(err, WireError::Shape { .. }), "got {err:?}");

    // `dims` disagrees with `n_positions`.
    let err = wire::decode(&fwd_raw(&fence, one, &[2, N_EMBD as u32], 1), &fence).unwrap_err();
    assert_eq!(err, WireError::Shape { what: "tensor dims[0] vs n_positions", expected: 1, got: 2 });

    // A one-dimensional `[len]` — what `encode_fwd` itself wrote before C3 — is refused: a shape
    // that cannot be cross-checked against a position count is not a shape.
    let err = wire::decode(&fwd_raw(&fence, one, &[N_EMBD as u32], 1), &fence).unwrap_err();
    assert_eq!(err, WireError::Shape { what: "tensor dims rank", expected: 2, got: 1 });

    // A zero-width boundary.
    let err = wire::decode(&fwd_raw(&fence, 0, &[1, 0], 1), &fence).unwrap_err();
    assert_eq!(err, WireError::Shape { what: "tensor dims[1] (n_embd)", expected: 1, got: 0 });
}

/// The same class on the durability body (rule 17: a class is fixed in every parser in the same
/// wave). `BOUNDARY_COPY` is what D1 recovery replays, so a lying shape here is a lying KV.
#[test]
fn a_boundary_copy_whose_bytes_disagree_with_its_declared_shape_is_refused() {
    let fence = SessionFence::dev(0x58);
    let build = |tensor_bytes: usize, dims: &[u32], n_positions: u16| -> Vec<u8> {
        let mut fbb = flatbuffers::FlatBufferBuilder::with_capacity(tensor_bytes + 4096);
        let wf = build_fence(&mut fbb, &fence);
        let data = fbb.create_vector::<u8>(&vec![0u8; tensor_bytes]);
        let dims = fbb.create_vector::<u32>(dims);
        let t = proto::Tensor::create(
            &mut fbb,
            &proto::TensorArgs { dtype: proto::DType::F32, dims: Some(dims), data: Some(data), block_scales: None },
        );
        let body = proto::BoundaryCopy::create(
            &mut fbb,
            &proto::BoundaryCopyArgs { boundary_id: 0, first_input_pos: 0, n_positions, chunk_id: 0, activations: Some(t) },
        );
        finish(fbb, wf, proto::Body::BoundaryCopy, body.as_union_value())
    };
    let one = 4 * N_EMBD;
    assert!(wire::decode(&build(one, &[1, N_EMBD as u32], 1), &fence).is_ok(), "control");
    assert!(matches!(wire::decode(&build(2 * one, &[1, N_EMBD as u32], 1), &fence).unwrap_err(), WireError::Shape { .. }));
    assert!(matches!(wire::decode(&build(one, &[1, N_EMBD as u32], 2), &fence).unwrap_err(), WireError::Shape { .. }));
    assert!(matches!(wire::decode(&build(one, &[N_EMBD as u32], 1), &fence).unwrap_err(), WireError::Shape { .. }));
    assert!(matches!(
        wire::decode(&build(one, &[1, N_EMBD as u32], MAX_POSITIONS_PER_FRAME + 1), &fence).unwrap_err(),
        WireError::LimitExceeded { what: "n_positions", .. }
    ));
}

/// `MAX_SNAPSHOT_BYTES` (1 MiB). Sampler snapshots are small by design (Philox key + counter +
/// sampled position + the penalty window); a peer offering a megabytes-long "snapshot" is either
/// broken or hostile, and either way it must not be copied first and judged after.
#[test]
fn an_oversized_sampler_snapshot_is_refused() {
    let fence = SessionFence::dev(0x53);
    assert!(wire::decode(&sampled(&fence, 128), &fence).is_ok(), "control: a real-sized snapshot decodes");

    let over = MAX_SNAPSHOT_BYTES as usize + 1;
    let err = wire::decode(&sampled(&fence, over), &fence).unwrap_err();
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
    let fence = SessionFence::dev(0x54);
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let wf = build_fence(&mut fbb, &fence);
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
    let payload = finish(fbb, wf, proto::Body::Sampled, body.as_union_value());

    let err = wire::decode(&payload, &fence).unwrap_err();
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

/// **THE STANDING FIXTURE-BLINDNESS REGRESSION (design-authority directive, 2026-08-23).**
///
/// Every wire cap is exercised against dimensions **the dev fixture never reaches**, so no future
/// cap bug can hide under it.
///
/// # Why this test exists, in its own words
///
/// The M4·1 audit enforced `n_positions <= MAX_POSITIONS_PER_FRAME` — a documented, normative,
/// never-checked constraint. It also found that `encode_fwd` had been putting `activations.len()`,
/// the number of **f32s**, into that field (the line carried a `// placeholder` comment). Harmless
/// while nothing read the field; live the instant the cap became real.
///
/// And it would have shipped, because **the fixture cannot see it**: the dev model's `n_embd` is
/// **896**, which slips under the 1024 cap. Every test in the workspace would have stayed green
/// while every model above 0.5 B had its boundaries refused — a 7 B has `n_embd = 4096`, a 70 B has
/// `8192`. A correct-looking security fix would have broken the product on the first real model.
///
/// **The durable lesson (PROJECT_STATE §7.29): a limit must be tested against values the fixtures
/// do not reach.** A cap whose only test data sits comfortably below it is not tested — it is
/// merely not triggered, and the two are indistinguishable from a green suite.
///
/// So the widths below are chosen to *straddle and exceed* the cap on purpose: the fixture's own
/// 896, then the cap itself, then real model widths well beyond it. A cap defect that only bites
/// above 1024 fails here on the very first run.
#[test]
fn wire_caps_hold_for_boundary_widths_the_dev_fixture_never_reaches() {
    let fence = SessionFence::dev(0x55);

    // 896 = the dev model (Qwen2.5-0.5B) — the width every other engine test uses, and the one
    // that hid the defect. 1024 = exactly MAX_POSITIONS_PER_FRAME, the value an off-by-one lands
    // on. 1536/2048/4096/8192 = real n_embd for 1.5 B / 3 B / 7 B / 70 B-class models, i.e. every
    // width this project intends to serve and none of which any fixture here exercises.
    for n_embd in [896usize, 1024, 1536, 2048, 4096, 8192] {
        let boundary = vec![0.25f32; n_embd];

        let payload = wire::encode_fwd(&fence, 0, 0, true, &boundary);
        let (_, msg) = wire::decode(&payload, &fence)
            .unwrap_or_else(|e| panic!("a {n_embd}-wide FWD boundary must decode, got {e:?}"));
        match msg {
            hydra_worker::wire::Msg::Fwd { activations, n_positions, n_embd: declared, .. } => {
                assert_eq!(activations.len(), n_embd, "the boundary must survive the round trip intact");
                // C3: the declared shape rides along, already proven against the bytes.
                assert_eq!((n_positions, declared as usize), (1, n_embd));
            }
            other => panic!("expected Fwd for n_embd={n_embd}, got {other:?}"),
        }

        // The field really carries a POSITION count, not a float count — the defect itself.
        let frame = flatbuffers::root::<proto::Frame>(&payload).unwrap();
        assert_eq!(
            frame.body_as_fwd().unwrap().n_positions(),
            1,
            "n_embd={n_embd}: one FWD carries one position in v1; putting the float count here is \
             the §7.27 defect, and it is invisible at the dev model's 896"
        );

        // The same for the durability path, which carries the same boundary over a different body.
        let bc = wire::encode_boundary_copy(&fence, 0, 0, 0, 0, &boundary);
        assert!(
            wire::decode(&bc, &fence).is_ok(),
            "a {n_embd}-wide BOUNDARY_COPY must decode — D1 recovery replays exactly these"
        );
    }
}

/// The fixture-blindness the test above exists to defeat, asserted directly so the gap is a
/// **recorded fact** rather than a comment: the dev model's boundary width really is below the
/// position cap, so the cap is genuinely untested by every other engine test in the workspace.
///
/// If someone later raises `MAX_POSITIONS_PER_FRAME` or shrinks the fixture until this stops being
/// true, this test fails and tells them the sweep above has lost its reason to exist — rather than
/// letting it decay into a test that proves nothing.
#[test]
fn the_dev_fixture_is_below_the_position_cap_which_is_why_the_sweep_exists() {
    const DEV_N_EMBD: usize = 896; // Qwen2.5-0.5B
    assert!(
        DEV_N_EMBD < MAX_POSITIONS_PER_FRAME as usize,
        "the dev fixture ({DEV_N_EMBD}) is no longer below the position cap ({MAX_POSITIONS_PER_FRAME}); \
         re-read PROJECT_STATE §7.29 and re-pick the sweep widths above"
    );
}
