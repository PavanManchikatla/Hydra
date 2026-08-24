//! `hydra-cli status` — what the cluster is, as far as this machine can honestly tell.

/// A line of cluster status. Deliberately a **fact plus its source**, so a reader can tell a
/// measurement from an assumption — the same discipline PROJECT_STATE is held to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub what: String,
    pub value: String,
    pub source: &'static str,
}

/// Render status for a coordinator data directory. Reports **only what it can observe**: an absent
/// file is reported as absent rather than as a default, because "no commit stream" and "an empty
/// commit stream" are different facts about a cluster.
pub fn status_for(data_dir: &std::path::Path) -> Vec<StatusLine> {
    let mut out = Vec::new();
    let commits = data_dir.join("commits.wal");
    out.push(match std::fs::metadata(&commits) {
        Ok(m) => StatusLine { what: "commit stream".into(), value: format!("{} bytes", m.len()), source: "on disk" },
        Err(_) => StatusLine { what: "commit stream".into(), value: "absent".into(), source: "on disk" },
    });
    let control = data_dir.join("control.wal");
    out.push(match std::fs::metadata(&control) {
        Ok(m) => StatusLine { what: "control wal".into(), value: format!("{} bytes", m.len()), source: "on disk" },
        Err(_) => StatusLine { what: "control wal".into(), value: "absent".into(), source: "on disk" },
    });
    out
}
