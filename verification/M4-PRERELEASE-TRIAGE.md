# M4 pre-release triage — every live §8 residual, closed or proposed-for-acceptance

**Purpose (§0(c) work item 2).** Before the M4 gate, every §8 residual is **either closed or
explicitly accepted for v1 with its reason recorded for the release notes**. No item may reach the
gate in the state "still there, nobody said anything".

> **⚑ THESE ARE PROPOSED DISPOSITIONS, NOT DECISIONS.** Accepting a defect for v1 changes what the
> package ships with, and **standing rule 5 reserves that to the owner**. Every "ACCEPT" row below
> is a recommendation with its reasoning exposed so it can be overruled cheaply. The M4 gate is
> where they are ratified — that is what the gate's pause is *for*.

**Verdict vocabulary.** **CLOSED** — fixed and evidenced this session. **ACCEPT (v1)** — ships as a
documented limitation; the release-notes line is written here. **BLOCK** — must not ship. **OWNER** —
cannot be discharged by the agent. **HARDWARE** — needs machines the project does not have.

---

## 1. The one item that blocks v1

| Item | Verdict | Reason |
|---|---|---|
| **`ACCEPT_LEGACY_ZERO_COMPLETION_HASH`** | ✅ **CLOSED 2026-08-25 — the constant is DELETED, not set false** | Details below. **It was not the one-line change its own §8 row advertised**, and the difference is the finding. |

### The v1-blocker is closed, and closing it falsified the row that described it

The §8 row called itself *"a single named constant… this row is its deletion notice"* and its
condition *"it must be `false` before v1 ships"*. Both true. **What the row did not know is that the
allowance was load-bearing for the project's own harnesses.**

Deleting it turned three things red that a one-line flip would have shipped straight into CI:

1. **`commit_then_finalize_reaches_active_final`** — the canonical *"activation succeeds"* test —
   was finalizing with `complete_record_hash: [0u8; 32]`. **The project's happy-path activation test
   had never once supplied genuine completion evidence**, and would have gone on passing had the
   evidence check been broken outright. Swept per rule 17: `recovery.rs` carried the same shape,
   with a comment reading *"ACTIVE_FINAL with evidence"* directly above a line passing zeros.
2. **`multiconn.rs::a_second_connection_is_served_while_the_first_is_held_open`** failed — and that
   one is not a fixture problem. It traced to the **encoder**.
3. **`hydra_wire::encode_finalize_activation` itself wrote `[0; 32]`.** The shipping coordinator
   (`hydra_coordinator::driver`) was already on the evidence-carrying encoder — but
   **`hydra-worker::pair` and the three demo/CI binaries (`hydra-wan`, `hydra-2node-ci`,
   `hydra-3node-kill`) were not.** `hydra-2node-ci` is what `container-2node` runs. **Flipping the
   constant without touching the encoder would have turned the standing multi-node verifier red for
   a reason having nothing to do with rollout compatibility** — five hours after this session
   finished restoring it from exactly that kind of failure.

**Fixed at the encoder, not at its thirteen call sites:** `encode_finalize_activation` now delegates
to `encode_finalize_activation_with_evidence` with the tuple's own hash, so **no caller can emit a
finalize the stage will refuse for want of evidence**. Callers naming a *specific* durable
`ACTIVATION_COMPLETE` record still use the explicit encoder.

**And the oracle that could not previously exist:** `an_all_zero_completion_hash_is_refused_now_that_the_rollout_allowance_is_gone`.
While the constant stood, the exact case H2 was about was the one case the suite was structurally
unable to refuse — rule 19, an oracle blinded by a deliberate allowance.

## 2. Closed in this session

| Item | Evidence |
|---|---|
| **`fuzz.yml` push-path drift** | Filter now names `crates/hydra-wire/**` + `crates/hydra-worker/**`. Rule 22 applied first: the row's named site was verified absent/present before the substance was touched (§7.60). |
| **Rule-17 register gap** | `hydra-wire` owns the parser `wire-body` drives and was in `hydra-fuzz/Cargo.toml` only **transitively**; now a direct dependency. A parser reachable but not *named* is absent from a register whose purpose is to be read (§7.60). |
| **HTTP request-size limit** | `MAX_REQUEST_BODY_BYTES = 1 MiB`, applied **router-wide** so later routes inherit it. Oracle: `a_body_over_the_limit_is_refused_before_it_is_parsed`, **verified to discriminate** — with the layer removed a 1 MiB body returns **200**, proving `axum`'s 2 MiB default would never have caught it. The §8 concern was real for bodies between 1 and 2 MiB. |
| **`vendored-gguf` reporting an unearned GREEN** | Now `verdict=UNAVAILABLE` with a reason, consuming no budget; classify fails on a required target reporting UNAVAILABLE or on `vendored-gguf` reporting GREEN in CI (§7.62). |
| **The clean-checkout build** | `HYDRA_FORCE_ENGINE_STUB=1` + the `clean-checkout` workflow (§7.60). |
| **Submodule-bump conditions (i) and (ii)** | Discharged and banked; the pin is deliberately unmoved (§7.63). |

## 3. Proposed ACCEPT for v1 — each with its release-notes line

| Item | Release-notes line (proposed verbatim) |
|---|---|
| **No API rate limiting** | *"The coordinator's HTTP API enforces bearer-token auth, `Host`/`Origin` validation and a 1 MiB request-body limit, but does **not** rate-limit. A caller holding a valid token can issue requests as fast as the machine accepts them. v1's threat model is one trusted household on a trusted LAN; the token is the control."* **Why accept:** admission control already bounds concurrent *sessions* to one (spec §1.4 Option A), so the unbounded quantity is request rate against a single-session service, not resource growth. |
| **API token handling has no story** (`ApiAuth::new` takes the token from its caller — no keyring, no rotation, no on-disk protection) | *"The API token is supplied by the operator via configuration or environment and is not stored in a system keyring. Rotating it means restarting the coordinator with a new value."* **Why accept:** the §8 row assigned this to M4·2 and **it was not done** — that is stated plainly rather than quietly re-scheduled. It is a UX and key-management gap, not a protocol one, and the token is already length-checked (`MIN_API_TOKEN_LEN`, audit H15). |
| **M3's revocation half** — pairing can re-issue but cannot revoke | *"A device certificate believed compromised cannot be revoked; it remains valid until it expires. Leaf lifetime is 397 days, which is a ceiling, not a remedy. Re-pairing issues new material but does not invalidate old."* **Why accept:** revocation needs a CRL distribution path and a re-pair UX; under the honest-worker assumption a valid cert holder is trusted by construction, so this is a **defence-in-depth** gap, not a hole in the stated model. |
| **H18's structural half** — handshake still inline in `accept()`; **verified 2026-08-25**, `accept()` still returns an already-authenticated `AcceptedConn` | *"A slow (not silent) peer can delay the next inbound connection by up to the 10 s handshake timeout."* **Why accept:** the timeout bounds the damage; moving the handshake into the spawned task changes the shape of every call site and is a refactor with its own regression surface. Bundling it into a release seam mixes two kinds of risk. |
| **M1's connection semaphore** — **verified 2026-08-25**, no per-peer or global permit exists | *"There is no cap on how many simultaneous connections one peer may open."* **Why accept:** it belongs at the same accept loop H18's refactor rewrites; doing them separately means touching it twice. **These two travel together or not at all.** |
| **M9 residual** — a duplicate *older* than the last position is refused (`ERR_GAP`) rather than answered | *"Retransmissions beyond the in-flight window are refused with `ERR_GAP` and a frontier rather than answered from cache."* **Why accept:** already ruled 2026-08-23 as **the right v1 answer** — answering needs either recomputation (R2 forbids it outright) or an unbounded per-position cache, which is exactly the shape H20 flags. A bounded refusal beats an unbounded cache, and the coordinator's resume rule already knows what to do with it. |
| **Coordinator-disk-loss = session loss** | *"The coordinator's commit stream is the single durable record of a session. If its disk is lost, the session is lost; D2 mirroring is not in v1."* **Why accept:** long-standing documented D-mode limitation, already marked *v1 accepted* in §8. |
| **`int8_blockq` boundary precision stays FORBIDDEN** | *"Boundary payloads may be `f32` or `f16`. `int8_blockq` is reserved and not offerable."* **Why accept:** §7.11's measurement stands and the bump's re-run reproduced it (max-abs up to 2.298e+00, top-10 dropping to 9/10). Upholding an existing constraint. |
| **Teacher-forced `APPLIED_ACK` digest still paid on PREFILL and REBUILD** | *"Prefill and recovery catch-up compute a BLAKE3 witness per position (≈9.6 ms/position on the dev model), which is visible in time-to-first-token on long prompts."* **Why accept:** owed **by directive** as an explicit opt-in rather than another heuristic; it is a performance cost on a correctness witness, not a correctness defect. |

## 4. Proposed ACCEPT — verification-reach gaps, stated rather than implied

These are places where the project's **evidence** is narrower than a casual reading of its claims.
They are listed separately because accepting them means accepting a **bound on what may be said**,
not a bug in what ships.

| Gap | Release-notes / claim-bounding line |
|---|---|
| **⛔ Every engine-backed claim is CPU-only evidence** (found 2026-08-25, §7.63; **all 31 engine-test configurations set `n_gpu_layers: 0`**), and **Metal's KV truncate+replay is not bit-exact** (`max_abs=4.059e-04`, deterministic, on **pinned and candidate engines alike**) | *"The bit-exactness and byte-identical-recovery results are measured on the CPU backend. The Metal backend is not exercised by the test suite, and the M−1 sweep shows its KV truncate-and-replay is not bit-exact in at least one case."* **This is the most consequential row in this document** — see §5. |
| **Fuzz: 4.150 / 24 CPU-hours** (corrected 2026-08-25 from 2.400; leg 3 banked 1.750 not 2.000) | *"The 24-CPU-hour parser-fuzzing DoD is not met. 4.15 CPU-hours are banked across three legs, 0 crashes."* **The M4 DoD explicitly names this number, so it cannot be accepted away — it is a countdown, not a gap.** |
| **No scheduled fuzz leg has ever run** (§7.62) | Not a release-notes item; a **record correction**. "~10 more weeks" is "~10 more legs" until a scheduled leg is observed to complete. |
| **Rule-19 oracle gaps** — **verified absent 2026-08-25**: no `on_frame` fuzz target, no timeout oracle (a hang is not a crash), no allocation oracle (a large-but-legal reservation is not a crash) | *"Fuzzing covers the parsers, not `Worker::on_frame`; it detects crashes, not hangs or excessive-but-legal allocations."* **Why accept:** each is additive work with a known shape, and all three are named in §6.0 rather than implied. |
| **TLC cannot construct ill-formed messages** | *"The model-checked guarantees are about the transition core given well-formed messages; malformed-input handling is covered by tests and fuzzing, not by TLC."* Already carried with the permitted claim. |
| **Normalise-before-validate sweep** — **verified not done 2026-08-25** | Not a release-notes item; an **owed reading task** with a grep-able shape. Recommend it stays open rather than being accepted, since its cost is bounded and its payoff is finding the next `recovery::read`. |
| **Container 2-node CI shares one kernel** | *"Multi-node correctness is verified between containers on a single host: no real network partition, no real clock skew, no wired-LAN timing."* |
| **Wired-LAN envelope (M3 row 15), 70B Q4, 7B split** | 🖥️ **HARDWARE** — permanent hardware-contingent annotations, **owed, not waived**. No number may be claimed. |

## 5. The row that should change what the gate says

**The CPU-only evidence base is not like the others.** Every other accept above bounds a *feature*.
This one bounds the project's **headline correctness claim**.

Hydra's differentiator, in its own words, is *"generation streams that survive any single machine
dying mid-sentence without duplicating or losing a single visible token"* — a **byte-identical**
claim. That claim is proven by `d1_recovery`, `three_node_recovery` and the rule-14 anchors, and
**every one of them runs `n_gpu_layers: 0`**. Meanwhile the M−1 sweep shows that on Metal — the
backend an Apple-silicon worker actually uses, and the project's own dev machine is one — KV
truncate-and-replay, *the operation teacher-forced recovery is built from*, is **not bit-exact**.

**What is NOT being claimed here:** 4.059e-04 sits well inside the 1e-3 tolerance, the argmax held,
and **no token divergence has been demonstrated**. This is not a known bug in recovery.

**What is being claimed:** nobody has looked. The gap is invisible to every gate the project has,
because no gate runs on that backend. **Rule 19 in its purest form — the oracle cannot produce the
failure because it never runs where the failure would live.**

**Recommendation: this is a NAMED GATE ROW, not an accept.** The check is cheap — re-run the
engine-gated suite with `n_gpu_layers: 99` and see whether the byte-identity assertions survive. If
they do, the claim is stronger than it was and the row closes. If they do not, the recovery claim
needs a per-backend qualifier **before** it ships, not after.

## 6. Owner-gated

| Item | Why it cannot be discharged by the agent |
|---|---|
| **L1's fork pin** — `PavanManchikatla/llama.cpp` | Creating a remote repository under the owner's account. **The validated tree is ready** (§7.63); the remaining step is mechanical. Until it lands: a clean checkout still cannot build the real engine, CI still never builds it, and `vendored-gguf` stays UNAVAILABLE there. |
| **The submodule bump itself** | Sequenced with the fork so the patch is re-ported once. Both binding conditions are discharged. |
| **The fresh-account run** (`docs/FRESH-ACCOUNT-TEST.md`) | The one measurement the builders cannot take. **A named DoD row that stays OPEN** until the owner supplies it; not waivable by the agent and not inferable from the quickstart having been executed by its author. |
| **Ratifying every ACCEPT above** | Rule 5. |
