# Hydra v1 — Release Notes

> **Status: DRAFT, pending the M4 gate.** The limitations below are **owner-ratified for v1**
> (2026-08-25). They are published because a limitation a user discovers is worse than one they were
> told about — and because the acceptance and its reason belong in the *same breath*, never in a
> "caveats" appendix nobody reads.

## What Hydra is

Hydra runs a single large open-weight LLM by **pipeline-sharding contiguous layer ranges across 2–3
heterogeneous desktop machines on a trusted LAN**. Its differentiator is not speed — physics caps a
70B at roughly 2–7 tok/s on wired desktop hardware — but **correctness under failure**: crash-safe
sessions, exactly-once token semantics, teacher-forced recovery, and generation streams that survive
a machine dying mid-sentence.

## The boundary this release is built on

**v1 assumes every certificate holder is honest.** mTLS, per-device Ed25519 identity and signed
manifests defend against **outsiders and accidents** — an unpaired machine, a tampered model file, a
browser on the LAN, a corrupted frame. They do **not** defend against a **compromised stage**: a
worker holding a valid cluster certificate can lie about its computation, and v1 has no mechanism
that would detect it. Worker-compromise resistance is v2 scope.

This is stated first because several limitations below are acceptable *only* given it.

---

## Known limitations, accepted for v1

### Security and access control

**There is no API rate limiting.** The coordinator's HTTP API enforces bearer-token auth,
`Host`/`Origin` validation and a 1 MiB request-body limit, but does **not** rate-limit. A caller
holding a valid token can issue requests as fast as the machine accepts them. v1's threat model is
one trusted household on a trusted LAN; the token is the control.

**The API token has no key-management story.** The API token is supplied by the operator via
configuration or environment and is not stored in a system keyring. Rotating it means restarting the
coordinator with a new value.

**A device certificate cannot be revoked — and this is accepted only because v1 assumes every
certificate holder is honest, so a valid holder is trusted by construction rather than merely
unverified.** A certificate believed compromised remains valid until it expires. Leaf lifetime is
397 days, which is a ceiling, not a remedy. Re-pairing issues new material but does not invalidate
old.

**A slow peer can delay inbound connections.** The TLS handshake runs inline in the accept loop, so
a slow (not silent) peer can delay the next inbound connection by up to the 10 s handshake timeout.

**There is no cap on simultaneous connections from one peer.** Neither per-peer nor global. This and
the previous item share one fix and will land together.

### Durability and recovery

**Coordinator disk loss is session loss.** The coordinator's commit stream is the single durable
record of a session. If its disk is lost, the session is lost; D2 mirroring is not in v1.

**Old retransmissions are refused rather than answered.** Retransmissions beyond the in-flight
window are refused with `ERR_GAP` and a frontier rather than answered from cache. Answering them
would require either in-shard recomputation (which the protocol forbids) or an unbounded per-position
cache; a bounded refusal is the deliberate choice.

### Performance

**Prefill and recovery pay a per-position hash.** Prefill and recovery catch-up compute a BLAKE3
witness per position (≈9.6 ms/position on the dev model), which is visible in time-to-first-token on
long prompts. Autoregressive decode does not pay it.

**No wired-LAN performance figure is published, because none has been measured — this is
hardware-contingent and is owed rather than waived, since the project has no second physical
machine.** Correctness is demonstrated on a local pair and on containerized two-node CI; **neither
implies a LAN number, and none is inferred from them.** The published envelope (70B Q4 ≈ 2–7 tok/s
wired, 1.5–4 over good Wi-Fi, TTFT tens of seconds at a 4 k prompt) is **report-derived target, not
measurement**.

**70B has not been run, and this is hardware-contingent rather than a design limit** — it needs
roughly 40 GB aggregate across the cluster, which requires machines the project does not have. The
per-worker memory ceiling was retired by sharded weights (measured −50.0 % per worker), so the bound
is aggregate cluster RAM, not per-machine.

**Multi-node correctness is verified between containers on a single host — which is a hardware
contingency, not a choice**: no real network partition, no real clock skew, and no wired-LAN timing
are exercised, because the cloud VMs the project used for real multi-machine runs no longer exist.
Real-hardware three-node results are banked from when they did, and remain valid as *demonstrated*.

### Formats and configuration

**Boundary payloads may be `f32` or `f16`. `int8_blockq` is reserved and not offerable.** Its
measured cost (max-abs up to 2.298e+00 on logits, top-10 dropping to 9/10 at mid-network splits) is
outside the semantic-continuity bar.

---

## What the verification does and does not cover

**Model checking.** The transition core is **model-checked to fixpoint at standing bounds**;
liveness holds under per-message-class fairness; all four mutations fire. **The bounds travel with
the claim**: `Stages = {s1,s2}`, `MaxEpoch = 1`, `MaxRId = 1`, `MaxAttempt = 2`, `MaxPos = 1`,
`MaxCrashes = 2`, `|msgs| ≤ 20`, `MaxCkpt = 3`. This is a **bounded** check — "fixpoint" means the
bounded state space was exhausted, not that the protocol is verified in general — and liveness rests
on a fairness assumption about the environment, not a theorem about the network. The model
constructs every message, so a receiver's handling of a **malformed** one is outside its reach.

**Fuzzing.** Parser fuzzing is an accumulating budget and **has not reached its target**; the
current total is published in `verification/ci-results/`. It covers the parsers, not
`Worker::on_frame`, and it detects crashes — **not hangs, and not large-but-legal allocations**.

**Backends.** The byte-identity and bit-exactness results are measured on **both CPU and Metal**.
Every such assertion that ran to completion on Metal passed, including the split-vs-unsplit anchors,
shard-loaded weights, chunked prefill, and D1 recovery.

Two Metal-specific limitations are known and are **not** correctness findings:

**A Metal-backed worker aborts on shutdown.** An assertion in the GPU backend's device teardown
fires after computation has completed and results have been produced. Nothing it touches has yielded
a wrong value; it is a clean-exit defect in the vendored engine.

**The multi-worker recovery suites have not been verified on Metal, and this is hardware-contingent
rather than a design limit** — they require two or three workers each mapping the full model into a
GPU working set, which exceeds the 8 GB development machine's capacity. They pass on CPU. Verifying
them on a GPU needs hardware the project does not have.

**Nothing here has been measured on CUDA.** No machine in this project has ever run it.

