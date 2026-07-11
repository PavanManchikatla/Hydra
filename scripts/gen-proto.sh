#!/usr/bin/env bash
# Regenerate hydra-proto Rust from the authoritative FlatBuffers schemas.
# BLUEPRINT §1.2: generated code is the source of truth; no handwritten shadow structs.
set -euo pipefail
cd "$(dirname "$0")/.."

CRATE=crates/hydra-proto
OUT=$CRATE/src/generated

command -v flatc >/dev/null || { echo "flatc not found (brew install flatbuffers)"; exit 1; }

flatc --rust -o "$OUT" "$CRATE/schemas/hydra-proto.fbs"
flatc --rust -o "$OUT" "$CRATE/schemas/wal-records.fbs"

# flatc derives module names from filenames; the schema uses hyphens, invalid as Rust paths.
mv -f "$OUT/hydra-proto_generated.rs" "$OUT/hydra_proto_generated.rs"
mv -f "$OUT/wal-records_generated.rs" "$OUT/wal_records_generated.rs"
# The WAL schema `include`s hydra-proto and references its types as `super::proto::*`
# (i.e. expects a `proto` module beside `wal` under a shared `hydra` module). Wire that up:
#  1. rewrite the invalid hyphenated include glob to the real generated path, and
#  2. re-export the proto module as `hydra::proto` so `super::proto::...` resolves.
perl -pi -e 's{use crate::hydra-proto_generated::\*;}{use crate::generated::hydra_proto_generated::hydra::proto::*;}g' \
    "$OUT/wal_records_generated.rs"
perl -0pi -e 's{(pub mod hydra \{\n)}{$1  pub use crate::generated::hydra_proto_generated::hydra::proto;\n}' \
    "$OUT/wal_records_generated.rs"

echo "regenerated $OUT/{hydra_proto,wal_records}_generated.rs"
