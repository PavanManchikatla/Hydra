# hydra-proto

Authoritative Hydra wire + WAL schemas (BLUEPRINT §1.2). The flatc-generated code in
`src/generated/` is the source of truth — no handwritten shadow structs. Regenerate with
`../../scripts/gen-proto.sh` (needs `flatc`).

Typed layers over the generated code:
- `limits`  — hard wire caps (`MAX_FRAME_BYTES` etc.), validated **before** allocation.
- `framing` — the `HYFR` frame header + BLAKE3 tag, validated before parsing the flatbuffer.
- `pos`     — `InputPos`/`OutputPos` newtypes: position discipline (spec I13) at the type level.
