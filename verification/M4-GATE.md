# M4 GATE — evidence table

**Assembled 2026-08-25. Status: ⏸️ NOT FLIPPED — this table is presented to the owner for a verdict.**

Format follows `M1-GATE.md` / `M2-GATE.md` / `M3-GATE.md`. Every row is a receipt or an explicit
deferral; a row that is not green says so, and says why. **Rule 16 applies throughout:** CI outcomes
appear as quoted `verdict=` lines or receipt files, never as job status.

> **⚑ TWO ROWS ARE OPEN AND NEITHER IS WAIVABLE BY THE AGENT.** Row 3 (the 24-CPU-hour fuzz DoD) is
> a **countdown the DoD names by number** — 4.150 of 24 — and row 1 (a non-author's 30-minute setup)
> is **the one measurement this project cannot take itself**. They are stated first rather than
> buried, because a gate table whose failures are on page two is doing the opposite of its job.

**Environment reality:** all cloud VMs died 2026-08-05 by plan. Every real-multi-machine result is
**historical/banked** and permanently valid as *demonstrated*. Mac + container-CI only from here.

---

## (a) The BLUEPRINT M4 DoD, verbatim

> *"a non-author can set up a 3-machine cluster from the README in under 30 minutes; security
> checklist from report Addendum 2 §E1/D1 passes (no 0.0.0.0 binds, API auth enforced, GGUF parser
> fuzzed for 24 CPU-hours without crashes)."*

| # | DoD component | Status | Evidence |
|---|---|---|---|
| 1 | **A non-author sets up a 3-machine cluster from the README in < 30 min** | ⏸️ **OPEN — OWNER-SUPPLIED, NOT WAIVABLE** | Protocol: `docs/FRESH-ACCOUNT-TEST.md`. **No non-author has run it.** The README currently claims only that **every command has been executed as written** (rule 23), and **does not claim the 30-minute figure**. This row closes on a dated line in §6 carrying elapsed time and confusion count; each recorded confusion becomes a §8 item. **It is not inferable from the quickstart having been executed by its author** — that is precisely the substitution rule 23 exists to forbid. *(Also: the DoD says three machines, and the project has one. Even a willing non-author cannot fully discharge this row on the hardware that exists.)* |
| 2a | **No 0.0.0.0 binds** | ✅ **MET** | `hydra-transport` refuses a wildcard bind without an explicit opt-in; `security_checklist.rs::a_wildcard_bind_is_refused_and_an_explicit_interface_is_not` drives `0.0.0.0:8080`, `[::]:8080`, `0.0.0.0:0` **and** the IPv4-mapped form (`platform_hardening.rs`, §7.45). The one wildcard bind in the tree, `hydra-2node-ci.rs`, is a container-internal CI harness taking the documented opt-in explicitly. |
| 2b | **API auth enforced** | ✅ **MET** | `ApiAuth::check` runs **before any state is read**, on the generation endpoint *and* the dashboard (M4·4 — the dashboard is another *client* of the surface, not a privileged path; verified live at 401/200). Token length floored by `MIN_API_TOKEN_LEN` (audit H15). **Plus, new 2026-08-25:** `MAX_REQUEST_BODY_BYTES = 1 MiB` applied **router-wide**, with an oracle **verified to discriminate** — remove the layer and a 1 MiB body returns 200, so `axum`'s 2 MiB default would never have caught it. |
| 3 | **GGUF parser fuzzed 24 CPU-hours without crashes** | ⛔ **NOT MET — 4.150 / 24 CPU-hours** | Receipts: `fuzz-32618764167.md`, `fuzz-32618760313.md`, **`fuzz-32795673202.md`**. 0 crashes across **12.3 billion** cases. **The number is 4.150, not 4.4 or 2.4**, and the corrections matter: leg 3 banks **1.750 not 2.000** because `vendored-gguf` spent 0.250 CPU-hours calling **nothing** while printing `verdict=GREEN` (§7.62, now fixed to `UNAVAILABLE`). **⚑ And the cadence claim was wrong: no SCHEDULED leg has ever run** — the cron was added 2026-08-23 and every leg to date is dispatch or push. **~10 further legs reach the bar.** This row is a countdown, not a gap; it cannot be accepted away because the DoD names the number. |
| 3b | **…and which parser** | ⚠️ **NAMED, NOT GREEN** | The 24-hour budget protects the **Rust** parsers. The **vendored** `gguf_init_from_file` — the one a worker actually loads through — is driven only on the dev machine, because **CI never builds the real engine (audit L1)**. Its CI status is **UNAVAILABLE**, and as of §7.62 the log says that word rather than `GREEN`. Composition is the current protection: since §7.43 the worker runs the hardened parser *first*. |

## (b) M4 build items (BLUEPRINT §3 "Build:")

| Item | Status | Evidence |
|---|---|---|
| **Pairing UX (`hydra-cli pair`, QR/PIN)** | ✅ **DELIVERED** | M4·2. **Revocation is NOT included** — see (c). |
| **Signed shard distribution** | ✅ **DELIVERED** | `hydra-modelsvc` splitter + Ed25519-signed manifest, per-tensor BLAKE3; verify-over-fd with `O_NOFOLLOW` (audit H6, §7.43). |
| **Dashboard (read-only)** | ✅ **DELIVERED** | M4·4 (§7.59). **Provenance survives to the screen:** `Unavailable` renders as `unknown`, `BestEffort` as *"estimated"*, a sensorless stage as *"unobserved… not the same as healthy"* — with no cluster average to hide behind. No control actions in v1, and the page says so (asserted: no form, no button, no post). |
| **systemd / launchd units** | ✅ **DELIVERED** | `packaging/systemd`, `packaging/launchd`; token kept out of the unit file. |
| **Docs** | ✅ **DELIVERED, AND EXECUTED** | `README.md`, `docs/BUILD.md`, `docs/SECURITY-CHECKLIST.md`, `docs/UNTRUSTED-INPUT-PARSERS.md`, `docs/FRESH-ACCOUNT-TEST.md`. **Rule 23 came from this work:** executing the quickstart found **three defects reading it had not** (a DER file `curl` refuses; a `--dev` CA unrelated to the paired one; a SAN naming the device where clients dial an address). |
| **Reserved-hook audit** | ✅ **DELIVERED** | M4·1, as *executing assertions* rather than a checklist (`reserved_hooks.rs`, 9 tests + the Option-A half). Found **ten** defects (§7.27), including Option B reachable by default and **no API auth at all**. |
| **Coordinator driver** | ✅ **DELIVERED (M4·0 / 0b / 0c)** | The state machine **is now deployed**: before M4·0 `hydra_state::Coordinator` was constructed in exactly one place in the workspace — the simulator — and every shipping activation was a hand-rolled `COMMIT`→`FINALIZE` with `attempt` hard-coded to 1. Activation, recovery, the strategy path and session termination now run through `ActivationDriver` over real mTLS. **"Crash-safe sessions" is a property of the product rather than of the test harnesses**, and the acceptance run kills a real OS process while holding no connection with which to intervene. |

## (c) Security posture at gate time

| | |
|---|---|
| **External audit** | **4 CRITICAL / 21 HIGH / 20 MEDIUM / 12 LOW accepted as a body; remediation waves 1–5 COMPLETE.** Auditor's own report banked at `verification/audit-2026-08-23-AUDITOR-REPORT.md` and treated as the primary source (rule 20). |
| **v1 threat model** | **Every certificate holder is assumed HONEST.** mTLS + signed manifests defend against **outsiders and accidents**, never a **compromised stage**. Worker-compromise resistance is v2 scope. *Stated loudly rather than fixed quietly* — BLUEPRINT §1.9. |
| **v1-blocking allowance** | ✅ **CLOSED 2026-08-25** — `ACCEPT_LEGACY_ZERO_COMPLETION_HASH` **deleted** (§7.64). Closing it revealed the allowance was load-bearing for `encode_finalize_activation` itself, hence for `hydra-2node-ci`, hence for **`container-2node`** — a flip-to-false would have turned the standing multi-node verifier red. It also revealed that the canonical *"activation succeeds"* test had **never once supplied real completion evidence**. |
| **Pre-release triage** | `verification/M4-PRERELEASE-TRIAGE.md` — **every live §8 residual is CLOSED, proposed-ACCEPT with its release-notes line written verbatim, BLOCK, OWNER, or HARDWARE.** Every ACCEPT is a recommendation; **rule 5 reserves the decision to the owner, and that is what this gate's pause is for.** |

## (d) Standing anchors — all green at assembly time

| Anchor | Result |
|---|---|
| `two_worker_teacher_forced_no_sample_bit_exact` (rule 14) | ✅ green |
| `direct_worker_to_worker_fwd_is_bit_exact` | ✅ green |
| `greedy_sample_across_pipeline_matches_unsplit_argmax` | ✅ green |
| `two_worker_anchor_is_bit_exact_with_shard_loaded_weights` | ✅ green |
| `chunked_prefill_is_bit_exact_with_unchunked_prefill` | ✅ green |
| **Workspace, real arm** | **94 suites / 392 passed / 0 failed / 7 ignored, EXIT=0, 0 mangled lines** |
| **Workspace, engine-stub arm (= a clean checkout)** | **94 suites / 392 passed / 0 failed / 7 ignored, EXIT=0, 0 mangled lines** — *new this session; before 2026-08-25 this configuration did not compile at all* |
| **clippy `-D warnings`, both arms** | ✅ EXIT=0 **under `rustc 1.98.0`, CI's stable** — not the agent's older default (§7.61) |
| `container-2node` | ✅ `verdict=GREEN (two-node docker-kill recovery held)` + `… under tc netem jitter` — run 32795673204, receipt `ci-restored-2026-08-25.md` |
| M1 / M2 / M3 (upstream gates) | ✅ PASSED |

## (e) Deferrals carried honestly into the gate

| Item | Why it is deferred, in its own terms |
|---|---|
| **⛔ Every engine-backed claim is CPU-ONLY evidence** | **All 31 engine-test configurations set `n_gpu_layers: 0`.** The M−1 sweep shows Metal's **KV truncate+replay is not bit-exact** (`max_abs=4.059e-04`, deterministic, **identical on pinned and candidate engines**, so not a bump regression) — on the very operation teacher-forced recovery is built from. **Not a demonstrated token divergence** (inside the 1e-3 tolerance, argmax held). **It is a demonstrated non-exactness on the backend the product actually runs on, underneath claims phrased as *byte-identical*.** Rule 19 at its purest: the oracle cannot produce the failure because it never runs where the failure would live. **Recommended as a NAMED GATE ROW, not an accept** — the check is cheap (re-run the suite at `n_gpu_layers: 99`). |
| **L1 — the layer-window patch is still working-tree state** | `build.rs` refuses to build an unpatched engine and `patch_integrity.rs` asserts the patch matches the committed diff, **but the real fix is a fork SHA and creating that fork is an OWNER action.** Until then CI never builds the engine and `vendored-gguf` stays UNAVAILABLE there. |
| **The submodule bump** | **Both binding conditions DISCHARGED** (§7.63): the 15-combination sweep re-ran **15/15 bit-exact** and the seven-file diff review is done. **The pin is deliberately unmoved** — it is sequenced with L1's fork so the patch is re-ported once. |
| **H18's structural half + M1's connection semaphore** | Travel together or not at all; both belong to the same accept-loop refactor. Timeout bounds the damage. Proposed ACCEPT. |
| **M3's revocation half** | Pairing re-issues but cannot revoke; 397-day leaf lifetime is a ceiling, not a remedy. Proposed ACCEPT under the honest-worker model. |
| **Rule-19 oracle gaps** | **Verified absent 2026-08-25:** no `on_frame` fuzz target, no timeout oracle (a hang is not a crash), no allocation oracle (a large-but-legal reservation is not a crash). Named in §6.0 rather than implied. |
| **Normalise-before-validate sweep** | **Verified not done.** Recommend it stays OPEN rather than accepted — bounded cost, and its payoff is finding the next `recovery::read`. |
| **The M−1 sweep's prompts were never recorded** | A step BLUEPRINT §1.2 makes **binding on every bump** could not be re-*run*, only re-*performed with different inputs*. The 2026-08-25 prompts **are** named. Rule 23's shape applied to a verification procedure. |
| **Wired-LAN envelope · 70B Q4 · 7B split** | 🖥️ Hardware-contingent. **Owed, not waived.** No number may be claimed. |
| **`mutation_preactive_maroon`** | Deferred with its original reason: the sim cannot reach the window until supersession rounds feed the stage track. *Do not contort the scheduler to manufacture it early.* |

---

## VERDICT — ⏸️ **PENDING THE OWNER**

**The agent does not flip this gate.** Two rows are open, and neither is the agent's to close:

* **Row 1** needs a **non-author**, on **three machines**. The project has one machine and one
  author. It is not waivable and not inferable.
* **Row 3** needs **~10 more fuzz legs**. The DoD states the number, so no argument closes it —
  only time. The first *scheduled* leg has not yet been observed to run.

**What the agent asserts, and no further:** every M4 build item is delivered; the security checklist
passes on both of its checkable clauses; the audit's five waves are complete; the one v1-blocking
allowance is deleted; and every remaining §8 residual has an explicit disposition awaiting
ratification.

**What the owner is being asked for:** (1) ratify or overrule each proposed ACCEPT in
`M4-PRERELEASE-TRIAGE.md`; (2) rule on whether the **CPU-only evidence base** is a gate row or an
accepted limitation; (3) create the `llama.cpp` fork so L1 and the validated bump can land; and
(4) run `docs/FRESH-ACCOUNT-TEST.md`, or rule on how row 1 is discharged given the hardware that
exists.
