//! P2·6 — **the platform telemetry collector.**
//!
//! Fills the `Heartbeat` table's five fields with **real** readings and labels each with the
//! provenance the platform actually justifies. The aggregation, smoothing and trigger logic are
//! pure and live in `hydra-sched::telemetry`; this is the half that has to touch the OS — the same
//! split as P2·1 (pure aggregator in `hydra-sched`, engine-touching measurement here).
//!
//! **No sensor is invented.** Where a platform does not expose something, the field is
//! `unavailable()` and the scheduler sees `Unknown` rather than a comfortable number. That matters
//! most in container CI, which is the standing multi-node verifier: a container typically exposes
//! neither thermal zones nor a battery, and reporting "not throttled, plenty of headroom" there
//! would silently disable P2·5's triggers on the platform they most need to work.
//!
//! Cadence note: collection runs at heartbeat rate (seconds), which is why the macOS path may
//! shell out to `vm_stat`/`pmset` — acceptable per-heartbeat, and deliberately not per-token.

use hydra_sched::telemetry::{Field, TelemetrySample};

/// Collect one sample for `device`. `queue_depth` is the worker's own pending-work count — an
/// application-level quantity, so it is always genuinely measured.
pub fn collect(device: &str, queue_depth: u16) -> TelemetrySample {
    TelemetrySample {
        device: device.to_string(),
        queue_depth: Field::measured(queue_depth),
        mem_headroom_mib: mem_headroom_mib(),
        soc_temp_dc: soc_temp_dc(),
        throttled: throttled(),
        on_battery: on_battery(),
    }
}

// ============================== Linux ==============================

#[cfg(target_os = "linux")]
fn mem_headroom_mib() -> Field<u32> {
    // Prefer cgroup v2: inside a container the cgroup limit is the real ceiling, and
    // /proc/meminfo reports the HOST's memory, which would wildly overstate headroom.
    if let (Ok(max), Ok(cur)) = (
        std::fs::read_to_string("/sys/fs/cgroup/memory.max"),
        std::fs::read_to_string("/sys/fs/cgroup/memory.current"),
    ) {
        let max = max.trim();
        if max != "max" {
            if let (Ok(m), Ok(c)) = (max.parse::<u64>(), cur.trim().parse::<u64>()) {
                return Field::measured(((m.saturating_sub(c)) / (1024 * 1024)) as u32);
            }
        }
    }
    // No cgroup ceiling: MemAvailable is the kernel's own estimate of what is obtainable without
    // swapping — an estimate, so best-effort.
    if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("MemAvailable:") {
                if let Some(kb) = rest.split_whitespace().next().and_then(|v| v.parse::<u64>().ok()) {
                    return Field::best_effort((kb / 1024) as u32);
                }
            }
        }
    }
    Field::unavailable()
}

#[cfg(target_os = "linux")]
fn soc_temp_dc() -> Field<i16> {
    // /sys/class/thermal is usually NOT exposed in a container. Absent ⇒ unavailable.
    for zone in 0..8 {
        let p = format!("/sys/class/thermal/thermal_zone{zone}/temp");
        if let Ok(s) = std::fs::read_to_string(&p) {
            if let Ok(milli_c) = s.trim().parse::<i64>() {
                return Field::measured((milli_c / 100) as i16); // milli-C -> deci-C
            }
        }
    }
    Field::unavailable()
}

#[cfg(target_os = "linux")]
fn throttled() -> Field<bool> {
    // Package thermal throttle counters exist on bare metal but are absent in containers and most
    // VMs. Reporting `false` when the counter is missing would be an invented sensor.
    if let Ok(s) = std::fs::read_to_string("/sys/devices/system/cpu/cpu0/thermal_throttle/core_throttle_count") {
        if let Ok(n) = s.trim().parse::<u64>() {
            return Field::measured(n > 0);
        }
    }
    Field::unavailable()
}

#[cfg(target_os = "linux")]
fn on_battery() -> Field<bool> {
    // VMs and containers have no battery — absent means "no information", not "on mains".
    if let Ok(s) = std::fs::read_to_string("/sys/class/power_supply/AC/online") {
        if let Ok(n) = s.trim().parse::<u8>() {
            return Field::measured(n == 0);
        }
    }
    Field::unavailable()
}

// ============================== macOS ==============================

#[cfg(target_os = "macos")]
fn sh(cmd: &str, args: &[&str]) -> Option<String> {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
}

#[cfg(target_os = "macos")]
fn mem_headroom_mib() -> Field<u32> {
    // free + inactive + speculative pages. This is an ESTIMATE of what is obtainable — macOS
    // memory accounting (compression, purgeable) means no exact number exists — so best-effort.
    let Some(out) = sh("vm_stat", &[]) else { return Field::unavailable() };
    let page_size: u64 = out
        .lines()
        .next()
        .and_then(|l| l.split("page size of ").nth(1))
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(4096);
    let mut pages = 0u64;
    for line in out.lines() {
        for key in ["Pages free:", "Pages inactive:", "Pages speculative:"] {
            if let Some(rest) = line.strip_prefix(key) {
                if let Ok(n) = rest.trim().trim_end_matches('.').parse::<u64>() {
                    pages += n;
                }
            }
        }
    }
    if pages == 0 {
        return Field::unavailable();
    }
    Field::best_effort(((pages * page_size) / (1024 * 1024)) as u32)
}

#[cfg(target_os = "macos")]
fn soc_temp_dc() -> Field<i16> {
    // macOS exposes NO public SoC temperature API. Reading it requires private SMC/IOKit access,
    // which this project will not take on for a telemetry field. UNAVAILABLE is the honest answer,
    // and the scheduler is written to handle it (Pressure::Unknown, never Nominal).
    Field::unavailable()
}

#[cfg(target_os = "macos")]
fn throttled() -> Field<bool> {
    // `pmset -g therm` reports CPU_Speed_Limit as a percentage of nominal; below 100 means the
    // system is limiting us. Coarse and advisory, hence best-effort.
    let Some(out) = sh("pmset", &["-g", "therm"]) else { return Field::unavailable() };
    for line in out.lines() {
        if let Some(rest) = line.split("CPU_Speed_Limit").nth(1) {
            if let Some(v) = rest.split('=').nth(1).and_then(|s| s.trim().parse::<u32>().ok()) {
                return Field::best_effort(v < 100);
            }
        }
    }
    Field::unavailable()
}

#[cfg(target_os = "macos")]
fn on_battery() -> Field<bool> {
    let Some(out) = sh("pmset", &["-g", "batt"]) else { return Field::unavailable() };
    if out.contains("'Battery Power'") {
        return Field::best_effort(true);
    }
    if out.contains("'AC Power'") {
        return Field::best_effort(false);
    }
    Field::unavailable()
}

// ============================== other platforms ==============================

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn mem_headroom_mib() -> Field<u32> {
    Field::unavailable()
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn soc_temp_dc() -> Field<i16> {
    Field::unavailable()
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn throttled() -> Field<bool> {
    Field::unavailable()
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn on_battery() -> Field<bool> {
    Field::unavailable()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hydra_sched::telemetry::Provenance;

    #[test]
    fn collect_never_invents_a_sensor() {
        // Runs on whatever platform CI is. The contract is not "these fields have values" — it is
        // that a field with a value has a provenance that justifies it, and a field without one
        // says so. An Unavailable field must carry NO value.
        let s = collect("test-node", 7);
        assert_eq!(s.queue_depth.get(), Some(7), "queue depth is application-level, always real");
        assert_eq!(s.queue_depth.provenance(), Provenance::Measured);
        for (name, avail, prov) in [
            ("mem_headroom_mib", s.mem_headroom_mib.is_available(), s.mem_headroom_mib.provenance()),
            ("soc_temp_dc", s.soc_temp_dc.is_available(), s.soc_temp_dc.provenance()),
            ("throttled", s.throttled.is_available(), s.throttled.provenance()),
            ("on_battery", s.on_battery.is_available(), s.on_battery.provenance()),
        ] {
            assert_eq!(
                avail,
                prov != Provenance::Unavailable,
                "{name}: an Unavailable field must carry no value, and a valued field must not claim Unavailable"
            );
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_does_not_pretend_to_read_soc_temperature() {
        // There is no public API. Claiming a number here would be the exact invented sensor the
        // binding point forbids.
        let s = collect("mac", 0);
        assert_eq!(s.soc_temp_dc.provenance(), Provenance::Unavailable);
        assert_eq!(s.soc_temp_dc.get(), None);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_memory_headroom_is_best_effort_never_measured() {
        // macOS memory accounting (compression, purgeable pages) means no exact obtainable-memory
        // number exists, so this must never claim Measured.
        let s = collect("mac", 0);
        assert_ne!(s.mem_headroom_mib.provenance(), Provenance::Measured);
    }
}

#[cfg(test)]
mod probe {
    /// Diagnostic: print what THIS platform actually exposes. `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn print_this_platform() {
        let s = super::collect("this-box", 0);
        eprintln!("queue_depth      {:?} {:?}", s.queue_depth.get(), s.queue_depth.provenance());
        eprintln!("mem_headroom_mib {:?} {:?}", s.mem_headroom_mib.get(), s.mem_headroom_mib.provenance());
        eprintln!("soc_temp_dc      {:?} {:?}", s.soc_temp_dc.get(), s.soc_temp_dc.provenance());
        eprintln!("throttled        {:?} {:?}", s.throttled.get(), s.throttled.provenance());
        eprintln!("on_battery       {:?} {:?}", s.on_battery.get(), s.on_battery.provenance());
    }
}
