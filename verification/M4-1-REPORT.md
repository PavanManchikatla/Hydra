# M4·1 completion report — reserved-hook audit, security checklist, parser fuzzing

**Date:** 2026-08-23 · **Milestone:** M4 (product hardening), slice 1 of 4
**Status: COMPLETE. The project is now PAUSED for the owner's external full-repo security and
penetration audit.** M4·2 (pairing UX), M4·3 (packaging/docs) and M4·4 (dashboard) are **not
started**, by instruction.

---

## 1. What was asked, and what changed about how it was done

BLUEPRINT §3's M4 scope names three items that M4·1 covers:

* *"the reserved-hook audit (every `[RESERVED]` spec field exists in `hydra-proto` and is fenced)"*;
* *"security checklist from report Addendum 2 §E1/D1 passes (no 0.0.0.0 binds, API auth enforced…)"*;
* *"…GGUF parser fuzzed for 24 CPU-hours without crashes"*.

The delegating instruction added one constraint that turned out to be the whole story: **the audit is
a test, not a reading.**

**Written as prose, both audits would have passed.** Every reserved field did exist. Every cap was
declared. The security posture was documented in BLUEPRINT §1.9 and had been for months. Written as
executing assertions, they found **ten defects**.

> A documented invariant that nothing checks is not an invariant — and the difference is invisible
> to a reading. That is the finding of this slice; the ten defects are its evidence.

---

## 2. The ten defects

All ten are fixed, each with a directed regression that runs on every push. Full narrative in
PROJECT_STATE §7.27; the checklist-to-assertion map is `docs/SECURITY-CHECKLIST.md`.

### (A) Three reserved hooks were present but unfenced

An unvalidated reserved field is **worse than an absent one**: it is an accepted input on a code path
nobody wrote.

| # | Defect | What it meant |
|---|---|---|
| 1 | **`Fence.branch_id`** — schema says *"RESERVED, must be 0 in v1"* | Written as 0, **never read**. A peer could set anything and the frame was accepted |
| 2 | **`Tensor.block_scales`** — schema says *present iff `dtype == I8_BLOCKQ`* | Never inspected: accepted-and-ignored on every boundary frame, leaving a field a future peer could use to smuggle state past a build that does not understand it |
| 3 | **Option B (spec §1.4)** — *"one active session per model instance (Option A); Option B [RESERVED]"* | **Option B was the DEFAULT.** The HTTP surface minted a fresh session on every POST without a matching `Idempotency-Key`, so a second client started a second generation against workers whose stage SMs, sampler and commit stream are single-session by construction. Not unimplemented — *reachable* |

Now: `branch_id != 0` refused; `block_scales` on a non-`I8_BLOCKQ` tensor refused; a second
concurrent session refused with a structured **409** that names the session holding the instance.

`model_instance_id` was already correct and is now pinned: a foreign one is refused at F1, and it is
**absent from `FenceView`**, asserted by an exhaustive destructure that *fails to compile* if it is
ever added — downstream code cannot branch on a value it is never handed.

### (B) Four of five normative wire caps were never called

Only `MAX_FRAME_BYTES` was enforced. `check_tensor_len`, `check_positions`, `MAX_SNAPSHOT_BYTES` and
`MAX_STRING_BYTES` lived in `limits.rs` and were invoked from **nowhere in the tree**.

*"The frame cap already bounds it"* is not an answer — it is a different cap on a different quantity:

* a legal 64 MiB frame could carry a **60 MiB tensor** against a declared 48 MiB cap;
* `n_positions` is a `uint16`, so a peer could declare **65 535 positions — 64× the cap — in a frame
  whose bytes look unremarkable**;
* a "sampler snapshot" could be 64 MiB against a declared 1 MiB cap.

All four are now checked **before** the copy that FlatBuffers' zero-copy access makes the actual
allocation point. Each test asserts the *error kind* (`LimitExceeded { what, value, cap }`), because
naming the quantity and the cap is only possible at the point the check is really made.

### (C) The LAN API had no authentication at all, and nothing stopped a wildcard bind

Report Addendum 2 §E1's threat is not a malicious user. It is **a web page the victim is merely
visiting**, executing in a browser that already holds the victim's network position — which is why
"it is only on localhost" is not a defence, and why Ollama-class servers have been exploited exactly
this way.

| Now enforced | How |
|---|---|
| Bearer token, **required by the type** | `AppState::new` takes an `ApiAuth`; there is no `none()` and no `Option`, so "auth not configured" is an unreachable state |
| No timing leak | Both sides BLAKE3-hashed and the digests compared — neither the token's length nor a matching prefix is observable (asserted with a prefix-of-correct case) |
| `Host` allow-list | A **missing** `Host` is refused rather than exempted; otherwise "send no Host" is the way around the check |
| `Origin` allow-list | Foreign refused, **absent allowed** (a normal API client sends none) — safe only because `Host` is unconditional |
| Refusals have no side effect | No session, no generation, with an admitted-request control |
| No `0.0.0.0` by default | `check_bind_addr` refuses any unspecified address **before the socket exists**; opt-in only, via `HYDRA_ALLOW_WILDCARD_BIND=1` |

**Deliberately an opt-in, not an opt-out.** An opt-out is a flag someone forgets to set; an opt-in is
a decision someone had to make. Exactly one file in the tree opts in — the containerised CI runner,
whose network namespace *is* the isolation boundary and whose port is published only on `127.0.0.1`
— and that is asserted **repository-wide**, as is the absence of any raw `TcpStream`/`TcpListener`
outside the mTLS module.

**API break, stated plainly:** `AppState::new` now takes an `ApiAuth`. An embedder must supply a
token; there is no unauthenticated path to fall back to.

### (D) Two CVE-class defects in the GGUF parser

Both found by the new fuzzer **within a second of first running it**. Report Addendum 2 §D1 names the
class (the 2024 llama.cpp heap-overflow family).

| # | Defect | Why it is serious |
|---|---|---|
| 8 | **Allocation amplification from a declared count** — `Vec::with_capacity(kv_count / tensor_count / array_len)` reserved memory read from the header *before any element was parsed* | A ~40-byte file requested **64 TiB** and **aborted the process**. An allocation failure is an *abort*, not a panic — not even catchable — so this was an unconditional file-triggered kill of any worker handed a hostile model |
| 9 | **Unchecked 64-bit shape arithmetic** — the dims product and the byte-length multiply | The debug panic is how the fuzzer *saw* it. The **release** behaviour is the vulnerability: a silent wrap to a small length that then sizes a read against a real buffer |

Fixed by `Cursor::reserve_for` (a declared count may never reserve more than the remaining input
could justify) and checked arithmetic reported as `ShapeOverflow` — deliberately an **error** and not
a `saturating_mul`, because a saturated length is still a number and a caller who does not look at it
carefully will use it. Also hardened: `offset + len` checked on `u64` **before** either side narrows
to `usize`, and a hostile `general.alignment` (0, non-power-of-two, `u32::MAX`) made survivable.

### (E) The tenth defect — the audit's own near-miss

Enforcing the position cap immediately exposed it, and it is the most instructive item in this report.

`encode_fwd` had been putting `activations.len()` — the **float** count — into `n_positions`, a field
the schema declares as `<= MAX_POSITIONS_PER_FRAME`. The line even carried a `// placeholder`
comment. Harmless while nothing read the field; **live** the moment the cap became real:

* the dev model's `n_embd` is **896**, which slips under the 1024 cap — so **every test would have
  stayed green**;
* **every larger model would have had its boundaries refused.** A 7 B has `n_embd = 4096`.

A correct-looking security fix would have shipped and broken the product on the first real model.
Fixed, and pinned by a regression that drives a **4096-float** boundary so the dev model's narrowness
cannot hide it again.

> **The general lesson, kept:** turning a documented-but-unenforced constraint into an enforced one is
> not a free change. It is a behaviour change against every producer that was quietly violating it —
> and a small dev model is exactly the wrong oracle for noticing.

---

## 3. Classification (standing rule 10 / rule 5)

**Every item is IMPLEMENTATION — proceed + log. No ratification pause is owed.**

* Each changes *what an effect carries* or *whether an input is admitted*. **None changes what any
  state machine decides.** No spec text, no TLA+ model, no invariant, and no `.fbs` schema moved.
* None is a rule-5 package change. BLUEPRINT §1.9 already *fixes* "API auth token + Host/Origin
  validation, never bind 0.0.0.0 by default"; spec §1.4 already fixes Option A; the caps are already
  normative in the schema. **This slice implements ratified decisions that had never been
  implemented. It does not re-decide anything.**

---

## 4. The fuzzing arm — what it is, stated rather than implied

`crates/hydra-fuzz` is a **deterministic, structure-aware mutation fuzzer**. It is **not**
coverage-guided (libFuzzer / AFL), and the trade is recorded rather than glossed:

* **Given up:** coverage feedback. A coverage-guided fuzzer will, given enough time, reach deeper
  paths than this will.
* **Gained:** every finding reproduces from a `(seed, iteration)` pair alone — the contract
  `hydra-sim` has run under since M1 (BLUEPRINT §4.2, *"determinism or it didn't happen"*) — and the
  CI arm builds on **stable** Rust, so it is a verification link this project can actually watch run
  rather than take on faith (standing rule 12).
* **Upgrade named, not pretended away:** `cargo-fuzz` targets over the same entry points are strictly
  additive and are an owed item in PROJECT_STATE §8.

Structure-awareness is what makes a blind mutator worth the CPU here: uniform random bytes fail the
four-byte GGUF magic with probability ≈ 1 − 2⁻³², so a naive fuzzer would spend 24 CPU-hours
re-testing a single `if`. The generators emit files and frames well-formed enough to reach the
arithmetic, then hostile in one specific way — an enormous declared count, a length that overruns the
buffer, a dimension product that overflows, an offset past the end.

**Two lanes:**

| Lane | Cadence | Purpose |
|---|---|---|
| `hydra-fuzz/tests/fuzz_smoke.rs` | every push, **fixed seeds** | Catch a regression the day it lands. Seeds are fixed on purpose: a randomly-seeded security test is flaky, and a flaky security test gets muted |
| `.github/workflows/fuzz.yml` | dispatch + weekly, 8 shards × **distinct** seeds | The exploratory search, accumulating toward the 24 CPU-hour DoD. Distinct seeds so parallelism buys coverage rather than repetition |

**A green cannot mean "the fuzzer emitted nothing":** a paired assertion checks that the generator
produces varied, non-trivial inputs — the same control-pairing principle as the D0 zero-traffic test.

The workflow's classify step follows rule 16: it reads the driver's `verdict=` lines, treats a
**missing** summary or a missing target as `INCONCLUSIVE` rather than a pass, and never maps a runner
exit to green.

---

## 5. Definition-of-done status

| M4 DoD clause | Status |
|---|---|
| Reserved-hook audit — every `[RESERVED]` field exists in `hydra-proto` and is fenced | ✅ **MET**, as executing assertions (`reserved_hooks.rs`, 9 tests; `session_http.rs` for the Option-A half) |
| No `0.0.0.0` binds | ✅ **MET**, including repository-wide assertions |
| API auth enforced | ✅ **MET**, with `Host`/`Origin` validation |
| All frame/tensor/record limits enforced pre-allocation | ✅ **MET** (`wire_limits.rs`) |
| mTLS on every link, refuse-on-fail everywhere | ✅ **MET** (`security_checklist.rs` + the existing durability/sampler/admission refusals, mapped in `docs/SECURITY-CHECKLIST.md`) |
| GGUF parser fuzzed **24 CPU-hours** without crashes | 🔄 **ACCUMULATING** — the arm exists, runs, and has already paid for itself. The hours are banked as receipts and the claim is made **only when they add up** |
| *Non-author 3-machine setup in under 30 min* | ⛔ **M4·2/M4·3** — not started, by instruction |

---

## 6. Known gaps, carried honestly into the audit

These are listed **for** the external auditor, not around them.

1. **Sandboxed model loader** — not attempted (report §D1 calls it *"ideally"*). The engine loads
   in-process. The parser is now hardened and fuzzed, which is the other half of §D1's fix; the
   sandbox is not.
2. **24 CPU-hours** — accumulating, not yet reached.
3. **Coverage-guided fuzzing** — not present; the trade is described above.
4. **Dashboard auth** — not applicable until the dashboard exists (M4·4).
5. **No rate limiting and no request-size limit on the HTTP surface** — a caller holding a valid token
   can submit an arbitrarily large body. Found while implementing §E1; not itself an §E1 item.
6. **`hydra-cli pair` (QR/PIN) does not exist yet** — a cluster CA is provisioned programmatically, so
   the *pairing ceremony's* security properties are not yet exercised at all.
7. **API token handling has no story** — `ApiAuth::new` takes the token from its caller: no keyring,
   no rotation, no on-disk protection. Belongs with the pairing UX (M4·2).
8. **This checklist is self-assessment.** It is the author auditing the author. That is precisely why
   the pause that follows exists.

---

## 7. Verification state at the pause

* Workspace: `cargo test --workspace` green; clippy clean.
* Standing rule-14 bit-exact anchors green: `two_worker_teacher_forced_no_sample_bit_exact`,
  `direct_worker_to_worker_fwd_is_bit_exact`,
  `two_worker_anchor_is_bit_exact_with_shard_loaded_weights`,
  `chunked_prefill_is_bit_exact_with_unchunked_prefill`.
* M1 **PASSED (FULL)**, M2 **PASSED**, M3 Track-A **PASSED** (2026-08-23,
  `verification/M3-GATE.md`).
* Upstream `ggml-org/llama.cpp#25577`: OPEN, zero maintainer replies (rule-8 check, 2026-08-23).

---

## 8. ⏸️ PAUSE

**M4·1 is complete. Nothing further in M4 is authorised until the external full-repo security and
penetration audit returns.** M4·2 (pairing UX), M4·3 (packaging/docs) and M4·4 (dashboard) are not
started.

What the audit is being handed: a core with mTLS on every link, signed model distribution, refuse-on-
fail at every layer that can fail, ten freshly-closed defects — and an honest list of what is still
missing, in §6 above.
