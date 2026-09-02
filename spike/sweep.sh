#!/usr/bin/env bash
# spike/sweep.sh — the M−1 spike sweep as a RE-RUNNABLE procedure (BLUEPRINT §1.2; PROJECT_STATE §8).
#
# Runs spike/build/shard_split over every split point × every prompt in spike/sweep-prompts.txt,
# printing the same `===== k=<k> prompt=P<n> =====` / `PROMPT_TEXT:` markers the banked sweep logs
# carry, so a new log is diffable against verification/ci-results/spike-sweep-*.log.
#
#   spike/sweep.sh [-m MODEL] [-ngl N] [-k "1 4 12 18 23"] [-p "1 2 3"] > sweep.log
#
# Defaults: MODEL=models/qwen2.5-0.5b-instruct-fp16.gguf, ngl=0 (the DoD backend is CPU — the Metal
# arm is a separate, characterised run), all five k, all prompts. Exit status is non-zero if any
# combination fails to print `=== M-1 DoD: PASS ===`, and the tally line at the end says how many did.
set -u
cd "$(dirname "$0")"
MODEL="../models/qwen2.5-0.5b-instruct-fp16.gguf"; NGL=0; KS="1 4 12 18 23"; PS=""
while [ $# -gt 0 ]; do
  case "$1" in
    -m) MODEL="$2"; shift 2;; -ngl) NGL="$2"; shift 2;; -k) KS="$2"; shift 2;; -p) PS="$2"; shift 2;;
    *) echo "unknown arg: $1" >&2; exit 2;;
  esac
done
[ -x build/shard_split ] || { echo "spike/build/shard_split not built (see spike/README.md)" >&2; exit 2; }
[ -f "$MODEL" ] || { echo "model not found: $MODEL" >&2; exit 2; }
PROMPTS=(); while IFS= read -r line; do PROMPTS+=("$line"); done < <(grep -vE '^[[:space:]]*(#|$)' sweep-prompts.txt)
[ "${#PROMPTS[@]}" -gt 0 ] || { echo "sweep-prompts.txt has no prompts" >&2; exit 2; }
[ -z "$PS" ] && PS=$(seq -s ' ' 1 "${#PROMPTS[@]}")
echo "SWEEP: model=$MODEL ngl=$NGL k={$KS} prompts={$PS} shard_split=$(shasum build/shard_split | cut -c1-12) $(date -u +%Y-%m-%dT%H:%M:%SZ)"
pass=0; total=0; fail=0
for k in $KS; do
  for n in $PS; do
    p="${PROMPTS[$((n-1))]}"
    total=$((total+1))
    echo "===== k=$k prompt=P$n ====="
    echo "PROMPT_TEXT: $p"
    if out=$(./build/shard_split -m "$MODEL" -k "$k" -p "$p" -ngl "$NGL" 2>&1); then :; fi
    printf '%s\n' "$out"
    if grep -q '^=== M-1 DoD: PASS ===' <<<"$out"; then pass=$((pass+1)); else fail=$((fail+1)); echo "SWEEP-RESULT: k=$k prompt=P$n FAIL"; fi
  done
done
echo "SWEEP SUMMARY combinations=$total pass=$pass fail=$fail verdict=$([ "$fail" -eq 0 ] && [ "$total" -gt 0 ] && echo PASS || echo FAIL)"
[ "$fail" -eq 0 ] && [ "$total" -gt 0 ]
