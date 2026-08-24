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
fn the_committed_patch_file_covers_every_modified_vendored_file() {
    let patch_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../spike/llama-cpp-layer-window.patch");
    let patch = match std::fs::read_to_string(patch_path) {
        Ok(p) => p,
        Err(e) => panic!("the layer-window patch must be committed — it is the only durable record of the change: {e}"),
    };
    assert!(!patch.is_empty(), "the patch file is empty");

    let repo = concat!(env!("CARGO_MANIFEST_DIR"), "/../../vendor/llama.cpp");
    let out = std::process::Command::new("git").arg("-C").arg(repo).args(["status", "--porcelain"]).output();
    let Ok(out) = out else {
        eprintln!("SKIP: git unavailable");
        return;
    };
    let status = String::from_utf8_lossy(&out.stdout);
    let modified: Vec<&str> = status
        .lines()
        .filter(|l| l.starts_with(" M") || l.starts_with("M "))
        .filter_map(|l| l.split_whitespace().nth(1))
        .collect();
    if modified.is_empty() {
        eprintln!("SKIP: the submodule working tree is clean (a pinned fork, or an unpatched checkout)");
        return;
    }
    for f in &modified {
        assert!(
            patch.contains(f),
            "vendored file {f} is modified in the working tree but does not appear in the committed \
             patch — the patch has drifted from the tree it claims to describe (audit L1)"
        );
    }
}
