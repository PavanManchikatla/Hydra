# Hydra

Trusted-LAN inference runtime that runs a single large open-weight LLM by pipeline-sharding
it across 2–3 heterogeneous desktop-class machines, with crash-safe sessions, exactly-once
token semantics, and recoverable generation streams.

**Read [`BLUEPRINT.md`](BLUEPRINT.md) first** — it is the root of the package and governs
process & scope. Normative documents live in [`docs/`](docs/); the machine-checked transition
core lives in [`verification/`](verification/).

## Milestone status

Per `BLUEPRINT.md §3`. A milestone is not started until the previous one's Definition of Done
passes. **Do not proceed past a milestone until its DoD passes.**

| Milestone | Description | Status |
|---|---|---|
| **M−1** | Engine feasibility spike (shard-style llama.cpp execution) | ✅ **PASS** — [`spike/FINDINGS.md`](spike/FINDINGS.md) |
| M0 | Skeleton + protocol types (`hydra-proto`, `hydra-wal`, `hydra-transport`) | ⏳ next |
| M1 | State machines + deterministic simulation (correctness heart) | ⏳ |
| M2 | Two-node real pipeline (first tokens) | ⏳ (now schedulable — M−1 note exists) |
| M3 | Heterogeneity, scheduler, hardening | ⏳ |
| M4 | Product hardening | ⏳ |

**M−1 result:** shard A → shard B reproduces unsplit llama.cpp logits **bit-exactly** on CPU
(F32 boundary, 0.0 max-abs across k ∈ {1,4,12,18,23} × 3 prompts); KV truncate+replay exact;
FP16 boundary payload costs ~0.04 logit max-abs with stable argmax (spec I8). Engine delta is a
47-line per-arch layer-window patch (`spike/llama-cpp-layer-window.patch`).

## Layout (BLUEPRINT §2)

```
hydra/
├── BLUEPRINT.md            # root instruction set
├── docs/                   # normative spec, WAL format, schemas, research report
├── verification/           # HydraActivationCore.tla + cfgs + VERIFICATION-README
├── vendor/llama.cpp/       # pinned submodule (MIT) — the compute engine
├── spike/                  # M−1 throwaway feasibility spike (not the product)
├── crates/                 # M0+ Rust workspace (hydra-proto, hydra-wal, ...)
└── tests/                  # integration + chaos (M2+)
```

## M−1 spike (current)

Goal (BLUEPRINT §3, M−1): prove the narrow llama.cpp/ggml FFI can support shard-style
execution before any transport/recovery code is written. DoD: a prompt applied through
shard A → shard B produces final logits matching unsplit llama.cpp on the same CPU backend
within **1e‑3 max‑abs**; KV truncate+replay reproduces them; a one-page findings note records
any FFI-boundary changes needed. See [`spike/README.md`](spike/README.md).
