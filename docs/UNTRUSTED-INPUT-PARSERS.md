# Untrusted-input parsers — the standing-rule-17 enumeration

**Swept 2026-08-23** (audit Wave 1a·2), after the **fourth** instance of the
`Vec::with_capacity(declared)` allocation class.

> **Standing rule 17.** When a defect class is found in one parser, **every** parser of untrusted
> input is audited for it in the same wave, and each becomes a fuzz target — or carries a written
> justification for why it needs none. *A class fixed in one instance and left in another is not
> fixed.*

**"Untrusted input" means anything the process did not compute itself** — the network, the disk, a
model file, a provisioning blob. Not merely "the network". Three of the four instances of the D2
class were reached from the *disk*, which is exactly why the narrower reading would have missed them.

---

## The sweep

| Parser | Reached from | (a) Reservation-clamped | (b) Fuzz target |
|---|---|---|---|
| **GGUF** — `hydra-modelsvc::gguf` | a downloaded model file | ✅ `Cursor::reserve_for` (§7.28 D2, M4·1) | ✅ `gguf` |
| **Signed manifest** — `hydra-modelsvc::manifest` | the model supply chain | ✅ `Reader::reserve_for` (**2nd instance**, audit C1 seam) | ✅ `manifest` |
| **Bootstrap blob** — `hydra-worker::bootstrap` | whatever provisioned the worker | ✅ `Reader::reserve_for` (**4th instance — audit L2**) | ✅ `bootstrap` |
| **WAL record** — `hydra-wal::record::read_record` | the coordinator's own disk | ✅ **by construction** — borrows, never allocates; length-checked before every slice | ✅ `wal-record` |
| **Frame header** — `hydra-proto::framing` | the network, pre-allocation | ✅ `check_frame_len` before the payload buffer exists | ✅ `frame-header` |
| **Wire body** — `hydra-worker::wire` | the network, post-header | ✅ `capped_bytes` + `check_tensor_len` / `check_positions` (M4·1) | ✅ `wire-body` |
| **HTTP request** — `hydra-coordinator::server` | a LAN client / a browser | ⚠️ **see below** | ⏳ **justified for now — Wave 4** |

All three `reserve_for` implementations are **deliberately spelled the same way** in three different
crates, so the class is recognisable at a glance rather than requiring three separate readings.

**The enumeration is `crates/hydra-fuzz/Cargo.toml`'s dependency list.** A parser whose crate is
absent from it is itself the finding — which makes the check mechanical rather than a memory
exercise.

---

## The two entries that are not a simple ✅

### `framed::Conn::recv` — a bounded amplification, recorded rather than waved past

`recv` reads a 12-byte header, validates it (`FrameHeader::parse` rejects bad magic, wrong major
version, and `payload_len > MAX_FRAME_BYTES`), and **then allocates `payload_len + 32` bytes before
the payload has arrived**. So **12 bytes of input reserve up to 64 MiB** — roughly a 5.6-million-fold
amplification — held for as long as the peer is willing to stall the `read_exact`.

This is **not** the D2 class (the cap is real and enforced pre-allocation, which is the whole point
of `MAX_FRAME_BYTES`), but it is worth stating plainly rather than counting as clean:

* **The bound exists and is enforced**: 64 MiB per in-flight frame, per connection.
* **The peer must hold a cluster certificate.** Under the **honest-worker assumption** (BLUEPRINT
  §1.9, ruled 2026-08-23) this is **accepted-with-assumption**: a certificate holder able to stall
  connections can exhaust memory, and v1 does not defend against a compromised stage.
* **What would change it**: streaming the payload into a bounded buffer rather than reserving the
  declared size up front, and/or a per-connection in-flight budget. Neither is v1 scope, and
  pretending otherwise by writing ✅ here would be exactly the §7.31 failure — a row claiming more
  than the code does.

### The HTTP surface — justified for now, revisited in Wave 4

`hydra-coordinator::server` is not a length-prefixed binary parser. It takes an axum `String` body
(bounded by axum's `DefaultBodyLimit`) and reads a handful of headers; `extract_prompt` scans the
body with `find`/`rfind` and allocates only the substring it locates. **There is no
attacker-declared count driving a reservation**, which is the class this sweep is about.

**What is genuinely open, and is Wave 4's scope, not this sweep's** (already recorded in
`docs/SECURITY-CHECKLIST.md`'s known gaps): no rate limiting, and no request-size limit of our own —
a caller holding a valid token can submit an arbitrarily large body up to axum's default. **The
audit's H15/H16/H21/M16 land there**, and Wave 4 is explicitly scheduled *before any binary serves
this surface*.

A fuzz target over the HTTP layer is deferred to Wave 4 rather than added here, because the thing
worth fuzzing is the **auth and admission logic** those findings will reshape, and fuzzing the
current shape would bank a corpus against code that is about to change.

---

## Why the enumeration is a document and not a comment

The three D2 instances were each found by a *different* activity — the first by a new fuzzer, the
second by an external audit, the third by sweeping for the second. None of them was found by
someone remembering the first. **A class is only closed when the check is mechanical**, and this
table plus the `hydra-fuzz` dependency list is that mechanism.
