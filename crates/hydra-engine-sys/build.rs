// Build the C++ shim and link the *vendored, build-tree* llama.cpp/ggml dylibs.
//
// DEV-ENVIRONMENT ASSUMPTION (M4 will replace this with portable packaging): this links the
// dylibs under `vendor/llama.cpp/build/bin`, i.e. it assumes the pinned submodule has been built
// locally (the M-1 spike's `cmake --build spike/build` produces them). If that tree is absent we
// degrade gracefully — skip the shim compile/link and emit `cfg(engine_unavailable)` so the crate
// (and the whole workspace) still *builds*; the FFI just reports unavailable at call time. The
// headers come with the submodule checkout, so a normal `git submodule update --init` suffices to
// compile the shim; only *linking* (tests/binaries) needs the built dylibs.

use std::path::PathBuf;

fn main() {
    // Declared FIRST, before any path that emits it. Every `return` below sets
    // `cfg(engine_unavailable)`, and the patch-missing arm used to return *above* the declaration,
    // so rustc saw an undeclared cfg on the one path this machine never takes.
    println!("cargo::rustc-check-cfg=cfg(engine_unavailable)");
    println!("cargo:rerun-if-env-changed=HYDRA_FORCE_ENGINE_STUB");

    // **Rule 19 — the stub arm needs an oracle that can be RUN.**
    //
    // The developer's machine always has a built, patched `vendor/llama.cpp`, so it compiles the
    // real arm and only the real arm; the stub arm is compiled exclusively by CI, where a break in
    // it reads as "the workflow is broken" rather than as a named defect. That is how
    // `pub use imp::{gguf_probe, ..}` failed to resolve for 28 hours across a dozen red runs while
    // the local suite reported 390/0/7.
    //
    // This switch makes the clean-checkout build reproducible in one command:
    //   HYDRA_FORCE_ENGINE_STUB=1 cargo check --workspace --all-targets
    // It is opt-in by explicit environment variable and announces itself, so it can never be the
    // silent cause of a stub build somebody meant to be real.
    if std::env::var_os("HYDRA_FORCE_ENGINE_STUB").is_some() {
        println!(
            "cargo:warning=hydra-engine-sys: HYDRA_FORCE_ENGINE_STUB is set — building the STUB \
             arm deliberately. This is the clean-checkout build; the FFI reports unavailable at \
             every call site. Unset it to link the real engine."
        );
        println!("cargo:rustc-cfg=engine_unavailable");
        return;
    }

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest.join("../../vendor/llama.cpp");
    let inc_llama = vendor.join("include");
    let inc_ggml = vendor.join("ggml/include");
    let libdir = vendor.join("build/bin");

    let headers_ok = inc_llama.join("llama.h").exists();
    // **Audit L1 — the layer-window patch must be PRESENT, not merely presumed.**
    //
    // The patch exists only as an **uncommitted working-tree modification** of the submodule
    // (7 files; it does match `spike/llama-cpp-layer-window.patch` byte-for-byte). A clean checkout
    // therefore gets stock `13f2b28b`, whose headers have no `il_*` fields — and this build script
    // happily proceeded on `llama.h.exists()` alone. That is a **release-integrity defect**: the
    // engine that gets built is not the engine that was tested, and nothing said so.
    //
    // Checking for the patch's own marker turns a silent mismatch into a loud, named refusal. It
    // does not make the patch durable — pinning a fork SHA is the real fix and is owed (§8) — but
    // it means no build can quietly produce an ABI-mismatched engine while the tests stay green.
    let patch_applied = std::fs::read_to_string(inc_llama.join("llama.h"))
        .map(|h| h.contains("il_load_start") && h.contains("il_start"))
        .unwrap_or(false);
    if headers_ok && !patch_applied {
        println!(
            "cargo:warning=hydra-engine-sys: vendored llama.cpp at {} is present but UNPATCHED              (no il_start/il_load_start in llama.h). The M-1 layer-window patch              (spike/llama-cpp-layer-window.patch) is NOT applied, so the shim would compile against              one ABI and link against another. Building a stub instead — apply the patch, or pin a              fork SHA (audit L1).",
            vendor.display()
        );
        println!("cargo:rustc-cfg=engine_unavailable");
        return;
    }
    let libs_ok = libdir.join("libllama.dylib").exists() || libdir.join("libllama.so").exists();

    println!("cargo:rerun-if-changed=csrc/hydra_engine.cpp");
    println!("cargo:rerun-if-changed=csrc/hydra_engine.h");
    println!("cargo:rerun-if-changed=build.rs");

    if !headers_ok || !libs_ok {
        println!(
            "cargo:warning=hydra-engine-sys: vendored llama.cpp build tree not found at {} \
             (headers_ok={headers_ok}, libs_ok={libs_ok}); building a stub. Run the submodule \
             init + spike cmake build to enable the real engine.",
            vendor.display()
        );
        println!("cargo:rustc-cfg=engine_unavailable");
        return;
    }

    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .file("csrc/hydra_engine.cpp")
        .include(&inc_llama)
        .include(&inc_ggml)
        .warnings(false)
        .compile("hydra_engine_shim");

    println!("cargo:rustc-link-search=native={}", libdir.display());
    for lib in ["llama", "ggml", "ggml-base", "ggml-cpu"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
    // Runtime search path for the build-tree dylibs (dev convenience; not for shipping).
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", libdir.display());

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "macos" {
        // ggml-metal / Accelerate backends pull these in.
        for fw in ["Metal", "MetalKit", "Foundation", "Accelerate"] {
            println!("cargo:rustc-link-lib=framework={fw}");
        }
    }
}
