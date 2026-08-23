//! P2·6 — **heartbeat telemetry: real values, honestly labelled.**
//!
//! The `Heartbeat` table has carried these five fields since M0 (`docs/hydra-proto.fbs`):
//! `queue_depth`, `mem_headroom_mib`, `soc_temp_dc`, `throttled`, `on_battery`. Until now nothing
//! filled them. This module defines what a filled heartbeat *means* and feeds it to the two
//! consumers that exist: **P2·1's EWMA** (a device that is throttling is a device whose measured
//! capability is stale) and **P2·5's triggers** (thermal and memory pressure are exactly the
//! sustained degradation the stability contract watches for).
//!
//! # Provenance is part of the value
//!
//! **No sensor is invented.** Not every field is readable on every platform, and the difference
//! between "measured 42 °C", "inferred that we are probably not throttling", and "this platform
//! does not expose it" is the difference between a trigger you can act on and one that fires on
//! fiction. Every field therefore carries a [`Provenance`], and a consumer that needs certainty
//! can demand [`Provenance::Measured`].
//!
//! What is actually available (see `hydra-worker::telemetry` for the collectors):
//!
//! | field | macOS (dev box) | Linux container (CI) |
//! |---|---|---|
//! | `queue_depth` | **Measured** — the worker's own queue, not an OS metric | **Measured** |
//! | `mem_headroom_mib` | **Best-effort** — `vm_stat`/`sysctl`, free+inactive is an estimate | **Measured** under cgroup v2, else best-effort from `/proc/meminfo` |
//! | `soc_temp_dc` | **Unavailable** — no public API; SMC access is private | **Measured** if `/sys/class/thermal` is exposed, usually **Unavailable** in a container |
//! | `throttled` | **Best-effort** where `pmset -g therm` reports `CPU_Speed_Limit`; **Unavailable** where it does not — *observed Unavailable on the dev Mac* | **Unavailable** in a container by default |
//! | `on_battery` | **Best-effort** — `pmset` | **Unavailable** — VMs and containers have no battery |
//!
//! Observed on the dev Mac (2026-08-22, real collection): `queue_depth 0 Measured`,
//! `mem_headroom_mib 1324 BestEffort`, `soc_temp_dc None Unavailable`, `throttled None
//! Unavailable`, `on_battery Some(true) BestEffort`. **That last field retroactively explains
//! P2·1's `UNSTABLE` capability reading on the same box** — the sustained benchmark ran on battery
//! power, and `capability_is_stale()` returns true for exactly that reason. The two slices agree
//! without being made to.
//!
//! A container-CI run therefore reports mostly `Unavailable`, and that is the correct output, not
//! a gap to paper over: **an absent sensor must never be reported as a comfortable reading.**

use crate::capability::Ewma;

/// How much a field's value can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Provenance {
    /// Read from a real sensor or an exact accounting source.
    Measured,
    /// Derived or approximate — directionally useful, not exact.
    BestEffort,
    /// This platform does not expose it. **Carries no value.**
    Unavailable,
}

/// A telemetry field: a value that may not exist, and never pretends to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Field<T> {
    value: Option<T>,
    provenance: Provenance,
}

impl<T: Copy> Field<T> {
    pub fn measured(v: T) -> Self {
        Field { value: Some(v), provenance: Provenance::Measured }
    }
    pub fn best_effort(v: T) -> Self {
        Field { value: Some(v), provenance: Provenance::BestEffort }
    }
    pub fn unavailable() -> Self {
        Field { value: None, provenance: Provenance::Unavailable }
    }
    pub fn provenance(&self) -> Provenance {
        self.provenance
    }
    /// The value at any provenance. `None` when unavailable.
    pub fn get(&self) -> Option<T> {
        self.value
    }
    /// The value **only if it was actually measured** — for consumers that must not act on an
    /// estimate.
    pub fn measured_only(&self) -> Option<T> {
        match self.provenance {
            Provenance::Measured => self.value,
            _ => None,
        }
    }
    pub fn is_available(&self) -> bool {
        self.value.is_some()
    }
}

/// One heartbeat's worth of device telemetry — the `Heartbeat` table's five fields, typed and
/// provenance-tagged.
#[derive(Debug, Clone, PartialEq)]
pub struct TelemetrySample {
    pub device: String,
    /// Pending work items on the worker. Application-level, so always measurable.
    pub queue_depth: Field<u16>,
    pub mem_headroom_mib: Field<u32>,
    /// Deci-Celsius, matching the wire type (`soc_temp_dc: int16`): 42.5 °C ⇒ 425.
    pub soc_temp_dc: Field<i16>,
    pub throttled: Field<bool>,
    pub on_battery: Field<bool>,
}

impl TelemetrySample {
    /// A sample from a platform that exposes nothing but its own queue — the honest container-CI
    /// shape, and the one a consumer must handle gracefully.
    pub fn minimal(device: &str, queue_depth: u16) -> Self {
        TelemetrySample {
            device: device.to_string(),
            queue_depth: Field::measured(queue_depth),
            mem_headroom_mib: Field::unavailable(),
            soc_temp_dc: Field::unavailable(),
            throttled: Field::unavailable(),
            on_battery: Field::unavailable(),
        }
    }
}

/// What telemetry says about a device's fitness to be planned against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pressure {
    /// Nothing observed that should change a placement.
    Nominal,
    /// Real pressure observed — P2·5 should treat the device's capability as suspect.
    Degraded,
    /// Not enough signal to say. **Distinct from `Nominal`**: "no sensor" is not "fine".
    Unknown,
}

/// Tracks telemetry per device and answers the two questions the scheduler actually asks.
#[derive(Debug, Clone)]
pub struct DeviceTelemetry {
    pub device: String,
    /// Smoothed memory headroom, so one allocation spike does not read as pressure.
    headroom: Ewma,
    last: Option<TelemetrySample>,
}

impl DeviceTelemetry {
    pub fn new(device: &str) -> Self {
        DeviceTelemetry { device: device.to_string(), headroom: Ewma::default_alpha(), last: None }
    }

    pub fn observe(&mut self, s: TelemetrySample) {
        if let Some(mib) = s.mem_headroom_mib.get() {
            // EWMA'd like every other measured quantity in this crate (P2·1's machinery reused,
            // not reimplemented) so a transient allocation is not mistaken for a trend.
            self.headroom.update(mib as f64);
        }
        self.last = Some(s);
    }

    pub fn smoothed_headroom_mib(&self) -> Option<f64> {
        self.headroom.value()
    }

    pub fn last(&self) -> Option<&TelemetrySample> {
        self.last.as_ref()
    }

    /// **Feeds P2·1:** a device that is throttling or on battery has a measured capability that no
    /// longer describes it, so the benchmark should be re-taken rather than trusted.
    pub fn capability_is_stale(&self) -> bool {
        let Some(s) = &self.last else { return false };
        // `.get()` not `.measured_only()`: a best-effort "we are throttling" is still worth
        // re-measuring on. The asymmetry is deliberate — acting on a soft positive costs a
        // benchmark, ignoring one costs a bad placement.
        s.throttled.get().unwrap_or(false) || s.on_battery.get().unwrap_or(false)
    }

    /// **Feeds P2·5:** thermal/memory pressure worth treating as sustained degradation.
    /// Returns `Unknown` when nothing is exposed — never `Nominal`, because an absent sensor is
    /// not a comfortable reading.
    pub fn pressure(&self, headroom_floor_mib: u32, temp_ceiling_dc: i16) -> Pressure {
        let Some(s) = &self.last else { return Pressure::Unknown };
        let mut saw_signal = false;

        if s.throttled.get() == Some(true) {
            return Pressure::Degraded;
        }
        if s.throttled.is_available() {
            saw_signal = true;
        }
        if let Some(t) = s.soc_temp_dc.get() {
            saw_signal = true;
            if t >= temp_ceiling_dc {
                return Pressure::Degraded;
            }
        }
        if let Some(h) = self.smoothed_headroom_mib() {
            saw_signal = true;
            if h < headroom_floor_mib as f64 {
                return Pressure::Degraded;
            }
        }
        if saw_signal {
            Pressure::Nominal
        } else {
            Pressure::Unknown
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full(device: &str, headroom: u32, temp: i16, throttled: bool, battery: bool) -> TelemetrySample {
        TelemetrySample {
            device: device.to_string(),
            queue_depth: Field::measured(3),
            mem_headroom_mib: Field::measured(headroom),
            soc_temp_dc: Field::measured(temp),
            throttled: Field::measured(throttled),
            on_battery: Field::measured(battery),
        }
    }

    #[test]
    fn an_unavailable_field_carries_no_value_and_cannot_be_mistaken_for_zero() {
        // The whole point: "no sensor" must not read as "0 °C" or "0 MiB free".
        let f: Field<i16> = Field::unavailable();
        assert_eq!(f.get(), None);
        assert_eq!(f.measured_only(), None);
        assert!(!f.is_available());
        assert_eq!(f.provenance(), Provenance::Unavailable);
    }

    #[test]
    fn best_effort_is_distinguishable_from_measured() {
        // A consumer that must not act on an estimate can demand measured_only().
        let e = Field::best_effort(425i16);
        assert_eq!(e.get(), Some(425), "usable when an estimate will do");
        assert_eq!(e.measured_only(), None, "but not when certainty is required");
        let m = Field::measured(425i16);
        assert_eq!(m.measured_only(), Some(425));
    }

    #[test]
    fn a_platform_that_exposes_nothing_reports_unknown_not_nominal() {
        // THE honesty test. A container with no thermal zone, no battery and no cgroup accounting
        // must not report a comfortable Nominal — that would silently disable P2·5's triggers on
        // exactly the platform where the standing multi-node verifier runs.
        let mut t = DeviceTelemetry::new("ci-container");
        t.observe(TelemetrySample::minimal("ci-container", 0));
        assert_eq!(t.pressure(512, 850), Pressure::Unknown);
        assert_eq!(t.smoothed_headroom_mib(), None, "no headroom sensor ⇒ no headroom estimate");
    }

    #[test]
    fn a_device_with_no_telemetry_at_all_is_unknown() {
        let t = DeviceTelemetry::new("silent");
        assert_eq!(t.pressure(512, 850), Pressure::Unknown);
        assert!(!t.capability_is_stale(), "silence is not evidence of throttling either");
    }

    #[test]
    fn throttling_is_degraded_pressure() {
        let mut t = DeviceTelemetry::new("mac");
        t.observe(full("mac", 4096, 600, true, false));
        assert_eq!(t.pressure(512, 850), Pressure::Degraded);
    }

    #[test]
    fn a_hot_device_is_degraded_pressure() {
        let mut t = DeviceTelemetry::new("mac");
        t.observe(full("mac", 4096, 900, false, false)); // 90.0 °C against an 85.0 ceiling
        assert_eq!(t.pressure(512, 850), Pressure::Degraded);
    }

    #[test]
    fn low_memory_headroom_is_degraded_pressure() {
        let mut t = DeviceTelemetry::new("mac");
        for _ in 0..20 {
            t.observe(full("mac", 100, 500, false, false)); // 100 MiB against a 512 floor
        }
        assert_eq!(t.pressure(512, 850), Pressure::Degraded);
    }

    #[test]
    fn headroom_is_smoothed_so_one_spike_is_not_pressure() {
        // A single allocation dip must not read as sustained memory pressure — the same reasoning
        // P2·1 applies to capability, reusing the same EWMA rather than reimplementing it.
        let mut t = DeviceTelemetry::new("mac");
        for _ in 0..20 {
            t.observe(full("mac", 4096, 500, false, false));
        }
        t.observe(full("mac", 10, 500, false, false)); // one bad sample
        assert_eq!(t.pressure(512, 850), Pressure::Nominal, "one dip is not a trend");
        // ...but sustained pressure does get through.
        for _ in 0..30 {
            t.observe(full("mac", 10, 500, false, false));
        }
        assert_eq!(t.pressure(512, 850), Pressure::Degraded);
    }

    #[test]
    fn a_healthy_device_with_real_sensors_is_nominal() {
        let mut t = DeviceTelemetry::new("mac");
        t.observe(full("mac", 4096, 500, false, false));
        assert_eq!(t.pressure(512, 850), Pressure::Nominal);
    }

    #[test]
    fn throttled_or_on_battery_marks_the_capability_measurement_stale() {
        // Feeds P2·1: a benchmark taken before the box started throttling no longer describes it.
        let mut t = DeviceTelemetry::new("mac");
        t.observe(full("mac", 4096, 500, true, false));
        assert!(t.capability_is_stale());

        let mut b = DeviceTelemetry::new("laptop");
        b.observe(full("laptop", 4096, 500, false, true));
        assert!(b.capability_is_stale(), "on battery, a laptop is not the machine that was benchmarked");

        let mut ok = DeviceTelemetry::new("mac");
        ok.observe(full("mac", 4096, 500, false, false));
        assert!(!ok.capability_is_stale());
    }

    #[test]
    fn a_best_effort_throttle_signal_still_triggers_a_re_measure() {
        // Deliberate asymmetry, asserted: acting on a soft positive costs a benchmark; ignoring
        // one costs a bad placement.
        let mut t = DeviceTelemetry::new("mac");
        let mut s = full("mac", 4096, 500, false, false);
        s.throttled = Field::best_effort(true);
        t.observe(s);
        assert!(t.capability_is_stale());
    }

    #[test]
    fn temperature_uses_the_wire_unit_deci_celsius() {
        // soc_temp_dc is int16 deci-Celsius on the wire; 42.5 °C is 425, and the type must hold a
        // realistic SoC range without overflow.
        let s = full("mac", 4096, 425, false, false);
        assert_eq!(s.soc_temp_dc.get(), Some(425));
        let hot = full("mac", 4096, 1100, false, false); // 110.0 °C
        assert_eq!(hot.soc_temp_dc.get(), Some(1100));
    }
}
