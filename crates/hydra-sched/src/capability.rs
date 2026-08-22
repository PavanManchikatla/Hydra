//! P2·1 — **device capability: the startup benchmark and its EWMA.**
//!
//! P1·2's finding is the reason this module exists: **capability does not track RAM or vCPU count**
//! (a 4 GiB `B2als_v2` measured ~2× a 8 GB `B2ms`). Capability must be *measured*, never inferred
//! from a spec sheet — so the scheduler's first input is a real per-device benchmark.
//!
//! The unit is **milliseconds per layer-token**: the time this device takes to push one token
//! through one transformer layer. It is the placement solver's natural unit because a stage's
//! wall-time is (its layer count) × (this number), so balancing stage times means allocating layers
//! in proportion to `1 / ms_per_layer_tok`.
//!
//! **Lower is faster.** Every guard in this module exists because a device that *looks* faster than
//! it is will be handed too many layers, and the pipeline runs at the speed of its slowest stage —
//! so an optimistic measurement is not a small error, it is a stall. That is why a zero sample, an
//! under-length run, and an unseeded EWMA are all hard errors rather than quietly-accepted values.
//!
//! Everything here is pure: samples are handed in by whoever ran the work (`hydra-bench` owns the
//! engine), so the aggregation, the EWMA, and the layer allocation are deterministic and testable
//! in container CI with no model and no GPU.

use std::collections::BTreeMap;

/// The sustained-window bounds the M3 plan fixes for the startup benchmark.
pub const DEFAULT_MIN_DURATION_S: f64 = 30.0;
pub const DEFAULT_MAX_DURATION_S: f64 = 120.0;

/// Relative spread (max−min)/median at or below which a measurement is called **stable**.
/// P1·2's numbers re-ran within ~1.5 % (10.37/10.37, 22.0/21.7), so 15 % is a loose ceiling that
/// still catches a genuinely noisy box (thermal throttling, a burstable instance out of credit).
pub const STABILITY_THRESHOLD: f64 = 0.15;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum BenchError {
    #[error("sample {0} ms/layer-tok is not a positive finite number — a zero or NaN sample would make this device look infinitely fast")]
    BadSample(f64),
    #[error("sample window {0}s is not a positive finite duration")]
    BadWindow(f64),
    #[error("benchmark ran {got:.1}s but the sustained window requires at least {want:.1}s — an under-length run is REFUSED, never reported as a weaker measurement")]
    TooShort { got: f64, want: f64 },
    #[error("no samples survived warm-up discard ({discarded} discarded)")]
    NoSamplesAfterWarmup { discarded: usize },
    #[error("ewma alpha {0} must be in (0, 1]")]
    BadAlpha(f64),
}

/// One timing window from a running benchmark.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// Milliseconds to push one token through one transformer layer, over this window.
    pub ms_per_layer_tok: f64,
    /// Wall-clock seconds this window covered.
    pub window_s: f64,
}

impl Sample {
    pub fn new(ms_per_layer_tok: f64, window_s: f64) -> Result<Sample, BenchError> {
        if !ms_per_layer_tok.is_finite() || ms_per_layer_tok <= 0.0 {
            return Err(BenchError::BadSample(ms_per_layer_tok));
        }
        if !window_s.is_finite() || window_s <= 0.0 {
            return Err(BenchError::BadWindow(window_s));
        }
        Ok(Sample { ms_per_layer_tok, window_s })
    }
}

/// How a sustained benchmark is run. Defaults match the M3 plan's 30–120 s window.
#[derive(Debug, Clone, Copy)]
pub struct BenchConfig {
    pub min_duration_s: f64,
    pub max_duration_s: f64,
    /// Leading samples discarded before aggregation. The first windows of a fresh context carry
    /// page-faults on freshly mmap'd weights and a cold KV cache; counting them would report the
    /// device as *slower* than it is, which under-allocates its layers.
    pub warmup_samples: usize,
}

impl Default for BenchConfig {
    fn default() -> Self {
        BenchConfig { min_duration_s: DEFAULT_MIN_DURATION_S, max_duration_s: DEFAULT_MAX_DURATION_S, warmup_samples: 2 }
    }
}

/// The result of one sustained benchmark run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    /// Median ms/layer-token over the post-warm-up samples. The **median**, not the mean: one
    /// scheduler-preemption spike must not drag a device's estimate.
    pub ms_per_layer_tok: f64,
    pub samples_used: usize,
    pub warmup_discarded: usize,
    /// Measured seconds counted toward the sustained window (post-warm-up).
    pub duration_s: f64,
    /// (max − min) / median over the used samples.
    pub spread: f64,
}

impl Measurement {
    /// Whether the box held still enough for this number to be trusted as a steady-state estimate.
    /// An unstable measurement is still *reported* — it is real data — but callers should treat it
    /// as provisional and re-measure, rather than silently placing layers on it.
    pub fn is_stable(&self) -> bool {
        self.spread <= STABILITY_THRESHOLD
    }

    /// Throughput form: layer-tokens per second. Convenience for ratio work.
    pub fn layer_toks_per_s(&self) -> f64 {
        1000.0 / self.ms_per_layer_tok
    }
}

/// Accumulates samples from one sustained benchmark run.
#[derive(Debug, Clone)]
pub struct SustainedBench {
    cfg: BenchConfig,
    samples: Vec<Sample>,
}

impl SustainedBench {
    pub fn new(cfg: BenchConfig) -> Self {
        SustainedBench { cfg, samples: Vec::new() }
    }

    /// Total wall-clock seconds pushed so far, including warm-up.
    pub fn elapsed_s(&self) -> f64 {
        self.samples.iter().map(|s| s.window_s).sum()
    }

    /// True once the run has covered its maximum window — the caller should stop measuring.
    /// The cap exists so a startup benchmark cannot delay a session unboundedly.
    pub fn should_stop(&self) -> bool {
        self.elapsed_s() >= self.cfg.max_duration_s
    }

    pub fn push(&mut self, s: Sample) {
        self.samples.push(s);
    }

    /// Aggregate. **Refuses** an under-length run rather than reporting a weaker number, because a
    /// short measurement is indistinguishable from a good one once it is just a float in a table.
    pub fn finish(&self) -> Result<Measurement, BenchError> {
        let discarded = self.cfg.warmup_samples.min(self.samples.len());
        let used = &self.samples[discarded..];
        if used.is_empty() {
            return Err(BenchError::NoSamplesAfterWarmup { discarded });
        }
        let duration_s: f64 = used.iter().map(|s| s.window_s).sum();
        if duration_s < self.cfg.min_duration_s {
            return Err(BenchError::TooShort { got: duration_s, want: self.cfg.min_duration_s });
        }
        let mut v: Vec<f64> = used.iter().map(|s| s.ms_per_layer_tok).collect();
        v.sort_by(|a, b| a.partial_cmp(b).expect("samples are finite by construction"));
        let median = if v.len() % 2 == 1 { v[v.len() / 2] } else { (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0 };
        let spread = (v[v.len() - 1] - v[0]) / median;
        Ok(Measurement {
            ms_per_layer_tok: median,
            samples_used: used.len(),
            warmup_discarded: discarded,
            duration_s,
            spread,
        })
    }
}

/// An exponentially-weighted moving average over successive measurements of one device.
///
/// **Seeded on first update, never on zero.** A zero-initialised EWMA of a "lower is faster" metric
/// would make an unmeasured device look infinitely capable and win every layer in the placement —
/// the one failure mode this whole module is defending against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ewma {
    alpha: f64,
    value: Option<f64>,
    updates: u32,
}

impl Ewma {
    pub fn new(alpha: f64) -> Result<Ewma, BenchError> {
        if !alpha.is_finite() || alpha <= 0.0 || alpha > 1.0 {
            return Err(BenchError::BadAlpha(alpha));
        }
        Ok(Ewma { alpha, value: None, updates: 0 })
    }

    /// `alpha = 0.3`: a new measurement moves the estimate about a third of the way. Fast enough to
    /// track a box that has genuinely changed (thermal throttle, competing load), slow enough that
    /// one noisy window does not re-plan the cluster.
    pub fn default_alpha() -> Ewma {
        Ewma::new(0.3).expect("0.3 is a valid alpha")
    }

    pub fn value(&self) -> Option<f64> {
        self.value
    }

    pub fn updates(&self) -> u32 {
        self.updates
    }

    pub fn update(&mut self, ms_per_layer_tok: f64) {
        debug_assert!(ms_per_layer_tok.is_finite() && ms_per_layer_tok > 0.0);
        self.value = Some(match self.value {
            None => ms_per_layer_tok,
            Some(prev) => self.alpha * ms_per_layer_tok + (1.0 - self.alpha) * prev,
        });
        self.updates += 1;
    }
}

/// One device's tracked capability.
#[derive(Debug, Clone)]
pub struct DeviceCapability {
    pub device: String,
    pub arch: String,
    ewma: Ewma,
    pub last: Option<Measurement>,
}

impl DeviceCapability {
    pub fn new(device: impl Into<String>, arch: impl Into<String>) -> Self {
        DeviceCapability { device: device.into(), arch: arch.into(), ewma: Ewma::default_alpha(), last: None }
    }

    pub fn observe(&mut self, m: Measurement) {
        self.ewma.update(m.ms_per_layer_tok);
        self.last = Some(m);
    }

    /// The current estimate, or `None` if this device has never been measured.
    /// **A never-measured device has no estimate — it does not default to anything.**
    pub fn ms_per_layer_tok(&self) -> Option<f64> {
        self.ewma.value()
    }

    pub fn updates(&self) -> u32 {
        self.ewma.updates()
    }
}

/// The cluster's measured capability set — the scheduler's first input.
#[derive(Debug, Default, Clone)]
pub struct CapabilityRegistry {
    devices: BTreeMap<String, DeviceCapability>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&mut self, device: &str, arch: &str, m: Measurement) {
        self.devices
            .entry(device.to_string())
            .or_insert_with(|| DeviceCapability::new(device, arch))
            .observe(m);
    }

    pub fn get(&self, device: &str) -> Option<&DeviceCapability> {
        self.devices.get(device)
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// Devices that have a real estimate, in a deterministic order (device name).
    /// A device that has never been measured is **omitted**, not defaulted — an unmeasured node
    /// must never be handed layers on the strength of a guess.
    pub fn measured(&self) -> Vec<(&str, f64)> {
        self.devices
            .values()
            .filter_map(|d| d.ms_per_layer_tok().map(|v| (d.device.as_str(), v)))
            .collect()
    }

    /// Capability ratios normalised so the **slowest** measured device is `1.0` — the form P1·2
    /// reports ("capability ratio 4.0 : 2.1 : 1.0").
    pub fn ratios(&self) -> Vec<(&str, f64)> {
        let m = self.measured();
        let slowest = m.iter().map(|(_, v)| *v).fold(f64::MIN, f64::max);
        m.into_iter().map(|(d, v)| (d, slowest / v)).collect()
    }

    /// Allocate `n_layer` contiguous layers across the measured devices in proportion to
    /// throughput (`1 / ms_per_layer_tok`), so every stage takes roughly equal wall-time.
    ///
    /// Largest-remainder allocation, so the shares sum to **exactly** `n_layer` — the pipeline must
    /// cover the model with no layer dropped and none run twice. Every device gets **at least one
    /// layer**; a device too slow to earn one is better excluded by the caller than given zero.
    ///
    /// This is the straightforward proportional split, and it is deliberately **not** the M3
    /// placement solver: P2·3 searches over splits with link costs and memory limits and is
    /// validated to within 15 % of brute-force TPOT. This is the baseline that reproduces P1·2's
    /// recorded decision, and the fixture P2·3 will be checked against.
    pub fn layer_shares(&self, n_layer: u32) -> Vec<(&str, u32)> {
        let m = self.measured();
        if m.is_empty() || n_layer == 0 {
            return Vec::new();
        }
        let k = m.len() as u32;
        if n_layer <= k {
            // Fewer layers than devices: give one each, in order, and stop.
            return m.iter().take(n_layer as usize).map(|(d, _)| (*d, 1u32)).collect();
        }
        // PURE largest-remainder over the whole model. (An earlier version reserved one layer per
        // device up front and split only the remainder — that skews the result away from true
        // proportionality and did NOT reproduce P1·2's deployed 14/7/3 split. Allocate
        // proportionally first, then repair starvation.)
        let total_tput: f64 = m.iter().map(|(_, v)| 1.0 / v).sum();
        let exact: Vec<f64> = m.iter().map(|(_, v)| (1.0 / v) / total_tput * n_layer as f64).collect();
        let mut alloc: Vec<u32> = exact.iter().map(|e| e.floor() as u32).collect();
        let mut left = n_layer - alloc.iter().sum::<u32>();
        // Largest remainder first; ties broken by index so the result is deterministic.
        let mut order: Vec<usize> = (0..m.len()).collect();
        order.sort_by(|&a, &b| {
            let (ra, rb) = (exact[a] - exact[a].floor(), exact[b] - exact[b].floor());
            rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal).then(a.cmp(&b))
        });
        for &i in order.iter().cycle().take(m.len() * 2) {
            if left == 0 {
                break;
            }
            alloc[i] += 1;
            left -= 1;
        }
        // Repair starvation: a zero-layer stage is a hop that computes nothing. Take from the
        // largest holder (which must have >= 2 by pigeonhole, since n_layer >= k). Deterministic:
        // lowest index among the maxima.
        while let Some(z) = alloc.iter().position(|a| *a == 0) {
            let donor = alloc
                .iter()
                .enumerate()
                .max_by_key(|(i, a)| (**a, std::cmp::Reverse(*i)))
                .map(|(i, _)| i)
                .expect("non-empty");
            debug_assert!(alloc[donor] >= 2, "pigeonhole guarantees a donor when n_layer >= devices");
            alloc[donor] -= 1;
            alloc[z] += 1;
        }
        m.iter().zip(alloc).map(|((d, _), a)| (*d, a)).collect()
    }

    /// The contiguous `[first, last)` ranges implied by [`Self::layer_shares`], in **registry
    /// order** (device name). The *shares* are what P2·1 establishes; which device sits at which
    /// pipeline position is a link-cost question and belongs to P2·2/P2·3 — do not read this
    /// ordering as a placement claim.
    pub fn layer_ranges(&self, n_layer: u32) -> Vec<(&str, u32, u32)> {
        let mut out = Vec::new();
        let mut cursor = 0u32;
        for (d, n) in self.layer_shares(n_layer) {
            out.push((d, cursor, cursor + n));
            cursor += n;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(vals: &[f64], window_s: f64) -> Vec<Sample> {
        vals.iter().map(|v| Sample::new(*v, window_s).unwrap()).collect()
    }

    fn run(vals: &[f64], window_s: f64, cfg: BenchConfig) -> Result<Measurement, BenchError> {
        let mut b = SustainedBench::new(cfg);
        for s in samples(vals, window_s) {
            b.push(s);
        }
        b.finish()
    }

    // ---------------------------------------------------------------- sample hygiene

    #[test]
    fn a_zero_or_nonfinite_sample_is_refused() {
        // A 0 ms/layer-token sample means "infinitely fast", which would win every layer in the
        // placement. It must never become a number in a table.
        assert_eq!(Sample::new(0.0, 1.0), Err(BenchError::BadSample(0.0)));
        assert!(matches!(Sample::new(-1.0, 1.0), Err(BenchError::BadSample(_))));
        assert!(matches!(Sample::new(f64::NAN, 1.0), Err(BenchError::BadSample(_))));
        assert!(matches!(Sample::new(f64::INFINITY, 1.0), Err(BenchError::BadSample(_))));
        assert!(matches!(Sample::new(1.0, 0.0), Err(BenchError::BadWindow(_))));
    }

    // ---------------------------------------------------------------- the sustained window

    #[test]
    fn an_under_length_run_is_refused_not_downgraded() {
        // 10 samples x 1s = 10s of post-warm-up data against a 30s requirement.
        let e = run(&[1.0; 12], 1.0, BenchConfig::default()).unwrap_err();
        assert!(matches!(e, BenchError::TooShort { .. }), "got {e}");
        // The message must say what was required — a refusal that does not say the bar is a puzzle.
        assert!(e.to_string().contains("30.0s"), "{e}");
    }

    #[test]
    fn a_sustained_run_reports_the_median_not_the_mean() {
        // One 10x spike must not drag the estimate: median 1.0, mean would be ~1.9.
        let cfg = BenchConfig { warmup_samples: 0, ..Default::default() };
        let m = run(&[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 10.0], 4.0, cfg).unwrap();
        assert_eq!(m.ms_per_layer_tok, 1.0);
        assert_eq!(m.samples_used, 10);
        assert_eq!(m.duration_s, 40.0);
    }

    #[test]
    fn warmup_samples_are_discarded_before_aggregation() {
        // Cold windows read SLOWER (page faults on fresh weights, cold KV). Counting them would
        // under-state the device and under-allocate its layers.
        let cfg = BenchConfig { warmup_samples: 2, ..Default::default() };
        let m = run(&[9.0, 9.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0], 4.0, cfg).unwrap();
        assert_eq!(m.ms_per_layer_tok, 2.0, "the two cold windows must not appear in the estimate");
        assert_eq!(m.warmup_discarded, 2);
        assert_eq!(m.samples_used, 10);
    }

    #[test]
    fn warmup_discard_that_eats_everything_is_an_error() {
        let cfg = BenchConfig { warmup_samples: 5, ..Default::default() };
        let e = run(&[1.0, 1.0, 1.0], 1.0, cfg).unwrap_err();
        assert_eq!(e, BenchError::NoSamplesAfterWarmup { discarded: 3 });
    }

    #[test]
    fn stability_is_reported_and_a_noisy_box_is_flagged() {
        let cfg = BenchConfig { warmup_samples: 0, ..Default::default() };
        let steady = run(&[2.00, 2.02, 1.99, 2.01, 2.00, 2.01, 1.98, 2.00], 5.0, cfg).unwrap();
        assert!(steady.is_stable(), "spread {} should be stable", steady.spread);

        // A box that swings 2x across the run — thermal throttling, or a burstable instance out of
        // credit. Still reported (it is real data) but explicitly not stable.
        let noisy = run(&[1.0, 2.0, 1.0, 2.0, 1.0, 2.0, 1.0, 2.0], 5.0, cfg).unwrap();
        assert!(!noisy.is_stable(), "spread {} should be flagged", noisy.spread);
    }

    #[test]
    fn should_stop_caps_the_startup_benchmark() {
        // The benchmark must not delay a session unboundedly.
        let mut b = SustainedBench::new(BenchConfig::default());
        for _ in 0..29 {
            b.push(Sample::new(1.0, 4.0).unwrap());
        }
        assert!(!b.should_stop(), "29 x 4s = 116s is under the 120s cap");
        b.push(Sample::new(1.0, 4.0).unwrap());
        assert!(b.should_stop(), "30 x 4s = 120s must hit the cap");
        assert!(b.elapsed_s() >= DEFAULT_MAX_DURATION_S);
    }

    // ---------------------------------------------------------------- EWMA

    #[test]
    fn ewma_seeds_on_first_update_and_never_reads_as_infinitely_fast() {
        let mut e = Ewma::default_alpha();
        assert_eq!(e.value(), None, "an unmeasured device has NO estimate");
        e.update(4.0);
        assert_eq!(e.value(), Some(4.0), "the first measurement seeds the EWMA outright");
        assert_eq!(e.updates(), 1);
    }

    #[test]
    fn ewma_tracks_a_changed_device_without_lurching() {
        let mut e = Ewma::default_alpha(); // alpha 0.3
        e.update(1.0);
        e.update(2.0);
        // 0.3*2.0 + 0.7*1.0 = 1.3 — moved toward the new value, nowhere near it.
        assert!((e.value().unwrap() - 1.3).abs() < 1e-12);
        for _ in 0..40 {
            e.update(2.0);
        }
        assert!((e.value().unwrap() - 2.0).abs() < 1e-6, "sustained change must converge");
    }

    #[test]
    fn ewma_rejects_an_out_of_range_alpha() {
        assert!(matches!(Ewma::new(0.0), Err(BenchError::BadAlpha(_))));
        assert!(matches!(Ewma::new(1.5), Err(BenchError::BadAlpha(_))));
        assert!(Ewma::new(1.0).is_ok(), "alpha=1 (take the latest) is legitimate");
    }

    // ---------------------------------------------------------------- registry

    #[test]
    fn an_unmeasured_device_is_omitted_never_defaulted() {
        let mut r = CapabilityRegistry::new();
        let m = run(&[1.0; 10], 4.0, BenchConfig { warmup_samples: 0, ..Default::default() }).unwrap();
        r.observe("mac", "aarch64", m);
        assert_eq!(r.measured().len(), 1);
        assert!(r.get("never-benchmarked").is_none());
        // And it wins no layers.
        assert_eq!(r.layer_shares(24).len(), 1);
    }

    // ------------------------------------------------- P1·2 real-hardware fixtures
    // docs/heterogeneity.md, 2026-07-19, Qwen2.5-0.5B fp16, CPU backend, 3 real nodes.

    /// The recorded ms/layer-token for the real 3-node set.
    fn p1_2_registry() -> CapabilityRegistry {
        let cfg = BenchConfig { warmup_samples: 0, ..Default::default() };
        let mut r = CapabilityRegistry::new();
        for (dev, arch, ms) in [("mac", "aarch64", 1.00), ("myvm-2", "x86_64", 1.89), ("myvm-1", "x86_64", 4.02)] {
            r.observe(dev, arch, run(&[ms; 10], 4.0, cfg).unwrap());
        }
        r
    }

    #[test]
    fn p1_2_fixture_reproduces_the_recorded_capability_ratio() {
        let r = p1_2_registry();
        let ratios: BTreeMap<&str, f64> = r.ratios().into_iter().collect();
        // docs/heterogeneity.md records "capability ratio 4.0 : 2.1 : 1.0" (Mac : myVm-2 : myVm-1).
        assert!((ratios["mac"] - 4.02).abs() < 0.01, "mac {}", ratios["mac"]);
        assert!((ratios["myvm-2"] - 2.13).abs() < 0.01, "myvm-2 {}", ratios["myvm-2"]);
        assert!((ratios["myvm-1"] - 1.00).abs() < 0.01, "myvm-1 {}", ratios["myvm-1"]);
    }

    #[test]
    fn p1_2_fixture_reproduces_the_recorded_placement_decision() {
        // THE fixture test for P2·1: fed the real recorded numbers, the pure allocator must produce
        // the split that was actually deployed on real hardware and is banked in
        // docs/heterogeneity.md and docs/wan-run.md — Mac [0,14) / myVm-2 [14,21) / myVm-1 [21,24).
        let r = p1_2_registry();
        let shares: BTreeMap<&str, u32> = r.layer_shares(24).into_iter().collect();
        assert_eq!(shares["mac"], 14, "Mac's 56 % share of 24 layers");
        assert_eq!(shares["myvm-2"], 7, "myVm-2's 30 % share");
        assert_eq!(shares["myvm-1"], 3, "myVm-1's 14 % share");
        assert_eq!(shares.values().sum::<u32>(), 24, "the pipeline must cover the model exactly");
    }

    #[test]
    fn layer_ranges_are_contiguous_and_cover_the_model_exactly() {
        let r = p1_2_registry();
        let ranges = r.layer_ranges(24);
        let mut cursor = 0;
        for (_, first, last) in &ranges {
            assert_eq!(*first, cursor, "ranges must be contiguous — no layer skipped");
            assert!(last > first, "no empty stage");
            cursor = *last;
        }
        assert_eq!(cursor, 24, "the last range must end at n_layer — no layer dropped or doubled");
    }

    #[test]
    fn allocation_sums_exactly_and_never_starves_a_stage() {
        // Largest-remainder must be exact for awkward layer counts, and every device keeps >= 1
        // layer — a zero-layer stage is a pipeline stage that does nothing but add a hop.
        let r = p1_2_registry();
        for n in 3u32..=80 {
            let shares = r.layer_shares(n);
            assert_eq!(shares.len(), 3, "n={n}");
            assert_eq!(shares.iter().map(|(_, v)| *v).sum::<u32>(), n, "n={n} must sum exactly");
            assert!(shares.iter().all(|(_, v)| *v >= 1), "n={n} starved a stage");
        }
    }

    #[test]
    fn fewer_layers_than_devices_gives_one_each_and_stops() {
        let r = p1_2_registry();
        let shares = r.layer_shares(2);
        assert_eq!(shares.len(), 2);
        assert!(shares.iter().all(|(_, v)| *v == 1));
    }

    #[test]
    fn a_device_that_gets_slower_loses_layers_after_re_measurement() {
        // The point of the EWMA: placement tracks reality. Sustained degradation on the Mac (say a
        // thermal ceiling) must move layers away from it, not be averaged into invisibility.
        let cfg = BenchConfig { warmup_samples: 0, ..Default::default() };
        let mut r = p1_2_registry();
        let before = r.layer_shares(24).into_iter().collect::<BTreeMap<_, _>>()["mac"];
        for _ in 0..20 {
            r.observe("mac", "aarch64", run(&[6.0; 10], 4.0, cfg).unwrap());
        }
        let after = r.layer_shares(24).into_iter().collect::<BTreeMap<_, _>>()["mac"];
        assert!(after < before, "mac degraded 1.00 -> 6.00 ms/layer-tok but kept {after} of {before} layers");
    }
}
