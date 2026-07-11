//! The §5 torn-write test contract (WAL-FORMAT.md), binding on `hydra-wal`:
//!  (a) truncate at EVERY byte offset of a multi-record log → last-complete-record recovery
//!      and a successful append afterward;
//!  (b) bit-flip fuzz in payload and checksum regions → detection;
//!  (c) crash-during-rotation (segment exists, dir not synced) → recoverable;
//!  (d) group-commit crash window → tokens sampled beyond the last durable GENERATION_COMMIT
//!      vanish on reopen, and every recovered GENERATION_COMMIT satisfies I19 on read.

use hydra_wal::file::{FileHeader, FILE_HEADER_LEN, FLAG_CONTAINS_CONTROL_WAL};
use hydra_wal::record::{rec_type, record_size, RECORD_HEADER_LEN};
use hydra_wal::reader::WalScan;
use hydra_wal::{WalError, WalWriter};

fn header() -> FileHeader {
    FileHeader { flags: FLAG_CONTAINS_CONTROL_WAL, cluster_id: [1u8; 16], session_scope: [0u8; 16] }
}

/// Build a WAL with the given payloads; return (file bytes, record-end boundaries incl. header).
fn build_log(dir: &std::path::Path, payloads: &[Vec<u8>]) -> (std::path::PathBuf, Vec<u8>, Vec<u64>) {
    let path = dir.join("wal.log");
    let mut boundaries = vec![FILE_HEADER_LEN as u64];
    {
        let mut w = WalWriter::create(&path, &header()).unwrap();
        for p in payloads {
            w.append(rec_type::BEGIN_RECOVERY, 0, p).unwrap();
            boundaries.push(w.len());
        }
    }
    let bytes = std::fs::read(&path).unwrap();
    (path, bytes, boundaries)
}

// ---- (a) truncate at every byte offset ----
#[test]
fn truncate_at_every_offset() {
    let dir = tempfile::tempdir().unwrap();
    let payloads: Vec<Vec<u8>> = vec![
        b"alpha".to_vec(),
        vec![0u8; 3],
        b"a-somewhat-longer-payload-1234".to_vec(),
        vec![9u8; 17],
        b"z".to_vec(),
    ];
    let (path, full, boundaries) = build_log(dir.path(), &payloads);
    let total = full.len();

    for t in 0..=total {
        let scan = WalScan::from_bytes(&full[..t]);
        if t < FILE_HEADER_LEN {
            assert!(scan.is_err(), "t={t}: header-incomplete prefix must not scan");
            continue;
        }
        let scan = scan.unwrap_or_else(|e| panic!("t={t}: scan failed: {e}"));
        let last_b = *boundaries.iter().filter(|&&b| b <= t as u64).last().unwrap();
        let complete = boundaries.iter().filter(|&&b| b <= t as u64).count() - 1;
        assert_eq!(scan.records.len(), complete, "t={t}: record count");
        assert_eq!(scan.durable_len, last_b, "t={t}: durable_len");
        assert_eq!(scan.truncated_tail, (t as u64) > last_b, "t={t}: truncated_tail");
    }

    // append-after-recovery from a mid-record truncation
    let mid = ((boundaries[2] + boundaries[3]) / 2) as usize; // inside record 3
    std::fs::write(&path, &full[..mid]).unwrap();
    let scan = WalScan::open(&path).unwrap();
    let (recovered, dl) = (scan.records.len(), scan.durable_len);
    {
        let mut w = WalWriter::open_append(&path, dl).unwrap();
        w.append(rec_type::SESSION_TERMINATE, 0, b"after-recovery").unwrap();
    }
    let scan2 = WalScan::open(&path).unwrap();
    assert_eq!(scan2.records.len(), recovered + 1);
    let last = scan2.records.last().unwrap();
    assert_eq!(last.record_type, rec_type::SESSION_TERMINATE);
    assert_eq!(last.payload, b"after-recovery");
    assert!(!scan2.truncated_tail);
}

// ---- (b) bit-flip fuzz ----
#[test]
fn bitflip_detection() {
    let dir = tempfile::tempdir().unwrap();
    let payloads: Vec<Vec<u8>> =
        vec![b"one".to_vec(), b"two-two".to_vec(), b"three".to_vec(), b"four".to_vec()];
    let (_path, full, boundaries) = build_log(dir.path(), &payloads);

    // flip a bit in a MIDDLE record's payload -> mid-stream corruption (valid records follow)
    let mut b = full.clone();
    b[boundaries[1] as usize + RECORD_HEADER_LEN] ^= 0x01;
    assert!(matches!(WalScan::from_bytes(&b), Err(WalError::CorruptMidStream { .. })));

    // flip a bit in a MIDDLE record's checksum tail -> also mid-stream corruption
    let mut b2 = full.clone();
    b2[boundaries[2] as usize - 1] ^= 0x01; // last byte of record 2 = its tag
    assert!(matches!(WalScan::from_bytes(&b2), Err(WalError::CorruptMidStream { .. })));

    // flip a bit in the LAST record's payload -> discarded as a torn tail (no valid record after)
    let mut b3 = full.clone();
    b3[boundaries[boundaries.len() - 2] as usize + RECORD_HEADER_LEN] ^= 0x01;
    let scan = WalScan::from_bytes(&b3).unwrap();
    assert_eq!(scan.records.len(), payloads.len() - 1);
    assert!(scan.truncated_tail);
}

// ---- (c) crash-during-rotation ----
#[test]
fn crash_during_rotation_recoverable() {
    let dir = tempfile::tempdir().unwrap();
    let seg1 = dir.path().join("wal-000001.log");
    let seg2 = dir.path().join("wal-000002.log");
    {
        let mut w = WalWriter::create(&seg1, &header()).unwrap();
        for i in 0..3 {
            w.append(rec_type::BEGIN_RECOVERY, 0, format!("r{i}").as_bytes()).unwrap();
        }
    }
    // Simulate a crash mid-rotation: the new segment's header made it to disk but no records did.
    {
        let _w = WalWriter::create(&seg2, &header()).unwrap();
    }
    let s1 = WalScan::open(&seg1).unwrap();
    assert_eq!(s1.records.len(), 3);
    assert!(!s1.truncated_tail);
    let s2 = WalScan::open(&seg2).unwrap();
    assert_eq!(s2.records.len(), 0, "half-rotated segment recovers to zero records");
    assert!(!s2.truncated_tail);

    // Alternatively the new segment never became durable at all: only seg1 exists -> still fine.
    std::fs::remove_file(&seg2).unwrap();
    assert!(WalScan::open(&seg2).is_err());
    assert_eq!(WalScan::open(&seg1).unwrap().records.len(), 3);
}

// ---- (d) group-commit crash window + I19 on read ----
fn build_gen_commit(first: i64, last: i64, generated_through: i64, sampled: i64) -> Vec<u8> {
    use hydra_proto::wal::*;
    let mut b = flatbuffers::FlatBufferBuilder::new();
    let cluster = b.create_vector(&[1u8; 16]);
    let sid = b.create_vector(&[2u8; 16]);
    let mid = b.create_vector(&[0u8; 16]);
    let mh = b.create_vector(&[3u8; 32]);
    let fence = WalFence::create(
        &mut b,
        &WalFenceArgs {
            cluster_id: Some(cluster),
            session_id: Some(sid),
            model_instance_id: Some(mid),
            manifest_hash: Some(mh),
            session_epoch: 0,
            recovery_id: 0,
            activation_attempt_id: 0,
        },
    );
    let rng_key = b.create_vector(&[4u8; 32]);
    let grammar = b.create_vector(&[] as &[u8]);
    let penalty = b.create_vector(&[] as &[u8]);
    let cfg_hash = b.create_vector(&[5u8; 32]);
    let state_ck = b.create_vector(&[6u8; 32]);
    let ckpt = SamplerCheckpointRec::create(
        &mut b,
        &SamplerCheckpointRecArgs {
            checkpoint_id: 1,
            rng_key: Some(rng_key),
            rng_counter: 0,
            generated_through_output_pos: generated_through,
            serialized_grammar_state: Some(grammar),
            serialized_penalty_state: Some(penalty),
            sampled_output_pos: sampled,
            sampling_config_hash: Some(cfg_hash),
            state_checksum: Some(state_ck),
        },
    );
    let tokens = b.create_vector::<flatbuffers::ForwardsUOffset<TokenEntry>>(&[]);
    let entries_ck = b.create_vector(&[7u8; 32]);
    let gc = GenerationCommit::create(
        &mut b,
        &GenerationCommitArgs {
            fence: Some(fence),
            commit_id: last as u64,
            previous_commit_id: 0,
            first_output_pos: first,
            last_output_pos: last,
            tokens: Some(tokens),
            checkpoint: Some(ckpt),
            entries_checksum: Some(entries_ck),
        },
    );
    b.finish(gc, None);
    b.finished_data().to_vec()
}

#[test]
fn group_commit_crash_window_i19() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commit.log");

    // Two durable, I19-valid generation commits (output pos ..1 and ..3).
    let gc1 = build_gen_commit(0, 1, 1, 1);
    let gc2 = build_gen_commit(2, 3, 3, 3);
    {
        let mut w = WalWriter::create(&path, &header()).unwrap();
        w.append(rec_type::GENERATION_COMMIT, 0, &gc1).unwrap();
        w.append(rec_type::GENERATION_COMMIT, 0, &gc2).unwrap();
    }
    let durable_before = std::fs::metadata(&path).unwrap().len();

    // "Sample ahead": begin a third commit, then a crash tears it mid-write.
    let gc3 = build_gen_commit(4, 5, 5, 5);
    {
        let mut w = WalWriter::open_append(&path, durable_before).unwrap();
        w.append(rec_type::GENERATION_COMMIT, 0, &gc3).unwrap();
    }
    let full = std::fs::read(&path).unwrap();
    let torn_len = durable_before as usize + record_size(gc3.len()) / 2; // half of gc3 hit disk
    std::fs::write(&path, &full[..torn_len]).unwrap();

    // On reopen the torn commit vanishes; the two durable commits remain and passed I19 on read.
    let scan = WalScan::open(&path).unwrap();
    assert_eq!(scan.records.len(), 2, "sampled-ahead torn commit must vanish");
    assert!(scan.truncated_tail);
    let last = scan.records.last().unwrap();
    let gc = flatbuffers::root::<hydra_proto::wal::GenerationCommit>(&last.payload).unwrap();
    assert_eq!(gc.last_output_pos(), 3, "durable prefix ends at the last complete commit");
    assert_eq!(scan.durable_len, durable_before, "file truncates back to the durable prefix");

    // A GENERATION_COMMIT that violates I19 is rejected on read.
    let bad_path = dir.path().join("bad.log");
    let bad = build_gen_commit(6, 7, 6, 7); // generated_through(6) != last(7)
    {
        let mut w = WalWriter::create(&bad_path, &header()).unwrap();
        w.append(rec_type::GENERATION_COMMIT, 0, &bad).unwrap();
    }
    assert!(matches!(WalScan::open(&bad_path), Err(WalError::I19Violation { .. })));
}
