//! M2 FWD slice, seam 1 — round-trip codec for the data-plane **durability bodies** (spec §4):
//! `BOUNDARY_COPY` / `DURABILITY_ACK` / `COMMIT_ACK` / `COMMIT_SYNC`. The schema (`hydra-proto.fbs`)
//! already declared them; this proves the `hydra-worker` wire codec encodes + decodes them under the
//! F1 fence (no shadow structs — the generated flatbuffer is the source of truth).

use hydra_worker::wire::{self, Msg, SessionKeys};

#[test]
fn durability_bodies_round_trip_under_the_fence() {
    let keys = SessionKeys::dev(0xB0);

    // BOUNDARY_COPY (Si -> durability target): a boundary residual chunk.
    let acts = vec![0.5f32, -1.25, 3.0, 42.0];
    let bc = wire::encode_boundary_copy(&keys, 0, 2, 100, 7, &acts);
    match wire::decode(&bc, &keys).unwrap().1 {
        Msg::BoundaryCopy { boundary_id, first_input_pos, chunk_id, activations } => {
            assert_eq!((boundary_id, first_input_pos, chunk_id), (2, 100, 7));
            assert_eq!(activations, acts);
        }
        other => panic!("expected BoundaryCopy, got {other:?}"),
    }

    // DURABILITY_ACK (the R3′ release condition).
    let da = wire::encode_durability_ack(&keys, 0, 2, 100, 9);
    match wire::decode(&da, &keys).unwrap().1 {
        Msg::DurabilityAck { boundary_id, durable_through_input_pos, storage_generation } => {
            assert_eq!((boundary_id, durable_through_input_pos, storage_generation), (2, 100, 9));
        }
        other => panic!("expected DurabilityAck, got {other:?}"),
    }

    // COMMIT_ACK / COMMIT_SYNC (piggybacked commit watermarks).
    match wire::decode(&wire::encode_commit_ack(&keys, 0, 55), &keys).unwrap().1 {
        Msg::CommitAck { committed_through_output_pos } => assert_eq!(committed_through_output_pos, 55),
        other => panic!("expected CommitAck, got {other:?}"),
    }
    match wire::decode(&wire::encode_commit_sync(&keys, 0, 66), &keys).unwrap().1 {
        Msg::CommitSync { commit_up_to_output_pos } => assert_eq!(commit_up_to_output_pos, 66),
        other => panic!("expected CommitSync, got {other:?}"),
    }

    // A foreign fence is rejected before the body is acted on (F1).
    let other_keys = SessionKeys::dev(0xB1);
    assert!(wire::decode(&bc, &other_keys).is_err(), "F1 fence must reject a foreign BOUNDARY_COPY");

    // The FWD frame-type peek used by the forwarding serve loop.
    let fwd = wire::encode_fwd(&keys, 0, 0, true, &acts);
    assert!(wire::is_fwd_frame(&fwd));
    assert!(!wire::is_fwd_frame(&bc), "BOUNDARY_COPY is not a FWD");
}
