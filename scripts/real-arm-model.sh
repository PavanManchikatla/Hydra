#!/usr/bin/env bash
# scripts/real-arm-model.sh — fetch the real-arm model by PINNED URL and verify its SHA-256 BEFORE
# anything uses it (.github/workflows/real-arm-nightly.yml; lane C, 2026-09-24).
#
#   scripts/real-arm-model.sh <dest-file> <url> <expected-sha256>
#   scripts/real-arm-model.sh --selftest            # this script's own oracle (no network)
#
# Rule 25: the failure mode must be expressible. Every outcome prints exactly one `verdict=` line:
#   verdict=VERIFIED       the file at <dest-file> has exactly <expected-sha256>        (exit 0)
#   verdict=INCONCLUSIVE   no usable pin, the fetch failed, or the hash does not match  (exit 1)
# A mismatch or a failed fetch is never a skip and never GREEN: nothing downstream may run on a
# file this script did not VERIFY, and a mismatching file is DELETED so a later step cannot use it
# by accident. An already-present <dest-file> (a cache restore) is re-hashed, never trusted.
#
# `observed-sha256=` is always printed when a file was obtained, so an unpinned first run and a
# mismatch both name the hash that was actually served (the pin is then a decision a human makes
# against a second source — e.g. the owner's own local copy — never a copy-paste from this log).
set -u

verify() {
  local dest="$1" url="$2" pin="$3" want got bytes
  want=$(printf '%s' "$pin" | tr 'A-F' 'a-f')
  if [ ! -s "$dest" ]; then
    mkdir -p "$(dirname "$dest")"
    echo "model: fetching $url"
    # A server's advertised hash is printed as a SECOND SOURCE only; it is never what we compare to.
    curl -fsSIL --retry 3 --max-time 60 "$url" 2>/dev/null | grep -iE '^(x-linked-etag|x-linked-size|x-repo-commit|etag):' | sed 's/^/model: advertised /' || true
    if ! curl -fL --retry 3 --retry-delay 5 --connect-timeout 30 -o "$dest.partial" "$url"; then
      rm -f "$dest.partial"
      echo "verdict=INCONCLUSIVE (model fetch failed: $url — nothing was verified, so nothing may run on it)"
      return 1
    fi
    mv "$dest.partial" "$dest"
  else
    echo "model: $dest already present (cache restore) — re-hashing, not trusting"
  fi
  got=$(sha256sum "$dest" | awk '{print $1}')
  bytes=$(stat -c %s "$dest" 2>/dev/null || wc -c <"$dest" | tr -d ' ')
  echo "observed-sha256=$got bytes=$bytes"
  if ! printf '%s' "$want" | grep -qE '^[0-9a-f]{64}$'; then
    rm -f "$dest"
    echo "verdict=INCONCLUSIVE (no pinned SHA-256 — expected '$pin' is not 64 hex digits; the file was deleted unused)"
    return 1
  fi
  if [ "$got" != "$want" ]; then
    rm -f "$dest"
    echo "verdict=INCONCLUSIVE (model SHA-256 mismatch: expected $want, observed $got — the file was deleted unused)"
    return 1
  fi
  echo "verdict=VERIFIED (sha256=$got bytes=$bytes)"
}

selftest() {
  local d; d=$(mktemp -d); local fails=0
  printf 'a model-shaped payload\n' > "$d/src.bin"
  local good; good=$(sha256sum "$d/src.bin" | awk '{print $1}')
  local bad; bad="$(printf '%s' "$good" | cut -c1-63)$( [ "${good: -1}" = 0 ] && echo 1 || echo 0 )"
  expect() { # label want-exit want-verdict-prefix -- verify args
    local label="$1" wexit="$2" wv="$3"; shift 4
    local out rc=0; out=$(verify "$@" 2>&1) || rc=$?
    if [ "$rc" -eq "$wexit" ] && printf '%s\n' "$out" | grep -q "^verdict=$wv"; then
      echo "selftest OK   [$label] exit=$rc $(printf '%s\n' "$out" | grep '^verdict=')"
    else
      echo "selftest FAIL [$label] expected exit=$wexit verdict=$wv, got exit=$rc: $out"; fails=$((fails+1))
    fi
  }
  expect "good hash" 0 "VERIFIED" -- "$d/1/m.gguf" "file://$d/src.bin" "$good"
  expect "cache hit re-hashed, good" 0 "VERIFIED" -- "$d/1/m.gguf" "file://$d/nonexistent" "$good"
  expect "corrupted hash (last hex digit flipped)" 1 "INCONCLUSIVE (model SHA-256 mismatch" -- "$d/2/m.gguf" "file://$d/src.bin" "$bad"
  [ -e "$d/2/m.gguf" ] && { echo "selftest FAIL [corrupted hash] the mismatching file was left on disk"; fails=$((fails+1)); }
  cp "$d/src.bin" "$d/3.gguf"; printf 'x' >> "$d/3.gguf"
  expect "tampered cached file" 1 "INCONCLUSIVE (model SHA-256 mismatch" -- "$d/3.gguf" "file://$d/nonexistent" "$good"
  expect "failed fetch" 1 "INCONCLUSIVE (model fetch failed" -- "$d/4/m.gguf" "file://$d/nonexistent" "$good"
  expect "unpinned" 1 "INCONCLUSIVE (no pinned SHA-256" -- "$d/5/m.gguf" "file://$d/src.bin" "UNPINNED"
  rm -rf "$d"
  [ "$fails" -eq 0 ] && { echo "selftest verdict=GREEN (all cases produced the verdict they guard)"; return 0; }
  echo "selftest verdict=RED ($fails case(s) did not produce the verdict they guard)"; return 1
}

if [ "${1:-}" = "--selftest" ]; then selftest; exit $?; fi
[ $# -eq 3 ] || { echo "usage: $0 <dest-file> <url> <expected-sha256> | --selftest" >&2; exit 2; }
verify "$1" "$2" "$3"
