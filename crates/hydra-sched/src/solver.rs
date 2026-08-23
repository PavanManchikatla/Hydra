//! P2·3 — **the placement solver.**
//!
//! Consumes P2·1's capability estimates and P2·2's link matrix and answers the M3 question: which
//! devices host which contiguous layer ranges, in what pipeline order?
//!
//! # The objective, stated explicitly
//!
//! Hydra's workload is **single-stream autoregressive decode**: token *t+1* cannot start until
//! token *t* has traversed every stage. So the per-token latency — TPOT — is a **sum**, not a max:
//!
//! ```text
//! TPOT = Σᵢ (fixedᵢ + layersᵢ × ms_per_layer_tokᵢ)
//!      + Σ crossings (protocol + rtt + bytes/throughput)
//! ```
//!
//! **The two constants are the §7.24 amendment (ratified 2026-08-23), not a tuning.** Both were
//! *measured before being proposed*, and both are sourced independently of the measurement the M3
//! gate asks them to predict — see [`CostConstants`] for the provenance rule. Pricing compute as
//! purely proportional to layer count was **false**: a decode carries a fixed per-context cost that
//! a 2-stage pipeline pays twice, and a crossing carries protocol processing that
//! `rtt + bytes/throughput` does not capture.
//!
//! This matters, and it is worth being blunt about: **under a sum objective, "balance the stage
//! times" is not the optimum.** With no other constraint the best placement puts *every* layer on
//! the single fastest device and pays no link cost at all. P1·2's recorded proportional split
//! (Mac 56 % / myVm-2 30 % / myVm-1 14 %) is the *pipeline-parallel* heuristic — correct when
//! several tokens are in flight, which is true for batched serving and for chunked prefill, and
//! **not** true for one interactive session decoding one token at a time.
//!
//! **RULED (§7.23, 2026-08-22): (a) `SingleStreamLatency` is the M3 gate objective.** v1's
//! normative workload decides it — one session per model instance, no speculative decoding, so
//! token *t+1* strictly waits for token *t* and there is never more than one token in flight
//! within a session. The solver optimises (a) subject to memory.
//!
//! **Its corollary is a feature, not an embarrassment:** when the model fits on one device, the
//! correct placement is *no split*. Hydra's value has always been correctness and running-at-all,
//! not parallel speedup — and the solver now proves that honestly instead of asserting it.
//!
//! (b) `PipelinedThroughput` stays implemented for the two places multiple positions genuinely
//! *are* in flight: **chunked prefill (P2·7 plans against it)** and, reserved, v2's
//! multi-session/speculative modes. It is reported as an **informational** metric alongside every
//! (a)-placement ([`Placement::other_objective_ms`]); a placement need not satisfy both.
//!
//! # What makes the problem non-trivial
//!
//! **Memory.** If the model fitted on the fastest device, Hydra would not exist. Every device
//! carries a `max_layers` limit (which P2·10b's sharded loading is what finally made a real,
//! measurable per-device quantity), and it is the binding constraint that forces work onto slower
//! nodes. A solver without memory limits answers a question nobody has.
//!
//! # Exhaustive by construction
//!
//! The M3 DoD asks for a placement "within 15 % of brute-force TPOT". At M3's scale — at most 3
//! stages over a handful of devices — the search space is small enough to enumerate **every**
//! ordered device subset and **every** contiguous split, so this solver *is* the brute force and is
//! optimal by construction, not merely within 15 %. HiGHS stays unnecessary until the stage count
//! or device count grows.

use crate::capability::CapabilityRegistry;
use crate::link::{LinkId, LinkMatrix};

/// Which cost model to optimise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Objective {
    /// Per-token latency for a single decoding stream: **sum** of stage times plus link costs.
    /// The honest model for one interactive session.
    SingleStreamLatency,
    /// Steady-state throughput with several tokens in flight. The rate is set by the **single
    /// slowest element of the pipeline**, and a link is itself an element — so the cost is
    /// `max({stage compute times} ∪ {link costs})`, not `max(stages) + Σ links`. Summing would
    /// model a pipeline that never overlaps, which is the latency case above.
    /// This is the model P1·2's proportional split assumes.
    PipelinedThroughput,
}

/// §7.24 — the amended cost model's per-device and per-crossing constants.
///
/// **Both terms are physically real and were measured before being proposed** (M3 gate row 8):
///
/// * **`fixed_ms`** — a decode carries a cost **independent of how many layers the context runs**:
///   graph dispatch, scheduler setup, output marshalling. Measured on the dev Mac by the
///   **windowed-context decomposition**, which is now the standard method: time the full range and
///   both halves, then solve `t(L) = fixed + L·per_layer`. Observed `full [0,24) = 16.88`,
///   `lower [0,12) = 10.68`, `upper [12,24) = 11.23` ms/tok ⇒ **`fixed ≈ 5.03 ms`**,
///   `per_layer ≈ 0.494 ms`. A 2-stage pipeline pays `fixed` **twice**; the unsplit reference pays
///   it once. That is why splitting is not free even before a byte crosses a wire.
/// * **`protocol_ms`** — per coordinator↔stage exchange: encode + BLAKE3 framing + mTLS + decode on
///   both ends. **Measured independently, with zero inference in the loop**
///   (`hydra-worker/tests/protocol_microbench.rs`): **0.438 ms** per exchange on loopback.
///
/// **Provenance rule (binding, §7.24):** every coefficient must be sourced **independently of the
/// measurement it is asked to predict**. `protocol_ms` in particular may **not** be a residual
/// fitted to the configuration the gate measures — *fitted-here-passes-here is worthless;
/// fitted-here-predicts-there is a cost model.*
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostConstants {
    /// Per-stage fixed decode cost (ms/token), independent of layer count.
    pub fixed_ms: f64,
    /// Per coordinator↔stage exchange protocol cost (ms/token).
    pub protocol_ms: f64,
}

impl Default for CostConstants {
    /// Zeroes — the **pre-amendment** model. Kept as the default so every existing caller and
    /// fixture keeps its old meaning until it opts in, and so a caller that forgets to supply
    /// measured constants gets the old (optimistic) answer rather than an invented one.
    fn default() -> Self {
        CostConstants { fixed_ms: 0.0, protocol_ms: 0.0 }
    }
}

/// One device's placement-relevant limits.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceLimits {
    /// Most layers this device can hold. **P2·10b made this real:** with sharded weights a device
    /// loads only its own layers, so this is a measured capacity rather than all-or-nothing.
    pub max_layers: u32,
}

/// One stage of a chosen placement.
#[derive(Debug, Clone, PartialEq)]
pub struct Stage {
    pub device: String,
    pub layer_first: u32,
    pub layer_last: u32,
}

impl Stage {
    pub fn n_layers(&self) -> u32 {
        self.layer_last - self.layer_first
    }
}

/// A complete placement and its predicted cost.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    pub stages: Vec<Stage>,
    /// Predicted per-token cost in milliseconds under the chosen [`Objective`].
    pub tpot_ms: f64,
    /// Compute term only (no link costs) — useful for showing what the links are costing.
    pub compute_ms: f64,
    /// Link term only.
    pub link_ms: f64,
    /// **Informational (§7.23 ruling):** this same placement's cost under the *other* objective —
    /// [`Objective::PipelinedThroughput`] when the search optimised latency, and vice versa.
    /// v1 gates on (a) alone; a placement need not satisfy both. This is reported so the
    /// throughput consequence of a latency-optimal placement is visible rather than invisible,
    /// and because chunked prefill (P2·7) plans against (b) on the same cluster.
    pub other_objective_ms: f64,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SolveError {
    #[error("no device has a capability estimate — an unmeasured cluster cannot be placed")]
    NoMeasuredDevices,
    #[error("no placement fits: {n_layer} layers exceed the total capacity of the measured devices ({capacity})")]
    DoesNotFit { n_layer: u32, capacity: u32 },
    #[error("no placement is fully costable — some required link was never probed (an unmeasured link is never assumed free)")]
    UnpricedLinks,
}

/// Search inputs.
pub struct SolveInput<'a> {
    pub caps: &'a CapabilityRegistry,
    pub links: &'a LinkMatrix,
    pub limits: &'a dyn Fn(&str) -> DeviceLimits,
    pub n_layer: u32,
    /// Bytes on the wire for one boundary residual (`n_embd × payload width`).
    pub boundary_bytes: u64,
    pub max_stages: usize,
    pub objective: Objective,
    /// §7.24 amended-model constants. Defaults to zeroes = the pre-amendment model.
    pub costs: CostConstants,
}

/// Enumerate every ordered device subset of size 1..=max_stages and every contiguous split, and
/// return the cheapest fully-costable, memory-feasible placement.
pub fn solve(input: &SolveInput) -> Result<Placement, SolveError> {
    let measured = input.caps.measured();
    if measured.is_empty() {
        return Err(SolveError::NoMeasuredDevices);
    }
    let capacity: u32 = measured.iter().map(|(d, _)| (input.limits)(d).max_layers).sum();
    if capacity < input.n_layer {
        return Err(SolveError::DoesNotFit { n_layer: input.n_layer, capacity });
    }

    let mut best: Option<Placement> = None;
    let mut any_priced = false;
    let k = input.max_stages.min(measured.len());

    for size in 1..=k {
        for perm in ordered_subsets(&measured, size) {
            for split in contiguous_splits(input.n_layer, size) {
                // Memory feasibility.
                let feasible = perm
                    .iter()
                    .zip(&split)
                    .all(|((d, _), n)| *n >= 1 && *n <= (input.limits)(d).max_layers);
                if !feasible {
                    continue;
                }
                // Compute term.
                let stage_ms: Vec<f64> =
                    perm.iter().zip(&split).map(|((_, ms), n)| input.costs.fixed_ms + *n as f64 * *ms).collect();
                let compute_ms = match input.objective {
                    Objective::SingleStreamLatency => stage_ms.iter().sum::<f64>(),
                    Objective::PipelinedThroughput => stage_ms.iter().cloned().fold(0.0, f64::max),
                };
                // Link accumulation differs by objective — see the combine below.
                // Link term: one boundary crossing per adjacent pair. An unpriced link
                // disqualifies the placement — it is never treated as free.
                let mut link_ms = 0.0;
                // Accumulated under the OTHER objective, so it can be reported informationally.
                let mut other_link_ms: f64 = 0.0;
                let mut priced = true;
                for w in perm.windows(2) {
                    let id = match LinkId::new(w[0].0, w[1].0) {
                        Ok(id) => id,
                        Err(_) => {
                            priced = false;
                            break;
                        }
                    };
                    match input.links.cost_ms(&id, input.boundary_bytes).map(|c| c + input.costs.protocol_ms) {
                        Some(c) => {
                            match input.objective {
                                // Latency: every crossing is paid, one after another.
                                Objective::SingleStreamLatency => {
                                    link_ms += c;
                                    other_link_ms = other_link_ms.max(c);
                                }
                                // Throughput: a link is itself a pipeline stage, so the SLOWEST
                                // link — not their sum — is what can bottleneck the rate.
                                Objective::PipelinedThroughput => {
                                    link_ms = link_ms.max(c);
                                    other_link_ms += c;
                                }
                            }
                        }
                        None => {
                            priced = false;
                            break;
                        }
                    }
                }
                if !priced {
                    continue;
                }
                any_priced = true;
                let tpot_ms = match input.objective {
                    Objective::SingleStreamLatency => compute_ms + link_ms,
                    // The rate is set by the single slowest element of the pipeline, whether that
                    // is a compute stage or a link. Adding them would model a pipeline that never
                    // overlaps, which is the latency case above.
                    Objective::PipelinedThroughput => compute_ms.max(link_ms),
                };
                let other_objective_ms = match input.objective {
                    // We optimised latency; report what this placement costs as a pipeline.
                    Objective::SingleStreamLatency => {
                        stage_ms.iter().cloned().fold(0.0, f64::max).max(other_link_ms)
                    }
                    // We optimised throughput; report its single-stream latency.
                    Objective::PipelinedThroughput => stage_ms.iter().sum::<f64>() + other_link_ms,
                };
                let cand = Placement {
                    stages: perm
                        .iter()
                        .zip(&split)
                        .scan(0u32, |cur, ((d, _), n)| {
                            let s = Stage { device: d.to_string(), layer_first: *cur, layer_last: *cur + n };
                            *cur += n;
                            Some(s)
                        })
                        .collect(),
                    tpot_ms,
                    compute_ms,
                    link_ms,
                    other_objective_ms,
                };
                // Strict improvement only, so ties keep the first (deterministic) candidate.
                if best.as_ref().map_or(true, |b| cand.tpot_ms < b.tpot_ms - 1e-12) {
                    best = Some(cand);
                }
            }
        }
    }

    match best {
        Some(p) => Ok(p),
        None if !any_priced => Err(SolveError::UnpricedLinks),
        None => Err(SolveError::DoesNotFit { n_layer: input.n_layer, capacity }),
    }
}

/// All ordered subsets (permutations of subsets) of `items` with exactly `size` elements.
/// Pipeline **order matters** — the link cost is directional.
fn ordered_subsets<'a>(items: &[(&'a str, f64)], size: usize) -> Vec<Vec<(&'a str, f64)>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    let mut used = vec![false; items.len()];
    fn rec<'a>(
        items: &[(&'a str, f64)],
        size: usize,
        used: &mut Vec<bool>,
        cur: &mut Vec<(&'a str, f64)>,
        out: &mut Vec<Vec<(&'a str, f64)>>,
    ) {
        if cur.len() == size {
            out.push(cur.clone());
            return;
        }
        for i in 0..items.len() {
            if used[i] {
                continue;
            }
            used[i] = true;
            cur.push(items[i]);
            rec(items, size, used, cur, out);
            cur.pop();
            used[i] = false;
        }
    }
    rec(items, size, &mut used, &mut cur, &mut out);
    out
}

/// Every way to split `n` layers into `parts` contiguous non-empty runs.
fn contiguous_splits(n: u32, parts: usize) -> Vec<Vec<u32>> {
    let mut out = Vec::new();
    if parts == 0 || n < parts as u32 {
        return out;
    }
    let mut cur = vec![0u32; parts];
    fn rec(parts: usize, i: usize, left: u32, cur: &mut Vec<u32>, out: &mut Vec<Vec<u32>>) {
        if i == parts - 1 {
            if left >= 1 {
                cur[i] = left;
                out.push(cur.clone());
            }
            return;
        }
        let remaining_slots = (parts - i - 1) as u32;
        for take in 1..=(left.saturating_sub(remaining_slots)) {
            cur[i] = take;
            rec(parts, i + 1, left - take, cur, out);
        }
    }
    rec(parts, 0, n, &mut cur, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{BenchConfig, CapabilityRegistry, Sample, SustainedBench};
    use crate::link::LinkSample;

    const MB: f64 = 1_000_000.0;

    fn cap(ms: f64) -> crate::capability::Measurement {
        let cfg = BenchConfig { warmup_samples: 0, ..Default::default() };
        let mut b = SustainedBench::new(cfg);
        for _ in 0..10 {
            b.push(Sample::new(ms, 4.0).unwrap());
        }
        b.finish().unwrap()
    }

    /// The P1·2 real 3-node set, by ms/layer-token.
    fn p1_2_caps() -> CapabilityRegistry {
        let mut r = CapabilityRegistry::new();
        r.observe("mac", "aarch64", cap(1.00));
        r.observe("myvm-2", "x86_64", cap(1.89));
        r.observe("myvm-1", "x86_64", cap(4.02));
        r
    }

    /// The recorded topology shape: Mac ↔ VMs over Tailscale WAN, VM ↔ VM over the sub-ms VNet.
    fn p1_2_links() -> LinkMatrix {
        let mut m = LinkMatrix::new();
        let mut put = |a: &str, b: &str, rtt: f64, tput: f64| {
            m.observe_solo(&LinkId::new(a, b).unwrap(), LinkSample::new(rtt, tput).unwrap());
        };
        for (a, b) in [("mac", "myvm-1"), ("myvm-1", "mac"), ("mac", "myvm-2"), ("myvm-2", "mac")] {
            put(a, b, 25.0, 10.0 * MB);
        }
        for (a, b) in [("myvm-1", "myvm-2"), ("myvm-2", "myvm-1")] {
            put(a, b, 0.4, 100.0 * MB);
        }
        m
    }

    fn limits_all(n: u32) -> impl Fn(&str) -> DeviceLimits {
        move |_| DeviceLimits { max_layers: n }
    }

    #[test]
    fn an_unmeasured_cluster_cannot_be_placed() {
        let caps = CapabilityRegistry::new();
        let links = LinkMatrix::new();
        let lim = limits_all(24);
        let e = solve(&SolveInput {
            caps: &caps, links: &links, limits: &lim, n_layer: 24,
            boundary_bytes: 3584, max_stages: 3, objective: Objective::SingleStreamLatency, costs: CostConstants::default(),
        })
        .unwrap_err();
        assert_eq!(e, SolveError::NoMeasuredDevices);
    }

    #[test]
    fn a_model_that_does_not_fit_is_refused_not_squeezed() {
        let caps = p1_2_caps();
        let links = p1_2_links();
        let lim = limits_all(4); // 3 devices x 4 = 12 < 24
        let e = solve(&SolveInput {
            caps: &caps, links: &links, limits: &lim, n_layer: 24,
            boundary_bytes: 3584, max_stages: 3, objective: Objective::SingleStreamLatency, costs: CostConstants::default(),
        })
        .unwrap_err();
        assert_eq!(e, SolveError::DoesNotFit { n_layer: 24, capacity: 12 });
    }

    #[test]
    fn an_unpriced_link_disqualifies_a_multi_stage_placement() {
        // Two devices, no link ever probed. A 1-stage placement is impossible (memory), so there is
        // nothing costable — and the solver must say so rather than assume the link is free.
        let mut caps = CapabilityRegistry::new();
        caps.observe("a", "x", cap(1.0));
        caps.observe("b", "x", cap(1.0));
        let links = LinkMatrix::new();
        let lim = limits_all(12); // forces 2 stages for 24 layers
        let e = solve(&SolveInput {
            caps: &caps, links: &links, limits: &lim, n_layer: 24,
            boundary_bytes: 3584, max_stages: 3, objective: Objective::SingleStreamLatency, costs: CostConstants::default(),
        })
        .unwrap_err();
        assert_eq!(e, SolveError::UnpricedLinks);
    }

    #[test]
    fn with_room_to_spare_single_stream_puts_everything_on_the_fastest_device() {
        // The blunt consequence of a SUM objective: if it fits on the fastest node, that is
        // optimal — one stage, zero link cost. Hydra exists because it usually does not fit.
        let caps = p1_2_caps();
        let links = p1_2_links();
        let lim = limits_all(24);
        let p = solve(&SolveInput {
            caps: &caps, links: &links, limits: &lim, n_layer: 24,
            boundary_bytes: 3584, max_stages: 3, objective: Objective::SingleStreamLatency, costs: CostConstants::default(),
        })
        .unwrap();
        assert_eq!(p.stages.len(), 1);
        assert_eq!(p.stages[0].device, "mac");
        assert_eq!(p.link_ms, 0.0);
        assert!((p.tpot_ms - 24.0).abs() < 1e-9, "24 layers x 1.00 ms = 24 ms, got {}", p.tpot_ms);
    }

    #[test]
    fn memory_pressure_is_what_forces_a_pipeline() {
        // Cap every device at 12 layers: 24 layers no longer fit anywhere alone, so the solver must
        // build a real pipeline — and it should prefer the two FASTEST devices.
        let caps = p1_2_caps();
        let links = p1_2_links();
        let lim = limits_all(12);
        let p = solve(&SolveInput {
            caps: &caps, links: &links, limits: &lim, n_layer: 24,
            boundary_bytes: 3584, max_stages: 3, objective: Objective::SingleStreamLatency, costs: CostConstants::default(),
        })
        .unwrap();
        assert!(p.stages.len() >= 2, "memory pressure must force a pipeline");
        let devs: Vec<&str> = p.stages.iter().map(|s| s.device.as_str()).collect();
        assert!(devs.contains(&"mac") && devs.contains(&"myvm-2"), "prefer the two fastest: {devs:?}");
        assert!(!devs.contains(&"myvm-1"), "the slowest device should not be needed: {devs:?}");
    }

    #[test]
    fn stages_are_contiguous_and_cover_the_model_exactly() {
        let caps = p1_2_caps();
        let links = p1_2_links();
        let lim = limits_all(10); // forces 3 stages
        let p = solve(&SolveInput {
            caps: &caps, links: &links, limits: &lim, n_layer: 24,
            boundary_bytes: 3584, max_stages: 3, objective: Objective::SingleStreamLatency, costs: CostConstants::default(),
        })
        .unwrap();
        let mut cursor = 0;
        for s in &p.stages {
            assert_eq!(s.layer_first, cursor, "contiguous — no layer skipped");
            assert!(s.n_layers() >= 1, "no empty stage");
            cursor = s.layer_last;
        }
        assert_eq!(cursor, 24, "must cover the model exactly — none dropped, none doubled");
    }

    #[test]
    fn the_solver_prefers_the_fast_vnet_leg_between_the_two_vms() {
        // With the Mac excluded by memory, the surviving pair sits on the sub-ms VNet leg, and the
        // link term should be negligible — the ordering P2·2's fixture asserts, now consumed.
        let caps = p1_2_caps();
        let links = p1_2_links();
        let lim = |d: &str| DeviceLimits { max_layers: if d == "mac" { 0 } else { 20 } };
        let p = solve(&SolveInput {
            caps: &caps, links: &links, limits: &lim, n_layer: 24,
            boundary_bytes: 3584, max_stages: 3, objective: Objective::SingleStreamLatency, costs: CostConstants::default(),
        })
        .unwrap();
        let devs: Vec<&str> = p.stages.iter().map(|s| s.device.as_str()).collect();
        assert_eq!(devs.len(), 2);
        assert!(!devs.contains(&"mac"));
        assert!(p.link_ms < 1.0, "the VNet leg must be near-free: {} ms", p.link_ms);
    }

    #[test]
    fn a_costly_link_can_make_fewer_stages_win() {
        // Two devices of EQUAL speed but an expensive link between them: splitting buys nothing
        // under a sum objective and costs the crossing, so one stage must win when it fits.
        let mut caps = CapabilityRegistry::new();
        caps.observe("a", "x", cap(1.0));
        caps.observe("b", "x", cap(1.0));
        let mut links = LinkMatrix::new();
        for (x, y) in [("a", "b"), ("b", "a")] {
            links.observe_solo(&LinkId::new(x, y).unwrap(), LinkSample::new(500.0, 1.0 * MB).unwrap());
        }
        let lim = limits_all(24);
        let p = solve(&SolveInput {
            caps: &caps, links: &links, limits: &lim, n_layer: 24,
            boundary_bytes: 3584, max_stages: 3, objective: Objective::SingleStreamLatency, costs: CostConstants::default(),
        })
        .unwrap();
        assert_eq!(p.stages.len(), 1, "a 500 ms crossing must not be paid for nothing");
    }

    /// All-fast links, so compute is what the balance is decided on.
    fn cheap_links() -> LinkMatrix {
        let mut m = LinkMatrix::new();
        for a in ["mac", "myvm-1", "myvm-2"] {
            for b in ["mac", "myvm-1", "myvm-2"] {
                if a != b {
                    m.observe_solo(&LinkId::new(a, b).unwrap(), LinkSample::new(0.4, 100.0 * MB).unwrap());
                }
            }
        }
        m
    }

    /// **This is a (b)-OBJECTIVE fixture** (§7.23 ruling): P1·2's deployed split was chosen by the
    /// pipeline-parallel heuristic, so it validates `PipelinedThroughput`, which v1 retains for
    /// chunked prefill (P2·7) and reserves for v2. It is **not** a validation of the M3 gate
    /// objective (a) — labelled so it is never read as one.
    #[test]
    fn pipelined_objective_b_reproduces_the_p1_2_deployed_split_exactly() {
        // THE P2·3 fixture validation. P1·2's deployed 14/7/3 is the PIPELINE-PARALLEL optimum:
        // minimise the slowest pipeline element. With links cheap enough not to bottleneck, the
        // exhaustive solver must land on exactly the split that was deployed on real hardware.
        //   14 x 1.00 = 14.0 | 7 x 1.89 = 13.2 | 3 x 4.02 = 12.1  ->  max 14.0 ms
        // and every alternative is worse (15/6/3 -> 15.0, 13/8/3 -> 15.1, 14/6/4 -> 16.1).
        let caps = p1_2_caps();
        let links = cheap_links();
        let lim = limits_all(24);
        let p = solve(&SolveInput {
            caps: &caps, links: &links, limits: &lim, n_layer: 24,
            boundary_bytes: 3584, max_stages: 3, objective: Objective::PipelinedThroughput, costs: CostConstants::default(),
        })
        .unwrap();
        assert_eq!(p.stages.len(), 3, "balancing wants every device: {:?}", p.stages);
        let by_dev: std::collections::BTreeMap<&str, u32> =
            p.stages.iter().map(|s| (s.device.as_str(), s.n_layers())).collect();
        assert_eq!(by_dev["mac"], 14, "the deployed Mac share");
        assert_eq!(by_dev["myvm-2"], 7, "the deployed myVm-2 share");
        assert_eq!(by_dev["myvm-1"], 3, "the deployed myVm-1 share");
        assert!((p.compute_ms - 14.0).abs() < 1e-9, "slowest stage 14.0 ms, got {}", p.compute_ms);
    }

    #[test]
    fn on_the_real_wan_links_a_small_model_should_not_be_split_at_all() {
        // An uncomfortable but honest consequence of pricing the links: with a 25 ms Tailscale RTT
        // per boundary and a whole 0.5B model costing only ~24 ms on the Mac, ANY split is slower
        // than not splitting. This matches what the real 3-node WAN run actually measured —
        // ~0.81 tok/s across the cluster versus tens of tok/s on the Mac alone (docs/wan-run.md).
        // The solver is not supposed to flatter the architecture; splitting pays when the model
        // does not fit, which is what the memory limits above express.
        let caps = p1_2_caps();
        let links = p1_2_links();
        let lim = limits_all(24);
        for obj in [Objective::SingleStreamLatency, Objective::PipelinedThroughput] {
            let p = solve(&SolveInput {
                caps: &caps, links: &links, limits: &lim, n_layer: 24,
                boundary_bytes: 3584, max_stages: 3, objective: obj, costs: CostConstants::default(),
            })
            .unwrap();
            assert_eq!(p.stages.len(), 1, "{obj:?}: a 25 ms WAN crossing is not worth paying here");
            assert_eq!(p.stages[0].device, "mac");
        }
    }

    #[test]
    fn the_search_is_exhaustive_so_the_result_is_optimal_by_construction() {
        // The M3 DoD asks for "within 15 % of brute-force TPOT". This solver IS the brute force at
        // M3's scale, so verify that directly: no enumerated feasible placement beats the answer.
        let caps = p1_2_caps();
        let links = p1_2_links();
        let lim = limits_all(12);
        let input = SolveInput {
            caps: &caps, links: &links, limits: &lim, n_layer: 24,
            boundary_bytes: 3584, max_stages: 3, objective: Objective::SingleStreamLatency, costs: CostConstants::default(),
        };
        let best = solve(&input).unwrap();
        // Independent re-enumeration.
        let measured = caps.measured();
        let mut brute = f64::MAX;
        for size in 1..=3 {
            for perm in ordered_subsets(&measured, size) {
                for split in contiguous_splits(24, size) {
                    if !perm.iter().zip(&split).all(|((d, _), n)| *n >= 1 && *n <= lim(d).max_layers) {
                        continue;
                    }
                    let compute: f64 = perm.iter().zip(&split).map(|((_, ms), n)| *n as f64 * *ms).sum();
                    let mut link = 0.0;
                    let mut ok = true;
                    for w in perm.windows(2) {
                        match links.cost_ms(&LinkId::new(w[0].0, w[1].0).unwrap(), 3584) {
                            Some(c) => link += c,
                            None => { ok = false; break; }
                        }
                    }
                    if ok {
                        brute = brute.min(compute + link);
                    }
                }
            }
        }
        assert!((best.tpot_ms - brute).abs() < 1e-9, "solver {} vs brute force {brute}", best.tpot_ms);
    }

    /// The measured §7.24 constants for the dev Mac. Sources, both independent of the M3 gate
    /// measurement: `fixed_ms` from the windowed-context decomposition
    /// (`hydra-engine-sys/tests/fixed_decode_cost.rs`), `protocol_ms` from the zero-inference
    /// microbench (`hydra-worker/tests/protocol_microbench.rs`).
    fn measured_costs() -> CostConstants {
        CostConstants { fixed_ms: 5.03, protocol_ms: 0.438 }
    }

    /// **§7.24 fixture check (required by the ruling).** The per-stage fixed cost also enters
    /// objective (b), so the P1·2 (b)-fixture must be re-run under the amendment and any change to
    /// its chosen split reported as a finding — never silently re-fixtured.
    ///
    /// **Result: the chosen split is UNCHANGED at 14/7/3**, and there is a reason rather than a
    /// coincidence. Under (b) the cost is `max` over stages, and `fixed` is added *uniformly* to
    /// every stage: `max(c+a, c+b, c+d) = c + max(a,b,d)`, so a uniform additive constant cannot
    /// move the argmin. What *does* change is the predicted **cost** — the balance point is the
    /// same, the number attached to it is larger.
    #[test]
    fn amendment_does_not_move_the_p1_2_b_fixture_split_and_here_is_why() {
        let caps = p1_2_caps();
        let links = cheap_links();
        let lim = limits_all(24);
        let before = solve(&SolveInput {
            caps: &caps, links: &links, limits: &lim, n_layer: 24,
            boundary_bytes: 3584, max_stages: 3, objective: Objective::PipelinedThroughput,
            costs: CostConstants::default(),
        })
        .unwrap();
        let after = solve(&SolveInput {
            caps: &caps, links: &links, limits: &lim, n_layer: 24,
            boundary_bytes: 3584, max_stages: 3, objective: Objective::PipelinedThroughput,
            costs: measured_costs(),
        })
        .unwrap();

        let split = |p: &Placement| -> std::collections::BTreeMap<String, u32> {
            p.stages.iter().map(|s| (s.device.clone(), s.n_layers())).collect()
        };
        assert_eq!(split(&before), split(&after), "the amendment must not silently re-fixture P1·2");
        let s = split(&after);
        assert_eq!((s["mac"], s["myvm-2"], s["myvm-1"]), (14, 7, 3), "still the deployed split");
        // The cost moves even though the split does not — the amendment is not a no-op.
        assert!(after.tpot_ms > before.tpot_ms, "the amendment must raise the predicted cost");
        assert!((after.tpot_ms - (before.tpot_ms + 5.03)).abs() < 1e-9, "by exactly one fixed term");
    }

    #[test]
    fn the_amendment_makes_splitting_dearer_so_the_no_split_conclusion_holds_a_fortiori() {
        // §7.23's corollary is strengthened, not weakened: paying `fixed` once beats paying it
        // twice, so a model that fits on one device is now even more clearly best left unsplit.
        let caps = p1_2_caps();
        let links = cheap_links();
        let lim = limits_all(24);
        let p = solve(&SolveInput {
            caps: &caps, links: &links, limits: &lim, n_layer: 24,
            boundary_bytes: 3584, max_stages: 3, objective: Objective::SingleStreamLatency,
            costs: measured_costs(),
        })
        .unwrap();
        assert_eq!(p.stages.len(), 1, "one stage pays `fixed` once; any split pays it per stage");
        assert!((p.tpot_ms - (24.0 + 5.03)).abs() < 1e-9, "24 layers x 1.00 + one fixed term");
    }

    #[test]
    fn the_other_objective_is_reported_alongside_every_placement() {
        // §7.23: v1 gates on (a), but the throughput consequence of a latency-optimal placement
        // must be visible rather than invisible — chunked prefill plans against (b) on the same
        // cluster. Forced into a 3-stage pipeline over cheap links so both numbers are non-trivial.
        let caps = p1_2_caps();
        let links = cheap_links();
        let lim = limits_all(10); // 24 layers over 3 devices
        let p = solve(&SolveInput {
            caps: &caps, links: &links, limits: &lim, n_layer: 24,
            boundary_bytes: 3584, max_stages: 3, objective: Objective::SingleStreamLatency, costs: CostConstants::default(),
        })
        .unwrap();
        assert_eq!(p.stages.len(), 3);
        // (a) is a sum over stages plus every crossing; (b) is the slowest single element.
        assert!(p.other_objective_ms < p.tpot_ms, "pipelined cost must be below summed latency: {} vs {}", p.other_objective_ms, p.tpot_ms);
        let slowest_stage = p.stages.iter().map(|s| s.n_layers()).max().unwrap();
        assert!(p.other_objective_ms >= slowest_stage as f64 * 1.0 - 1e-9, "(b) must be at least the slowest stage");
    }

    #[test]
    fn contiguous_splits_are_complete_and_exact() {
        let s = contiguous_splits(5, 3);
        assert_eq!(s.len(), 6, "compositions of 5 into 3 positive parts = C(4,2) = 6");
        assert!(s.iter().all(|v| v.iter().sum::<u32>() == 5 && v.iter().all(|x| *x >= 1)));
        assert!(contiguous_splits(2, 3).is_empty(), "cannot split 2 layers into 3 non-empty stages");
    }
}
