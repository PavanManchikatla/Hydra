// Dev-environment rpath for the worker's binaries + tests.
//
// `hydra-worker` transitively links the vendored, build-tree llama.cpp/ggml dylibs (via
// `hydra-engine-sys`). Cargo propagates the engine's link-lib/link-search to us, but a build
// script's `-rpath` link-arg does NOT propagate to dependents — so the *runtime* search path must
// be re-emitted here for the artifacts this crate actually produces. Same dev-environment
// assumption as `hydra-engine-sys/build.rs` (portable packaging = M4): when the build tree is
// absent (e.g. CI without the spike cmake build), the engine is a stub and nothing links libllama,
// so we emit nothing and stay green.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let libdir = manifest.join("../../vendor/llama.cpp/build/bin");
    let present = libdir.join("libllama.dylib").exists() || libdir.join("libllama.so").exists();
    if !present {
        return;
    }
    let dir = libdir.display();
    println!("cargo:rustc-link-search=native={dir}");
    // The general form applies to binaries, examples, benches, and tests (incl. the lib unit-test
    // harness) — the same instruction `hydra-engine-sys/build.rs` uses for its own artifacts.
    println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
}
