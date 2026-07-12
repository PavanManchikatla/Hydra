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
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest.join("../../vendor/llama.cpp");
    let inc_llama = vendor.join("include");
    let inc_ggml = vendor.join("ggml/include");
    let libdir = vendor.join("build/bin");

    let headers_ok = inc_llama.join("llama.h").exists();
    let libs_ok = libdir.join("libllama.dylib").exists() || libdir.join("libllama.so").exists();

    // We set cfg(engine_unavailable) ourselves below; declare it so rustc doesn't warn.
    println!("cargo::rustc-check-cfg=cfg(engine_unavailable)");
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
