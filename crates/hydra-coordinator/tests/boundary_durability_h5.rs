//! **Audit Wave 2 — H5 (the durability plane) and M7 (contiguity).**
//!
//! # Standing rule 19: what the oracles could not see
//!
//! The durability tests in this project all drove the store the way the pipeline drives it: one
//! boundary per input position, in order, every write succeeding. Under that driver a `max()`
//! frontier and a contiguous frontier are **the same function** — they return the same number for
//! every input the harness could produce. So no test distinguished them, and the difference is the
//! whole finding: `max()` acks durability over a hole, and the ack is a licence for the upstream
//! stage to *free the boundary a recovery needs* (R3′).
//!
//! Every test below therefore misbehaves on purpose: out of order, twice, from the wrong session,
//! after a failure. That is the adversarial driver rule 19(b) asks for.

use hydra_coordinator::boundary_store::{BoundaryError, BoundaryFence, BoundaryStore};

fn fence() -> BoundaryFence {
    BoundaryFence { cluster_id: [7u8; 16], session_id: [1u8; 16], epoch: 3 }
}

fn store(dir: &std::path::Path, name: &str) -> BoundaryStore {
    BoundaryStore::create_fenced(dir.join(name), fence()).expect("create")
}

fn b(n: usize) -> Vec<f32> {
    vec![0.5f32; n]
}

/// **H5 — the frontier never runs ahead of a hole.**
///
/// The concrete harm: the returned frontier is the `DURABILITY_ACK`, and R3′ releases the upstream
/// retain buffer up to it. Acking 5 while 4 is missing frees boundary 4 — which is exactly the one
/// a D1 rebuild replays. The old `max()` did that silently.
#[test]
fn an_out_of_order_boundary_does_not_advance_the_frontier_over_the_hole() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store(dir.path(), "b.wal");

    assert_eq!(s.append_boundary(0, 0, 0, &b(4)).unwrap(), 0);
    assert_eq!(s.append_boundary(0, 1, 0, &b(4)).unwrap(), 1);

    // Position 2 never arrives; position 3 does. Under max() this returned 3 and R3′ freed 2.
    let jumped = s.append_boundary(0, 3, 0, &b(4));
    assert!(
        matches!(jumped, Err(BoundaryError::NotContiguous { got: 3, frontier: 1 })),
        "a boundary past the frontier must be refused, not acked over the gap: {jumped:?}"
    );
    assert_eq!(s.durable_through_input_pos(), 1, "and the frontier must not have moved");

    // The hole fills, and only then does the frontier advance — one position at a time.
    assert_eq!(s.append_boundary(0, 2, 0, &b(4)).unwrap(), 2);
    assert_eq!(s.append_boundary(0, 3, 0, &b(4)).unwrap(), 3);

    // The file agrees: four boundaries, contiguous from 0.
    let read = BoundaryStore::read(dir.path().join("b.wal")).expect("read");
    assert_eq!(read.len(), 4);
    assert_eq!(read.iter().map(|r| r.first_input_pos).collect::<Vec<_>>(), vec![0, 1, 2, 3]);
}

/// **H5 — a retransmitted boundary is idempotent, not a second record.**
#[test]
fn a_duplicate_boundary_is_acked_without_writing_a_second_copy() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store(dir.path(), "b.wal");
    for pos in 0..3i64 {
        s.append_boundary(0, pos, 0, &b(4)).unwrap();
    }

    // The R1 retransmit: same position, again. And again.
    assert_eq!(s.append_boundary(0, 1, 0, &b(4)).unwrap(), 2, "acked from the frontier");
    assert_eq!(s.append_boundary(0, 1, 0, &b(4)).unwrap(), 2);
    assert_eq!(s.append_boundary(0, 0, 0, &b(4)).unwrap(), 2);

    let read = BoundaryStore::read(dir.path().join("b.wal")).expect("read");
    assert_eq!(read.len(), 3, "three positions, three records — a duplicate wrote nothing");
}

/// **H5 — stored records are fenced to the session and epoch.**
///
/// A boundary from another session, or from a superseded epoch, is not durability: replayed into a
/// rebuild it is a wrong-context KV, and it would be replayed, because `read` hands back whatever
/// the file holds.
#[test]
fn a_boundary_from_another_session_or_epoch_is_refused_before_it_is_stored() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store(dir.path(), "b.wal");
    s.append_boundary(0, 0, 0, &b(4)).unwrap();

    let mut foreign_session = fence();
    foreign_session.session_id = [0xEE; 16];
    assert!(matches!(
        s.append_boundary_fenced(foreign_session, 0, 1, 0, &b(4)),
        Err(BoundaryError::FenceMismatch { what: "session_id" })
    ));

    let mut foreign_cluster = fence();
    foreign_cluster.cluster_id = [0xEE; 16];
    assert!(matches!(
        s.append_boundary_fenced(foreign_cluster, 0, 1, 0, &b(4)),
        Err(BoundaryError::FenceMismatch { what: "cluster_id" })
    ));

    // A superseded epoch: the in-flight boundary from before a recovery — the C4 accident, on the
    // durability plane rather than the data plane.
    let mut stale_epoch = fence();
    stale_epoch.epoch = fence().epoch - 1;
    assert!(matches!(
        s.append_boundary_fenced(stale_epoch, 0, 1, 0, &b(4)),
        Err(BoundaryError::FenceMismatch { what: "session_epoch" })
    ));

    assert_eq!(s.durable_through_input_pos(), 0, "no refusal moved the frontier");
    assert_eq!(BoundaryStore::read(dir.path().join("b.wal")).unwrap().len(), 1, "and none was stored");

    // The control: the store's own fence still works, so the refusals are caused by the mismatch.
    assert_eq!(s.append_boundary_fenced(fence(), 0, 1, 0, &b(4)).unwrap(), 1);
}

/// **M7 on the read side** — a file with a gap is refused rather than replayed. The write side
/// cannot produce one any more, but the file is what a *different process* recovers from, and a
/// rebuild that skips a position produces a KV attending over a history that never existed.
#[test]
fn reading_a_boundary_log_with_a_gap_is_refused() {
    use hydra_wal::file::FileHeader;
    use hydra_wal::record::rec_type;
    use hydra_wal::writer::WalWriter;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gappy.wal");
    {
        // Write positions 0, 1, 3 directly through the WAL writer, bypassing the store's rules —
        // i.e. exactly what an older build (or a different implementation) would have left behind.
        let mut w = WalWriter::create(&path, &FileHeader { flags: 0, cluster_id: [7u8; 16], session_scope: [1u8; 16] }).unwrap();
        for pos in [0i64, 1, 3] {
            let payload = hydra_coordinator::boundary_store::encode_boundary_record(0, pos, 0, &b(4));
            w.append(rec_type::BOUNDARY_COPY, 0, &payload).unwrap();
        }
    }
    let read = BoundaryStore::read(&path);
    assert!(
        matches!(read, Err(BoundaryError::NotContiguous { got: 3, frontier: 1 })),
        "a gapped boundary log must be refused on read, not replayed: {read:?}"
    );
}
