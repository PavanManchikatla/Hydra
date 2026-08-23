# Hydra security checklist (M4·1)

**Status: verified 2026-08-23. Every line below is backed by an assertion that runs, not by a
reading.** BLUEPRINT §3's M4 DoD names this checklist explicitly: *"security checklist from report
Addendum 2 §E1/D1 passes (no 0.0.0.0 binds, API auth enforced, GGUF parser fuzzed for 24 CPU-hours
without crashes)."*

> **Scope, stated first.** This is the **author's own** checklist against the report's named threats.
> It is deliberately **not** a substitute for the external full-repo security and penetration audit
> the owner is commissioning — this document is what that audit is handed, not what replaces it.
> Where an item is partly met, it says so and says what is missing.

The v1 trust boundary is **one household LAN**: per-device Ed25519 identity, mTLS with a cluster CA
created at pairing, signed placement and shard manifests, hard size caps validated before allocation,
API auth, and no `0.0.0.0` bind by default (BLUEPRINT §1.9).

---

## §E1 — DNS rebinding / CSRF against the LAN API

> *"Local inference servers (Ollama-class) have been exploited via browsers on the same LAN hitting
> unauthenticated localhost/0.0.0.0 HTTP APIs. The coordinator's OpenAI endpoint and dashboard need
> auth even on LAN, Host/Origin validation, and must not bind 0.0.0.0 by default."*

The threat model worth naming: the attacker is **not** a user on the network. It is a **web page the
victim is merely visiting**, which executes in a browser that already holds the victim's network
position. That is why "it is only on localhost" is not a defence.

| Requirement | Status | Proof |
|---|---|---|
| API auth **even on the LAN** | ✅ | `ApiAuth` — bearer token, **required by the type**: `AppState::new` takes an `ApiAuth`, there is no `none()` and no `Option`, so "auth not configured" is unreachable. `session_http.rs::the_api_refuses_an_unauthenticated_request` (missing, wrong, and *prefix-of-correct* token all refused, with a correct-token control) |
| Token comparison does not leak | ✅ | Both sides are BLAKE3-hashed and the 32-byte digests compared, so neither the token's length nor a matching prefix is observable. Asserted by the prefix case above |
| `Host` validation (rebinding) | ✅ | Allow-list, port included. A **missing** `Host` is refused rather than exempted — otherwise "send no Host" is the way around the check. `session_http.rs::the_api_refuses_a_foreign_host_header` |
| `Origin` validation (CSRF) | ✅ | Foreign origin refused; **absent** origin allowed (a normal API client sends none) — which is safe only because `Host` is checked unconditionally. `session_http.rs::the_api_refuses_a_cross_site_origin_but_allows_a_plain_client` |
| A refusal has no side effect | ✅ | `session_http.rs::a_refused_request_starts_no_session_and_no_generation` — no session, no generation, with an admitted-request control |
| Never bind `0.0.0.0` by default | ✅ | `hydra_transport::check_bind_addr` refuses any unspecified address; **opt-in only**, via `HYDRA_ALLOW_WILDCARD_BIND=1`. Checked **before** the socket exists, so there is no window in which the port is open while the decision is being made. `security_checklist.rs::{a_wildcard_bind_is_refused_and_an_explicit_interface_is_not, the_real_listener_refuses_a_wildcard_bind}` |
| …and nothing opts in by accident | ✅ | **Repository-wide** assertions: exactly one file carries the opt-in (the containerised CI runner, whose network namespace *is* the isolation boundary and whose port is published only on `127.0.0.1`), and no other file hard-codes a wildcard listen address. `security_checklist.rs::{only_the_container_ci_runner_opts_into_a_wildcard_bind, no_source_file_hardcodes_a_wildcard_listen_address}` |

**Deliberately an opt-in, not an opt-out:** an opt-out is a flag someone forgets to set; an opt-in is
a decision someone had to make.

**Named gap:** there is **no dashboard yet** (M4·4), so the dashboard half of §E1 is not-applicable
rather than done. When the dashboard lands it must route through the same `ApiAuth`, and this row
becomes a real one.

---

## §D1 — Model files are untrusted input

> *"GGUF parsing has had real CVEs in llama.cpp (2024 heap-overflow class). A cluster that
> auto-downloads community quantizations and streams shards to every device multiplies the blast
> radius. Fix: hash-pinned manifests from a trusted source, hardened/fuzzed parser path, ideally a
> sandboxed loader."*

| Requirement | Status | Proof |
|---|---|---|
| Hash-pinned manifests from a trusted source | ✅ | `hydra-modelsvc::manifest` — per-tensor BLAKE3 + the three admission hashes + the layer-range map, **Ed25519-signed** (`ring`). A worker verifies the signature and every tensor hash **before** any shard weights are read and **refuses** on any mismatch — structured error, never a warning (`hydra-worker::shard`, P2·10b) |
| Hardened parser: no allocation from a declared count | ✅ | **Defect found and fixed 2026-08-23** — `Vec::with_capacity(declared)` on header counts let a 40-byte file request **64 TiB** and abort the process. `Cursor::reserve_for` now clamps every reservation to what the remaining input could justify. `gguf_hardening.rs::a_declared_count_far_beyond_the_file_does_not_allocate_for_it` |
| Hardened parser: checked shape arithmetic | ✅ | **Defect found and fixed 2026-08-23** — the dims product and byte-length multiply were unchecked: a debug panic, and in **release** a *silent wrap to a small length* that would then size a read against a real buffer. That release behaviour is the heap-overflow shape itself. `gguf_hardening.rs::an_overflowing_declared_shape_is_an_error_not_a_wrap` |
| Hardened parser: offsets checked before narrowing | ✅ | `offset + len` is a checked `u64` add compared against the data length **before** either side becomes a `usize`. `gguf_hardening.rs::a_wrapping_tensor_offset_is_refused_rather_than_wrapped_into_range` |
| Hardened parser: hostile `general.alignment` | ✅ | Zero, non-power-of-two and `u32::MAX` all survivable; `align_up` saturates rather than overflowing. `gguf_hardening.rs::a_hostile_alignment_value_does_not_overflow_the_data_offset` |
| Fuzzed for 24 CPU-hours without crashes | 🔄 **accumulating** | `crates/hydra-fuzz` + `.github/workflows/fuzz.yml` (dispatch + weekly, 8 shards × distinct seeds). **CPU-hours are accumulated across banked receipts and the DoD is met when they add up — not before.** Running total in `verification/ci-results/`. Fast lane on every push: `hydra-fuzz/tests/fuzz_smoke.rs` |
| Sandboxed loader | ❌ **not done** | The report calls it "ideally". Not attempted in v1; the engine loads in-process. Recorded as an owed item rather than quietly dropped |

**On the fuzzer's kind, stated plainly:** it is a **deterministic structure-aware mutation fuzzer**,
not coverage-guided (libFuzzer/AFL). It gives up coverage feedback; it gains reproduction from a
`(seed, iteration)` pair and a stable-Rust CI arm this project can actually watch run (standing rule
12). Adding `cargo-fuzz` targets over the same entry points is strictly additive and is an owed item.

---

## Frame, tensor and record limits — enforced **pre-allocation**

`hydra-proto.fbs` declares five hard caps. The M4·1 audit found that **only `MAX_FRAME_BYTES` was
ever called**; the other four existed in `limits.rs` and were invoked from nowhere in the tree.

> A cap that is defined and never called is documentation, not enforcement. And *"the frame cap
> already bounds it"* is not an answer: it is a different cap on a different quantity. A legal 64 MiB
> frame could carry a 60 MiB tensor against a declared 48 MiB tensor cap, and `n_positions` is a
> `uint16` — a peer could declare 65 535 positions, 64× the cap, in a frame whose *bytes* are
> unremarkable.

| Cap | Value | Where enforced | Proof |
|---|---|---|---|
| `MAX_FRAME_BYTES` | 64 MiB | `FrameHeader::parse`, from the 12-byte header alone | `wire_limits.rs::the_frame_cap_is_enforced_at_the_header_before_any_payload_is_read` |
| `MAX_TENSOR_BYTES` | 48 MiB | `wire::check_boundary_tensor`, before `bytes_to_f32_le` copies | `wire_limits.rs::an_oversized_tensor_is_refused_before_it_is_copied` |
| `MAX_POSITIONS_PER_FRAME` | 1024 | `wire::check_boundary_tensor`, on `Fwd` **and** `BoundaryCopy` | `wire_limits.rs::an_oversized_position_count_is_refused` (its control was rewritten by audit C3 — see below) |
| `MAX_SNAPSHOT_BYTES` | 1 MiB | `wire::capped_bytes`, on sampler snapshots | `wire_limits.rs::an_oversized_sampler_snapshot_is_refused` |
| Fixed-width digests | 32 B | `wire::capped_bytes` | `wire_limits.rs::a_fixed_width_digest_field_is_capped_at_its_width` |

**Enforcing the position cap exposed a tenth defect, and it is the most instructive one here.**
`encode_fwd` had been writing `activations.len()` — the **float** count — into `n_positions` (the
line carried a `// placeholder` comment). Harmless while nothing read the field; live the moment the
cap became real. The dev model's `n_embd` is **896**, under the 1024 cap, so every test would have
stayed green — while **every larger model would have had its boundaries refused** (a 7 B has
`n_embd = 4096`). A correct-looking security fix would have shipped and broken the product on the
first real model. Fixed, and pinned by
`wire_limits.rs::wire_caps_hold_for_boundary_widths_the_dev_fixture_never_reaches`, which drives
896/1024/1536/2048/4096/8192-float boundaries so the dev model's narrowness cannot hide it again.
*(Row corrected 2026-08-23: it previously named `a_real_boundary_wider_than_the_position_cap_still_decodes`,
a test that had been renamed — a row pointing at a test that does not run is the §7.31 class.)*

### Boundary-tensor shape — cross-checked **before the FFI** (audit Wave 1c, 2026-08-23)

The caps above bound *how much*; they said nothing about *whether the parts agree*. A `Fwd` /
`BoundaryCopy` frame makes three independent claims about its own shape — `n_positions`,
`Tensor.dims`, and the byte count — and before C3 the codec discarded the first two and let the
third define the shape: the engine derived `n = len / n_embd`, so two positions' worth of bytes
under `n_positions = 1` were applied as two positions, and a byte count that was not a multiple of
four was silently truncated by `chunks_exact`. The only component that ever compared a declaration
to a length was the FFI shim, and it compared the wrong two things.

| Check | Where | Proof |
|---|---|---|
| **C3** `dims == [n_positions, n_embd]`, `n_embd ≥ 1`, `data.len() == n_positions × 4 × n_embd` | `wire::check_boundary_tensor` (engine-free: internal consistency), before `bytes_to_f32_le` copies; on **both** boundary bodies | `wire_limits.rs::a_boundary_whose_bytes_disagree_with_its_declared_shape_is_refused`, `…::a_boundary_copy_whose_bytes_disagree_with_its_declared_shape_is_refused` |
| **C3** declared `n_embd == engine.n_embd` | `worker::Engine::apply_boundary`, before `hydra_apply` | `pre_ffi.rs::a_self_consistent_boundary_of_the_wrong_width_is_refused_before_the_ffi` |
| **C3 (disk)** the same shape check on a `BOUNDARY_COPY` record read back from the boundary store | `hydra_coordinator::boundary_store::decode_boundary_record` + the `boundary-record` fuzz target | `fuzz_smoke.rs::the_boundary_record_parser_survives_a_seeded_hostile_corpus` |
| **M4** `n_positions == 1` in v1 | `wire::check_boundary_tensor` | `wire_limits.rs::more_than_one_position_per_boundary_is_refused_in_v1` |
| **H7** `n_positions ≤ n_batch` | `worker::Engine::apply_boundary` (unreachable from the wire once M4 holds) **and** `hydra_engine_sys::Context::apply` (a public API with multi-position callers) **and** the shim | `pre_ffi.rs::a_position_count_above_n_batch_is_refused_by_the_wrapper_before_the_shim` |
| **`i32::try_from`** on every network-derived position, bounded to `[0, n_ctx)` | `worker::wire_pos` (`APPLY_TOKEN.input_pos`, `FWD.first_input_pos`, `BEGIN_RECOVERY.truncate_to`) | `pre_ffi.rs::a_position_outside_the_context_or_i32_is_refused_before_the_ffi` |
| **M5** `token_id < n_vocab` **before it becomes durable** (coordinator) and before it becomes compute (worker) | `Session::push_sampled` (refused token leaves no trace in buffer, disk or event log); `worker::Engine::apply_token`; the shim | `session_http.rs::an_out_of_vocabulary_token_is_refused_before_anything_is_written`, `pre_ffi.rs::an_out_of_vocabulary_token_is_refused_before_the_ffi` |
| **`try/catch(...)`** on all 14 shim entry points — a C++ exception can never cross the C ABI into Rust | `csrc/hydra_engine.cpp` (`HYDRA_GUARD_BEGIN/END`) | by inspection: 14 entry points, 14 guards (`grep -c HYDRA_GUARD_BEGIN` = 15 incl. the define) — **no test can provoke a throw on demand without a hostile GGUF; recorded as an inspection row, not a proof row** |

**The blind oracle, named.** `wire_limits.rs::an_oversized_position_count_is_refused` used as its
*control* a frame with one position's bytes declaring `n_positions = 1024` — it asserted the exact
inconsistency C3 forbids was legal, and would have gone red on the fix. The oracle was "the cap
check", asked one question about one of three mutually-constraining claims, with a fixture chosen
for convenience; an oracle that checks one claim in isolation cannot express a violation of the
claims' *relationship*. Same shape as the checklist rows and M6's hard-coded simulator fsync: a
harness that cannot produce the failure it nominally guards.

> **Turning a documented-but-unenforced constraint into an enforced one is not a free change.** It is
> a behaviour change against every producer that was quietly violating it — and a small dev model is
> exactly the wrong oracle for noticing.

**Why "before" is the whole claim:** FlatBuffers access is zero-copy, so the allocation happens when
the decoder copies a field out. The cap has to sit between the two. Each test therefore asserts the
**error kind** — `LimitExceeded { what, value, cap }` — because naming the quantity and the cap is
only possible at the point the check is actually made.

---

## mTLS on every link

| Requirement | Status | Proof |
|---|---|---|
| Every cluster link is mutually authenticated | ✅ | `hydra-transport` exposes only `TcpMtls`; `security_checklist.rs::the_transport_exposes_no_plaintext_path` asserts **repository-wide** that no raw `TcpStream::connect` / `TcpListener::bind` exists outside the mTLS module |
| The cluster CA is the trust boundary | ✅ | `security_checklist.rs::a_peer_from_a_foreign_ca_is_rejected_at_the_handshake` — a peer holding a certificate from a *different* cluster CA cannot complete the handshake |
| Shard distribution is authenticated too | ✅ | Ed25519-signed manifests (above) — the security posture extends to model distribution, not just to sockets |

---

## Reserved hooks are fenced, not merely unimplemented

Full detail in `crates/hydra-worker/tests/reserved_hooks.rs` and PROJECT_STATE §7.27. The
security-relevant summary: **a reserved field that exists but is not validated is worse than one
that does not exist** — it is an accepted input on a code path nobody implemented.

| Hook | Status | Proof |
|---|---|---|
| `Fence.branch_id` ≡ 0 | ✅ **fixed 2026-08-23** (was written and never read) | `reserved_hooks.rs::a_nonzero_branch_id_is_refused` |
| `Fence.model_instance_id` validated, never branched on | ✅ | `a_foreign_model_instance_id_is_refused_at_f1`; and it is absent from `FenceView`, asserted by an exhaustive destructure that **fails to compile** if it is ever added |
| `DType::I8_BLOCKQ` not offerable | ✅ | `an_i8_blockq_boundary_is_refused_as_reserved` |
| `Tensor.block_scales` only with `I8_BLOCKQ` | ✅ **fixed 2026-08-23** (was accepted-and-ignored) | `block_scales_on_a_non_i8_tensor_is_refused_not_ignored` |
| Option B (multi-session per model instance) absent | ✅ **fixed 2026-08-23** (was the *default*) | `session_http.rs::a_second_concurrent_session_is_refused_option_b_stays_reserved` |
| No out-of-scope message type has a wire surface | ✅ | `the_wire_body_union_is_exactly_the_spec_4_message_inventory` |
| An unimplemented body is refused, never dropped | ✅ | `an_unimplemented_but_in_spec_body_is_refused_never_silently_dropped` |

---

## Refuse-on-fail everywhere — no silent downgrade

This is a posture rather than a single check, so it is listed as the places it is *asserted*:

| Path | The forbidden outcome | Proof |
|---|---|---|
| Shard manifest verification | Loading a shard whose signature or tensor hash does not verify | `hydra-worker::shard` refuse-on-fail (P2·10b) |
| Durability | A failed or stalled `fdatasync` advancing a watermark, or a session degrading silently instead of failing per I9 | `chaos_disk.rs` (5 tests, incl. ten retries with no half-progress) |
| Sampler drift | Silently repairing a checkpoint-id or config-hash mismatch | `ERR_CHECKPOINT_MISMATCH`, fatal (M2 slice 3) |
| Admission | Trimming a context to make it fit | `hydra-sched::admission` — **the API cannot express it**; a smaller context must be asked for |
| Placement | Treating an unpriced link or an unmeasured device as free | `hydra-sched::solver` — disqualifies the placement |
| Telemetry | Reading "no sensor" as a good value | `Unavailable` carries **no value at all**; `pressure()` returns `Unknown`, never `Nominal` |
| Boundary precision | Widening a narrower dtype instead of refusing it | `reserved_hooks.rs::a_non_f32_boundary_is_refused_rather_than_widened` |
| Wire caps | Truncating an oversized record instead of refusing it | `wire_limits.rs` (above) |

---

## Known gaps, carried honestly

1. **Sandboxed model loader** — not attempted (report §D1 "ideally"). The engine loads in-process.
2. **24 CPU-hours of fuzzing** — accumulating; the claim is made only when the banked receipts add up.
3. **Coverage-guided fuzzing** — the current arm is deterministic and structure-aware, not
   coverage-guided. Additive upgrade, named in PROJECT_STATE §8.
4. **Dashboard auth** — not applicable until the dashboard exists (M4·4).
5. **Rate limiting / request-size limits on the HTTP surface** — not implemented. Auth is the current
   control; a valid token can still submit an arbitrarily large body.
6. **`hydra-cli pair` (QR/PIN pairing UX)** — M4·2. Today a cluster CA is provisioned
   programmatically, so the *pairing ceremony's* security properties are not yet exercised.
7. **Secret handling** — the API token is passed to `ApiAuth::new` by the caller; there is no keyring
   integration, rotation, or on-disk protection story yet.
8. **This checklist is self-assessment** — the external audit is the point of the pause that follows.
