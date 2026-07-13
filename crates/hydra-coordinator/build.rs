// Dev-environment rpath for this crate's tests (it transitively links the vendored build-tree
// llama.cpp dylibs via hydra-tokenizer → hydra-engine-sys). A build script's -rpath link-arg does
// not propagate to dependents, so re-emit it here. Same dev-environment assumption as
// hydra-engine-sys/build.rs (portable packaging = M4); absent the tree the engine is a stub.

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
    println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
}
