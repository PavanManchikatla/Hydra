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

A stage can be killed with `kill -9` mid-generation; the session recovers and the client's stream
continues with no duplicated or missing text.

**What the evidence covers, precisely:**

| Half | Status | Where |
|---|---|---|
| **Control plane** — the coordinator detects the loss, durably records `BEGIN_RECOVERY`, drives the transaction, re-activates | **Covered in CI**, on every push | `crates/hydra-coordinator/tests/coordinator_driver.rs`, `crates/hydra-worker/tests/coordinator_drives_production.rs` |
| **Data-plane rebuild** — replaying tokens or durable boundaries into a replacement's KV cache | **A local, engine-gated run.** It needs a built engine and a model file, and **CI does not build the engine**, so its CI status is *unavailable*, not *green* | same file, `the_coordinator_drives_the_whole_recovery_including_the_strategy_path` |

Every recovery demonstration is held to a three-assertion bar: **SSE id continuity**, **byte-identical
to an uninterrupted seeded run**, and **disk truth** (no output position committed twice).

The recovery decisions are the state machine's, not a test harness's — the acceptance test
structurally *cannot* participate, because it holds no connection to send on.

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

### 2. Pair the cluster

On the machine that will coordinate:

```bash
hydra-cli pair --out ~/.hydra
```

It prints a **6-digit PIN** and a QR payload, mints the cluster CA, and provisions the
coordinator's own identity. **The CA private key never leaves this machine** — there is no flag
that emits it, and no API that returns it.

The window is open for **180 seconds** and tolerates **3 wrong PINs** before it burns. Both are
deliberate: a PIN proves physical proximity for a short window, and it is not a password.

### 3. Start the coordinator

```bash
export HYDRA_API_TOKEN="$(openssl rand -hex 24)"   # required; must be ≥16 bytes
hydra-coordinator --pairing-dir ~/.hydra --api-addr 127.0.0.1:8443 --data-dir ~/.hydra/data
```

`--pairing-dir` is the directory step 2 wrote: the coordinator uses **the cluster you just paired**
rather than inventing its own. (`--dev` exists for a throwaway single-machine identity and says so
loudly; a client cannot pre-trust a CA that is minted at startup.)

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
  -H "authorization: Bearer $HYDRA_API_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"messages":[{"role":"user","content":"hello"}],"stream":true}'
```

Certificate verification is real here — no `-k`. Without the token you get `401`; with the wrong CA
you get a TLS failure, not a warning.

> **Status of this quickstart, precisely.** Every command above has been **executed as written** on
> the development machine, and the last step returns `200` with certificate verification on and
> `401` without a token. Running it is what found three defects that reading it would not have:
> the CA was written only as DER (which `curl --cacert` refuses outright), `--dev` minted a CA
> unrelated to the one pairing had just created (so pairing was meaningless to the coordinator), and
> the API certificate's only SAN was `DNS:coordinator` while clients dial `127.0.0.1`.
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
