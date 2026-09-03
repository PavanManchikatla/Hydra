<p align="center">
  <img src="assets/hydra-hero.png" alt="Hydra" width="420">
</p>

# Hydra

A trusted-LAN runtime that pipeline-shards a large open-weight LLM across 2–3 heterogeneous
desktop machines, built so that **a machine dying mid-sentence is a recoverable event rather than a
corrupted one**.

*Named for the Greek myth — many heads, one body; sever one and it regrows. No relation to other
Hydra projects or fictional organizations.*

---

## What is actually unusual here

It is not the speed. Physics caps a 70B-class model at a few tokens per second on desktop hardware,
and no amount of engineering here changes that. What is unusual is that **the session protocol is
machine-checked, the implementation is tested against the model by mutation, and every claim below
names the evidence that supports it and the reach of that evidence.**

### The protocol core is model-checked

> *The transition core is **model-checked to fixpoint at standing bounds**; **liveness holds under
> per-message-class fairness**; **all four mutations fire**; **six implementation mutations 200/200**
> in the randomized simulator.*

**The bounds are part of the claim** and travel with it everywhere:
`Stages = {s1, s2}`, `MaxEpoch = 1`, `MaxRId = 1`, `MaxAttempt = 2`, `MaxPos = 1`, `MaxCrashes = 2`,
`|msgs| ≤ 20`, `MaxCkpt = 3`.

**This is a bounded model check.** It is not a proof about unbounded executions. "Fixpoint" means
the bounded state space was exhausted, not that the protocol is verified in general.

*Evidence:* [`verification/HydraActivationCore.tla`](verification/HydraActivationCore.tla), the
CI receipts under [`verification/ci-results/`](verification/ci-results/), and
[`verification/VERIFICATION-README.md`](verification/VERIFICATION-README.md). CI runs the checker on
a schedule; a run counts as evidence only as a quoted `verdict=` line, never as a green check-mark.

**The model found real defects.** TLC-1 (an aborted activation being resurrected and completed via
stale acknowledgements) became spec invariant I25 and a standing mutation test. That is the reason
the model exists.

### The adversarial simulator

**10M+ randomized steps across ≥1,000 seeds**, with message drop / duplicate / reorder / delay,
crash-restart of any actor at any step, virtual disks with torn-write injection, and **all 25 spec
invariants checked after every step**. Six deliberately re-introduced implementation defects are
caught **200/200**.

*Evidence:* [`crates/hydra-sim`](crates/hydra-sim). Reproduction is from `(seed, schedule)` alone —
every failure prints its coordinates, because a failure you cannot reproduce is a rumour.

### Crash-safe sessions, driven by the checked state machine

A stage can be killed with `kill -9` mid-generation and the session recovers; **the coordinator
process itself can be killed mid-generation and, restarted against the same ledger, resumes** — and
the client's stream continues with no duplicated or missing text in both cases.

**Proven in a harness is not present in the product (standing rule 27).** Every claim below maps to
the BINARY that exercises it and the oracle that kills THAT binary; a claim whose only evidence is a
test harness is a *library* claim and says so.

| Claim | Binary that exercises it | Oracle that kills that binary | Status |
|---|---|---|---|
| A prompt produces tokens, byte-identical to the unsplit model's greedy argmax | `hydra-coordinator` (crates/hydra-node) driving two `hydra-worker` stages | `crates/hydra-node/tests/generation_e2e.rs` — the shipped process, real TLS, real token; text compared to the pair driver the rule-14 anchor proves against the unsplit model | **✅ product claim — GREEN on 2026-09-03** (`hydra-node/tests/generation_e2e.rs`, real arm: `test the_shipped_binary_generates_the_pair_drivers_tokens_byte_for_byte ... ok`). Local, engine-gated: CI does not build the engine, so its CI status is *unavailable*, not green. |
| Coordinator restart resumes the generation (spec §6.5a: derive from the WAL, fence forward) | `hydra-coordinator` | `crates/hydra-node/tests/restart_e2e.rs` — `kill -9` of the real process in three windows (after the first event; mid-stream; INTENT fsynced, COMMIT unsent), restart, `Last-Event-ID` reconnect; three assertions | {{RESTART_STATUS}} |
| Stage loss is recovered by the coordinator state machine (control plane) | library (`hydra-coordinator`) + harness | `crates/hydra-coordinator/tests/coordinator_driver.rs`, `crates/hydra-worker/tests/coordinator_drives_production.rs` (kills a real `hydra-worker` process) | **library claim, in CI on every push** |
| Stage loss is recovered end to end incl. the data-plane rebuild | library + harness, engine-gated | `coordinator_drives_production.rs::the_coordinator_drives_the_whole_recovery_including_the_strategy_path` | **library claim; local run only (CI does not build the engine)** |
| API auth is enforced by the shipped process | `hydra-coordinator` | `crates/hydra-node/tests/binary_auth.rs` (401 + structured error over TLS) | **product claim** |
| The token file is protected | `hydra-cli pair` / `hydra-coordinator` | `crates/hydra-node/tests/token_file.rs` (0600 minted; a 0644 file refuses startup) | **product claim** |

Every recovery demonstration is held to a three-assertion bar: **SSE id continuity**, **byte-identical
to an uninterrupted seeded run**, and **disk truth** (no output position committed twice).

**Not yet in the product (stated, not implied):** stage-loss recovery is driven by the harnesses'
coordinator, not yet by `hydra-coordinator`'s own loss detection; the direct-FWD / D1 topology
(boundary durability) is the demo binaries' shape, and the shipped coordinator drives the relayed
two-stage topology only.

---

## Honest boundaries

These are load-bearing, not disclaimers.

**The trust boundary is one household LAN, and every certificate holder is assumed honest.**
mTLS and signed manifests defend against **outsiders and accidents** — an unpaired machine, a
tampered model file, a browser on the same network. They do **not** defend against a *compromised
stage*: a worker holding a valid cluster certificate can lie about its computation, and v1 has no
mechanism that detects it. What rests on this is named explicitly in
[`BLUEPRINT.md`](BLUEPRINT.md) §1.9. Worker-compromise resistance is v2 scope.

**The formal guarantees cover a protocol abstraction, not the whole system.** The TLA+ model covers
the recovery/activation transition core. It does not model the transport, the FFI, the vendored
inference engine, or the wire codec. Defects have been found in all four of those layers by other
means (fuzzing, an external audit, and adversarial tests), which is the point: the model is one
instrument, not the whole toolkit.

**The proportions are worth knowing.** Measured with `wc -l` on this tree:

| | Lines | Verified how |
|---|---|---|
| Hydra Rust, `src/` (hand-written) | ~21,700 | protocol core model-checked; all of it under the test suite below |
| Hydra Rust, tests | ~12,200 | — |
| Generated FlatBuffers code | ~9,900 | schema is the source of truth; no shadow structs |
| C++ FFI shim | ~400 | every entry point exception-guarded |
| **Vendored `llama.cpp` (whole tree, C/C++/headers)** | **~729,000** | **not verified by this project**; used as a library, pinned as a submodule |

The engine does the arithmetic; Hydra does the protocol. The verified part is the smaller part.

**Security posture:** an external penetration audit (2026-08-23) found 4 critical, 21 high, 20
medium and 12 low findings, all accepted as a body. The report is in-repo
([`verification/audit-2026-08-23-AUDITOR-REPORT.md`](verification/audit-2026-08-23-AUDITOR-REPORT.md))
alongside the remediation ledger. Waves 1–4 are complete; the remaining accepted-for-now items are
tracked with their conditions in `PROJECT_STATE.md` §8.

**Parser fuzzing:** eight parsers of untrusted input have fuzz targets. The budget target is 24
CPU-hours; **2.4 CPU-hours have accumulated so far**, on a weekly schedule. Until it reaches the
target, that is what the receipts say.

---

## Performance: what is measured and what is not

**No wired-LAN measurement exists yet.** The numbers below are *report-derived targets*, not
results, and are labelled as such until a wired run exists.

| Configuration | Target decode | Status |
|---|---|---|
| 2× desktop-class, wired, 7B Q4 | ≥ 15 tok/s | **target, not measured** |
| Desktop + Mac, wired, 70B Q4 | 2–7 tok/s | **target, not measured**; 70B is hardware-contingent and has never been run |
| Recovery to resumed stream (D1, 4k ctx) | < 15 s | **target**; measured recoveries are annotated with their own conditions |

**What has been measured**, and only over **WAN/Tailscale** — never comparable to wired LAN:

- A 3-node cluster (Apple Silicon + two x86 VMs), layer ranges `[0,14) / [14,21) / [21,24)`:
  **12/12 argmax agreement** against the single-machine reference, **~0.81 tok/s**, both durable
  boundary edges capturing a strictly-increasing prefix. Annotated **WAN/Tailscale** at the source
  ([`docs/wan-run.md`](docs/wan-run.md)).
- A real-hardware 3-node kill/recover: **23.8 s** to resumed stream, over WAN.

The cloud machines those runs used no longer exist, so they are historical and permanently valid
*as demonstrated* — and cannot be re-run. Nothing multi-machine has been measured since.

---

## Quickstart

Aiming at: a person who did not write this gets a cluster running in under 30 minutes.

### 1. Build

```bash
git clone https://github.com/PavanManchikatla/Hydra && cd Hydra
git submodule update --init          # vendored llama.cpp
cargo build --release                # workspace; the engine is optional (see below)
```

The workspace builds without a compiled engine — `hydra-engine-sys` degrades to a stub and says so.
For actual inference you need the vendored engine built; see [`docs/BUILD.md`](docs/BUILD.md).

### 2. Pair the cluster, then provision it

On the machine that will coordinate:

```bash
hydra-cli pair --out ~/.hydra
```

It prints a **6-digit PIN** and a QR payload, mints the cluster CA, provisions the coordinator's own
identity, and **mints the API token** into `~/.hydra/api-token` (0600). **The CA private key never
leaves this machine** — there is no flag that emits it, and no API that returns it. The window is
open for **180 seconds** and tolerates **3 wrong PINs** before it burns.

Then place the model across two stages. `provision` mints the **session fence** (the identity every
stage and the coordinator must share; its session id comes from the system CSPRNG), writes one
bootstrap per stage and the stage table, all beside the CA material:

```bash
hydra-cli provision --pairing-dir ~/.hydra --model models/qwen2.5-0.5b-instruct-fp16.gguf \
  --stages worker-s1=127.0.0.1:9001,worker-s2=127.0.0.1:9002
```

(`--split K` names the first layer of the final stage; without it the model's layer count is read
from the file, which needs the built engine.)

### 3. Start the stages and the coordinator

```bash
hydra-worker ~/.hydra/worker-s1.boot &
hydra-worker ~/.hydra/worker-s2.boot &
hydra-coordinator --pairing-dir ~/.hydra --api-addr 127.0.0.1:8443 --data-dir ~/.hydra/data
```

`--pairing-dir` is the directory steps 2 wrote: the coordinator uses **the cluster you paired and
provisioned** — its trust anchor, session fence, stage table and API token all come from there —
rather than inventing any of them. (`HYDRA_API_TOKEN` in the environment overrides the token file;
a token file readable by anyone but you makes the coordinator refuse to start.)

The API is served **over TLS** with the cluster's own material. The cluster CA is self-signed, so a
client must be told to trust it — that is a real setup step, and it is the price of not shipping a
plaintext API on a LAN.

### 4. Check status

```bash
hydra-cli status --data-dir ~/.hydra/data
```

Each line reports a fact **and its source**, so you can tell a measurement from an assumption.

### 5. Talk to it

```bash
curl --cacert ~/.hydra/cluster-ca.pem https://127.0.0.1:8443/v1/chat/completions \
  -H "authorization: Bearer $(cat ~/.hydra/api-token)" \
  -H 'content-type: application/json' \
  -d '{"messages":[{"role":"user","content":"hello"}],"stream":true}'
```

Certificate verification is real here — no `-k`. Without the token you get `401`; with the wrong CA
you get a TLS failure, not a warning. **The stream carries tokens the two stages produced**, sampled
greedily at the final stage and committed to the coordinator's ledger before each one is sent.

> **Status of this quickstart, precisely.** **Re-executed as written on 2026-09-03T14:49:50Z** on this tree: tree `5fbdec2` (dirty files at run time: 28 — the tree under test, committed as the next seams); `cargo build --release` exit=0; `hydra-cli pair --out ~/.hydra` exit=0 (token minted 0600); `hydra-cli provision …` exit=0; two `hydra-worker` stages from the written bootstraps; `hydra-coordinator --pairing-dir ~/.hydra …` printed `API listening on https://`; `hydra-cli status` exit=0; step 5 `curl` with the minted token → HTTP 200, 34 SSE events, text 'Hello! How can I assist you today?Human: How do I get a job in the tech industry'; step 5b without the token → HTTP 401 **Observed and recorded, not claimed:** the stream ran to the binary's default budget (`--max-tokens 64`, 64 tokens generated) THROUGH the model's end-of-turn token — after `…today?\n` it continued into a `Human: How do I get a job…` turn: the product coordinator does not stop at EOS (neither does `pair::run_generation`, which the byte-identity oracle compares it to); owed in §8.. **A prompt to the shipped process produced tokens over TLS with the minted token, and 401 without it.** The 30-minute DoD is still NOT claimed: no non-author has run it (M4-GATE row 1).
>
> **What is still unverified is the DoD itself:** M4 requires that a *non-author* complete this in
> under 30 minutes on a *fresh account*. That has not happened yet, so the 30-minute claim is an
> aspiration and is not made anywhere above. It will be measured, not asserted.

---

## Documentation

| Document | What it is |
|---|---|
| [`BLUEPRINT.md`](BLUEPRINT.md) | What to build and in what order; governs process and scope |
| [`docs/hydra-session-protocol.md`](docs/hydra-session-protocol.md) | **Normative** protocol: messages, state machines, invariants I1–I25 |
| [`docs/WAL-FORMAT.md`](docs/WAL-FORMAT.md) | On-disk format: records, fsync rules, torn-write contract |
| [`docs/SECURITY-CHECKLIST.md`](docs/SECURITY-CHECKLIST.md) | Every security row and the test that backs it |
| [`docs/UNTRUSTED-INPUT-PARSERS.md`](docs/UNTRUSTED-INPUT-PARSERS.md) | Every parser of untrusted input, and its fuzz target |
| [`docs/wan-run.md`](docs/wan-run.md) | The multi-machine runbook, with its measurements |
| [`PROJECT_STATE.md`](PROJECT_STATE.md) | The living status record: what is true now, what is owed, and why |

`PROJECT_STATE.md` is the honest one. If a claim here and a claim there disagree, that file wins,
and the disagreement is a defect to fix.

---

## Status

**Milestones M−1 through M3 have passed their gates**; M4 (product hardening) is in progress.
The security audit's remediation waves 1–4 are complete. Known-owed items — certificate revocation,
a connection-count bound, a rollout compatibility allowance that must be removed before v1, and the
remaining fuzz hours — are listed with their unblocking conditions in `PROJECT_STATE.md` §8 and get
a triage before any release.

## License

See [`LICENSE`](LICENSE). Vendored `llama.cpp` is MIT and retains its own copyright headers.
