# WAL-FORMAT.md — Authoritative on-disk format (Hydra v0.10.1)

Governs the coordinator's **commit stream** (spec §2.6a) and **control WAL** (spec §1.2).
They MAY share one physical log; record types are disjoint. `hydra-wal` implements this
document exactly; any deviation is a defect. All integers little-endian.

## 1. File layout

```
file := file_header record*
file_header := magic:u32 = 0x4859574C ("HYWL")
             | format_version:u16 = 0x0100   # normative encoding: (major<<8)|minor; this file
                                             #   documents major=1, minor=0. Major mismatch => refuse;
                                             #   higher minor, same major => open read-compatible.
             | flags:u16                     # bit0: contains-commit-stream, bit1: contains-control-wal
             | cluster_id:[16]u8 | session_scope:[16]u8   # zero UUID = multi-session file
             | header_blake3:[32]u8          # of preceding header bytes
record := magic:u16 = 0x5243 ("RC")
        | record_type:u16                    # §2 registry
        | flags:u16                          # bit0: CRITICAL (unknown+critical => refuse to open)
        | reserved:u16
        | payload_len:u32                    # <= 64 MiB
        | payload:[payload_len]u8            # FlatBuffer per record type (schemas in the INCLUDED
                                             #   companion file wal-records.fbs; authoritative)
        | pad to 8-byte alignment (zero bytes)
        | record_blake3:[32]u8               # of record bytes from record magic through padding
```

## 2. Record-type registry (never renumber; append only)

| id | record | stream |
|----|--------|--------|
| 1 | INITIAL_COMMIT (admission metadata + initial sampler checkpoint) | commit |
| 2 | SEGMENT_COMMIT (PROMPT/TOOL_RESULT entries + resulting checkpoint snapshot) | commit |
| 3 | GENERATION_COMMIT (token entries + matching sampler snapshot; spec I19 equalities are validated on read AND write) | commit |
| 4 | INPUT_CHUNK_COMMIT (segment_id, chunk_id, first/last input pos, per-boundary durability refs) — **the durable event that advances `prefill_stable_pos`** (spec §2.4); no watermark advancement without it | commit |
| 10 | BEGIN_RECOVERY intent | control |
| 11 | RESET_RECOVERY_ATTEMPT | control |
| 12 | ACTIVATION_COMMIT_INTENT (full ActivationTuple) | control |
| 13 | ACTIVATION_COMPLETE (completion_id, tuple hash) | control |
| 14 | ACTIVATION_ABORT (epoch, r, attempt) | control |
| 15 | ACTIVATION_UNSERVABLE (completion_id, failed shard set, predecessor link) | control |
| 16 | SESSION_TERMINATE | control |
| 17 | CANCEL_CUTOFF (cancel_cutoff_output_pos, cutoff_event_seq) | control |
| 18 | PLACEMENT_INSTALL intent | control |
| 30 | EVENT_LOG segment (SSE events; pure function of ledger — rebuildable, non-critical) | aux |

## 3. Durability rules (normative)

1. **Write path:** append record bytes → `fdatasync(fd)`. A record is durable only after
   fdatasync returns. Group commit batches GENERATION_COMMIT per spec §3 (k=8 / 50 ms).
2. **File creation/rotation:** after creating or renaming a log segment, `fsync` the parent
   directory before writing any record.
3. **Watermark rule:** `generation_durable_pos`, `committed_sampler_checkpoint_id`,
   `prefill_stable_pos`, epochs, attempts, and completion ids advance ONLY on return of the
   fdatasync that made the corresponding record durable (WAL-before-wire, spec §1.2).
4. **Open/recovery scan:** read records sequentially, verifying magic, length caps, and
   BLAKE3. First failure => the durable log ends at the previous record; **truncate the file
   to that boundary** (partial-tail discard, spec §2.6a) and log an audit event. A checksum
   failure NOT at the tail (valid records follow it) is corruption: refuse to open; operator
   intervention (this is the fallback-checkpoint case in the report's fault table).
5. **Unknown record types:** same major version, CRITICAL flag clear => skip (length-prefixed).
   CRITICAL set => refuse to open (a newer coordinator wrote state this one cannot honor).
   Higher major version => refuse to open.
6. **Segments:** rotate at 256 MiB. A segment is deletable only when every watermark it
   supports has been superseded by a later durable snapshot record (checkpoint compaction is
   M4; until then, retain).

## 4. Effect IDs (replay determinism for `hydra-state`)

`hydra-state` outputs effects (send X, write record Y, start timer Z). Every effect carries
a **stable effect id**: `blake3(session_id || session_epoch || recovery_id ||
activation_attempt_id || effect_kind || monotonic_seq)` truncated to u64, where
`monotonic_seq` is per-(session, epoch) and part of the state machine's state (thus itself
replayed deterministically). Rules: identical (state, event) inputs yield identical effect
ids; the runtime deduplicates effect execution by id across coordinator restarts (sends are
naturally idempotent by protocol design — the ids make *tests* able to assert exactly-once
effect emission, and make trace diffs stable across refactors).

## 5. Torn-write test contract (binding on `hydra-wal`)

CI must include: (a) for a log of N records, truncate at EVERY byte offset, reopen, assert
recovery to the last complete record and successful append afterward; (b) bit-flip fuzz in
payload and checksum regions, assert detection; (c) crash-during-rotation (segment exists,
dir not synced) simulated via rename interposition; (d) group-commit crash window: tokens
sampled beyond the last durable GENERATION_COMMIT must vanish on reopen and the restored
sampler checkpoint must satisfy I19's equalities (this is the spec's Failure A/B pair).
