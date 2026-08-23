//! **Audit Wave 1d — M9 (I1/R2 per-position idempotency) and C4 (the data-plane fence).**
//!
//! # The blind oracle, named (standing instruction)
//!
//! Nothing in this workspace could express either failure before Wave 1d, and the reason is the
//! same for both: **every driver in the harness was a perfect coordinator.** It sent each position
//! exactly once, in order, to a stage it never bothered to activate, always in the epoch it started
//! in. So the harness could not produce a retransmit, a gap, a stale epoch, or an unactivated
//! serve — and a defect you cannot produce is a defect you cannot guard. The rule-14 bit-exact
//! anchors are the sharpest case: they are the project's standing regression gate, they were green
//! throughout, and they were **structurally incapable** of noticing that `APPLY_TOKEN` was applied
//! twice on a retry, because they never retry.
//!
//! The tests below deliberately misbehave — resend, skip, speak the wrong epoch, serve before
//! activation — and then assert that the *good* path is still byte-identical afterwards, so the
//! refusals are shown to be refusals rather than corruption.

use hydra_worker::pair::dev_model_path;
use hydra_worker::wire::{self, Msg, SessionFence};
use hydra_worker::worker::{Worker, WorkerConfig};

const N_CTX: i32 = 64;

fn worker(path: &str, epoch: u32) -> Worker {
    Worker::new(WorkerConfig {
        fence: SessionFence::dev(0xB1),
        rank: 0,
        layer_first: 0,
        layer_last: -1,
        is_final: true,
        receives_tokens: true,
        epoch,
        recovery_id: 0,
        model_path: Some(path.to_string()),
        n_gpu_layers: 0,
        n_ctx: N_CTX,
        sampler_config: None,
        recovery_start: false,
        shard_manifest: None,
    })
    .expect("worker")
}

/// Bring a worker to `ACTIVE_FINAL` through the real stage SM (spec §6.6 steps 2 and 4).
fn activate(w: &mut Worker, fence: &SessionFence, epoch: u32) {
    let t = hydra_state::ActivationTuple {
        kind: hydra_state::ActivationKind::Initial,
        epoch,
        recovery_id: 0,
        attempt: 1,
        sampler_checkpoint_id: 0,
    };
    let r = w.on_frame(&wire::encode_commit_activation(fence, &t, 1)).expect("commit");
    assert!(matches!(wire::decode(&r[0], fence).unwrap().1, Msg::ActivationCommitted(_)));
    let r = w.on_frame(&wire::encode_finalize_activation(fence, &t, 1)).expect("finalize");
    assert!(matches!(wire::decode(&r[0], fence).unwrap().1, Msg::ActivationFinalized));
}

fn err_code(reply: &[Vec<u8>], fence: &SessionFence) -> u16 {
    match wire::decode(&reply[0], fence).expect("decode").1 {
        Msg::Err { code } => code,
        other => panic!("expected an error frame, got {other:?}"),
    }
}

/// **M9 — a retransmitted position is answered from the cache, never re-applied.**
///
/// The observable that matters is not the ack: it is the **KV**. If the duplicate had reached
/// `hydra_apply`, position p would occupy two KV slots and every later position would attend over
/// a history that never existed — so the run's final logits would differ from a run with no
/// duplicate. The test therefore compares digests against a clean run, which is the only way to
/// distinguish "the duplicate was refused" from "the duplicate was applied and the ack looked
/// fine".
#[test]
fn a_retransmitted_position_is_re_acked_without_being_applied_again() {
    let Some(path) = dev_model_path() else {
        eprintln!("skip: engine/model unavailable");
        return;
    };
    let fence = SessionFence::dev(0xB1);
    let tokens: [u32; 6] = [9707, 3837, 1879, 264, 4013, 220];

    // (a) the clean run: each position once, in order.
    let mut clean = worker(&path, 0);
    activate(&mut clean, &fence, 0);
    let mut clean_digest = Vec::new();
    for (pos, &tok) in tokens.iter().enumerate() {
        let r = clean.on_frame(&wire::encode_apply_token(&fence, 0, pos as i64, tok, true)).expect("apply");
        clean_digest = match wire::decode(&r[0], &fence).unwrap().1 {
            Msg::AppliedAck { output_checksum, .. } => output_checksum,
            other => panic!("expected APPLIED_ACK, got {other:?}"),
        };
    }

    // (b) the same run with every position sent TWICE — the R1 retransmit the protocol prescribes.
    let mut dup = worker(&path, 0);
    activate(&mut dup, &fence, 0);
    let mut dup_digest = Vec::new();
    for (pos, &tok) in tokens.iter().enumerate() {
        let frame = wire::encode_apply_token(&fence, 0, pos as i64, tok, true);
        let first = dup.on_frame(&frame).expect("apply");
        let again = dup.on_frame(&frame).expect("retransmit");
        assert_eq!(first, again, "pos {pos}: the retransmit must be answered byte-for-byte from the cache");
        dup_digest = match wire::decode(&again[0], &fence).unwrap().1 {
            Msg::AppliedAck { output_checksum, .. } => output_checksum,
            other => panic!("expected APPLIED_ACK, got {other:?}"),
        };
    }

    assert_eq!(
        clean_digest, dup_digest,
        "the doubly-sent run must be BIT-IDENTICAL to the clean one — if the duplicates had reached \
         the engine, the KV would hold each position twice and these digests would differ"
    );
    assert!(!clean_digest.is_empty(), "the teacher-forced witness digest is what makes this comparison real");
}

/// **M9 — a gap is refused with `ERR_GAP` (4), and the stage stays usable.**
#[test]
fn a_skipped_position_is_refused_with_err_gap_and_the_stage_survives_it() {
    let Some(path) = dev_model_path() else {
        eprintln!("skip: engine/model unavailable");
        return;
    };
    let fence = SessionFence::dev(0xB1);
    let mut w = worker(&path, 0);
    activate(&mut w, &fence, 0);

    // Position 1 before position 0.
    assert_eq!(err_code(&w.on_frame(&wire::encode_apply_token(&fence, 0, 1, 9707, true)).unwrap(), &fence), 4);
    // Position 0 is fine…
    w.on_frame(&wire::encode_apply_token(&fence, 0, 0, 9707, true)).expect("pos 0");
    // …then a jump over position 1.
    assert_eq!(err_code(&w.on_frame(&wire::encode_apply_token(&fence, 0, 2, 3837, true)).unwrap(), &fence), 4);
    // …and a position already behind the retransmittable one.
    w.on_frame(&wire::encode_apply_token(&fence, 0, 1, 3837, true)).expect("pos 1");
    w.on_frame(&wire::encode_apply_token(&fence, 0, 2, 1879, true)).expect("pos 2");
    assert_eq!(err_code(&w.on_frame(&wire::encode_apply_token(&fence, 0, 0, 9707, true)).unwrap(), &fence), 4);
    // The refusals left the frontier where it was: the next position is still accepted.
    w.on_frame(&wire::encode_apply_token(&fence, 0, 3, 264, true)).expect("the stage is still usable");
}

/// **C4 — a stale-epoch data-plane frame is refused with `ERR_FENCED` (1) rather than applied.**
///
/// This is the accident the honest-worker assumption does not excuse: an in-flight `APPLY_TOKEN`
/// from before a recovery, delivered afterwards. Before C4 it was applied to the KV the recovery
/// had just rebuilt.
#[test]
fn a_stale_epoch_data_plane_frame_is_fenced_not_applied() {
    let Some(path) = dev_model_path() else {
        eprintln!("skip: engine/model unavailable");
        return;
    };
    let fence = SessionFence::dev(0xB1);
    let mut w = worker(&path, 1); // a stage that has already moved to epoch 1
    activate(&mut w, &fence, 1);

    assert_eq!(err_code(&w.on_frame(&wire::encode_apply_token(&fence, 0, 0, 9707, true)).unwrap(), &fence), 1, "epoch 0 is stale");
    assert_eq!(err_code(&w.on_frame(&wire::encode_apply_token(&fence, 2, 0, 9707, true)).unwrap(), &fence), 1, "epoch 2 is not this stage's either");
    w.on_frame(&wire::encode_apply_token(&fence, 1, 0, 9707, true)).expect("the current epoch is served");
}

/// **C4 — an unactivated stage does not serve a sampled decode, and does serve the rebuild class.**
///
/// The split is spec §1.1 F1: `ACTIVE_FINAL` for normal decode (I20 forbids serving from
/// `PREACTIVE`), the rebuild/catch-up classes while rebuilding. A stage that has committed but not
/// finalized its activation — `PREACTIVE` — is refused for **both**, which is the case I20 names.
#[test]
fn serving_eligibility_follows_the_frame_class() {
    let Some(path) = dev_model_path() else {
        eprintln!("skip: engine/model unavailable");
        return;
    };
    let fence = SessionFence::dev(0xB1);

    // FROZEN_READY (no activation yet): teacher-forced prefill is served, a sampled decode is not.
    let mut w = worker(&path, 0);
    w.on_frame(&wire::encode_apply_token(&fence, 0, 0, 9707, true)).expect("NO_SAMPLE is the rebuild class");
    assert_eq!(
        err_code(&w.on_frame(&wire::encode_apply_token(&fence, 0, 1, 3837, false)).unwrap(), &fence),
        1,
        "a decode from an unactivated stage is ERR_FENCED — I20: serving happens only from a finalized activation"
    );

    // PREACTIVE (committed, not finalized): refused for BOTH classes.
    let mut p = worker(&path, 0);
    let t = hydra_state::ActivationTuple {
        kind: hydra_state::ActivationKind::Initial,
        epoch: 0,
        recovery_id: 0,
        attempt: 1,
        sampler_checkpoint_id: 0,
    };
    p.on_frame(&wire::encode_commit_activation(&fence, &t, 1)).expect("commit");
    assert_eq!(err_code(&p.on_frame(&wire::encode_apply_token(&fence, 0, 0, 9707, true)).unwrap(), &fence), 1, "PREACTIVE serves nothing (I20)");
    assert_eq!(err_code(&p.on_frame(&wire::encode_apply_token(&fence, 0, 0, 9707, false)).unwrap(), &fence), 1, "PREACTIVE serves nothing (I20)");

    // …and once finalized, the decode class is served.
    let r = p.on_frame(&wire::encode_finalize_activation(&fence, &t, 1)).expect("finalize");
    assert!(matches!(wire::decode(&r[0], &fence).unwrap().1, Msg::ActivationFinalized));
    p.on_frame(&wire::encode_apply_token(&fence, 0, 0, 9707, false)).expect("ACTIVE_FINAL serves a decode");
}

/// **H19 — `CATCH_UP_CONTEXT{goal}` is bounded by `n_ctx` before the loop runs.**
///
/// `goal = i64::MAX − 2` used to make the single-threaded worker spin ~2⁶³ times. The refusal must
/// be immediate: the test's own wall-clock is the oracle, since a regression here does not fail,
/// it hangs.
#[test]
fn an_absurd_catch_up_goal_is_refused_immediately_rather_than_looped() {
    let Some(path) = dev_model_path() else {
        eprintln!("skip: engine/model unavailable");
        return;
    };
    let fence = SessionFence::dev(0xB1);
    let mut w = worker(&path, 0);

    let t0 = std::time::Instant::now();
    for goal in [i64::MAX - 2, i64::MAX, N_CTX as i64 + 1, -1, i64::MIN] {
        let err = w.on_frame(&wire::encode_catch_up_context(&fence, 0, 0, goal)).unwrap_err();
        assert!(
            format!("{err}").contains("CATCH_UP_CONTEXT goal"),
            "goal {goal} must be refused by the bound, got {err}"
        );
    }
    assert!(t0.elapsed().as_secs() < 5, "the refusals must be immediate — a loop here is the H19 self-DoS");

    // A legal goal still works (the bound refuses absurdity, not catch-up).
    let mut r = worker(&path, 0);
    let replies = r.on_frame(&wire::encode_catch_up_context(&fence, 0, 0, 0)).expect("a goal inside n_ctx is legal");
    assert!(replies.is_empty() || matches!(wire::decode(&replies[0], &fence).unwrap().1, Msg::CatchUpReady { .. }));
}
