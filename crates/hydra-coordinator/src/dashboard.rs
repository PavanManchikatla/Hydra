//! **M4·4 — the read-only dashboard.**
//!
//! # What it is, and what it deliberately is not
//!
//! It is **another client of the API surface**, not a privileged path: same TLS material, same
//! bearer token, same `Host`/`Origin` checks. A dashboard that could be reached without the token
//! would be a second, weaker front door to the same information — and the information is a user's
//! session state.
//!
//! **There are no control actions in v1**, and the page says so in as many words. A read-only page
//! that merely *happens* to have no buttons invites someone to add one; a page that states the
//! constraint makes adding one a visible decision.
//!
//! # The provenance discipline is the whole point
//!
//! `hydra-sched`'s [`Field`] carries `Measured` / `BestEffort` / `Unavailable`, and P2·6's
//! collector **never invents a sensor**: a platform that does not expose thermals reports
//! `Unavailable`, not a comfortable number. A dashboard that rendered that as "Nominal" would undo
//! the discipline at the last inch — the operator would read reassurance where the system said
//! *"I do not know"*.
//!
//! So: **`Unavailable` renders as `unknown`, visibly, and never as a value.** `BestEffort` renders
//! with its qualifier. Only `Measured` is shown plainly. The rule is that a reader can always tell
//! a measurement from an estimate from an absence, which is the same rule `PROJECT_STATE.md` is
//! held to.

use hydra_sched::telemetry::{Provenance, TelemetrySample};

/// One rendered fact: a value **and how it is known**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub label: String,
    /// The rendered value. `"unknown"` when the platform does not expose it — never a stand-in.
    pub value: String,
    pub provenance: Provenance,
}

impl Reading {
    fn from_field<T: Copy + std::fmt::Display>(label: &str, f: &hydra_sched::telemetry::Field<T>, unit: &str) -> Reading {
        let value = match (f.provenance(), f.get()) {
            (Provenance::Unavailable, _) | (_, None) => "unknown".to_string(),
            (_, Some(v)) => format!("{v}{unit}"),
        };
        Reading { label: label.to_string(), value, provenance: f.provenance() }
    }

    /// How the value should be presented to a human: the qualifier that keeps a reader honest.
    pub fn qualifier(&self) -> &'static str {
        match self.provenance {
            Provenance::Measured => "measured",
            Provenance::BestEffort => "estimated",
            Provenance::Unavailable => "not exposed by this platform",
        }
    }
}

/// A stage's rendered health.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageView {
    pub device: String,
    pub readings: Vec<Reading>,
}

impl StageView {
    pub fn from_sample(s: &TelemetrySample) -> StageView {
        StageView {
            device: s.device.clone(),
            readings: vec![
                Reading::from_field("queue depth", &s.queue_depth, ""),
                Reading::from_field("memory headroom", &s.mem_headroom_mib, " MiB"),
                Reading::from_field("SoC temperature", &s.soc_temp_dc, " dC"),
                Reading::from_field("throttled", &s.throttled, ""),
                Reading::from_field("on battery", &s.on_battery, ""),
            ],
        }
    }

    /// **How many of this stage's sensors this platform does not expose.**
    ///
    /// Surfaced rather than hidden: a stage reporting four unknowns is not a healthy stage, it is
    /// an **unobserved** one, and an operator deciding whether to trust a cluster needs to know
    /// which of those they are looking at.
    pub fn unknown_count(&self) -> usize {
        self.readings.iter().filter(|r| r.provenance == Provenance::Unavailable).count()
    }
}

/// The session half of the page: what the coordinator knows about the live session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    pub session_id_prefix: String,
    /// `generation_durable_pos` — the last output position that is **actually on disk**.
    pub generation_durable_pos: i64,
    /// `prefill_stable_pos` — the input frontier that is durably committed.
    pub prefill_stable_pos: i64,
    pub committed_sampler_checkpoint_id: u64,
    pub coordinator_state: String,
}

/// The whole page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dashboard {
    pub session: Option<SessionView>,
    pub stages: Vec<StageView>,
}

impl Dashboard {
    /// Render as HTML. Hand-written and dependency-free: a read-only status page is not worth a
    /// template engine, and a smaller dependency surface on the one process that holds the cluster
    /// CA is worth more than the convenience.
    pub fn to_html(&self) -> String {
        let mut h = String::new();
        h.push_str("<!doctype html><meta charset=\"utf-8\"><title>Hydra</title>");
        h.push_str("<style>body{font:14px/1.5 ui-monospace,monospace;margin:2rem;max-width:60rem}\
table{border-collapse:collapse;margin:1rem 0}td,th{border:1px solid #ccc;padding:.3rem .6rem;text-align:left}\
.unknown{color:#8a6d00;font-style:italic}.est{color:#555}.note{background:#f5f5f5;padding:.6rem;border-left:3px solid #888}</style>");
        h.push_str("<h1>Hydra</h1>");

        // The constraint, stated in the UI rather than merely implemented.
        h.push_str("<p class=\"note\"><strong>Read-only.</strong> This page performs no control \
actions — it cannot start, stop, recover, or reconfigure anything. That is a v1 constraint, not an \
oversight, and adding an action here is a decision someone has to make deliberately.</p>");

        match &self.session {
            Some(s) => {
                h.push_str("<h2>Session</h2><table>");
                h.push_str(&format!("<tr><th>session</th><td>{}…</td></tr>", esc(&s.session_id_prefix)));
                h.push_str(&format!("<tr><th>coordinator state</th><td>{}</td></tr>", esc(&s.coordinator_state)));
                h.push_str(&format!(
                    "<tr><th>generation durable pos</th><td>{}</td></tr>",
                    s.generation_durable_pos
                ));
                h.push_str(&format!("<tr><th>prefill stable pos</th><td>{}</td></tr>", s.prefill_stable_pos));
                h.push_str(&format!(
                    "<tr><th>committed checkpoint</th><td>{}</td></tr>",
                    s.committed_sampler_checkpoint_id
                ));
                h.push_str("</table><p>Watermarks are positions that are <em>durable</em> — what is on disk, not what is in flight.</p>");
            }
            None => h.push_str("<h2>Session</h2><p class=\"unknown\">no active session</p>"),
        }

        h.push_str("<h2>Stages</h2>");
        if self.stages.is_empty() {
            h.push_str("<p class=\"unknown\">no stage telemetry received</p>");
        }
        for st in &self.stages {
            h.push_str(&format!("<h3>{}</h3>", esc(&st.device)));
            if st.unknown_count() > 0 {
                h.push_str(&format!(
                    "<p class=\"unknown\">{} of {} sensors are not exposed on this platform — this stage is \
<strong>unobserved</strong> in those respects, which is not the same as healthy.</p>",
                    st.unknown_count(),
                    st.readings.len()
                ));
            }
            h.push_str("<table><tr><th>reading</th><th>value</th><th>how it is known</th></tr>");
            for r in &st.readings {
                let class = match r.provenance {
                    Provenance::Unavailable => " class=\"unknown\"",
                    Provenance::BestEffort => " class=\"est\"",
                    Provenance::Measured => "",
                };
                h.push_str(&format!(
                    "<tr><td>{}</td><td{}>{}</td><td{}>{}</td></tr>",
                    esc(&r.label),
                    class,
                    esc(&r.value),
                    class,
                    r.qualifier()
                ));
            }
            h.push_str("</table>");
        }
        h
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
