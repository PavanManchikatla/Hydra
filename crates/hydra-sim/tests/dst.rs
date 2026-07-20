//! DST harness tests: default runs are violation-free; each of the four mutations (two
//! coordinator-side, two stage-side) is caught by *randomized* runs within a bounded step budget
//! (median steps-to-detection recorded).

// ---- default: randomized runs find no invariant violation ----
// Gated out under EVERY mutation: with the stage track now driven, Mut2/Mut3 fire in the sim too,
// so the violation-free run is only meaningful on the faithful build.
#[cfg(not(any(
    feature = "mutation_no_abort_finality",
    feature = "mutation_no_unservable",
    feature = "mutation_label_reset",
    feature = "mutation_no_attempt_fence",
    feature = "mutation_unservable_restart"
)))]
#[test]
fn randomized_runs_are_violation_free() {
    // 300 seeds × 3000 steps = 900k steps; the marathon binary scales this to the 10M DoD in CI.
    match hydra_sim::run_many(1, 300, 2, 3000) {
        Ok(total) => assert_eq!(total, 900_000),
        Err(f) => panic!("unexpected violation:\n{f}"),
    }
}

/// Detection statistics across `k` randomized seeds under the active mutation: how many caught
/// the sabotage within `budget`, and the median steps-to-detection over the caught runs. The
/// **ensemble** must catch the sabotage reliably (a low catch-rate means the schedule is too
/// gentle — a sim bug to fix, per the DoD).
#[cfg(any(
    feature = "mutation_no_abort_finality",
    feature = "mutation_no_unservable",
    feature = "mutation_label_reset",
    feature = "mutation_no_attempt_fence",
    feature = "mutation_unservable_restart",
    feature = "mutation_candidate_leak"
))]
fn detection_stats(k: u64, budget: u64) -> (u64, u64, u64) {
    let mut steps: Vec<u64> = Vec::new();
    for i in 0..k {
        let seed = (i + 1).wrapping_mul(0x100_0000_01B3);
        if let Some(f) = hydra_sim::run(seed, 2, budget) {
            steps.push(f.step);
        }
    }
    steps.sort_unstable();
    let median = if steps.is_empty() { 0 } else { steps[steps.len() / 2] };
    (steps.len() as u64, k, median)
}

#[cfg(feature = "mutation_no_abort_finality")]
#[test]
fn mut4_i25_caught_by_randomized_runs() {
    let (caught, total, median) = detection_stats(200, 20_000);
    println!("Mut4 (I25 AbortFinality): {caught}/{total} seeds caught; median steps-to-detection = {median}");
    assert!(caught * 100 >= total * 95, "catch-rate {caught}/{total} too low — schedule too gentle");
}

#[cfg(feature = "mutation_no_unservable")]
#[test]
fn mut1_post_decision_loss_caught_by_randomized_runs() {
    let (caught, total, median) = detection_stats(200, 20_000);
    println!("Mut1 (I22 PostDecisionLoss): {caught}/{total} seeds caught; median steps-to-detection = {median}");
    assert!(caught * 100 >= total * 95, "catch-rate {caught}/{total} too low — schedule too gentle");
}

#[cfg(feature = "mutation_label_reset")]
#[test]
fn mut2_caseb_caught_by_randomized_runs() {
    let (caught, total, median) = detection_stats(200, 20_000);
    println!("Mut2 (CaseBPure): {caught}/{total} seeds caught; median steps-to-detection = {median}");
    assert!(caught * 100 >= total * 95, "catch-rate {caught}/{total} too low — schedule too gentle");
}

#[cfg(feature = "mutation_candidate_leak")]
#[test]
fn mut6_candidate_isolation_caught_by_randomized_runs() {
    let (caught, total, median) = detection_stats(200, 20_000);
    println!("Mut6 (I24 CandidateIsolation): {caught}/{total} seeds caught; median steps-to-detection = {median}");
    assert!(caught * 100 >= total * 95, "catch-rate {caught}/{total} too low — schedule too gentle");
}

#[cfg(feature = "mutation_no_attempt_fence")]
#[test]
fn mut3_attempt_fence_caught_by_randomized_runs() {
    let (caught, total, median) = detection_stats(200, 20_000);
    println!("Mut3 (F2 AttemptFence): {caught}/{total} seeds caught; median steps-to-detection = {median}");
    assert!(caught * 100 >= total * 95, "catch-rate {caught}/{total} too low — schedule too gentle");
}

// Mut5 (F-UNSERVABLE monotone-mutation): omit the durable ACTIVATION_UNSERVABLE record — the WAL
// effect is still emitted (so the virtual disk records it) but self.wal is not, so restart
// misclassifies. The WAL-codec cross-check (the very check that found F-UNSERVABLE) must re-find it.
#[cfg(feature = "mutation_unservable_restart")]
#[test]
fn mut5_unservable_restart_caught_by_randomized_runs() {
    println!("scheduler: {}", hydra_sim::SCHED_VERSION);
    let (caught, total, median) = detection_stats(200, 20_000);
    println!("Mut5 (WalCodecDivergence / omitted durable UNSERVABLE): {caught}/{total} seeds caught; median steps-to-detection = {median}");
    assert!(caught * 100 >= total * 95, "catch-rate {caught}/{total} too low — schedule too gentle");
}
