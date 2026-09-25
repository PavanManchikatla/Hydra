#!/usr/bin/env bash
# scripts/real-arm-classify.sh — classify ONE scope of the CI real arm
# (.github/workflows/real-arm-nightly.yml; lane C, 2026-09-24).
#
#   scripts/real-arm-classify.sh <scope-label> <receipt-log> <receipt-out-dir>
#   scripts/real-arm-classify.sh --selftest
#
# The receipt itself comes ONLY from scripts/test-receipt.sh (rule 16); this script quotes its
# `arm=` and `verdict=` lines unchanged and adds ONE thing the receipt cannot express:
#
# **An engine-gated test that finds no engine or no model prints `SKIP:` and RETURNS — libtest
# counts it as PASSED.** Observed in this session (2026-09-24, cloud container, no model): the three
# product oracles gave `arm=real exit=0 running=3 readable=3 mangled=0 passed=8 failed=0 ignored=0
# ... verdict=GREEN` while every one of the 8 tests had printed `SKIP: no engine/model`. The
# receipt's GREEN is honest about the counts and silent about the skips (rule 25). So:
#   - the run must be made with libtest `--show-output` (the workflow passes it as
#     HYDRA_TEST_FILTER), so a PASSING test's captured stderr reaches the stdout log — proven here
#     by the `successes:` section it prints; without that section the skip count is unreadable,
#     and the scope is INCONCLUSIVE;
#   - any `SKIP:` line in that log makes the scope INCONCLUSIVE, whatever the receipt says;
#   - the receipt's own `stub_msgs` must be 0 (an engine-less build is not the real arm).
# Only a receipt `verdict=GREEN` that survives all three is GREEN. RED stays RED.
set -u

classify() {
  local scope="$1" log="$2" out="$3" so arm final skips stub verdict
  so="$out/real.stdout"
  arm=$(grep -E '^arm=' "$log" | tail -1)
  final=$(grep -E '^verdict=' "$log" | tail -1)
  echo "scope=$scope receipt: ${arm:-(no arm= line)}"
  echo "scope=$scope receipt: ${final:-(no verdict= line)}"
  if [ -z "$arm" ] || [ -z "$final" ]; then
    verdict="INCONCLUSIVE (the receipt printed no arm=/verdict= line — the run did not finish)"
  elif [[ "$arm" == *"verdict=RED"* ]]; then
    verdict="RED (the receipt says RED)"
  elif [[ "$arm" != *"verdict=GREEN"* ]]; then
    verdict="INCONCLUSIVE (the receipt is not GREEN: ${arm##*verdict=})"
  else
    skips=$(grep -c 'SKIP:' "$so" 2>/dev/null || true); skips=${skips:-0}
    stub=$(printf '%s' "$arm" | grep -oE 'stub_msgs=[0-9]+' | cut -d= -f2)
    if ! grep -qE '^successes:$' "$so" 2>/dev/null; then
      verdict="INCONCLUSIVE (no 'successes:' section in the stdout log — --show-output did not take effect, so skipped tests cannot be told from passed ones)"
    elif [ "$skips" -ne 0 ]; then
      verdict="INCONCLUSIVE (skipped=$skips — $skips test(s) printed SKIP: and were counted as passed; the engine or the model was not reachable from the test)"
    elif [ "${stub:-1}" != "0" ]; then
      verdict="INCONCLUSIVE (stub_msgs=${stub:-?} — the engine built as a stub; this is not the real arm)"
    else
      verdict="GREEN (receipt GREEN, skipped=0, stub_msgs=0)"
    fi
  fi
  echo "scope=$scope verdict=$verdict"
  [[ "$verdict" == GREEN* ]]
}

selftest() {
  local d; d=$(mktemp -d); local fails=0
  mk() { # dir arm-line final-line stdout-body
    mkdir -p "$d/$1"; printf '%s\n%s\n' "$2" "$3" > "$d/$1/log"; printf '%b' "$4" > "$d/$1/real.stdout"
  }
  expect() { local out; out=$(classify "$1" "$d/$1/log" "$d/$1" || true)
    if printf '%s\n' "$out" | grep -q "^scope=$1 verdict=$2"; then echo "selftest OK   [$1] -> $2"; else echo "selftest FAIL [$1] expected $2, got: $out"; fails=$((fails+1)); fi; }
  local G='arm=real exit=0 running=3 readable=3 mangled=0 passed=8 failed=0 ignored=0 ggml_assert=0 stub_msgs=0 wall=1m verdict=GREEN'
  local F='verdict=GREEN (every arm GREEN; counts cross-checked: running == readable on each)'
  mk green "$G" "$F" 'successes:\n\n---- t stdout ----\nfine\n\ntest result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n'
  expect green "GREEN"
  mk skipped "$G" "$F" 'successes:\n\n---- t stderr ----\nSKIP: no engine/model (CI status: unavailable)\n\ntest result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n'
  expect skipped "INCONCLUSIVE (skipped=1"
  mk noshow "$G" "$F" 'test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n'
  expect noshow "INCONCLUSIVE (no 'successes:'"
  mk stub "${G/stub_msgs=0/stub_msgs=1}" "$F" 'successes:\n\ntest result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n'
  expect stub "INCONCLUSIVE (stub_msgs=1"
  mk red "${G/verdict=GREEN/verdict=RED (failed=1)}" 'verdict=NOT-GREEN (x)' 'test result: FAILED. 7 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n'
  expect red "RED"
  mk zero "${G/verdict=GREEN/verdict=INCONCLUSIVE (passed+failed+ignored=0 — x)}" 'verdict=NOT-GREEN (x)' ''
  expect zero "INCONCLUSIVE (the receipt is not GREEN"
  mk unfinished "" "" ''
  expect unfinished "INCONCLUSIVE (the receipt printed no"
  rm -rf "$d"
  [ "$fails" -eq 0 ] && { echo "selftest verdict=GREEN (all cases produced the verdict they guard)"; return 0; }
  echo "selftest verdict=RED ($fails case(s) did not produce the verdict they guard)"; return 1
}

if [ "${1:-}" = "--selftest" ]; then selftest; exit $?; fi
[ $# -eq 3 ] || { echo "usage: $0 <scope-label> <receipt-log> <receipt-out-dir> | --selftest" >&2; exit 2; }
classify "$1" "$2" "$3"
