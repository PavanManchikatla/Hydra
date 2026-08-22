//! P2·2 — **link prober and contention-group discovery.**
//!
//! P2·1 measures what each device can *compute*. This measures what it costs to move a **boundary
//! residual** between them, which is the other half of the placement solver's cost model: a stage
//! boundary crossing costs `rtt + bytes / throughput`, and P1·1b's real WAN runs were explicitly
//! **latency-bound** (12 tokens in 10.73 s ≈ 1.12 tok/s on a 0.5 B model — `docs/wan-run.md`), so
//! on a real heterogeneous cluster the link term can dominate the compute term entirely.
//!
//! Two things are modelled:
//!
//! 1. **The full-mesh matrix.** Every *ordered* pair is its own measurement. Links are **not
//!    symmetric** — a home uplink is routinely an order of magnitude slower than its downlink, and
//!    averaging the two directions would hide exactly the asymmetry that decides which end of a
//!    pipeline a stage belongs on.
//!
//! 2. **Contention groups.** Two links that look independently fast can share a bottleneck (one
//!    uplink, one Wi-Fi radio, one VNet gateway). Probing them together and comparing against
//!    their solo throughput is what reveals it. This matters because the solver's whole premise is
//!    that stage times add up predictably; a shared bottleneck it cannot see makes every estimate
//!    optimistic **at runtime**, which is when it is expensive to discover.
//!
//! **The conservative bias, stated on purpose.** The two errors are not symmetric. Declaring
//! contention that does not exist costs some parallelism. *Missing* contention oversubscribes a
//! shared bottleneck and makes the placement wrong in production. So when the evidence is
//! ambiguous this module declares contention. That is a deliberate asymmetry, not a tuning
//! accident — see [`ContentionConfig`].
//!
//! Pure, like the rest of `hydra-sched`: probe results are handed in, so discovery is
//! deterministic and testable with no network.

use std::collections::{BTreeMap, BTreeSet};

use crate::capability::Ewma;

/// Concurrent throughput at or below this fraction of solo throughput ⇒ the links contend.
/// 0.8 means "losing a fifth of your throughput when a sibling link is active is contention".
pub const DEFAULT_CONTENTION_RATIO: f64 = 0.8;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum LinkError {
    #[error("a link from a device to itself is not a link ({0})")]
    SelfLink(String),
    #[error("rtt {0} ms is not a positive finite number")]
    BadRtt(f64),
    #[error("throughput {0} bytes/s is not a positive finite number — a zero would read as an infinitely slow link, an infinity as a free one")]
    BadThroughput(f64),
}

/// A directed link between two devices. Ordered: `a -> b` is not `b -> a`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LinkId {
    pub from: String,
    pub to: String,
}

impl LinkId {
    pub fn new(from: &str, to: &str) -> Result<LinkId, LinkError> {
        if from == to {
            return Err(LinkError::SelfLink(from.to_string()));
        }
        Ok(LinkId { from: from.to_string(), to: to.to_string() })
    }
}

impl std::fmt::Display for LinkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}->{}", self.from, self.to)
    }
}

/// One probe result for one directed link.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkSample {
    pub rtt_ms: f64,
    pub bytes_per_s: f64,
}

impl LinkSample {
    pub fn new(rtt_ms: f64, bytes_per_s: f64) -> Result<LinkSample, LinkError> {
        if !rtt_ms.is_finite() || rtt_ms <= 0.0 {
            return Err(LinkError::BadRtt(rtt_ms));
        }
        if !bytes_per_s.is_finite() || bytes_per_s <= 0.0 {
            return Err(LinkError::BadThroughput(bytes_per_s));
        }
        Ok(LinkSample { rtt_ms, bytes_per_s })
    }
}

/// A link's tracked estimate. Both terms are EWMA'd, so a link that degrades is followed.
#[derive(Debug, Clone)]
pub struct LinkEstimate {
    rtt: Ewma,
    tput: Ewma,
}

impl Default for LinkEstimate {
    fn default() -> Self {
        LinkEstimate { rtt: Ewma::default_alpha(), tput: Ewma::default_alpha() }
    }
}

impl LinkEstimate {
    pub fn observe(&mut self, s: LinkSample) {
        self.rtt.update(s.rtt_ms);
        self.tput.update(s.bytes_per_s);
    }
    pub fn rtt_ms(&self) -> Option<f64> {
        self.rtt.value()
    }
    pub fn bytes_per_s(&self) -> Option<f64> {
        self.tput.value()
    }
    /// Cost in milliseconds to move `bytes` across this link: latency plus transfer.
    /// This is the number a boundary crossing actually costs the pipeline.
    pub fn cost_ms(&self, bytes: u64) -> Option<f64> {
        match (self.rtt.value(), self.tput.value()) {
            (Some(r), Some(t)) => Some(r + (bytes as f64 / t) * 1000.0),
            _ => None,
        }
    }
}

/// How aggressively contention is inferred.
#[derive(Debug, Clone, Copy)]
pub struct ContentionConfig {
    /// Concurrent/solo throughput ratio at or below which two links are called contending.
    pub ratio: f64,
    /// **When a pair was never probed concurrently, assume they contend.** Deliberately
    /// conservative: an unobserved pair is unknown, and treating unknown as independent is the
    /// error that bites in production. Set `false` only when the mesh has been probed exhaustively.
    pub assume_unprobed_contends: bool,
}

impl Default for ContentionConfig {
    fn default() -> Self {
        ContentionConfig { ratio: DEFAULT_CONTENTION_RATIO, assume_unprobed_contends: true }
    }
}

/// The cluster's link picture — the scheduler's second input.
#[derive(Debug, Default, Clone)]
pub struct LinkMatrix {
    links: BTreeMap<LinkId, LinkEstimate>,
    /// Solo throughput per link, kept separately from the EWMA so contention comparisons are made
    /// against a like-for-like baseline rather than against a number the concurrent probes moved.
    solo: BTreeMap<LinkId, f64>,
    /// Observed concurrent throughput for an unordered pair of links, keyed canonically.
    concurrent: BTreeMap<(LinkId, LinkId), (f64, f64)>,
}

impl LinkMatrix {
    pub fn new() -> Self {
        Self::default()
    }

    /// The full-mesh probe plan: every ordered pair, deterministically ordered. `n(n-1)` probes.
    pub fn probe_plan(devices: &[&str]) -> Vec<LinkId> {
        let mut out = Vec::new();
        let mut sorted: Vec<&str> = devices.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        for a in &sorted {
            for b in &sorted {
                if a != b {
                    out.push(LinkId { from: a.to_string(), to: b.to_string() });
                }
            }
        }
        out
    }

    /// Record a **solo** probe (this link measured alone).
    pub fn observe_solo(&mut self, link: &LinkId, s: LinkSample) {
        self.links.entry(link.clone()).or_default().observe(s);
        self.solo.insert(link.clone(), s.bytes_per_s);
    }

    /// Record a **concurrent** probe: both links driven at once.
    pub fn observe_concurrent(&mut self, a: &LinkId, b: &LinkId, a_bytes_per_s: f64, b_bytes_per_s: f64) {
        let key = if a <= b { (a.clone(), b.clone()) } else { (b.clone(), a.clone()) };
        let val = if a <= b { (a_bytes_per_s, b_bytes_per_s) } else { (b_bytes_per_s, a_bytes_per_s) };
        self.concurrent.insert(key, val);
    }

    pub fn get(&self, link: &LinkId) -> Option<&LinkEstimate> {
        self.links.get(link)
    }

    /// Cost of moving `bytes` across a link. `None` for a link never probed — **an unmeasured link
    /// has no cost estimate and must never be treated as free.**
    pub fn cost_ms(&self, link: &LinkId, bytes: u64) -> Option<f64> {
        self.links.get(link).and_then(|e| e.cost_ms(bytes))
    }

    pub fn measured(&self) -> Vec<&LinkId> {
        self.links.keys().collect()
    }

    /// Discover contention groups over the measured links.
    ///
    /// Two links contend when their concurrent throughput falls to `ratio` or less of solo on
    /// **either** side — a shared bottleneck usually starves one direction first, and requiring
    /// both to degrade would miss it. Groups are the transitive closure (union-find): if A
    /// contends with B and B with C, all three share a bottleneck for scheduling purposes.
    pub fn contention_groups(&self, cfg: ContentionConfig) -> Vec<BTreeSet<LinkId>> {
        let links: Vec<&LinkId> = self.links.keys().collect();
        let n = links.len();
        let idx: BTreeMap<&LinkId, usize> = links.iter().enumerate().map(|(i, l)| (*l, i)).collect();
        let mut parent: Vec<usize> = (0..n).collect();

        fn find(parent: &mut [usize], mut x: usize) -> usize {
            while parent[x] != x {
                parent[x] = parent[parent[x]];
                x = parent[x];
            }
            x
        }

        for i in 0..n {
            for j in (i + 1)..n {
                let (a, b) = (links[i], links[j]);
                let key = if a <= b { (a.clone(), b.clone()) } else { (b.clone(), a.clone()) };
                let contends = match self.concurrent.get(&key) {
                    Some((ta, tb)) => {
                        // Map the stored pair back to (a, b) order.
                        let (ta, tb) = if a <= b { (*ta, *tb) } else { (*tb, *ta) };
                        let degraded = |link: &LinkId, conc: f64| match self.solo.get(link) {
                            // No solo baseline ⇒ cannot prove independence ⇒ conservative.
                            None => cfg.assume_unprobed_contends,
                            Some(solo) => conc <= solo * cfg.ratio,
                        };
                        degraded(a, ta) || degraded(b, tb)
                    }
                    // Never probed together: unknown, and unknown is treated as contending.
                    None => cfg.assume_unprobed_contends,
                };
                if contends {
                    let (ra, rb) = (find(&mut parent, idx[a]), find(&mut parent, idx[b]));
                    if ra != rb {
                        parent[ra] = rb;
                    }
                }
            }
        }

        let mut groups: BTreeMap<usize, BTreeSet<LinkId>> = BTreeMap::new();
        for (i, l) in links.iter().enumerate() {
            let r = find(&mut parent, i);
            groups.entry(r).or_default().insert((*l).clone());
        }
        groups.into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(a: &str, b: &str) -> LinkId {
        LinkId::new(a, b).unwrap()
    }
    const MB: f64 = 1_000_000.0;

    #[test]
    fn a_self_link_is_refused() {
        assert!(matches!(LinkId::new("mac", "mac"), Err(LinkError::SelfLink(_))));
    }

    #[test]
    fn degenerate_samples_are_refused() {
        // Zero throughput would read as an infinitely slow link; infinity as a free one. Both
        // would silently rewrite the solver's cost model.
        assert!(matches!(LinkSample::new(1.0, 0.0), Err(LinkError::BadThroughput(_))));
        assert!(matches!(LinkSample::new(1.0, f64::INFINITY), Err(LinkError::BadThroughput(_))));
        assert!(matches!(LinkSample::new(0.0, MB), Err(LinkError::BadRtt(_))));
        assert!(matches!(LinkSample::new(f64::NAN, MB), Err(LinkError::BadRtt(_))));
    }

    #[test]
    fn the_probe_plan_is_the_full_ordered_mesh_and_deterministic() {
        let plan = LinkMatrix::probe_plan(&["mac", "vm1", "vm2"]);
        assert_eq!(plan.len(), 6, "n(n-1) ordered pairs, both directions");
        assert_eq!(plan, LinkMatrix::probe_plan(&["vm2", "mac", "vm1"]), "order of input must not matter");
        assert!(plan.iter().all(|p| p.from != p.to));
    }

    #[test]
    fn an_unmeasured_link_has_no_cost_and_is_never_free() {
        let m = LinkMatrix::new();
        assert_eq!(m.cost_ms(&l("mac", "vm1"), 1024), None, "an unprobed link must not read as free");
    }

    #[test]
    fn link_direction_is_not_averaged_away() {
        // A real home uplink: fast down, slow up. Averaging would hide which end of the pipeline
        // a stage belongs on.
        let mut m = LinkMatrix::new();
        m.observe_solo(&l("mac", "vm1"), LinkSample::new(20.0, 1.0 * MB).unwrap()); // slow up
        m.observe_solo(&l("vm1", "mac"), LinkSample::new(20.0, 20.0 * MB).unwrap()); // fast down
        let up = m.cost_ms(&l("mac", "vm1"), 10_000_000).unwrap();
        let down = m.cost_ms(&l("vm1", "mac"), 10_000_000).unwrap();
        assert!(up > down * 5.0, "asymmetry must survive: up {up:.0} ms vs down {down:.0} ms");
    }

    #[test]
    fn cost_is_latency_plus_transfer() {
        let mut m = LinkMatrix::new();
        m.observe_solo(&l("a", "b"), LinkSample::new(10.0, 1.0 * MB).unwrap());
        // 1 MB at 1 MB/s = 1000 ms, plus 10 ms rtt.
        let c = m.cost_ms(&l("a", "b"), 1_000_000).unwrap();
        assert!((c - 1010.0).abs() < 1e-6, "got {c}");
    }

    #[test]
    fn a_small_boundary_on_a_wan_link_is_latency_dominated() {
        // The P1·1b WAN runs were explicitly latency-bound. A 0.5B boundary residual is ~3.5 KB
        // (896 dims x f32); on a Tailscale WAN leg the RTT term must dominate the transfer term,
        // which is exactly why the solver cannot use bandwidth alone.
        let mut m = LinkMatrix::new();
        m.observe_solo(&l("mac", "vm1"), LinkSample::new(25.0, 10.0 * MB).unwrap());
        let bytes = 896 * 4;
        let cost = m.cost_ms(&l("mac", "vm1"), bytes).unwrap();
        let transfer = (bytes as f64 / (10.0 * MB)) * 1000.0;
        assert!(transfer < 1.0 && cost > 25.0, "transfer {transfer:.3} ms vs total {cost:.1} ms");
    }

    // ---------------------------------------------------------------- contention

    #[test]
    fn independent_links_are_not_grouped() {
        let mut m = LinkMatrix::new();
        let (a, b) = (l("vm1", "vm2"), l("vm2", "vm1"));
        m.observe_solo(&a, LinkSample::new(1.0, 100.0 * MB).unwrap());
        m.observe_solo(&b, LinkSample::new(1.0, 100.0 * MB).unwrap());
        // Concurrently they keep essentially all their throughput — a full-duplex VNet leg.
        m.observe_concurrent(&a, &b, 99.0 * MB, 98.0 * MB);
        let g = m.contention_groups(ContentionConfig::default());
        assert_eq!(g.len(), 2, "independent links must stay in their own groups: {g:?}");
    }

    #[test]
    fn links_sharing_an_uplink_are_grouped() {
        // The real shape from docs/wan-run.md: the Mac reaches both VMs over one WAN uplink.
        // Each looks fine alone; together each gets about half.
        let mut m = LinkMatrix::new();
        let (a, b) = (l("mac", "vm1"), l("mac", "vm2"));
        m.observe_solo(&a, LinkSample::new(25.0, 10.0 * MB).unwrap());
        m.observe_solo(&b, LinkSample::new(30.0, 10.0 * MB).unwrap());
        m.observe_concurrent(&a, &b, 5.0 * MB, 5.0 * MB);
        let g = m.contention_groups(ContentionConfig::default());
        assert_eq!(g.len(), 1, "both links share the uplink and must be one group");
        assert_eq!(g[0].len(), 2);
    }

    #[test]
    fn contention_is_declared_when_either_direction_degrades() {
        // A shared bottleneck often starves one side first. Requiring BOTH to degrade would miss it.
        let mut m = LinkMatrix::new();
        let (a, b) = (l("mac", "vm1"), l("mac", "vm2"));
        m.observe_solo(&a, LinkSample::new(25.0, 10.0 * MB).unwrap());
        m.observe_solo(&b, LinkSample::new(25.0, 10.0 * MB).unwrap());
        m.observe_concurrent(&a, &b, 2.0 * MB, 9.9 * MB); // only `a` collapses
        assert_eq!(m.contention_groups(ContentionConfig::default()).len(), 1);
    }

    #[test]
    fn an_unprobed_pair_is_assumed_to_contend() {
        // THE conservative bias, asserted explicitly. Missing contention oversubscribes a shared
        // bottleneck and makes the placement wrong in production; over-declaring merely costs some
        // parallelism. Unknown therefore means "contends".
        let mut m = LinkMatrix::new();
        let (a, b) = (l("mac", "vm1"), l("mac", "vm2"));
        m.observe_solo(&a, LinkSample::new(25.0, 10.0 * MB).unwrap());
        m.observe_solo(&b, LinkSample::new(25.0, 10.0 * MB).unwrap());
        // No concurrent probe at all.
        assert_eq!(m.contention_groups(ContentionConfig::default()).len(), 1, "unknown must be conservative");
        // And the opt-out exists for an exhaustively-probed mesh, but must be asked for.
        let permissive = ContentionConfig { assume_unprobed_contends: false, ..Default::default() };
        assert_eq!(m.contention_groups(permissive).len(), 2);
    }

    #[test]
    fn contention_groups_are_transitively_closed() {
        // A contends with B, B with C => all three share a bottleneck for scheduling purposes,
        // even though A and C were never seen to degrade each other.
        let mut m = LinkMatrix::new();
        let (a, b, c) = (l("mac", "vm1"), l("mac", "vm2"), l("mac", "vm3"));
        for x in [&a, &b, &c] {
            m.observe_solo(x, LinkSample::new(25.0, 10.0 * MB).unwrap());
        }
        m.observe_concurrent(&a, &b, 4.0 * MB, 4.0 * MB); // contend
        m.observe_concurrent(&b, &c, 4.0 * MB, 4.0 * MB); // contend
        m.observe_concurrent(&a, &c, 9.9 * MB, 9.9 * MB); // look independent
        let g = m.contention_groups(ContentionConfig { assume_unprobed_contends: false, ..Default::default() });
        assert_eq!(g.len(), 1, "transitive closure must merge all three: {g:?}");
        assert_eq!(g[0].len(), 3);
    }

    #[test]
    fn grouping_is_deterministic() {
        let mut m = LinkMatrix::new();
        let (a, b) = (l("mac", "vm1"), l("mac", "vm2"));
        m.observe_solo(&a, LinkSample::new(25.0, 10.0 * MB).unwrap());
        m.observe_solo(&b, LinkSample::new(25.0, 10.0 * MB).unwrap());
        m.observe_concurrent(&a, &b, 5.0 * MB, 5.0 * MB);
        let g1 = m.contention_groups(ContentionConfig::default());
        let g2 = m.contention_groups(ContentionConfig::default());
        assert_eq!(g1, g2);
    }

    #[test]
    fn a_degrading_link_is_followed_by_the_ewma() {
        let mut m = LinkMatrix::new();
        let a = l("mac", "vm1");
        m.observe_solo(&a, LinkSample::new(25.0, 10.0 * MB).unwrap());
        let before = m.cost_ms(&a, 10_000_000).unwrap();
        for _ in 0..30 {
            m.observe_solo(&a, LinkSample::new(120.0, 1.0 * MB).unwrap());
        }
        let after = m.cost_ms(&a, 10_000_000).unwrap();
        assert!(after > before * 5.0, "sustained degradation must move the cost: {before:.0} -> {after:.0} ms");
    }

    /// The recorded 3-node topology shape (docs/wan-run.md): Mac ↔ VMs over Tailscale WAN, VM ↔ VM
    /// over the cloud VNet fast path. The numbers are illustrative of the SHAPE — the run banked
    /// "latency-bound WAN" and "sub-ms VNet" qualitatively, never an RTT matrix — so this fixture
    /// asserts the ordering the solver must respect, not absolute values.
    #[test]
    fn wan_run_topology_shape_orders_the_legs_correctly() {
        let mut m = LinkMatrix::new();
        let vnet = l("vm1", "vm2");
        let wan = l("mac", "vm1");
        m.observe_solo(&vnet, LinkSample::new(0.4, 100.0 * MB).unwrap()); // sub-ms VNet
        m.observe_solo(&wan, LinkSample::new(25.0, 10.0 * MB).unwrap()); // Tailscale WAN
        let bytes = 896 * 4; // one 0.5B boundary residual
        let (cv, cw) = (m.cost_ms(&vnet, bytes).unwrap(), m.cost_ms(&wan, bytes).unwrap());
        assert!(cw > cv * 10.0, "the WAN leg must cost far more per boundary: {cw:.1} vs {cv:.1} ms");
    }
}
