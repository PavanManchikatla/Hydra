//! M2 FWD slice, seam 2 — **BOUNDARY_COPY durability + the R3′ release rule**, end to end.
//!
//! In D1 the upstream stage retains each forwarded boundary until BOTH the downstream `APPLIED_ACK`
//! and the durability target's `DURABILITY_ACK` clear (R3′). The durability target is the
//! coordinator's `BoundaryStore` (a real `hydra-wal` file). This proves the two halves compose: a
//! boundary needed for recovery is **never released before it is durable**, and the durable
//! boundaries read back for a replacement S_P's rebuild (seam 3).

use std::sync::atomic::{AtomicU32, Ordering};

use hydra_coordinator::BoundaryStore;
use hydra_worker::retain::R3Buffer;

static SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_path() -> std::path::PathBuf {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("hydra-bcopy-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join("boundaries.wal")
}

#[test]
fn a_boundary_needed_for_recovery_is_never_released_before_it_is_durable() {
    let path = temp_path();
    let mut store = BoundaryStore::create(&path, [1; 16], [2; 16]).unwrap();
    let mut retain = R3Buffer::new(true); // D1: durability required

    // S1 forwards + retains boundaries for input positions 0..3 (downstream applies them all).
    let boundaries: Vec<Vec<f32>> = (0..3).map(|p| vec![p as f32, p as f32 + 0.5, -1.0]).collect();
    for (p, b) in boundaries.iter().enumerate() {
        retain.retain(p as i64, b.clone());
    }
    retain.on_applied_ack(2); // downstream S2 has applied through position 2

    // But NOTHING is durable yet → the R3′ rule refuses to release any boundary.
    assert!(retain.release().is_empty(), "no release before any DURABILITY_ACK, even though downstream applied");
    assert!(retain.is_retained(0) && retain.is_retained(2), "all retained for a possible recovery");

    // The durability target persists positions 0 and 1 and acks them.
    let d0 = store.append_boundary(0, 0, 0, &boundaries[0]).unwrap();
    let d1 = store.append_boundary(0, 1, 0, &boundaries[1]).unwrap();
    assert_eq!((d0, d1), (0, 1), "durable frontier advances per fdatasync'd append");
    retain.on_durability_ack(store.durable_through_input_pos()); // DURABILITY_ACK = 1

    // Now 0 and 1 may release; position 2 is NOT durable → still retained (recovery-relevant).
    assert_eq!(retain.release(), vec![0, 1]);
    assert!(retain.is_retained(2), "position 2 not durable → must NOT be released");

    // Durably copy position 2; only now may it release.
    store.append_boundary(0, 2, 0, &boundaries[2]).unwrap();
    retain.on_durability_ack(store.durable_through_input_pos());
    assert_eq!(retain.release(), vec![2]);
    assert!(retain.retained().is_empty());

    // The durable boundaries read back (ascending) — what a replacement S_P rebuilds from (seam 3).
    drop(store);
    let durable = BoundaryStore::read(&path).unwrap();
    assert_eq!(durable.len(), 3);
    assert_eq!(durable.iter().map(|b| b.first_input_pos).collect::<Vec<_>>(), vec![0, 1, 2]);
    for (i, db) in durable.iter().enumerate() {
        assert_eq!(db.activations, boundaries[i], "durable boundary bytes round-trip exactly");
    }
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
