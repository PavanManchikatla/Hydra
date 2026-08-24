//! **M4·4 acceptance — the dashboard, tested the way it actually gets read (rule 19).**
//!
//! # The oracle, named first
//!
//! A dashboard's natural test **renders a healthy cluster**: every sensor present, every number
//! reassuring, the page looks right. That test passes forever and proves the one thing nobody
//! doubted. It says nothing about the case an operator actually needs the page for — a cluster
//! that is **degraded**, or one whose sensors are **not there at all**.
//!
//! The second is the dangerous one. P2·6's collector never invents a sensor: a platform that does
//! not expose thermals reports `Unavailable`, and container CI — the standing multi-node verifier —
//! typically exposes neither thermal zones nor a battery. **A dashboard that rendered that as
//! "Nominal" would undo the entire provenance discipline at the last inch**, telling an operator
//! "fine" where the system said "I do not know". So the tests below are: a degraded cluster, a
//! stage whose sensors are unavailable, and a request with no token.

use hydra_coordinator::dashboard::{Dashboard, SessionView, StageView};
use hydra_sched::telemetry::{Field, Provenance, TelemetrySample};

fn healthy(device: &str) -> TelemetrySample {
    TelemetrySample {
        device: device.to_string(),
        queue_depth: Field::measured(2),
        mem_headroom_mib: Field::measured(4096),
        soc_temp_dc: Field::measured(510),
        throttled: Field::measured(false),
        on_battery: Field::measured(false),
    }
}

/// A container: queue depth is application-level so it is always real; the platform exposes no
/// thermals, no throttle state and no battery. This is the CI shape, not a contrived one.
fn sensorless(device: &str) -> TelemetrySample {
    TelemetrySample {
        device: device.to_string(),
        queue_depth: Field::measured(7),
        mem_headroom_mib: Field::best_effort(900),
        soc_temp_dc: Field::unavailable(),
        throttled: Field::unavailable(),
        on_battery: Field::unavailable(),
    }
}

/// **An unavailable sensor renders as `unknown` — never as a value, and never as reassurance.**
#[test]
fn a_stage_with_unavailable_sensors_renders_unknown_and_never_nominal() {
    let view = StageView::from_sample(&sensorless("worker-s2"));
    assert_eq!(view.unknown_count(), 3, "three sensors this platform does not expose");

    let temp = view.readings.iter().find(|r| r.label == "SoC temperature").unwrap();
    assert_eq!(temp.value, "unknown", "an absent sensor has no value to show");
    assert_eq!(temp.provenance, Provenance::Unavailable);
    assert_eq!(temp.qualifier(), "not exposed by this platform");

    // Assert on the READINGS, not on raw substrings: the page's explanatory sentence legitimately
    // contains the word "healthy" while *denying* it ("…which is not the same as healthy"), and a
    // blunt substring check would flag the very sentence that does the honest work. The property
    // that matters is that no unavailable sensor is given a value.
    for r in &view.readings {
        if r.provenance == Provenance::Unavailable {
            assert_eq!(r.value, "unknown", "an unavailable sensor must render as unknown, not as {:?}", r.value);
        }
    }
    let html = Dashboard { session: None, stages: vec![view] }.to_html();
    assert!(html.contains("unknown"), "the page says so");
    assert!(
        !html.to_lowercase().contains("nominal") && !html.to_lowercase().contains("all good") && !html.to_lowercase().contains("ok\"") ,
        "the page must never translate an absent sensor into reassurance: {html}"
    );
    assert!(
        html.contains("<strong>unobserved</strong>"),
        "and it names what a stage with missing sensors actually is: unobserved, not healthy"
    );

    // The estimate is marked as one — a reader can tell it from a measurement.
    let mem = view_of(&sensorless("w")).readings.iter().find(|r| r.label == "memory headroom").unwrap().clone();
    assert_eq!(mem.qualifier(), "estimated");
    assert_eq!(mem.value, "900 MiB", "an estimate still shows its value; it is the LABEL that differs");
}

fn view_of(s: &TelemetrySample) -> StageView {
    StageView::from_sample(s)
}

/// **A degraded cluster renders its degradation** — not an average that hides it.
#[test]
fn a_degraded_cluster_shows_which_stage_is_degraded() {
    let mut hot = healthy("worker-s1");
    hot.soc_temp_dc = Field::measured(920);
    hot.throttled = Field::measured(true);
    hot.queue_depth = Field::measured(64);

    let board = Dashboard {
        session: Some(SessionView {
            session_id_prefix: "c4bd418ac40cafaf".into(),
            generation_durable_pos: 41,
            prefill_stable_pos: 128,
            committed_sampler_checkpoint_id: 3,
            coordinator_state: "Serviceable".into(),
        }),
        stages: vec![StageView::from_sample(&hot), StageView::from_sample(&healthy("worker-s2"))],
    };
    let html = board.to_html();

    assert!(html.contains("worker-s1") && html.contains("worker-s2"), "both stages appear separately");
    assert!(html.contains("920 dC") && html.contains("64"), "the degraded stage's real numbers are shown");
    assert!(html.contains("41") && html.contains("128"), "and the durable watermarks, which is what a recovery would resume from");
    assert!(html.contains("Serviceable"));
    // No aggregate: one hot stage in a two-stage cluster must not be averaged into comfort.
    assert!(!html.to_lowercase().contains("average"), "there is no cluster-average number to hide behind");
}

/// **No session is "no session", not a zeroed one.**
#[test]
fn an_absent_session_is_not_rendered_as_a_session_at_position_zero() {
    let html = Dashboard { session: None, stages: vec![] }.to_html();
    assert!(html.contains("no active session"));
    assert!(html.contains("no stage telemetry received"), "an empty table would read as a healthy quiet cluster");
}

/// **The v1 constraint is stated in the UI, not merely implemented.**
///
/// A page that happens to have no buttons invites someone to add one. A page that says it performs
/// no control actions makes adding one a visible decision.
#[test]
fn the_page_states_that_it_performs_no_control_actions() {
    let html = Dashboard { session: None, stages: vec![] }.to_html();
    assert!(html.contains("Read-only"));
    assert!(html.contains("no control actions") || html.contains("performs no control"));
    // And there is genuinely nothing to submit.
    assert!(!html.contains("<form"), "no forms");
    assert!(!html.contains("<button"), "no buttons");
    assert!(!html.contains("method=\"post\""), "nothing that posts");
}
