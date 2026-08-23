# Hydra Penetration Audit — THE AUDITOR'S ORIGINAL REPORT (transcribed verbatim)

> **⚑ PROVENANCE — read this first.**
>
> This is the **auditor's own report**, transcribed from `Hydra-Penetration-Audit.docx` (the file
> the owner was supplied with). It is the document `verification/audit-2026-08-23.md` has been
> asking for since Wave 1: that file is the **design authority's directive** — a summary of these
> findings — and it says so in its own provenance note, adding that *"the remainder are known only
> as counts"* and *"the auditor's original report should replace or accompany this file."*
>
> **It now accompanies it.** Every finding below is named, with its severity, its confirmation
> status, its file:line references, its exploit, and its proposed fix. **The 12 lows and the 20
> mediums are no longer "named only by count".**
>
> **Transcription note:** text is verbatim from the document's paragraphs; `<code>` spans are
> rendered as fenced blocks. Nothing has been paraphrased, summarised, or reordered. Where this
> report and the directive disagree, **this file is the primary source** and the directive is a
> secondary reading of it — see `audit-2026-08-23.md`'s reconciliation table for where the two
> differ and what that changed.
>
> **Scope stated by the auditor:** commit `a025d9b` (main), llama.cpp `13f2b28b` + layer-window
> patch, read-only.

---

Hydra Penetration Audit

Independent, adversarial, from-scratch read of the Hydra distributed-LLM inference runtime, commissioned after the team's own M4·1 hardening pass. Scope: the core runtime — transport, wire protocol, fencing, model supply chain, durability, the FFI boundary, secrets, and dependencies. No files in the repository were modified.

2026-08-23   ·   COMMIT A025D9B (MAIN)   ·   LLAMA.CPP 13F2B28B + LAYER-WINDOW PATCH   ·   READ-ONLY


## Contents


## Summary

The ten-defect internal pass was real work and the things it fixed stay fixed. But the pass audited the checklist, and the checklist's own framing — "every line is backed by an assertion" — is exactly where this audit found its gaps: several assertions prove a narrower property than the row they back. The four critical findings share one shape: a guarantee exists in one layer and is assumed, not enforced, in the next.

4

CRITICAL

21

HIGH

20

MEDIUM

12

LOW / INFO

The four that matter most. (1) The signed-manifest gate verifies against a public key carried inside the manifest, so anyone can sign any model. (2) mTLS proves a peer holds a cluster certificate but the verified identity is discarded at accept() — any worker's key is the coordinator's key. (3) A forward frame's declared n_positions is capped and then thrown away; the real position count is derived from the payload length and reaches a GGML_ASSERT that aborts the worker. (4) The data plane carries epoch and generation fences that the worker echoes but never checks. Findings 2 and 4 together mean the entire formally-verified fencing story is enforced only against honest senders — which is the one thing the model assumes and the transport does not provide.

What held up. No custom or disabled certificate verifier anywhere; the cluster CA private key never leaves the coordinator process; every FlatBuffers root goes through the verifier; the five wire caps are checked before copy; the three 2026-08-23 GGUF fixes are correct as written; check_bind_addr sits in front of the only listener; watermark advances are strictly after sync_data on the same fd; the abort/complete exclusion (I25) and the §6.5 restart classifier match the TLA model; cargo audit reports zero advisories across 122 crates.

Read the severities against the right threat model. The spec (§12) declares untrusted workers out of scope and assumes a trusted LAN. Under that model, the fencing findings (C2, C4, H1–H5) are "by design". They are rated here against the threat model this engagement was asked to attack — any holder of a cluster certificate, including a compromised worker — because the blueprint's own security posture (per-device identity, mTLS, signed manifests) is only worth building if that is the adversary. Findings in the supply chain, HTTP, WAL, and FFI sections do not depend on this choice.


## Critical


### C1 — Manifest signature verifies against a key embedded in the manifest itself

**CRITICAL   CONFIRMED**

crates/hydra-modelsvc/src/manifest.rs:100-104 · crates/hydra-worker/src/shard.rs:73-77

```text
pub fn verify(&self) -> R<()> {
    let sig = self.signature.as_ref().ok_or(ManifestError::Unsigned)?;
    let pk = signature::UnparsedPublicKey::new(&signature::ED25519, &self.signer_pubkey);
    pk.verify(&self.canonical_bytes(), sig)...
```

signer_pubkey is read from the manifest bytes and is itself inside the signed payload. Nothing in the workspace compares it to a pinned, configured, or cluster key — grep signer_pubkey finds only the manifest codec, the splitter (which zero-fills it), and a CLI print. verify_shard calls manifest.verify() with no other argument.

**EXPLOIT  Run hydra-modelsvc split hostile.gguf out with no --key: the tool mints a fresh keypair and emits a manifest that verifies. Place it where the worker's config points. Every "refuse-on-fail" check downstream passes; the worker loads arbitrary weights, metadata, layer ranges, and (via H6) hands an attacker-controlled file to llama.cpp's parser.**

**FIX  Replace verify() with verify_against(trusted: &[u8;32]) that requires self.signer_pubkey == trusted (or ignores the embedded key entirely) before the Ed25519 check. Source the trusted key from the bootstrap / cluster identity. Remove the argument-less form from the public API so no caller can reach the weak path. Pair with H14 so a stale signed manifest cannot be substituted either.**


### C2 — mTLS peer identity is discarded at accept; any cluster certificate speaks for any role

**CRITICAL   CONFIRMED**

crates/hydra-transport/src/tcp_mtls.rs:56-60 · framed.rs:26-33 · tls.rs:128-142 · crates/hydra-worker/src/worker.rs:228, 533-547

```text
let (tcp, _peer) = self.listener.accept().await?;
let tls = self.acceptor.accept(tcp).await?;
Ok(Conn::new(tls))            // peer: None — Conn::with_peer / peer_identity() have zero call sites
```

The client verifier requires a chain to the cluster CA and nothing more. Every identity is issued with both ServerAuth and ClientAuth EKUs. No SAN/CN/SPKI is ever read; on_frame has no notion of who sent a frame; the frame tag is an unkeyed BLAKE3 (integrity only). The client→server direction is name-bound via SNI, which proves "I am talking to s2", not "only the coordinator may talk to me". The only remaining gate is SessionKeys, which are static per session and present in every worker's bootstrap (and in every shipped binary are SessionKeys::dev(<const>), M12).

**EXPLOIT  A compromised stage (or any local user who reads a 0644 .boot file, H17) connects to S_P or S1 and issues SAMPLE_NEXT, BEGIN_RECOVERY, COMMIT_ACTIVATION, INSTALL_SAMPLER_CHECKPOINT, CATCH_UP_CONTEXT, or FWD — full coordinator and upstream authority. H1–H5, H19, H20 and M9 are all instances of this.**

**FIX  In accept(), extract peer_certificates()[0], parse its SAN, attach via Conn::with_peer. Bind each message family to an allowed sender: activation/recovery/sampler control = the coordinator identity from the bootstrap; FWD = the configured upstream rank; DURABILITY_ACK = the durability target. Prefer role-distinct EKUs (coordinator ClientAuth-only). Optionally key the frame tag per session.**


### C3 — Forward-frame position count is derived from payload length, not the capped field; oversize batch aborts the worker

**CRITICAL   CONFIRMED**

crates/hydra-worker/src/wire.rs:252-275 · crates/hydra-engine-sys/src/lib.rs:276-284 · csrc/hydra_engine.cpp:176-191 · vendor/llama.cpp/src/llama-context.cpp:1732

```text
// wire.rs — n_positions is range-checked, then dropped; Msg::Fwd has no such field
Ok(Msg::Fwd { first_input_pos, no_sample, activations: bytes_to_f32_le(t.data().bytes()) })
// engine-sys — the real count
let n = boundary_in.len() / self.n_embd as usize;
// llama.cpp — unconditional (not NDEBUG-gated)
GGML_ASSERT(n_tokens_all <= cparams.n_batch);
```

On a middle stage the boundary_out length check accidentally rejects a mismatch. On the final stage (boundary_out = None) there is no guard. Every deployed configuration uses n_batch = n_ctx with n_ctx = 64. The control case in wire_limits.rs::an_oversized_position_count_is_refused sends 3 584 bytes labelled as 1 024 positions and asserts is_ok() — the test enshrines the inconsistency. docs/hydra-engine-sys-sketch.md:72 claims "every call is total: it returns a status, never aborts."

**EXPLOIT  One FWD with n_positions = 1 and (n_ctx+1) × n_embd × 4 bytes of data (~233 KB for the dev model, far under every cap) → SIGABRT on the whole worker process. Not a task panic; nothing in Rust sees it. Below the abort threshold, a multi-position frame advances KV by n positions while the state machine assumes one, silently desynchronising applied.**

**FIX  In decode_body, require data.len() == n_positions × 4 × n_embd (the worker knows n_embd), and in v1 require n_positions == 1. Independently, have Context::apply_boundary take n explicitly and reject n > n_batch before the FFI; in hydra_apply reject n > llama_n_batch(ctx) with HYDRA_E_ARG. Fix the control case in the test.**


### C4 — Data-plane frames are not fenced: FWD / APPLY_TOKEN accepted in any stage state, any epoch, any generation

**CRITICAL   CONFIRMED**

crates/hydra-worker/src/worker.rs:231-248 · wire.rs:131-165

wire::decode checks the four identity vectors and branch_id. view.epoch is used only to echo into replies (:238, :245); view.stage_generation is decoded and read by no code in the tree; self.stage.state() is never consulted on the data path (spec F1/I20: serve only from ACTIVE_FINAL). Positions are narrowed with as i32.

**EXPLOIT  With C2, any certificate holder sends FWD{first_input_pos=p, activations=garbage} to S_P → KV[p] overwritten, all later logits poisoned, the coordinator commits whatever S_P samples. I3 ("the ledger determines output") breaks silently — the APPLIED_ACK witness is only emitted under NO_SAMPLE. A frame from a previous epoch, replayed after recovery, is applied to the new epoch's KV (I1/I7a). A PREACTIVE or FROZEN stage serves decode (I20/I22). input_pos = 2³² + 5 applies at position 5.**

**FIX  In on_frame, refuse data-plane bodies unless view.epoch == stage.epoch(), view.stage_generation == stage.generation(), and the state is serve-eligible; reply ERR_FENCED. Use i32::try_from on every position. Add the per-shard applied_pos idempotency check the spec's I1/R2 require (M9).**


## High


### H1 — Forged COMMIT_ACTIVATION with attempt u32::MAX permanently fences out the real coordinator

**HIGH   CONFIRMED**

crates/hydra-state/src/stage.rs:143-145, 183-197, 226-239

```text
fn attempt_passes_fence(&self, attempt: AttemptId) -> bool {
    cfg!(feature = "mutation_no_attempt_fence") || attempt >= self.highest_attempt
}
```

The F2 fence is >= with no upper bound. One COMMIT_ACTIVATION{attempt: u32::MAX} moves the stage to PREACTIVE(u32::MAX); every real attempt is then Fenced, FINALIZE and ABORT fail their equality checks, and neither RESET_RECOVERY_ATTEMPT (sets attempt = 0, leaves highest_attempt) nor BEGIN_RECOVERY Case A clears the floor. Stage is bricked until restart. The Preactive→higher-attempt supersede branch at :235-239 is not in the TLA model (StageRecvCommitAt allows only FROZEN_READY or same-attempt replay), and TLC explores attempts only in 1..MaxAttempt.

**FIX  Sender binding (C2); accept attempt ∈ {highest_attempt, highest_attempt+1} only (the coordinator increments by exactly one); define highest_attempt semantics for reset / Case A in the spec — it is silent today.**


### H2 — FINALIZE_ACTIVATION accepted with no completion evidence; a forged finalize produces a stage-side I25 violation

**HIGH   CONFIRMED**

crates/hydra-worker/src/wire.rs:311-314 · crates/hydra-state/src/stage.rs:249-256

completion_id and complete_record_hash are dropped on decode (the encoder sends completion_id: 0). The stage checks state == Preactive && attempt == self.attempt — no epoch or recovery_id check (the model checks m.tgt = stEpoch[s]). Scenario: coordinator sends COMMIT(a); attacker sends FINALIZE(a); stage is ACTIVE_FINAL with final_evidence. Coordinator then durably ABORT(a)s → stage ignores it; COMMIT(a+1) falls to the silent _ => Vec::new() arm and is never acked. A later BEGIN_RECOVERY hits Case B′ — which the spec calls a fatal audit event.

**FIX  Sender binding; carry and check complete_record_hash on the stage; check view.epoch == self.epoch.**


### H3 — Forged BEGIN_RECOVERY freezes a serving stage and wipes its KV cache

**HIGH   CONFIRMED**

crates/hydra-state/src/stage.rs:162-169 · crates/hydra-worker/src/worker.rs:267-272

Case A checks only epoch == base; target is not required to be base+1, recovery_id is taken from the header unverified, truncate_to is unbounded and negative values are accepted. kv_truncate((truncate_to + 1).max(0) as i32) with truncate_to = 0 (or anything negative, or i32::MAX via wrap — M4) discards the entire KV; applied.min(truncate_to) can go negative.

**FIX  Sender binding; require target == base+1, 0 ≤ truncate_to < n_ctx; i32::try_from.**


### H4 — Activation quorum is a count of self-reported ranks; one compromised stage can forge the whole quorum

**HIGH   SUSPICIOUS — PRODUCTION DRIVER DOES NOT EXIST YET**

crates/hydra-state/src/coordinator.rs:164-169, 254-259, 294-298 · crates/hydra-worker/src/worker.rs:409 · tests/three_node_recovery.rs:132-139

all_committed() / all_finalized() are len() == n_stages over a set keyed by rank; rank comes from the replying worker's own config, and the wire decode of ACTIVATION_COMMITTED carries no rank at all. The TLA model collects acks as a set of sender stages — set semantics give one-ack-per-stage for free; the implementation has no equivalent. There is no production coordinator-side network driver today (the ack-counting logic lives in harness bins), so this is a latent defect in the SM that the driver will call.

**FIX  Key the ack set on the mTLS-authenticated sender bound to a stage index; reject a second ack from the same peer for a different rank.**


### H5 — Durability plane: unauthenticated boundary writes, max-based frontier, swallowed append errors → acks over holes; duplicates double-apply

**HIGH   CONFIRMED**

crates/hydra-coordinator/src/boundary_store.rs:72-93 · crates/hydra-worker/src/bin/hydra-3node-kill.rs:114, 273 · hydra-3node-wan.rs:150 · crates/hydra-worker/src/retain.rs:38-66

```text
let d = store.append_boundary(...).unwrap_or(-1);       // error swallowed, loop continues
self.durable_through_input_pos = self.durable_through_input_pos.max(first_input_pos);
```

A failed append for p followed by a successful one for p+1 acks durable_through = p+1; R3′ releases p from the upstream retain buffer; D1 recovery then has no boundary p. read() does not dedupe by position and the consumer indexes by count, so a retransmitted BOUNDARY_COPY rebuilds position p twice and omits the last one (I7a). The stored record carries no session/epoch fence. A forged APPLIED_ACK{i64::MAX} (trusted via max) releases every retained boundary.

**FIX  Require first_input_pos == durable_through + 1; make append errors fatal to the connection; dedupe on read with an epoch fence; index by position; require cumulative_input_pos ≤ input_pos just forwarded.**


### H6 — The worker never runs the hardened GGUF parser; it hashes the file, then hands the path to llama.cpp's parser (TOCTOU; wrong parser fuzzed)

**HIGH   CONFIRMED**

crates/hydra-worker/src/shard.rs:91-95, 119-121 · crates/hydra-engine-sys/src/lib.rs:150-153 · csrc/hydra_engine.cpp:48-56

```text
let bytes = std::fs::read(shard_path)...;   // hashed from this open()
...
Model::load_shard(&v.shard_path, ...)      // llama_model_load_from_file(path) — second open() + mmap
```

The Rust Gguf::parse — and the entire 24-CPU-hour fuzz budget — protects the offline splitter only. The parser on the worker's load path is the vendored C++ one, which is not fuzzed by this project. Replacing the file (or re-pointing a symlink) between the two opens loads unverified bytes. Under C1 the bytes reaching llama.cpp are attacker-chosen anyway.

**FIX  Open once with O_NOFOLLOW, fstat regular file, hash via the fd, and load from the same fd/mapping (a gguf_init_from_file-over-fd shim, or llama_model_load_from_buffer). At minimum compare (dev, ino, size, mtime) before and after. Add a fuzz target that drives the vendored gguf_init_from_file with gen::gguf_case output.**


### H7 — Out-of-vocab token id from SAMPLED throws a C++ exception across extern "C"; it is committed to the WAL first, so the crash replays on every restart

**HIGH   CONFIRMED**

crates/hydra-coordinator/src/session.rs:30-31, 136 · crates/hydra-tokenizer/src/tokenizer.rs:49-50 · csrc/hydra_engine.cpp:128 · vendor/llama.cpp/src/llama-vocab.cpp:3090-3093, 4326-4334

```text
return id_to_token.at(id).attr;      // std::out_of_range — no try/catch between here and Rust
```

token_id is a u32 from the wire, cast as i32 (negative for ≥ 2³¹), never compared to n_vocab in coordinator, tokenizer, or shim. The Rust declaration is extern "C" (nounwind): unwinding into it is abort at best, UB at worst. The id lands in GENERATION_COMMIT before detokenisation, so recovery::read feeds it straight back. (The decode path is safe — llama-batch.cpp rejects out-of-range ids by return value; only token_to_piece is unguarded.)

**FIX  Validate token_id < n_vocab in wire.rs for Sampled and ApplyToken before anything is durable; bounds-check in hydra_token_to_piece; wrap all 14 shim entry points in try { } catch (...) { return -HYDRA_E_… } (M5).**


### H8 — WAL: a corrupt magic or length in any mid-stream record silently truncates the log — resurrecting discarded state and re-committing positions

**HIGH   CONFIRMED**

crates/hydra-wal/src/reader.rs:109-113 · record.rs:112-121 · tests/torn_write.rs:86-101

```text
ReadStep::Incomplete | ReadStep::BadFraming => { truncated_tail = true; break; }   // no resync, no refusal
```

WAL-FORMAT §3.4's "corruption with valid records after it: refuse to open" is implemented only for BadChecksum. A single bit flip in the 2-byte magic or 4-byte payload_len of a record at position 100 of 1 000 makes the scan return Ok with durable_len at that record; open_append then set_len()s the file, making the discard permanent. Client has already seen tokens 100–999 ("never un-see" broken); the next commit re-appends positions ≥ 100 (I7a/I7b). On the control WAL, a flipped magic on ACTIVATION_ABORT or UNSERVABLE resurrects pre-abort state (I25/I22). The torn-write test flips only payload/tag bytes of middle records, never header bytes.

**FIX  On BadFraming/Incomplete, scan forward for RECORD_MAGIC followed by a checksum-valid record; if found → CorruptMidStream. Bound the discardable tail to one max record. Add header bit-flip cases to the test.**


### H9 — WalWriter is not poisoned after an I/O error; size desyncs from the fd cursor and later fsynced records become unrecoverable (I19)

**HIGH   CONFIRMED**

crates/hydra-wal/src/writer.rs:60-67 · tests/chaos_disk.rs:47-56

A partial write_all (ENOSPC, EINTR, NFS) leaves n bytes on disk and the cursor at size+n while self.size is unchanged; the next append lands at size+n, fsyncs, returns Ok, and the watermark advances. On reopen the scanner sees a valid header at size, computes BLAKE3 over the wrong bytes, probes any_valid_record_after mid-next-record → BadFraming → treats it as torn tail → discards the successfully fsynced record. The fsyncgate case (EIO then later success) produces the same outcome via H8. The chaos sinks never leave bytes behind on failure, so this is unexercised. No O_APPEND.

**FIX  Poison the writer on any Err from write or sync; set_len(size) before resuming; consider positional writes; make the chaos sink model "bytes persisted, error returned".**


### H10 — Recovery's discard decision is never made durable; post-recovery commits go to a fresh file that re-appends already-durable positions

**HIGH   CONFIRMED FOR THE CURRENT SHAPE**

crates/hydra-coordinator/src/recovery.rs:78-134 · tests/d1_recovery.rs:193-208

No BEGIN_RECOVERY / truncation record is written anywhere in hydra-coordinator (only INITIAL, GENERATION, INPUT_CHUNK, BOUNDARY_COPY); there is no CommitStream::open caller outside hydra-wal tests, so every recovery creates a new file with generation_durable_pos = -1 and re-commits positions 0..k. Two files then claim the same positions with nothing durable saying which is authoritative. recovery::read ignores INPUT_CHUNK_COMMIT entirely, so prefill_stable_pos is never restored and a crash mid-prefill re-applies chunks.

**FIX  Implement CommitStream::open → (stream, RecoveryState) over WalScan + open_append; restore every watermark and next_commit_id; fsync a recovery record before any wire traffic; never re-append recovered positions.**


### H11 — Splitter: general.architecture from the input GGUF becomes an output path — arbitrary file write, including the signing key

**HIGH   CONFIRMED**

crates/hydra-modelsvc/src/split.rs:61, 104 · src/bin/hydra-modelsvc.rs:80-90

```text
let file_name = format!("{arch}-stage{stage}-L{first}_{last}.gguf");
let kp_path = Path::new(out_dir).join(format!("{arch}.signing.pkcs8"));   // join() discards out_dir on an absolute RHS
```

A community model with general.architecture = "../../tmp/x" or an absolute path writes shards, manifest, and the private signing key wherever the operator can write. No character validation anywhere.

**FIX  Validate against ^[A-Za-z0-9_.-]+$ (no leading dot), or derive names from a slug + hash; refuse if the joined path does not start with the canonicalised out_dir.**


### H12 — GGUF nested arrays recurse without a depth bound — stack overflow; the fuzzer caps nesting at 2 so it could not see it

**HIGH   CONFIRMED BY READING, NOT EXECUTED**

crates/hydra-modelsvc/src/gguf.rs:193-202 · crates/hydra-fuzz/src/gen.rs:155

Each nesting level costs 12 wire bytes (elem_type=9, n=1); a ~1 MB file yields ~80 k frames against an 8 MB main-thread (2 MB spawned-thread) stack. Stack overflow is SIGSEGV, the same uncatchable class the reserve_for fix targeted. gen.rs: let elem = if depth >= 2 { 4 } else { … } — the banked fuzz verdicts structurally exclude it. llama.cpp's gguf does not support nested arrays at all.

**FIX  Reject elem_type == 9 or cap depth at 2 with an explicit counter; extend the generator.**


### H13 — Manifest::from_bytes pre-allocates from declared counts before the signature is checked — the exact class fixed in gguf.rs, reintroduced one file over

**HIGH   CONFIRMED**

crates/hydra-modelsvc/src/manifest.rs:126-127, 134-135 · crates/hydra-worker/src/shard.rs:73-77

```text
let n_shards = r.u32()? as usize;  let mut shards = Vec::with_capacity(n_shards);   // 2³² × ~96 B
```

A 60-byte manifest aborts the worker; parse precedes verify, so no key is needed. Manifest::from_bytes is not a fuzz target.

**FIX  Clamp reservations to remaining / min_entry; verify the signature over the raw bytes before structural parsing (it is the trailing 64 bytes); add the fuzz target.**


### H14 — No model-identity or rollback binding: any validly signed manifest is accepted; SessionKeys.manifest_hash is never compared to the manifest loaded

**HIGH   CONFIRMED**

crates/hydra-worker/src/shard.rs:64-116 · wire.rs:140 · manifest.rs:51-60

manifest_hash is compared fence-to-bootstrap (both from the coordinator), never to blake3(manifest file). Manifest has no version, id, or timestamp; from_bytes ignores trailing bytes so one manifest has many byte encodings. Once C1 is fixed, substituting an older signed manifest for the same stage range is the primary attack.

**FIX  In verify_shard, require blake3(raw) == keys.manifest_hash; bind model_instance_id into the canonical bytes; reject trailing bytes.**


### H15 — ApiAuth::new("") yields a server that accepts requests with no Authorization header

**HIGH   CONFIRMED — LATENT: NO BINARY SERVES THE ROUTER YET**

crates/hydra-coordinator/src/server.rs:104-110, 122-130

```text
.and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
if *blake3::hash(presented.as_bytes()).as_bytes() != self.token_digest {
```

A missing header collapses to ""; an operator whose token is empty (unset env var, unwrap_or_default()) gets hash("") == hash(""). The checklist row "required by the type" is true of the argument, not of a non-empty secret. The test uses a fixed 14-char token and cannot catch this. Also router()/AppState are referenced only from tests — the HTTP surface is library code, so H15/H16/H21 are latent.

**FIX  ApiAuth::new → Result, refuse short tokens; refuse an absent header before hashing; add an_empty_token_is_refused_at_construction.**


### H16 — Single-session HTTP surface has no cancellation: a client disconnect, a pump panic, or a poisoned mutex wedges the instance until restart

**HIGH   CONFIRMED (LATENT)**

crates/hydra-coordinator/src/server.rs:67-72, 185-199, 268-309, 344-422

done is set only when the generator channel closes; make()/gen() panics, a hung pipeline, or a poisoned st.lock().unwrap() leave it false forever → 409 for every later client. Dropping GenStream drops a JoinHandle (no abort); yield_one swallows send errors; max_tokens is ignored by extract_prompt, which is a substring scanner, not a JSON parser. client_drained is never called by the server, so after emit_capacity events the stream silently ends with HTTP 200 (M16).

**FIX  Guard struct whose Drop sets done; PoisonError::into_inner; abort on stream drop; a CancellationToken through GenFn; parse and bound max_tokens; a wall-clock deadline per session.**


### H17 — Device private keys written 0644 to predictable names in shared /tmp, scp'd to remotes, and left behind

**HIGH   CONFIRMED**

crates/hydra-worker/src/bootstrap.rs:60-64 · pair.rs:629-633, 673-679 · bin/hydra-wan.rs:159, 309 · hydra-3node-wan.rs:240-245 · hydra-3node-kill.rs:189-207 · hydra-modelsvc.rs:88-92

File::create (follows symlinks, default umask) on temp_dir()/hydra-wan-sp.boot etc., containing key_pkcs8_der and the CA cert. Cleanup is Drop-only, so a kill -9 of the runner leaves the key. Remote copies at /home/azureuser/hydra/*.boot are never deleted. The splitter's signing key is written the same way. Any local user on the host reads a worker key and, via C2, becomes the coordinator. No zeroization anywhere (no zeroize dependency).

**FIX  OpenOptions::new().create_new(true).mode(0o600) inside a 0700 private directory; delete remote copies after start; zeroize on drop.**


### H18 — TLS handshake runs inline in the accept loop with no timeout — one silent TCP connection stalls the listener for everyone

**HIGH   CONFIRMED**

crates/hydra-transport/src/tcp_mtls.rs:56-60 · crates/hydra-worker/src/worker.rs:533-547 · pair.rs:77, 124, 159, 200

serve_multi_conn awaits accept() — which includes the handshake — sequentially, and only spawn_locals the post-handshake connection. There is no tokio::time::timeout anywhere in the transport or worker. No certificate is needed: open a TCP socket to the worker port and send nothing. Handshake failures are also unlogged (peer address discarded).

**FIX  Return the raw stream from accept() and perform the handshake in the spawned task under a 5–10 s timeout; log peer + error on failure.**


### H19 — CATCH_UP_CONTEXT{goal} drives a loop of goal+2 iterations; a large goal hangs the single-threaded worker

**HIGH   CONFIRMED**

crates/hydra-worker/src/worker.rs:302-313 · crates/hydra-state/src/stage.rs:198-213

```text
for _ in 0..goal.max(0) + 2 { … if ready.is_some() { break; } }
```

RebuildStep is a no-op outside Frozen|Rebuilding, so with the stage elsewhere the loop never breaks: goal = i64::MAX − 2 is ~9.2 × 10¹⁸ iterations inside RefCell::borrow_mut on a current-thread LocalSet — every connection to that worker stalls. No fence is applied to CatchUpContext first.

**FIX  Bound by n_ctx; refuse larger goals; loop only while the SM is in a rebuild state.**


### H20 — sampled_ring is unbounded and keyed by attacker-chosen output_pos; with a 1 MiB installed penalty window each SAMPLE_NEXT retains ~1 MiB

**HIGH   CONFIRMED**

crates/hydra-worker/src/worker.rs:270, 361-372 · crates/hydra-worker/src/sampler.rs:176-202, 206-216, 237

No monotonicity on output_pos; cleared only on an accepted BEGIN_RECOVERY. A 1 MiB snapshot (cap-compliant) carries a 262 144-entry penalty window that serialize() re-emits into every subsequent snapshot and apply_repeat_penalty walks on every call. The snapshot's state_checksum is self-computed — integrity, not authentication.

**FIX  Ring of the last W positions or evict below the committed watermark; require output_pos to be sampled_pos+1 or an existing key; cap penalty_window.len() ≤ config.penalty_last_n on install.**


### H21 — The one link that carries user prompts and the API secret is plaintext HTTP

**HIGH ON A LAN BIND · MEDIUM ON LOOPBACK   CONFIRMED (LATENT)**

crates/hydra-coordinator/src/server.rs:113-121, 179-181 · crates/hydra-coordinator/Cargo.toml

No TLS layer on the axum router; "every cluster link is mutually authenticated" excludes the client link. ApiAuth::loopback() allow-lists https:// origins the server cannot serve.

**FIX  Serve the API with the same rustls material as the transport, or make ApiAuth::new refuse non-loopback hosts and document loopback-only as a hard constraint.**


## Medium


### M1 — Per-connection 64 MiB zeroed pre-read buffer, no read timeout, no connection cap, three full copies per frame

**MEDIUM   CONFIRMED · POST-AUTH**

crates/hydra-transport/src/framed.rs:49-58 · crates/hydra-worker/src/worker.rs:514, 538-546

Header cap is correctly enforced before allocation, but a header claiming 64 MiB followed by silence pins that memory indefinitely; rest → full → payload.to_vec() peaks near 192 MiB per delivered frame. Sixteen idle connections from one certificate holder commit 1 GiB.

**FIX  Read timeout; per-peer and global semaphore; single buffer with incremental hashing.**


### M2 — check_bind_addr bypass via the IPv4-mapped wildcard [::ffff:0.0.0.0]

**MEDIUM   LOGIC CONFIRMED · OS BEHAVIOUR NEEDS VERIFICATION**

crates/hydra-transport/src/lib.rs:62-65 · tests/security_checklist.rs:69, 146

Ipv6Addr::is_unspecified is true only for ::. On Linux an AF_INET6 socket bound to ::ffff:0.0.0.0 with IPV6_V6ONLY=0 accepts on all IPv4 interfaces. The listen address is a runtime string from the bootstrap, so the source-grep test does not cover it; the unit test checks only 0.0.0.0 and [::].

**FIX  Also reject v6.to_ipv4_mapped() == Some(UNSPECIFIED); set IPV6_V6ONLY explicitly; extend the test.**


### M3 — Certificates never expire (rcgen defaults 1975→4096), no CRL/OCSP, no rotation, CA has no path-length constraint

**MEDIUM   CONFIRMED**

crates/hydra-transport/src/tls.rs:104-142

A leaked worker key (H17) is valid forever. FIX  Short leaf validity, with_crls, BasicConstraints::Constrained(0), a re-pair/revoke flow in M4·2.


### M4 — Silent i64 → i32 narrowing on truncate_to, input_pos, first_input_pos desynchronises the engine KV from the state machine

**MEDIUM   CONFIRMED**

crates/hydra-worker/src/worker.rs:237, 244, 269 · crates/hydra-state/src/stage.rs:166

truncate_to = i32::MAX → +1 → as i32 = i32::MIN → seq_rm(0, −1) wipes the KV while applied is unchanged; the next apply on an empty KV computes garbage that is then acked with a digest. No [profile] in Cargo.toml, so release builds wrap silently.

**FIX  i32::try_from at decode, bounded to [−1, n_ctx).**


### M5 — No exception barrier at any of the 14 shim entry points; text_len narrowed usize → i32

**MEDIUM   CONFIRMED**

crates/hydra-engine-sys/csrc/hydra_engine.cpp (all extern "C") · src/lib.rs:195, 203 · vendor/llama.cpp/src/llama-vocab.cpp:4041 · llama-context.cpp:853-856, 4044-4053

llama_tokenize (std::length_error on negative length), llama_token_to_piece (H7), llama_decode (bad_alloc), and llama_batch_init (unchecked malloc) all throw or abort freely. The shim silently depends on the vendored build being -DNDEBUG: a debug rebuild turns get_logits_ith's nullptr return into GGML_ABORT. A ≥ 2 GiB prompt wraps negative; nothing caps prompt bytes at ingress.

**FIX  try/catch(...) macro around every body; i32::try_from; cap prompt bytes.**


### M6 — ACTIVATION_UNSERVABLE / SESSION_TERMINATE are treated as durable at write time; the simulator hard-codes the same assumption

**MEDIUM   CONFIRMED IN THE SM**

crates/hydra-state/src/coordinator.rs:199, 309-330 · crates/hydra-sim/src/lib.rs:236-243

WalKindTag has only Intent|Complete|Abort; ProceedStartSuperseding is enabled immediately after the push, before any WalDurable. Spec §6.7 step 1 requires the fsync first. The sim fsyncs these two types inline, so crash_tear can never produce the write-then-crash case and the codec-divergence check is structurally blind here. Outcome: restart classifier sees COMPLETE without UNSERVABLE and re-enters finalisation under the incomplete configuration — the I22 hole F-UNSERVABLE was meant to close.

**FIX  Add Unservable|Terminate tags, gate the transition on WalDurable, remove the sim special case.**


### M7 — No contiguity check on GENERATION_COMMIT; a failed commit drops the batch and creates a gap that recovery mis-aligns; verify's monotonicity check runs after the sort

**MEDIUM   CONFIRMED**

crates/hydra-coordinator/src/session.rs:128-131 · commit_stream.rs:215-261 · recovery.rs:57-72, 121 · crates/hydra-proto/src/validate.rs:10-24

**FIX  Reject first != durable+1 and non-dense entries on write; reject gaps on read; check monotonicity before sorting; retain or fail (I9) on commit error.**


### M8 — WAL files are not bound to the session that reads them; per-record fences and header ids are parsed and never compared; symlinks followed on open

**MEDIUM   CONFIRMED**

crates/hydra-coordinator/src/recovery.rs:78-118 · boundary_store.rs:79-93 · crates/hydra-wal/src/file.rs:47-72, reader.rs:69-72

A second INITIAL_COMMIT overwrites the prompt while earlier commits are kept; previous_commit_id chaining and entries_checksum are never verified; BLAKE3 tags are unkeyed. Pointing config at another session's file replays it without error. Acceptable only if the disk is inside the trust boundary — which should be stated.


### M9 — Duplicate FWD / APPLY_TOKEN is applied twice; I1 exactly-once and R2's ERR_GAP are not implemented on the worker

**MEDIUM   CONFIRMED**

crates/hydra-worker/src/worker.rs:242-248

SAMPLE_NEXT is deduped via sampled_ring; the data plane has no equivalent. A replayed frame at p re-applies at p. grep ERR_GAP finds nothing in the worker. FIX  Track applied_pos per shard; no-op on ≤, ERR_GAP on > applied+1.


### M10 — Decode and limit errors tear down the connection instead of the spec'd structured reply; the durability target silently drops bad frames, and a survivor in the backpressure loop then waits forever

**MEDIUM   CONFIRMED**

crates/hydra-worker/src/worker.rs:443, 466, 523, 699-707, 762, 826 · bin/hydra-3node-kill.rs:112-117

ERR_UNSUPPORTED_VERSION, ERR_LIMIT_EXCEEDED, ERR_BAD_CHECKSUM are never emitted on the wire. Stream desync is not possible (fixed header + tag), so "continue" is safe at the framing layer; the missing reply is the bug.


### M11 — bytes_to_f32_le silently truncates a trailing partial word; Tensor.dims is read by no code

**MEDIUM   CONFIRMED**

crates/hydra-worker/src/wire.rs:694-696 · crates/hydra-coordinator/src/boundary_store.rs:89

"Reject, never truncate" is violated on both the wire and the replay path. FIX  Reject len % 4 != 0; require dims == [n_positions, n_embd].


### M12 — Session identity is a deterministic seed derivation in every shipped binary; no random ids anywhere

**MEDIUM   CONFIRMED**

crates/hydra-worker/src/wire.rs:56-64 · bin/hydra-wan.rs:157 · hydra-3node-kill.rs:58, 185 · hydra-2node-ci.rs:105, 138

SessionKeys::dev(seed) fills every identity vector from one byte. Epoch and recovery_id start at 0 and increment predictably. The F1 identity check therefore has zero secrecy, and a frame captured across a restart still matches. FIX  CSPRNG session ids minted by the coordinator and distributed over the authenticated channel; no dev() outside cfg(test).


### M13 — RESET_RECOVERY_ATTEMPT has no wire decode arm, and the SM's reset does not clear highest_attempt — a spec gap

**MEDIUM   CONFIRMED**

crates/hydra-worker/src/wire.rs:238-378 (no Body::ResetRecoveryAttempt arm) · crates/hydra-state/src/stage.rs:183-197

An inbound reset falls to UnsupportedBody, so the documented recovery reversal is unreachable over the wire, and the H1 brick has no wire-reachable escape. The spec is silent on what reset does to the attempt floor; that should be decided and modelled.


### M14 — Bootstrap fields receives_tokens / is_final / layer window are never cross-validated; a mismatch reads uninitialised backend memory as a "boundary"

**MEDIUM   SUSPICIOUS — NEEDS A RUNTIME REPRO**

vendor/llama.cpp/src/llama-graph.cpp (patched build_inp_embd) · crates/hydra-worker/src/worker.rs:231-237 · bootstrap.rs:147-150

With receives_tokens=true on a shard loaded at l0 > 0 (no token_embd), llm_graph_input_embd::set_input never writes inp->embd; layers run on whatever the buffer held. Operator-trusted input, so wrong output rather than remote compromise. FIX  Refuse tokens != nullptr when il_start > 0; validate receives_tokens == (layer_first == 0) in Engine::try_new.


### M15 — The compute window is honoured by only two architecture builders; any other arch builds its full graph over null out-of-window tensors

**MEDIUM   CONFIRMED BY CODE; OPERATOR-TRUSTED INPUT TODAY, ATTACKER-CHOSEN UNDER C1**

vendor/llama.cpp/src/models/{llama,qwen2}.cpp · csrc/hydra_engine.cpp:136-162

hydra_context_new's window-containment check is necessary but not sufficient. FIX  Refuse any llama_model_arch outside {LLAMA, QWEN2} with a new HYDRA_E_ARCH. Related: the shim sizes batch.embd by n_embd while llama reads n_embd_inp() — a heap over-read for any arch where they differ.


### M16 — HTTP surface: backpressure never relieved, unbounded session registry, body read before auth, digest compare not constant-time

**MEDIUM   CONFIRMED (LATENT)**

crates/hydra-coordinator/src/server.rs:52-56, 130, 201-207, 248-255 · session.rs:102-127

client_drained has no caller outside its unit test, so the stream ends early with HTTP 200 and no error. sessions / by_idempotency never prune and keep full event logs; Idempotency-Key has no length bound. Extractors run before auth.check (2 MiB default body buffered per unauthenticated request). != on [u8;32] may early-exit — compare blake3::Hash values (constant-time) or use subtle; the leak is only digest-prefix, so practically minor, but the doc claim is overstated.


### M17 — Manifest admission hashes use a different algorithm than the runtime's and are compared to nothing; the per-tensor hash list is signed but never verified

**MEDIUM   CONFIRMED**

crates/hydra-modelsvc/src/manifest.rs:45-46, 175-207 · crates/hydra-tokenizer/src/tokenizer.rs:64-74 · crates/hydra-worker/src/shard.rs:93

Manifest tokenizer_hash = BLAKE3 over raw tokenizer.ggml.* KVs with no domain tag; runtime = tagged hash over engine vocab pieces. Nothing outside the modelsvc crate reads tokenizer_hash, chat_template_hash, or inference_config_hash. The signed fields imply a check that does not exist. FIX  Unify definitions and compare at INITIAL_COMMIT, or remove the fields.


### M18 — A manifest-configured worker silently degrades to control-plane-only when the shard file is missing, contradicting its own doc comment

**MEDIUM   CONFIRMED**

crates/hydra-worker/src/worker.rs:81-85, 120-122

The exists() filter runs before the manifest branch. FIX  When shard_manifest.is_some(), a missing path is ShardRefused::ShardUnreadable.


### M19 — hydra-modelsvc verify joins a manifest-controlled file_name onto the directory — path traversal (read + hash-compare only)

**MEDIUM / LOW   CONFIRMED**

crates/hydra-modelsvc/src/bin/hydra-modelsvc.rs:105

The worker path is safe (compares basename of the configured path rather than joining). FIX  Require a single Normal path component.


### M20 — Checklist rows whose cited test proves a narrower property than the row claims

**MEDIUM · ASSURANCE   CONFIRMED**

docs/SECURITY-CHECKLIST.md · crates/hydra-transport/tests/security_checklist.rs:177-200 · crates/hydra-worker/tests/wire_limits.rs:112, 137 · tests/relink.rs:45

See the table below. The pattern matters more than any single row: the M4·1 report's thesis — "a documented invariant that nothing checks is not an invariant" — applies to the tests too when a test name promises more than its assertion.


## Low / informational

Id

Finding

Where

L1

The llama.cpp layer-window patch exists only as uncommitted working-tree modification of the submodule (it does match spike/llama-cpp-layer-window.patch byte-for-byte). A clean checkout builds an unpatched engine whose headers lack the new il_* params, while the shim still writes them — a silent ABI mismatch, not a compile error. CI never builds the real engine at all (build.rs degrades to a stub), so no automated run exercises the FFI. Pin a fork SHA.

vendor/llama.cpp (7 files ` M`) · crates/hydra-engine-sys/build.rs:30-38 · .github/workflows/*

L2

Bootstrap::decode reserves Vec::with_capacity(u32) from the file before bounds; local file, so abort-on-craft only.

crates/hydra-worker/src/bootstrap.rs:132-137, 268

L3

Frame header flags (16 reserved bits) unchecked; payload_len == 0 accepted; trailing bytes after the flatbuffer tolerated; WAL record reserved accepted silently.

crates/hydra-proto/src/framing.rs:70 · crates/hydra-wal/src/record.rs:116-138

L4

&mut [f32] written through an as_ptr()-derived pointer — UB under Stacked/Tree Borrows (Miri will flag it). Use as_mut_ptr().

crates/hydra-engine-sys/src/lib.rs:304

L5

Unsynchronised static bool g_backends_loaded; llama_backend_init never called. Use std::call_once.

csrc/hydra_engine.cpp:28-77

L6

Shim and Rust share one negative namespace: an error code is indistinguishable from a negated required count in tokenize_ex/token_to_piece; -need panics on i32::MIN in debug.

crates/hydra-engine-sys/src/lib.rs:197-218 · csrc/hydra_engine.h:64

L7

No hardening flags on the shim (-fstack-protector-strong, _FORTIFY_SOURCE); vendored build is -O3 -DNDEBUG GGML_NATIVE=ON, sanitizers off; no panic = "abort" profile.

crates/hydra-engine-sys/build.rs:40-47 · vendor/llama.cpp/build/CMakeCache.txt

L8

WalScan::open reads the entire file into memory; no size cap. Segment rotation (should_rotate) is declared but not implemented, so the §3.2 dir-fsync-after-rename rule has no code to audit.

crates/hydra-wal/src/reader.rs:69-72 · writer.rs:78-81

L9

Fence counters are u32 with unchecked += 1; wrap collides with INITIAL. Practically unreachable without H3.

crates/hydra-state/src/coordinator.rs:232, 334

L10

TLS 1.2 enabled; unnecessary for a closed rustls-only cluster. from_der never checks that key and cert match.

crates/hydra-transport/Cargo.toml:13-14 · tls.rs:48-58

L11

GgufValue amplification: u8/bool arrays cost ~32 B per wire byte (bounded by file size × 32). n_dims not capped at 4; duplicate tensor names and overlapping offsets accepted; type table mislabels 28 (F64, not I64) and rejects all IQ* quants.

crates/hydra-modelsvc/src/gguf.rs:108, 197-200, 238-246, 366-387

L12

HTTP: under HTTP/2 hyper puts :authority in the URI so every h2 request is refused (fail-closed). Axum's built-in 404 for unknown paths answers without auth. docs/wan-run.md documents an SSH key path and real Tailscale IPs (nothing secret committed). telemetry.rs resolves vm_stat/pmset via PATH.

server.rs:139-146 · docs/wan-run.md:35 · telemetry.rs:103-111

Where the formal guarantees apply — and where they stop

The state machines in hydra-state faithfully mirror HydraActivationCore.tla, and within the model's assumptions they are sound: the I25 exclusion, the §6.5 classifier order, the Intent/Complete/Abort gating on WalDurable, and the attempt-monotonicity logic all match. The model's own header states its network abstraction plainly — "msgs is a monotonically growing set… duplication and reordering are free; loss = never received." What it does not model is where every critical and high fencing finding lives:

Honest senders only. Every StageRecv*/CoordRecv* consumes a message that a modelled honest action produced. There is no adversary action that fabricates a field. C2, C4, H1–H5 are all fields the code accepts with no sender authentication. The model's comment at lines 145–166 notes that mutation 3 (no attempt fence) was unreachable because "consistent ack discipline subsumes attempt fencing" — i.e. TLC proved fencing redundant given honest acks, which is exactly what a compromised worker breaks (H4).

No payload, KV, or position semantics. "No tensors, tokens, KV… payloads." FWD content poisoning (C4), position narrowing (M4), duplicate-apply corruption (M9), and the KV wipe (H3) have no model counterpart; stApplied is an abstract integer advanced by +1.

Finite attempt domain. StageRecvCommit(s) == \E a0 \in 1..MaxAttempt. The >= fence is safe inside that box; the code accepts u32::MAX (H1). The forged-future-attempt attack is unrepresentable in the model.

An unmodelled transition. stage.rs:235-239's PREACTIVE→higher-attempt rebind has no counterpart in StageRecvCommitAt; TLC never checked it.

Set-valued acks. The model collects AcksFrom as a set of stages, so one stage cannot ack twice. The wire carries a self-reported rank (H4).

Durability. The model abstracts the WAL as a set with an fsync event; the simulator models only prefix-truncation and single-bit flips in the pending write. Mid-stream header corruption (H8), partial writes with continued appends (H9), fsync-then-error (H9), and the UNSERVABLE write-then-crash window (M6, hard-coded away in the sim) are all outside it. And no production code wires Effect::WriteWal to hydra-wal — the coordinator-side protocol driver does not exist yet, so the SM's durability discipline is not yet connected to a real disk anywhere outside harness binaries.

The C++ engine and transport are entirely outside the model, by design. C3, H6, H7, H18, M5, M14, M15 live there. The fuzz arm covers Gguf::parse (the splitter's parser, not the worker's — H6), FrameHeader::parse, and wire::decode with four seed body types and bit-flip mutations; it never reaches on_frame, the sampler, the engine, or any serve loop, and has no timeout or allocation oracle, so C3, H19, H20, and H12 are structurally invisible to it.

HydraCert.tla adds only a message-count bound, and its large-bound runs are recorded INCONCLUSIVE since 2026-07-13. It offers nothing against any finding here.

Net: the protocol is well-verified as a protocol. The security boundary is the transport, the FFI, and the durability implementation — none of which the proof touches — and the spec's §12 "untrusted workers: out of scope" is currently doing all the work that the blueprint's §1.9 identity story claims to do.

Checklist rows vs. what their tests prove

Checklist row

Cited test

What it actually asserts

"Every cluster link is mutually authenticated… asserted repository-wide"

the_transport_exposes_no_plaintext_path

Greps only crates/hydra-transport/src/. tests/relink.rs:45 has a raw TcpListener::bind; Conn<S> is generic over any stream, so a plaintext Conn is trivially constructible. The claim holds only because the bins happen to use TcpMtlsListener.

"The cluster CA is the trust boundary"

a_peer_from_a_foreign_ca_is_rejected…

True, and genuinely asserted. But no test asserts a worker cert cannot act as the coordinator (C2) — the boundary is the CA, and nothing finer.

"API auth required by the type; 'not configured' is unreachable"

the_api_refuses_an_unauthenticated_request

Uses a fixed 14-char token. Empty token (H15) is uncovered.

"Token comparison does not leak"

same

Asserts prefix rejection, not timing. Comparison is != on bytes (M16).

"Never bind 0.0.0.0"

a_wildcard_bind_is_refused…

Tests 0.0.0.0 and [::]; not [::ffff:0.0.0.0] (M2).

"MAX_TENSOR_BYTES before copy"

an_oversized_tensor_is_refused_before_it_is_copied

Asserts the error variant, not ordering. Ordering is true by inspection but unprotected against regression.

"MAX_POSITIONS_PER_FRAME on Fwd"

an_oversized_position_count_is_refused

Proves the declared u16 is capped. Its control case labels 896 floats as 1 024 positions and asserts is_ok() — enshrining C3.

"A worker verifies the signature… before any shard weights are read"

P2·10b refuse-on-fail

Verifies against the manifest's own key (C1); then re-opens the file by path for llama.cpp (H6).

"Hardened parser… fuzzed for 24 CPU-hours"

hydra-fuzz gguf target

Fuzzes the splitter's parser, not the worker's (H6); nesting capped at depth 2 (H12); Manifest::from_bytes not a target (H13).

"Durability: failed fdatasync never advances a watermark"

chaos_disk.rs

True for the cases modelled. Fake sinks never persist bytes on failure (H9) and never corrupt header bytes mid-stream (H8).

"Backpressure pauses at the commit stage"

backpressure_pauses_at_the_commit_stage

Calls client_drained by hand; the HTTP layer never does (M16).

Dependencies and the vendored engine

Component

Version

Status

RustSec scan (cargo audit, db of 1 225 advisories, 2026-08-23)

122 crates

0 advisories, 0 warnings

rustls / tokio-rustls / rustls-webpki / ring

0.23.41 / 0.26.4 / 0.103.13 / 0.17.14

current; tls12 feature on (L10)

rcgen

0.13.2

default validity 1975→4096 used as-is (M3)

flatbuffers

25.12.19

verifier defaults; schema depth ≤ 4 so pathological nesting is unreachable — sound

axum / hyper / tokio / blake3

0.7.9 / 1.10.1 / 1.52.3 / 1.8.5

current

llama.cpp submodule

13f2b28b (2026-07-11, master)

six weeks behind master; patch uncommitted in working tree (L1); C++ GGUF parser is the live one on the worker (H6); no project fuzzing of it. Upstream advisories after this audit's knowledge horizon should be checked against the pin before any release.

Absent

subtle, zeroize

no constant-time compare, no key zeroization

Method and limits of this audit

Static adversarial read of the full workspace (≈35 k lines of Rust, the C++ shim, the vendored patch, the TLA model, the spec, and the M4·1 evidence), tracing every untrusted input — network frame, model file, manifest, WAL byte, bootstrap file, HTTP request — to the point it is trusted. Every critical and high finding was confirmed against the quoted source by a second independent read; where a finding rests on reading rather than execution it is marked so. Nothing was executed against a running cluster and no proof-of-concept was built, so "confirmed" means the code path is as described, not that an exploit was run. Two findings (H4, M14) are marked suspicious because the production code that would make them live does not yet exist or a runtime repro was not attempted. Items the spec deliberately leaves out of v1 (untrusted workers, sandboxed loader, rate limiting) are reported rather than excused, because the blueprint's stated posture promises more than §12's scope delivers.

Suggested remediation order

C1 + H14 + H13 — pin the manifest signer key, bind the manifest hash and model id, verify before parse. Until then the supply-chain story is decorative.

C2 — bind the mTLS peer identity to a role and rank at accept(). Most of the fencing findings (C4, H1–H5, H19, H20) collapse to "defence in depth" once this lands; they still need their own checks, but the root is here.

C3 + M4 + H7 + M5 — make every network-derived integer that reaches the FFI bounds-checked and every shim entry point exception-safe. These are remote aborts under the current threat model.

C4 + M9 — enforce epoch / generation / state on the data plane and implement I1's per-position idempotency.

H8 + H9 + H10 + M6 + M7 — the WAL: refuse mid-stream corruption, poison the writer on error, make the recovery discard durable, gate UNSERVABLE on fsync, check contiguity. Extend the chaos sink and torn-write tests to the cases listed.

H6 + H12 + H11 — verify-over-fd and fuzz the vendored parser; cap array nesting; sanitise arch.

H17 + H18 + M1 + M2 + M3 — key file permissions, handshake timeouts, connection bounds, the mapped-wildcard bypass, certificate lifetime.

H15 + H16 + H21 + M16 — before the HTTP surface is served by any binary.

M20 — tighten the checklist tests so each asserts the property its row claims; add the absent ones named above.
