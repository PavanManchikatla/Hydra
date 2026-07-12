//! DST marathon runner — the CI-runnable long DST (M1 DoD: 10M+ steps across ≥1,000 seeds).
//! Local use is smoke-only (small counts); the marathon runs in GitHub Actions (thermal rule).
//!
//! Usage: marathon [--seeds N] [--steps M] [--stages S] [--base-seed B]

use std::process::ExitCode;

fn arg(args: &[String], key: &str, default: u64) -> u64 {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let seeds = arg(&args, "--seeds", 1_000);
    let steps = arg(&args, "--steps", 10_000);
    let stages = arg(&args, "--stages", 2) as u16;
    let base = arg(&args, "--base-seed", 1);

    println!("hydra-sim marathon [{}]", hydra_sim::SCHED_VERSION);
    println!("hydra-sim marathon: {seeds} seeds × {steps} steps × {stages} stages (base seed {base})");
    let total_target = seeds.saturating_mul(steps);
    match hydra_sim::run_many(base, seeds, stages, steps) {
        Ok(total) => {
            println!("OK: {total} steps, 0 invariant violations (target {total_target}).");
            ExitCode::SUCCESS
        }
        Err(f) => {
            eprintln!("{f}");
            ExitCode::FAILURE
        }
    }
}
