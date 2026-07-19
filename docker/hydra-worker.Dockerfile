# Control-plane `hydra-worker` image for the containerized two-node CI (M2 FWD slice, seam 4).
#
# The engine is intentionally ABSENT: with no `vendor/llama.cpp` build tree (and no submodule
# headers — the CI checkout does not init the submodule), `hydra-engine-sys` degrades to its
# `engine_unavailable` stub, so the worker builds and runs **control-plane only** (no C++ toolchain,
# no model). That is exactly what this CI exercises: the real `hydra-state` stage SM + real TCP+mTLS
# between two containers, with `docker kill` as the kill −9 mechanism. Engine-gated byte-identical
# recovery is proven locally (seams 2b/3); this proves the two-node kill/recover machinery in CI.
FROM rust:1-slim AS build
WORKDIR /src
COPY . .
# Build only the worker (+ its non-engine deps). engine-sys stubs cleanly with no headers present.
RUN cargo build --release --bin hydra-worker -p hydra-worker

FROM debian:bookworm-slim
COPY --from=build /src/target/release/hydra-worker /usr/local/bin/hydra-worker
# argv[1] = the bootstrap file (mTLS material + role); mounted by the runner.
ENTRYPOINT ["/usr/local/bin/hydra-worker"]
