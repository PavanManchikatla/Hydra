//! M2 slice 5 sub-slice A — the real commit stream on a coordinator disk file.
//!
//! Proves the *wiring* (the codec's torn-write contract is already proven in `hydra-wal`): an
//! `INITIAL_COMMIT` + a run of `GENERATION_COMMIT`s land on a real file, `generation_durable_pos`
//! advances only after the durable append, I19's equalities are validated on write (a violating
//! snapshot is refused, nothing written), and everything reads back via the real `WalScan`.

use std::sync::atomic::{AtomicU32, Ordering};

use flatbuffers::FlatBufferBuilder;
use hydra_coordinator::{CommitStream, GroupCommitter, WalFenceCtx};
use hydra_proto::validate_generation_commit_i19;
use hydra_proto::wal;
use hydra_tokenizer::Admission;

static SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_dir() -> std::path::PathBuf {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("hydra-commit-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn fence() -> WalFenceCtx {
    WalFenceCtx {
        cluster_id: [1; 16],
        session_id: [2; 16],
        model_instance_id: [3; 16],
        manifest_hash: [4; 32],
        epoch: 0,
        recovery_id: 0,
        activation_attempt_id: 0,
    }
}

/// A minimal valid `SamplerCheckpointRec` — as S_P would produce for output position `pos`
/// (`generated_through == sampled == pos`, so an embedding GENERATION_COMMIT at `last=pos` meets I19).
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
        prompt_tokens: vec![10, 20, 30, 40],
    }
}

#[test]
fn initial_and_generation_commits_are_durable_and_i19_valid() {
    let dir = temp_dir();
    let path = dir.join("commit-stream.wal");
    let mut cs = CommitStream::create(&path, [1; 16], [2; 16]).expect("create");

    cs.append_initial_commit(&fence(), &admission(), &snapshot(1, -1, -1), 1).expect("initial");
    assert_eq!(cs.generation_durable_pos(), -1, "no generation durable yet");
    assert_eq!(cs.committed_sampler_checkpoint_id(), 1);

    // Two group commits: positions [0..=2] then [3..=5]. Each embeds snapshot(last).
    let c1 = cs.append_generation_commit(&fence(), 0, 2, &[(0, 100), (1, 101), (2, 102)], &snapshot(1, 2, 2)).expect("gen1");
    assert_eq!(cs.generation_durable_pos(), 2, "durable pos advances after fdatasync");
    let c2 = cs.append_generation_commit(&fence(), 3, 5, &[(3, 103), (4, 104), (5, 105)], &snapshot(1, 5, 5)).expect("gen2");
    assert_eq!(cs.generation_durable_pos(), 5);
    assert_eq!((c1, c2), (1, 2), "commit ids are dense");
    let durable_len = cs.durable_len();
    drop(cs);

    // Read back through the REAL scanner.
    let scan = hydra_wal::reader::WalScan::open(&path).expect("scan");
    assert_eq!(scan.durable_len, durable_len, "scanner sees the full durable file");
    let types: Vec<u16> = scan.records.iter().map(|r| r.record_type).collect();
    assert_eq!(types, vec![1, 3, 3], "INITIAL_COMMIT then two GENERATION_COMMITs");
    // Every generation record satisfies I19 on read too.
    for r in scan.records.iter().filter(|r| r.record_type == 3) {
        validate_generation_commit_i19(&r.payload).expect("I19 holds on read");
    }
    // The last generation record's last_output_pos is 5.
    let last_gen = scan.records.iter().rfind(|r| r.record_type == 3).unwrap();
    let gc = flatbuffers::root::<wal::GenerationCommit>(&last_gen.payload).unwrap();
    assert_eq!(gc.last_output_pos(), 5);
    assert_eq!(gc.tokens().len(), 3);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn i19_violating_snapshot_is_refused_before_the_disk() {
    let dir = temp_dir();
    let path = dir.join("commit-stream.wal");
    let mut cs = CommitStream::create(&path, [1; 16], [2; 16]).expect("create");
    cs.append_initial_commit(&fence(), &admission(), &snapshot(1, -1, -1), 1).expect("initial");
    let len_before = cs.durable_len();

    // snapshot(generated_through=1) but last_output_pos=2 → I19 (generated_through != last).
    let err = cs.append_generation_commit(&fence(), 0, 2, &[(0, 1), (1, 2), (2, 3)], &snapshot(1, 1, 1)).unwrap_err();
    assert!(matches!(err, hydra_coordinator::CommitError::I19(_)), "got {err:?}");
    assert_eq!(cs.durable_len(), len_before, "nothing was written on an I19 violation");
    assert_eq!(cs.generation_durable_pos(), -1, "durable pos did not advance");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn group_committer_thresholds_at_k() {
    let mut g = GroupCommitter::new(8);
    assert!(g.is_empty());
    for i in 0..7 {
        g.push(i, 200 + i as u32, snapshot(1, i, i));
        assert!(!g.count_ready(), "not ready before k");
    }
    g.push(7, 207, snapshot(1, 7, 7));
    assert!(g.count_ready(), "ready at k=8");
    let batch = g.take().unwrap();
    assert_eq!((batch.first_pos, batch.last_pos), (0, 7));
    assert_eq!(batch.tokens.len(), 8);
    assert!(!batch.snapshot.is_empty(), "the last position's snapshot is carried");
    assert!(g.is_empty() && g.take().is_none());
}
