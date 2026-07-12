fn main() {
    let budget: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(5000);
    let k: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(50);
    let mut caught = 0u64; let mut steps: Vec<u64> = vec![];
    for i in 0..k {
        let seed = (i + 1).wrapping_mul(0x100_0000_01B3);
        if let Some(f) = hydra_sim::run(seed, 2, budget) { caught += 1; steps.push(f.step); }
    }
    steps.sort_unstable();
    let med = if steps.is_empty() {0} else {steps[steps.len()/2]};
    println!("caught {caught}/{k} (budget {budget}); median {med}");
}
