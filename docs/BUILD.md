# Building Hydra

## The short version

```bash
git clone https://github.com/PavanManchikatla/Hydra && cd Hydra
git submodule update --init
cargo build --release
cargo test --workspace
```

That gives you everything **except a working inference engine**. Read on for why, and for how to
get one.

## Two build modes, and the difference matters

Hydra's FFI crate (`hydra-engine-sys`) links against a **built** `llama.cpp`. If it does not find
one, it compiles a stub and prints a warning naming what was missing:

```
warning: hydra-engine-sys: vendored llama.cpp build tree not found at vendor/llama.cpp
         (headers_ok=…, libs_ok=…); building a stub.
```

**A stub build is fully useful for everything except inference.** The protocol, the state machines,
the simulator, the WAL, the transport, the coordinator and its recovery logic all build and all
their tests run. What you cannot do is load a model or produce a token.

This is also why some tests print `SKIP:` and return. A skipped engine-gated test is **not a
passing test** — it is an unavailable one, and the project's records say so rather than counting it
as green.

## Building the engine

The engine needs a patch. `vendor/llama.cpp` is pinned at a specific commit, and Hydra adds a
~47-line **layer-window patch** that lets a context execute only layers `[l0, l1)` — the change
that makes pipeline sharding possible at all.

```bash
cd vendor/llama.cpp
git apply ../../spike/llama-cpp-layer-window.patch
cmake -B build -DCMAKE_BUILD_TYPE=Release -DGGML_METAL=ON   # or -DGGML_CUDA=ON, or neither for CPU
cmake --build build -j
cd ../..
cargo build --release
```

`build.rs` checks that the patch is actually applied — it looks for `il_start` / `il_load_start` in
`llama.h` — and **refuses to build against unpatched headers**, degrading to the stub with a named
reason instead. That check exists because an unpatched engine would otherwise produce a silent ABI
mismatch rather than a compile error.

> **Known gap, stated plainly:** the patch currently lives as a working-tree modification plus that
> `.patch` file, not as a pinned fork. A clean checkout must apply it by hand, as above. Pinning a
> fork SHA is tracked in `PROJECT_STATE.md` §8 and is the real fix.

### A model to test with

The engine-gated tests look for a GGUF at `models/qwen2.5-0.5b-instruct-fp16.gguf`, or wherever
`HYDRA_TEST_MODEL` points. Models are **not** in the repository.

## Running the test suite

```bash
cargo test --workspace -- --test-threads=1
```

Single-threaded is deliberate: several suites spawn real worker processes on real sockets, and
running them concurrently makes failures harder to attribute.

**Do not run overlapping `cargo` invocations** against the same target directory. They contend for
the build lock and the resulting stall looks exactly like a deadlock in the code — a mistake that
has cost this project real debugging time more than once.

## Verification tooling

The TLA+ model is checked with TLC. The jar is **not** committed:

```bash
mkdir -p verification/tools
curl -L -o verification/tools/tla2tools.jar \
  https://github.com/tlaplus/tlaplus/releases/download/v1.7.4/tla2tools.jar
```

Then use the project's own runner — **never an ad-hoc `java -cp` invocation**:

```bash
verification/run-tlc.sh -config smoke/Mut2-CaseBPure.cfg
```

The runner passes flags (notably `-deadlock`) that the model depends on. A checker invoked
differently is a result about a different system, and this project has been caught by that once
already.

## Platform notes

- **macOS**: no `timeout(1)`; concurrent TLC runs need distinct `-metadir` paths.
- **Memory**: the engine-gated suites load real weights. On an 8 GB machine they are slow rather
  than broken; be patient before assuming a hang.
