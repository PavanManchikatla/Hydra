//! **Audit H10(c), folded 2026-09-02 — `recovery::read` consumes `INPUT_CHUNK_COMMIT`.**
//!
//! Until this seam `read` ignored chunk commits entirely: `CommitStream::open` restored the prefill
//! watermark, `read` did not, and a caller reconstructing from `read` alone would re-apply every
//! prefill chunk after a crash mid-prefill (the auditor's H10 text, third clause). Now both readers
//! agree, and a chunk record that moves the watermark backwards is refused on read the way a
//! generation gap is — a damaged ledger is never quietly "repaired" by a sort or a max.

use flatbuffers::FlatBufferBuilder;
use hydra_coordinator::{recovery, CommitStream, WalFenceCtx};
use hydra_proto::wal;
use hydra_tokenizer::Admission;

fn temp_path(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("hydra-recovery-prefill-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join("commits.wal")
}

fn wal_fence() -> WalFenceCtx {
    WalFenceCtx { cluster_id: [1; 16], session_id: [2; 16], model_instance_id: [3; 16], manifest_hash: [4; 32], epoch: 0, recovery_id: 0, activation_attempt_id: 0 }
}

fn snapshot(checkpoint_id: u64, generated_through: i64, sampled: i64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let rng_key = Some(fbb.create_vector(&[0u8; 8]));
    let grammar = Some(fbb.create_vector::<u8>(&[]));
    let penalty = Some(fbb.create_vector::<u8>(&[]));
    let cfg = Some(fbb.create_vector(&[7u8; 32]));
    let sum = Some(fbb.create_vector(&[9u8; 32]));
    let rec = wal::SamplerCheckpointRec::create(
        &mut fbb,
        &wal::SamplerCheckpointRecArgs {
            checkpoint_id,
            rng_key,
            rng_counter: 42,
            generated_through_output_pos: generated_through,
            serialized_grammar_state: grammar,
            serialized_penalty_state: penalty,
            sampled_output_pos: sampled,
            sampling_config_hash: cfg,
            state_checksum: sum,
        },
    );
    fbb.finish(rec, None);
    fbb.finished_data().to_vec()
}

fn admission() -> Admission {
    Admission {
        tokenizer_hash: [0xA1; 32],
        chat_template_hash: [0xB2; 32],
        rendered_prompt_bytes_hash: [0xC3; 32],
        rendered_prompt: "<|im_start|>user\nhi<|im_end|>\n<|im_start|>assistant\n".to_string(),
        prompt_tokens: (0..64).collect(),
    }
}

#[test]
fn read_reports_the_prefill_watermark_and_the_position_a_restart_resumes_from() {
    let path = temp_path("watermark");
    let mut cs = CommitStream::create(&path, [1; 16], [2; 16]).expect("create");
    cs.append_initial_commit(&wal_fence(), &admission(), &snapshot(1, -1, -1), 1).expect("initial");

    // No chunk committed yet: the whole prompt is to be (re)applied.
    let st = recovery::read(&path).expect("read");
    assert_eq!(st.prefill_stable_pos, -1);
    assert_eq!(st.prefill_resume_pos(), 0, "nothing durable ⇒ prefill restarts at 0");

    // Two chunks land, then the process dies mid-prefill (chunk 3 never commits).
    cs.append_input_chunk_commit(&wal_fence(), 0, 0, 0, 31, &[31]).expect("chunk 0");
    cs.append_input_chunk_commit(&wal_fence(), 0, 1, 32, 47, &[47]).expect("chunk 1");
    drop(cs);

    let st = recovery::read(&path).expect("read after two chunks");
    assert_eq!(st.prefill_stable_pos, 47, "the last durable chunk's last_input_pos");
    assert_eq!(st.prefill_resume_pos(), 48, "a restart re-applies from 48, not from 0");
    // And `CommitStream::open` — the other reader — agrees, so there is one truth, not two.
    let reopened = CommitStream::open(&path, &[1; 16], &[2; 16]).expect("open");
    assert_eq!(reopened.prefill_stable_pos(), st.prefill_stable_pos, "read() and open() agree on the watermark");
}

#[test]
fn a_chunk_record_that_moves_the_watermark_backwards_is_refused_on_read() {
    // The writer refuses a backwards chunk (§2.4); a ledger containing one anyway is damaged, and
    // `read` must say so rather than take the max. Forge the record through the raw WAL writer.
    let path = temp_path("backwards");
    {
        let mut cs = CommitStream::create(&path, [1; 16], [2; 16]).expect("create");
        cs.append_initial_commit(&wal_fence(), &admission(), &snapshot(1, -1, -1), 1).expect("initial");
        cs.append_input_chunk_commit(&wal_fence(), 0, 0, 0, 31, &[31]).expect("chunk 0");
    }
    // Append a forged INPUT_CHUNK_COMMIT with last_input_pos = 15 via hydra-wal directly.
    let scan = hydra_wal::reader::WalScan::open(&path).expect("scan");
    let mut w = hydra_wal::writer::WalWriter::open_append(&path, scan.durable_len).expect("append");
    let mut fbb = FlatBufferBuilder::new();
    let f = wal_fence();
    let cluster_id = Some(fbb.create_vector(&f.cluster_id));
    let session_id = Some(fbb.create_vector(&f.session_id));
    let model_instance_id = Some(fbb.create_vector(&f.model_instance_id));
    let manifest_hash = Some(fbb.create_vector(&f.manifest_hash));
    let fence = Some(wal::WalFence::create(
        &mut fbb,
        &wal::WalFenceArgs { cluster_id, session_id, model_instance_id, manifest_hash, session_epoch: f.epoch, recovery_id: f.recovery_id, activation_attempt_id: f.activation_attempt_id },
    ));
    let bdt = Some(fbb.create_vector(&[15i64]));
    let rec = wal::InputChunkCommit::create(
        &mut fbb,
        &wal::InputChunkCommitArgs { fence, segment_id: 0, chunk_id: 1, first_input_pos: 0, last_input_pos: 15, boundary_durable_through: bdt },
    );
    fbb.finish(rec, None);
    // `append` is the durable append (it returns only after the record is on disk).
    w.append(hydra_wal::record::rec_type::INPUT_CHUNK_COMMIT, 0, fbb.finished_data()).expect("forged append");

    let err = recovery::read(&path).expect_err("a backwards chunk must be refused on read");
    assert!(matches!(err, recovery::RecoveryError::OutOfOrder { previous: 31, found: 15 }), "got {err:?}");
}
