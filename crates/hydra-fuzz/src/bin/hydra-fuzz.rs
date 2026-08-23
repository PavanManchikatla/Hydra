//! The fuzzing driver (M4·1 c).
//!
//! ```text
//! hydra-fuzz --target <gguf|frame-header|wire-body|all> --seconds N [--seed S] [--iterations N]
//! ```
//!
//! Prints exactly one machine-readable line per target and one summary line, in the project's
//! standing receipt format (rule 16 — a CI result enters the record only as a quoted `verdict=`
//! line or a receipt file):
//!
//! ```text
//! target=gguf seed=1 iterations=8123456 cpu_seconds=600.0 crashes=0 verdict=GREEN
//! FUZZ SUMMARY targets=3 cpu_seconds=1800.0 crashes=0 verdict=GREEN
//! ```
//!
//! A crash prints its full replay coordinates **before** the verdict line, because a `verdict=RED`
//! that does not say how to reproduce the case is not actionable:
//!
//! ```text
//! CRASH target=gguf seed=1 iteration=412345 input_len=812 message="..."
//! ```
//!
//! Replay is `--seed S --replay N`, which runs that single case and nothing else.

use std::time::{Duration, Instant};

use hydra_fuzz::{run_case, Target};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };

    let seed: u64 = get("--seed").and_then(|s| s.parse().ok()).unwrap_or(1);
    let seconds: f64 = get("--seconds").and_then(|s| s.parse().ok()).unwrap_or(10.0);
    let max_iters: u64 = get("--iterations").and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);
    let target_arg = get("--target").unwrap_or_else(|| "all".to_string());

    let targets: Vec<Target> = if target_arg == "all" {
        Target::ALL.to_vec()
    } else {
        match Target::parse(&target_arg) {
            Some(t) => vec![t],
            None => {
                eprintln!("unknown target {target_arg:?}; expected one of gguf, frame-header, wire-body, all");
                std::process::exit(2);
            }
        }
    };

    // Single-case replay: run exactly the reported case, with panics VISIBLE (the default hook), so
    // the operator gets the backtrace the fuzzing run deliberately suppressed.
    if let Some(iteration) = get("--replay").and_then(|s| s.parse::<u64>().ok()) {
        let t = targets[0];
        println!("REPLAY target={} seed={seed} iteration={iteration}", t.name());
        match run_case(t, seed, iteration) {
            Some(c) => {
                println!("CRASH target={} seed={seed} iteration={iteration} input_len={} message={:?}", t.name(), c.input_len, c.message);
                std::process::exit(1);
            }
            None => {
                println!("no crash on replay");
                return;
            }
        }
    }

    // One backtrace per case would produce gigabytes of log and no information. Suppressed for the
    // whole run; `--replay` restores it.
    std::panic::set_hook(Box::new(|_| {}));

    let per_target = Duration::from_secs_f64(seconds / targets.len() as f64);
    let mut total_crashes = 0usize;
    let mut total_cpu = 0.0f64;

    for t in &targets {
        let start = Instant::now();
        let mut iterations = 0u64;
        let mut crashes: Vec<hydra_fuzz::Crash> = Vec::new();
        while start.elapsed() < per_target && iterations < max_iters {
            // Check the clock every 1024 cases: `Instant::now()` per case would dominate the
            // measurement of a parser that runs in microseconds.
            for _ in 0..1024 {
                if let Some(c) = run_case(*t, seed, iterations) {
                    crashes.push(c);
                }
                iterations += 1;
                if iterations >= max_iters {
                    break;
                }
            }
        }
        let cpu = start.elapsed().as_secs_f64();
        total_cpu += cpu;
        total_crashes += crashes.len();

        // Crash detail first, verdict last — so a log tail always ends on the verdict.
        for c in crashes.iter().take(20) {
            println!(
                "CRASH target={} seed={} iteration={} input_len={} message={:?}",
                c.target.name(),
                c.seed,
                c.iteration,
                c.input_len,
                c.message
            );
        }
        if crashes.len() > 20 {
            println!("... {} further crashes not listed", crashes.len() - 20);
        }
        println!(
            "target={} seed={seed} iterations={iterations} cpu_seconds={cpu:.1} crashes={} verdict={}",
            t.name(),
            crashes.len(),
            if crashes.is_empty() { "GREEN" } else { "RED" }
        );
    }

    println!(
        "FUZZ SUMMARY targets={} cpu_seconds={total_cpu:.1} crashes={total_crashes} verdict={}",
        targets.len(),
        if total_crashes == 0 { "GREEN" } else { "RED" }
    );
    // Restore the hook before exiting so any later panic is visible.
    let _ = std::panic::take_hook();
    if total_crashes > 0 {
        std::process::exit(1);
    }
}
