//! P2·4 — **admission control + KV reservation.**
//!
//! Answers exactly one question, and says so: **"does this session fit, with headroom?"**
//! It is **not** multi-tenancy. v1 runs one session per model instance (spec §12 reserves
//! multi-session for v2), so admission here is a *feasibility and headroom* gate, not a scheduler
//! of competing tenants. The machinery below is written so the v2 consumer can arrive without
//! redesign, but no claim is made that v1 does multi-tenancy.
//!
//! # Everything is computed from measured quantities
//!
//! Nothing here is a guess:
//!
//! * **Shard weight bytes** come from the real per-stage shard produced by P2·10a/b — the same
//!   splitter output the worker verifies and loads. Before sharded loading this number could not
//!   even be stated per device (every worker held the whole model); P2·10b is what made it real.
//! * **KV bytes per position** are *computed* from the model config —
//!   `2 (K and V) × layers_on_this_stage × n_kv_head × head_dim × bytes_per_elem` — and scale with
//!   the layers this stage actually hosts, not with the whole model.
//! * **Capability** is P2·1's sustained measurement; **link costs and contention groups** are
//!   P2·2's.
//!
//! # Headroom, and why refusal beats squeezing
//!
//! Admission enforces the §11 stability contract's headroom (numbers per the 2026-08-22 design
//! ruling, since §11's prose defers to v0.8 without restating them):
//!
//! * **memory: 15–30 %** kept free — KV grows with context length *during* a session, and a
//!   placement admitted at 100 % of RAM is one long prompt away from an OOM that costs the whole
//!   session, not a slow one;
//! * **compute/thermal: ≥ 20 %** — a device benchmarked at X does not sustain X under thermal
//!   load, so planning is done against a **derated** capability.
//!
//! A request that does not fit is **REFUSED with a structured reason naming the device, the
//! demand, the budget and the shortfall**. It is never squeezed in by trimming context or ignoring
//! headroom: silently admitting a session that will die mid-generation converts a clean, immediate
//! "no" into a corrupted user-visible failure, and this project's whole posture is that the
//! honest refusal is the better outcome.

use std::collections::BTreeMap;

use crate::link::{ContentionConfig, LinkId, LinkMatrix};

/// Fraction of memory kept free. The §11 band is 15–30 %; the default is the conservative end.
pub const DEFAULT_MEMORY_HEADROOM: f64 = 0.30;
pub const MEMORY_HEADROOM_MIN: f64 = 0.15;
pub const MEMORY_HEADROOM_MAX: f64 = 0.30;
/// Fraction of compute kept in reserve for thermal/contention slack (§11: at least 20 %).
pub const DEFAULT_COMPUTE_HEADROOM: f64 = 0.20;

/// What a stage's model config implies for KV growth. All fields come from the model, not guesses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KvGeometry {
    pub n_kv_head: u32,
    pub head_dim: u32,
    /// 2 for f16 KV (the engine default), 4 for f32.
    pub bytes_per_elem: u32,
}

impl KvGeometry {
    /// Bytes of KV cache one position costs on a stage hosting `layers` layers.
    /// `2` because both K and V are cached.
    pub fn bytes_per_position(&self, layers: u32) -> u64 {
        2 * layers as u64 * self.n_kv_head as u64 * self.head_dim as u64 * self.bytes_per_elem as u64
    }

    /// Bytes of KV for a full context window on this stage.
    pub fn bytes_for_context(&self, layers: u32, n_ctx: u32) -> u64 {
        self.bytes_per_position(layers) * n_ctx as u64
    }
}

/// One device's physical budget.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeviceBudget {
    pub total_memory_bytes: u64,
    /// Fraction of memory to keep free (clamped into the §11 band on construction).
    pub memory_headroom: f64,
}

impl DeviceBudget {
    pub fn new(total_memory_bytes: u64, memory_headroom: f64) -> DeviceBudget {
        DeviceBudget {
            total_memory_bytes,
            memory_headroom: memory_headroom.clamp(MEMORY_HEADROOM_MIN, MEMORY_HEADROOM_MAX),
        }
    }
    /// Memory a placement may actually plan to use.
    pub fn usable_memory_bytes(&self) -> u64 {
        (self.total_memory_bytes as f64 * (1.0 - self.memory_headroom)) as u64
    }
}

/// What one stage of a candidate placement demands of its device.
#[derive(Debug, Clone, PartialEq)]
pub struct StageDemand {
    pub device: String,
    pub layers: u32,
    /// The stage's shard file size — measured, from the P2·10 splitter output.
    pub shard_bytes: u64,
    /// KV for the full requested context on this stage's layers — computed from config.
    pub kv_bytes: u64,
}

impl StageDemand {
    pub fn total_bytes(&self) -> u64 {
        self.shard_bytes + self.kv_bytes
    }
}

/// A traffic class sharing contention-group airtime (§11: "contention-group airtime shared by all
/// traffic classes"). v1 has one session, so the demand is one session's worth — the *machinery*
/// is v1's, the multi-session consumer is v2's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TrafficClass {
    /// Per-token boundary residuals on the pipeline links, plus their durability copies.
    BoundaryCopy,
    /// Shard distribution / rebuild after a worker replacement. Bursty and large.
    ShardTransfer,
    /// Liveness traffic. Small, but must never be the thing that gets starved.
    Heartbeat,
}

/// Sustained bytes/second a traffic class needs across a link.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassDemand {
    pub link: LinkId,
    pub class: TrafficClass,
    pub bytes_per_s: f64,
}

/// Why a session was refused. Every variant names the numbers, because a refusal that does not say
/// what would have fitted is not actionable.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Refusal {
    #[error("device {device} has no budget declared — an undeclared device is never admitted on optimism")]
    NoBudget { device: String },
    #[error(
        "device {device}: needs {demand_bytes} B (shard {shard_bytes} + KV {kv_bytes}) but only \
         {usable_bytes} B is usable of {total_bytes} B total ({headroom_pct:.0}% headroom reserved) \
         — short by {shortfall_bytes} B"
    )]
    Memory {
        device: String,
        demand_bytes: u64,
        shard_bytes: u64,
        kv_bytes: u64,
        usable_bytes: u64,
        total_bytes: u64,
        headroom_pct: f64,
        shortfall_bytes: u64,
    },
    #[error(
        "device {device}: capability measurement is UNSTABLE (spread {spread_pct:.1}%) — planning \
         against a number the box does not hold is how a placement dies mid-session; re-measure, or \
         admit explicitly with allow_unstable"
    )]
    UnstableCapability { device: String, spread_pct: f64 },
    #[error(
        "contention group [{group}]: traffic classes demand {demand_bytes_per_s:.0} B/s but the \
         group's shared budget after {headroom_pct:.0}% headroom is {budget_bytes_per_s:.0} B/s"
    )]
    ContentionGroup { group: String, demand_bytes_per_s: f64, budget_bytes_per_s: f64, headroom_pct: f64 },
    #[error("link {link} carries traffic but was never probed — an unmeasured link is never assumed free")]
    UnpricedLink { link: LinkId },
}

/// The result of an admission check. Deliberately not a bool: an admitted session carries the
/// reservation that was made on its behalf, so the caller holds the numbers it was admitted under.
#[derive(Debug, Clone, PartialEq)]
pub struct Admitted {
    /// Per-device bytes reserved.
    pub reserved: BTreeMap<String, u64>,
    /// Per-device memory utilisation after admission, as a fraction of TOTAL (not usable).
    pub memory_utilisation: BTreeMap<String, f64>,
}

/// Admission inputs.
pub struct AdmissionRequest<'a> {
    pub stages: &'a [StageDemand],
    pub budgets: &'a BTreeMap<String, DeviceBudget>,
    /// P2·1 stability per device: `Some(spread)` when measured. An UNSTABLE device is refused
    /// unless `allow_unstable`.
    pub capability_spread: &'a BTreeMap<String, f64>,
    pub allow_unstable: bool,
    pub links: &'a LinkMatrix,
    pub class_demands: &'a [ClassDemand],
    pub contention: ContentionConfig,
    /// Fraction of a contention group's shared capacity kept in reserve (§11: ≥ 20 %).
    pub compute_headroom: f64,
}

/// Decide. Returns the reservation, or **every** reason it was refused — a caller fixing one
/// shortfall should not have to re-run to discover the next.
pub fn admit(req: &AdmissionRequest) -> Result<Admitted, Vec<Refusal>> {
    let mut refusals = Vec::new();
    let mut reserved: BTreeMap<String, u64> = BTreeMap::new();
    let mut utilisation: BTreeMap<String, f64> = BTreeMap::new();

    // ---- per-device memory, including KV for the FULL requested context ----
    for st in req.stages {
        let Some(budget) = req.budgets.get(&st.device) else {
            refusals.push(Refusal::NoBudget { device: st.device.clone() });
            continue;
        };
        // P2·1's stability flag gets a real consequence here rather than being decoration.
        if let Some(spread) = req.capability_spread.get(&st.device) {
            if *spread > crate::capability::STABILITY_THRESHOLD && !req.allow_unstable {
                refusals.push(Refusal::UnstableCapability {
                    device: st.device.clone(),
                    spread_pct: spread * 100.0,
                });
            }
        }
        let demand = st.total_bytes();
        let usable = budget.usable_memory_bytes();
        if demand > usable {
            refusals.push(Refusal::Memory {
                device: st.device.clone(),
                demand_bytes: demand,
                shard_bytes: st.shard_bytes,
                kv_bytes: st.kv_bytes,
                usable_bytes: usable,
                total_bytes: budget.total_memory_bytes,
                headroom_pct: budget.memory_headroom * 100.0,
                shortfall_bytes: demand - usable,
            });
        } else {
            reserved.insert(st.device.clone(), demand);
            utilisation.insert(st.device.clone(), demand as f64 / budget.total_memory_bytes as f64);
        }
    }

    // ---- contention-group airtime, shared across all traffic classes ----
    //
    // This is where P2·2's contention groups finally get a consumer. A group's capacity is the
    // MINIMUM throughput among its member links, never their sum: the whole meaning of a shared
    // bottleneck is that the members cannot all run at their solo rate at once. Summing would
    // reintroduce exactly the optimism the grouping exists to prevent.
    // Any declared traffic on a link with no estimate is refused outright, whether or not that
    // link landed in a discovered group. Grouping is not the gate — being priced is. A demand on
    // an unprobed link would otherwise contribute to no group's budget and pass as free traffic,
    // which is the precise optimism P2·2 exists to prevent.
    for d in req.class_demands {
        if d.bytes_per_s > 0.0 && req.links.get(&d.link).and_then(|e| e.bytes_per_s()).is_none() {
            refusals.push(Refusal::UnpricedLink { link: d.link.clone() });
        }
    }

    let groups = req.links.contention_groups(req.contention);
    for group in &groups {
        let mut capacity = f64::MAX;
        let mut unpriced = false;
        for link in group {
            match req.links.get(link).and_then(|e| e.bytes_per_s()) {
                Some(t) => capacity = capacity.min(t),
                None => {
                    // Only matters if this group actually carries demand; checked below.
                    unpriced = true;
                }
            }
        }
        let demand: f64 = req
            .class_demands
            .iter()
            .filter(|d| group.contains(&d.link))
            .map(|d| d.bytes_per_s)
            .sum();
        if demand <= 0.0 {
            continue; // an idle group constrains nothing
        }
        if unpriced || capacity == f64::MAX {
            if let Some(d) = req.class_demands.iter().find(|d| {
                group.contains(&d.link) && req.links.get(&d.link).and_then(|e| e.bytes_per_s()).is_none()
            }) {
                refusals.push(Refusal::UnpricedLink { link: d.link.clone() });
            }
            continue;
        }
        let budget = capacity * (1.0 - req.compute_headroom);
        if demand > budget {
            let names: Vec<String> = group.iter().map(|l| l.to_string()).collect();
            refusals.push(Refusal::ContentionGroup {
                group: names.join(", "),
                demand_bytes_per_s: demand,
                budget_bytes_per_s: budget,
                headroom_pct: req.compute_headroom * 100.0,
            });
        }
    }

    if refusals.is_empty() {
        Ok(Admitted { reserved, memory_utilisation: utilisation })
    } else {
        Err(refusals)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::LinkSample;

    const MB: u64 = 1_000_000;
    const MBF: f64 = 1_000_000.0;

    /// Qwen2.5-0.5B: 24 layers, 2 KV heads, head_dim 64, f16 KV — the real dev model's geometry.
    fn qwen_kv() -> KvGeometry {
        KvGeometry { n_kv_head: 2, head_dim: 64, bytes_per_elem: 2 }
    }

    fn budgets(pairs: &[(&str, u64)]) -> BTreeMap<String, DeviceBudget> {
        pairs
            .iter()
            .map(|(d, b)| (d.to_string(), DeviceBudget::new(*b, DEFAULT_MEMORY_HEADROOM)))
            .collect()
    }

    fn req<'a>(
        stages: &'a [StageDemand],
        budgets: &'a BTreeMap<String, DeviceBudget>,
        links: &'a LinkMatrix,
        spread: &'a BTreeMap<String, f64>,
        demands: &'a [ClassDemand],
    ) -> AdmissionRequest<'a> {
        AdmissionRequest {
            stages,
            budgets,
            capability_spread: spread,
            allow_unstable: false,
            links,
            class_demands: demands,
            contention: ContentionConfig::default(),
            compute_headroom: DEFAULT_COMPUTE_HEADROOM,
        }
    }

    // ---------------------------------------------------------------- KV math

    #[test]
    fn kv_bytes_are_computed_from_config_not_guessed() {
        let g = qwen_kv();
        // 2 (K+V) x 12 layers x 2 kv-heads x 64 dim x 2 bytes = 6144 B per position.
        assert_eq!(g.bytes_per_position(12), 6_144);
        // A 4096-position context on those 12 layers.
        assert_eq!(g.bytes_for_context(12, 4096), 6_144 * 4096);
        // And it scales with THIS STAGE's layers, not the whole model.
        assert_eq!(g.bytes_per_position(24), 2 * g.bytes_per_position(12));
    }

    #[test]
    fn kv_grows_with_context_which_is_why_headroom_exists() {
        let g = qwen_kv();
        let short = g.bytes_for_context(12, 512);
        let long = g.bytes_for_context(12, 8192);
        assert_eq!(long, short * 16, "KV is linear in context — admission must price the FULL window");
    }

    // ---------------------------------------------------------------- headroom

    #[test]
    fn the_memory_headroom_band_is_clamped_to_the_stability_contract() {
        // §11's band is 15-30%. A caller cannot opt out by passing 0.
        assert_eq!(DeviceBudget::new(1000, 0.0).memory_headroom, MEMORY_HEADROOM_MIN);
        assert_eq!(DeviceBudget::new(1000, 0.9).memory_headroom, MEMORY_HEADROOM_MAX);
        assert_eq!(DeviceBudget::new(1000, 0.2).memory_headroom, 0.2);
    }

    #[test]
    fn a_session_that_fits_raw_capacity_but_not_headroom_is_refused() {
        // THE headroom test. 700 MB demand on an 800 MB device: it "fits" — and is refused,
        // because at 30% headroom only 560 MB may be planned. Admitting it would leave a session
        // one long prompt away from an OOM that costs the whole session.
        let stages = vec![StageDemand {
            device: "mac".into(),
            layers: 12,
            shard_bytes: 600 * MB,
            kv_bytes: 100 * MB,
        }];
        let b = budgets(&[("mac", 800 * MB)]);
        let (links, spread, demands) = (LinkMatrix::new(), BTreeMap::new(), vec![]);
        let e = admit(&req(&stages, &b, &links, &spread, &demands)).unwrap_err();
        assert_eq!(e.len(), 1);
        match &e[0] {
            Refusal::Memory { demand_bytes, usable_bytes, shortfall_bytes, .. } => {
                assert_eq!(*demand_bytes, 700 * MB);
                assert_eq!(*usable_bytes, 560 * MB);
                assert_eq!(*shortfall_bytes, 140 * MB);
            }
            other => panic!("expected a Memory refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_refusal_names_the_device_the_demand_and_the_shortfall() {
        // A refusal that does not say what would have fitted is not actionable.
        let stages = vec![StageDemand { device: "myvm-1".into(), layers: 12, shard_bytes: 600 * MB, kv_bytes: 100 * MB }];
        let b = budgets(&[("myvm-1", 800 * MB)]);
        let (links, spread, demands) = (LinkMatrix::new(), BTreeMap::new(), vec![]);
        let e = admit(&req(&stages, &b, &links, &spread, &demands)).unwrap_err();
        let msg = e[0].to_string();
        for needle in ["myvm-1", "700000000", "560000000", "140000000", "30%"] {
            assert!(msg.contains(needle), "refusal must state {needle}: {msg}");
        }
    }

    #[test]
    fn an_undeclared_device_is_never_admitted_on_optimism() {
        let stages = vec![StageDemand { device: "ghost".into(), layers: 12, shard_bytes: 1, kv_bytes: 1 }];
        let b = budgets(&[("mac", 8_000 * MB)]);
        let (links, spread, demands) = (LinkMatrix::new(), BTreeMap::new(), vec![]);
        let e = admit(&req(&stages, &b, &links, &spread, &demands)).unwrap_err();
        assert_eq!(e[0], Refusal::NoBudget { device: "ghost".into() });
    }

    #[test]
    fn admission_never_squeezes_it_only_refuses() {
        // There is deliberately no "trim the context to make it fit" path: the API cannot express
        // it. A caller that wants a smaller context must ASK for a smaller context, so the
        // reduction is the caller's explicit decision and not a silent one.
        let g = qwen_kv();
        let stages = vec![StageDemand {
            device: "mac".into(),
            layers: 12,
            shard_bytes: 601 * MB, // the real measured P2·10b shard
            kv_bytes: g.bytes_for_context(12, 100_000),
        }];
        let b = budgets(&[("mac", 800 * MB)]);
        let (links, spread, demands) = (LinkMatrix::new(), BTreeMap::new(), vec![]);
        let res = admit(&req(&stages, &b, &links, &spread, &demands));
        assert!(res.is_err(), "must refuse");
        // And the admitted-map is not partially populated with a trimmed reservation.
        assert!(matches!(res, Err(ref v) if v.iter().any(|r| matches!(r, Refusal::Memory { .. }))));
    }

    #[test]
    fn a_real_p2_10b_shard_fits_a_real_device_with_headroom() {
        // The measured 601.04 MiB stage-0 shard plus a 4k-context KV on 12 layers, on an 8 GB box.
        let g = qwen_kv();
        let stages = vec![StageDemand {
            device: "mac".into(),
            layers: 12,
            shard_bytes: 630_000_000,
            kv_bytes: g.bytes_for_context(12, 4096),
        }];
        let b = budgets(&[("mac", 8_000 * MB)]);
        let (links, spread, demands) = (LinkMatrix::new(), BTreeMap::new(), vec![]);
        let ok = admit(&req(&stages, &b, &links, &spread, &demands)).expect("should admit");
        assert!(ok.memory_utilisation["mac"] < 0.10, "plenty of room: {:?}", ok.memory_utilisation);
        assert_eq!(ok.reserved["mac"], 630_000_000 + 6_144 * 4096);
    }

    // ---------------------------------------------------------------- stability

    #[test]
    fn an_unstable_capability_measurement_blocks_admission() {
        // P2·1's UNSTABLE flag gets a real consequence here instead of being decoration: planning
        // against a number the box does not hold is how a placement dies mid-session.
        let stages = vec![StageDemand { device: "mac".into(), layers: 12, shard_bytes: MB, kv_bytes: MB }];
        let b = budgets(&[("mac", 8_000 * MB)]);
        let (links, demands) = (LinkMatrix::new(), vec![]);
        let spread: BTreeMap<String, f64> = [("mac".to_string(), 0.40)].into_iter().collect();
        let e = admit(&req(&stages, &b, &links, &spread, &demands)).unwrap_err();
        assert!(matches!(e[0], Refusal::UnstableCapability { .. }), "{e:?}");

        // ...but it is an explicit, named override, never a default.
        let mut r = req(&stages, &b, &links, &spread, &demands);
        r.allow_unstable = true;
        assert!(admit(&r).is_ok());
    }

    // ---------------------------------------------------------------- contention

    fn shared_uplink_links() -> LinkMatrix {
        // The recorded shape: the Mac reaches both VMs over ONE uplink, so the two links contend.
        let mut m = LinkMatrix::new();
        let (a, b) = (LinkId::new("mac", "vm1").unwrap(), LinkId::new("mac", "vm2").unwrap());
        m.observe_solo(&a, LinkSample::new(25.0, 10.0 * MBF).unwrap());
        m.observe_solo(&b, LinkSample::new(25.0, 8.0 * MBF).unwrap());
        m.observe_concurrent(&a, &b, 5.0 * MBF, 4.0 * MBF);
        m
    }

    #[test]
    fn a_contention_group_budget_is_the_minimum_member_not_the_sum() {
        // The whole meaning of a shared bottleneck is that members cannot all run at solo rate at
        // once. Summing would reintroduce exactly the optimism the grouping exists to prevent.
        // Group capacity = min(10, 8) = 8 MB/s; with 20% headroom the budget is 6.4 MB/s.
        // Demand 7 MB/s is under the SUM (18) but over the budget => refused.
        let links = shared_uplink_links();
        let demands = vec![
            ClassDemand { link: LinkId::new("mac", "vm1").unwrap(), class: TrafficClass::BoundaryCopy, bytes_per_s: 6.0 * MBF },
            ClassDemand { link: LinkId::new("mac", "vm2").unwrap(), class: TrafficClass::Heartbeat, bytes_per_s: 1.0 * MBF },
        ];
        let stages = vec![StageDemand { device: "mac".into(), layers: 12, shard_bytes: MB, kv_bytes: MB }];
        let b = budgets(&[("mac", 8_000 * MB)]);
        let spread = BTreeMap::new();
        let e = admit(&req(&stages, &b, &links, &spread, &demands)).unwrap_err();
        match e.iter().find(|r| matches!(r, Refusal::ContentionGroup { .. })) {
            Some(Refusal::ContentionGroup { demand_bytes_per_s, budget_bytes_per_s, .. }) => {
                assert!((*demand_bytes_per_s - 7.0 * MBF).abs() < 1.0);
                assert!((*budget_bytes_per_s - 6.4 * MBF).abs() < 1.0, "min(10,8) x 0.8");
            }
            _ => panic!("expected a ContentionGroup refusal: {e:?}"),
        }
    }

    #[test]
    fn all_traffic_classes_share_the_group_airtime() {
        // §11: "contention-group airtime shared by all traffic classes". Boundary copies, shard
        // transfer and heartbeats are priced against ONE budget — a shard rebuild that saturates
        // the uplink must be visible as pressure on the boundary path, not accounted separately.
        let links = shared_uplink_links();
        let stages = vec![StageDemand { device: "mac".into(), layers: 12, shard_bytes: MB, kv_bytes: MB }];
        let b = budgets(&[("mac", 8_000 * MB)]);
        let spread = BTreeMap::new();

        let modest = vec![ClassDemand { link: LinkId::new("mac", "vm1").unwrap(), class: TrafficClass::BoundaryCopy, bytes_per_s: 1.0 * MBF }];
        assert!(admit(&req(&stages, &b, &links, &spread, &modest)).is_ok(), "1 MB/s fits under 6.4");

        // Add a shard rebuild on the OTHER link of the same group: together they exceed the budget.
        let mut with_rebuild = modest.clone();
        with_rebuild.push(ClassDemand { link: LinkId::new("mac", "vm2").unwrap(), class: TrafficClass::ShardTransfer, bytes_per_s: 6.0 * MBF });
        let e = admit(&req(&stages, &b, &links, &spread, &with_rebuild)).unwrap_err();
        assert!(e.iter().any(|r| matches!(r, Refusal::ContentionGroup { .. })), "{e:?}");
    }

    #[test]
    fn traffic_on_an_unpriced_link_is_refused_not_assumed_free() {
        let mut links = LinkMatrix::new();
        // A probed link so a group exists, plus demand on a link never probed.
        links.observe_solo(&LinkId::new("mac", "vm1").unwrap(), LinkSample::new(25.0, 10.0 * MBF).unwrap());
        let demands = vec![ClassDemand { link: LinkId::new("mac", "vm9").unwrap(), class: TrafficClass::BoundaryCopy, bytes_per_s: MBF }];
        let stages = vec![StageDemand { device: "mac".into(), layers: 12, shard_bytes: MB, kv_bytes: MB }];
        let b = budgets(&[("mac", 8_000 * MB)]);
        let spread = BTreeMap::new();
        // The unprobed link lands in NO discovered group, so grouping alone would let its demand
        // pass as free traffic. Being priced — not being grouped — is the gate.
        let e = admit(&req(&stages, &b, &links, &spread, &demands)).unwrap_err();
        assert!(
            e.contains(&Refusal::UnpricedLink { link: LinkId::new("mac", "vm9").unwrap() }),
            "declared traffic on an unprobed link must be refused: {e:?}"
        );
    }

    #[test]
    fn an_idle_contention_group_constrains_nothing() {
        let links = shared_uplink_links();
        let stages = vec![StageDemand { device: "mac".into(), layers: 12, shard_bytes: MB, kv_bytes: MB }];
        let b = budgets(&[("mac", 8_000 * MB)]);
        let (spread, demands) = (BTreeMap::new(), vec![]);
        assert!(admit(&req(&stages, &b, &links, &spread, &demands)).is_ok());
    }

    #[test]
    fn every_reason_is_reported_not_just_the_first() {
        // A caller fixing one shortfall should not have to re-run to discover the next.
        let stages = vec![
            StageDemand { device: "mac".into(), layers: 12, shard_bytes: 700 * MB, kv_bytes: 100 * MB },
            StageDemand { device: "ghost".into(), layers: 12, shard_bytes: MB, kv_bytes: MB },
        ];
        let b = budgets(&[("mac", 800 * MB)]);
        let (links, spread, demands) = (LinkMatrix::new(), BTreeMap::new(), vec![]);
        let e = admit(&req(&stages, &b, &links, &spread, &demands)).unwrap_err();
        assert_eq!(e.len(), 2, "both the memory shortfall and the missing budget: {e:?}");
    }
}
