# Hydra

Hydra is an open-source, **trusted-LAN inference runtime** that runs a single large open-weight
LLM (70B-class dense; MoE later) by **pipeline-sharding contiguous layer ranges across 2–3
heterogeneous desktop-class machines** (CUDA desktop, Apple Silicon Macs, CPU nodes). Its
differentiator is not speed — physics caps a 70B at ~2–7 tok/s on wired desktop hardware — but
**correctness under failure**: crash-safe sessions, exactly-once token semantics, teacher-forced
recovery, and generation streams that survive any single machine dying mid-sentence without
duplicating or losing a single visible token.

## Start here

👉 **[`PROJECT_STATE.md`](PROJECT_STATE.md) is the current-status entry point** — the single,
always-current narrative of what Hydra is, what has happened, what is true right now, what is
owed, and what happens next. Read it first; it is kept accurate in the same commit as any change
to project reality.

## Reading order (from `PROJECT_STATE.md` §2)

| # | File | Role |
|---|------|------|
| 0 | [`PROJECT_STATE.md`](PROJECT_STATE.md) | Current truth (narrative authority) |
| 1 | [`BLUEPRINT.md`](BLUEPRINT.md) | What to build, order, DoD gates, fixed decisions |
| 2 | [`docs/hydra-session-protocol.md`](docs/hydra-session-protocol.md) | Messages, state machines, **invariants I1–I25** (correctness authority) |
| 3 | [`docs/hydra-proto.fbs`](docs/hydra-proto.fbs) + [`docs/wal-records.fbs`](docs/wal-records.fbs) | Wire + WAL payload schemas (generated code is source of truth) |
| 4 | [`docs/WAL-FORMAT.md`](docs/WAL-FORMAT.md) | On-disk format, fsync rules, torn-write contract |
| 5 | [`verification/`](verification/) | TLA+ model + 6 configs + VERIFICATION-README (CI gate for transition-logic changes) |
| 6 | [`docs/federated-llm-inference-report.md`](docs/federated-llm-inference-report.md) | Research rationale (consult for *why*) |

## Layout

```
crates/     hydra-proto | hydra-wal | hydra-transport   (M1: + hydra-state, hydra-sim)
spike/      M−1 engine feasibility spike + FINDINGS.md + llama.cpp layer-window patch
verification/  TLA+ transition-core model, configs, run-gate.sh
vendor/llama.cpp   pinned submodule (a pointer, not vendored source)
docs/       protocol spec, schemas, WAL format, research report
```

## Build prerequisites

- **Rust** (stable ≥ 1.80) — `cargo test --workspace`
- **flatc** (FlatBuffers compiler, ≥ 25.x) — only to regenerate `hydra-proto` (`scripts/gen-proto.sh`); generated code is committed
- **CMake ≥ 3.20 + a C/C++ toolchain** — only for the M−1 `spike/` (llama.cpp)
- **JDK + `tla2tools.jar`** — only to run the `verification/` TLC gate (`tla2tools.jar` is not committed; fetch from the tlaplus GitHub release)

## Clone

The compute engine is a **pinned git submodule**, so after cloning:

```sh
git clone https://github.com/PavanManchikatla/Hydra.git
cd Hydra
git submodule update --init      # fetch vendor/llama.cpp at the pinned commit
cargo test --workspace
```

## Status

Package **v0.10.2**. Gates passed: **M−1** (engine feasibility) ✅ · **M0** (protocol types) ✅.
**M1** (state machines + deterministic simulation) in progress. See
[`PROJECT_STATE.md`](PROJECT_STATE.md) for the live picture.

## License

MIT (matching the vendored llama.cpp/ggml). See individual crate manifests.
