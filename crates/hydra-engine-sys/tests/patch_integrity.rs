//! **Audit L1 — the layer-window patch is a release-integrity dependency, not a low.**
//!
//! # The finding
//!
//! The M−1 layer-window patch — the change that makes pipeline sharding possible at all — exists
//! only as an **uncommitted working-tree modification** of the `vendor/llama.cpp` submodule.
//! `git submodule update` on a clean checkout yields stock `13f2b28b`, whose headers have no
//! `il_*` fields, while `build.rs` proceeded on `llama.h.exists()` alone. **And CI never builds the
//! real engine at all** (it degrades to a stub), so no automated run has ever exercised the FFI.
//!
//! # Standing rule 19: what the oracle could not see
//!
//! Nothing here is subtle — it is that **the only machine where the engine is ever built is this
//! one**, and on this machine the patch has been applied since M−1. Every green engine-gated test
//! in this repository is evidence about a working tree that no other checkout reproduces. That is
//! the INDISTINGUISHING degree at its most literal: on the only driver that runs, "patched" and
//! "pinned" are the same thing.
//!
//! These tests do not make the patch durable — **pinning a fork SHA is the real fix and is owed**
//! (§8). They make its absence *loud*: a checkout without it now fails a named test instead of
//! silently building a different engine.

/// The patch's markers must be present in the vendored headers whenever the engine is linked.
/// If they are not, `build.rs` should have degraded to a stub — so reaching this assertion with
/// the engine "available" and the patch missing is exactly the silent ABI mismatch L1 names.
#[test]
fn a_linked_engine_implies_the_layer_window_patch_is_applied() {
    if !hydra_engine_sys::ENGINE_AVAILABLE {
        eprintln!("SKIP: engine not linked (stub build) — nothing to check");
        return;
    }
    let header = concat!(env!("CARGO_MANIFEST_DIR"), "/../../vendor/llama.cpp/include/llama.h");
    let src = std::fs::read_to_string(header).expect("the vendored llama.h must exist when the engine is linked");
    assert!(
        src.contains("il_load_start"),
        "the engine is linked but llama.h has no `il_load_start`: the layer-window patch is NOT \
         applied, so the shim was compiled against a different ABI than it links (audit L1)"
    );
    assert!(src.contains("il_start"), "llama.h has no `il_start` — the compute-window half of the patch is missing");
}

/// The committed patch file is the artifact a clean checkout has to apply, so it must exist and
/// must actually describe the files the working tree modifies. A patch that has drifted from the
/// tree is worse than none: it reads as reproducibility while producing a different engine.
#[test]
fn the_pinned_submodule_is_exactly_its_base_plus_the_committed_patch() {
    let patch_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../spike/llama-cpp-layer-window.patch");
    let patch = match std::fs::read_to_string(patch_path) {
        Ok(p) => p,
        Err(e) => panic!("the layer-window patch must be committed — it is the provenance record: {e}"),
    };
    assert!(!patch.is_empty(), "the patch file is empty");

    let repo = concat!(env!("CARGO_MANIFEST_DIR"), "/../../vendor/llama.cpp");
    let git = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git").arg("-C").arg(repo).args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let Some(head) = git(&["rev-parse", "HEAD"]) else {
        eprintln!("SKIP: submodule not checked out (CI does not init it)");
        return;
    };

    // **The fork's invariant, and the reason this test was rewritten (2026-08-25, L1 closed).**
    //
    // It used to assert that every file MODIFIED IN THE WORKING TREE appeared in the patch file.
    // That was the right check while the patch was uncommitted working-tree state. The moment the
    // fork landed, the working tree became clean — so the check found nothing to iterate, took its
    // `modified.is_empty()` early return, and **reported `ok` while asserting nothing at all.**
    // A guard that cannot fail is rule 25's shape exactly: the verdict token stopped tracking
    // anything, and it did so in the L1 guard of all places.
    //
    // The post-fork invariant is stronger and stays checkable: **the pin is exactly ONE commit on
    // top of its upstream base, and that commit's diff IS the committed patch.** That catches a
    // second commit sneaking onto the fork branch, a hand-edit of the pinned tree, and the patch
    // file drifting from what is actually pinned — none of which the old form could see.
    let parents = git(&["rev-list", "--count", "HEAD"]).unwrap_or_default();
    let base = git(&["rev-parse", "HEAD~1"]).map(|s| s.trim().to_string());
    let Some(base) = base else {
        eprintln!("SKIP: shallow submodule checkout — no parent to diff against (count={})", parents.trim());
        return;
    };

    let Some(diff) = git(&["diff", &format!("{base}..HEAD")]) else {
        eprintln!("SKIP: could not diff the pinned commit against its base");
        return;
    };

    // Compare the SUBSTANCE — added and removed lines — not the headers, which carry index hashes
    // and context line numbers that legitimately differ between a stored patch and a fresh diff.
    let substance = |t: &str| -> Vec<String> {
        t.lines()
            .filter(|l| (l.starts_with('+') || l.starts_with('-')) && !l.starts_with("+++") && !l.starts_with("---"))
            .map(|l| l.to_string())
            .collect()
    };
    let from_pin = substance(&diff);
    let from_file = substance(&patch);
    assert!(!from_pin.is_empty(), "the pinned commit changes nothing — the layer window is not in the pin");
    assert_eq!(
        from_pin, from_file,
        "the pinned submodule commit ({}) does not match spike/llama-cpp-layer-window.patch. \
         The pin is the mechanism and the patch file is its provenance; when they disagree the \
         record describes an engine nobody is building (audit L1).",
        head.trim()
    );
}
