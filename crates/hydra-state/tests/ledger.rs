//! Ledger + watermark tests (M1 slice 5): the four watermarks advance independently; the
//! provisional window rolls back; emit-after-commit; I19 equalities; cancellation cutoff;
//! teacher-forcing. Invariants asserted after each mutation.

use hydra_state::invariants::check_ledger;
use hydra_state::ledger::LedgerError;
use hydra_state::{InputPos, Ledger, OutputPos};

fn ck(l: &Ledger) {
    let v = check_ledger(l);
    assert!(v.is_empty(), "ledger invariant violated: {v:?}");
}

#[test]
fn four_watermarks_advance_independently() {
    let mut l = Ledger::new();
    // input-side commit advances ONLY prefill_stable_pos
    l.commit_input_chunk(InputPos(5));
    assert_eq!(l.prefill_stable_pos(), InputPos(5));
    assert_eq!(l.generation_durable_pos(), OutputPos(-1));
    assert_eq!(l.sampled_pos(), OutputPos(-1));
    ck(&l);
    // sampling advances ONLY sampled_pos (not generation_durable_pos)
    for i in 0..3 {
        l.sample_next(100 + i).unwrap();
    }
    assert_eq!(l.sampled_pos(), OutputPos(2));
    assert_eq!(l.generation_durable_pos(), OutputPos(-1), "sampling must not advance the durable frontier");
    assert_eq!(l.prefill_stable_pos(), InputPos(5), "output-side work must not touch the input watermark");
    ck(&l);
    // GENERATION_COMMIT advances ONLY generation_durable_pos
    l.commit_generation(OutputPos(2)).unwrap();
    assert_eq!(l.generation_durable_pos(), OutputPos(2));
    assert_eq!(l.prefill_stable_pos(), InputPos(5), "still independent");
    ck(&l);
}

#[test]
fn provisional_window_rolls_back_on_recovery() {
    let mut l = Ledger::new();
    for i in 0..3 {
        l.sample_next(i).unwrap();
    }
    assert_eq!(l.sampled_pos(), OutputPos(2));
    assert_eq!(l.provisional_len(), 3);
    // recovery: erase all provisional including luck (I7b/I15)
    l.rollback_provisional();
    assert_eq!(l.sampled_pos(), OutputPos(-1));
    assert_eq!(l.provisional_len(), 0);
    ck(&l);
}

#[test]
fn emit_gated_on_generation_durable_pos() {
    let mut l = Ledger::new();
    l.sample_next(10).unwrap();
    l.sample_next(11).unwrap();
    assert!(l.emittable().is_empty(), "nothing emittable before commit (I6)");
    l.commit_generation(OutputPos(1)).unwrap();
    let e = l.emittable();
    assert_eq!(e.len(), 2, "committed tokens become emittable");
    assert_eq!(l.emitted_pos(), OutputPos(1));
    assert!(l.emittable().is_empty(), "already emitted");
    ck(&l);
}

#[test]
fn i19_equalities_enforced_on_commit() {
    let mut l = Ledger::new();
    l.sample_next(1).unwrap();
    l.sample_next(2).unwrap(); // sampled_pos = 1
    assert_eq!(
        l.commit_generation(OutputPos(0)),
        Err(LedgerError::I19Mismatch { generated_through: OutputPos(0), sampled: OutputPos(1) }),
        "generated_through must equal sampled_pos == last_output_pos"
    );
    l.commit_generation(OutputPos(1)).unwrap();
    ck(&l);
}

#[test]
fn cancellation_suppresses_provisional_and_flushes_committed() {
    let mut l = Ledger::new();
    l.sample_next(1).unwrap();
    l.sample_next(2).unwrap();
    l.commit_generation(OutputPos(1)).unwrap(); // durable through pos 1
    l.sample_next(3).unwrap(); // provisional pos 2
    assert_eq!(l.provisional_len(), 1);
    let flushed = l.cancel(); // I9
    assert!(l.is_cancelled());
    assert_eq!(l.cancel_cutoff_pos(), Some(OutputPos(1)), "cutoff == generation_durable_pos");
    assert_eq!(l.provisional_len(), 0, "provisional suppressed");
    assert_eq!(flushed.len(), 2, "committed-but-unemitted flushed through the cutoff");
    ck(&l);
    // teacher-forcing / cancellation: no further sampling
    assert_eq!(l.sample_next(9), Err(LedgerError::Cancelled));
}

#[test]
fn teacher_forcing_never_resamples_a_committed_position() {
    let mut l = Ledger::new();
    l.sample_next(1).unwrap();
    l.commit_generation(OutputPos(0)).unwrap(); // pos 0 durable
    // any subsequent sample is strictly beyond the durable frontier — a committed pos is never revisited
    let p = l.sample_next(2).unwrap();
    assert!(p.get() > l.generation_durable_pos().get());
    assert_eq!(p, OutputPos(1));
    ck(&l);
}
