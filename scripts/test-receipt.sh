#!/usr/bin/env bash
# scripts/test-receipt.sh — the CHECKED full-suite summariser (PROJECT_STATE §9, §8 "checked
# summariser" row; design-authority directive 2026-09-01 item 1).
#
# Runs `cargo test --workspace --no-fail-fast -- --test-threads=1` once per arm — the real engine,
# then HYDRA_FORCE_ENGINE_STUB=1 — strictly one after the other (overlapping cargo invocations
# contend for the target lock, §0(d)), with stdout and stderr in SEPARATE files (§9: ggml's Metal
# teardown writes to stderr and corrupts libtest's stdout result lines when merged), and prints a
# receipt whose numbers are checked against each other.
#
# Rule 25 — the failure mode must be expressible. The receipt EXITS NON-ZERO with
# `verdict=INCONCLUSIVE` when running == 0, when readable == 0, or when mangled > 0. A run that
# produced no events is not a clean run; it is no run. Test this on an empty log before trusting it
# on a real one (`--summarise /dev/null /dev/null` must say INCONCLUSIVE).
#
# Usage:
#   scripts/test-receipt.sh [--out DIR] [--arms real,stub] [--toolchain 1.98.0] [-- <extra cargo test args>]
#   scripts/test-receipt.sh --summarise STDOUT STDERR [ARM-LABEL]      # classify an existing pair of logs
#
# Extra args after `--` are appended to `cargo test` BEFORE the libtest `--` (package/test filters),
# e.g. `-- -p hydra-worker --test d1_two_stage`. HYDRA_TEST_ENV="K=V K2=V2" adds environment for the
# run (used for n_gpu_layers overrides) and is echoed into the receipt.
set -u

OUT=""; ARMS="real,stub"; TOOLCHAIN=""; SUMMARISE=0; EXTRA=()
while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT="$2"; shift 2;;
    --arms) ARMS="$2"; shift 2;;
    --toolchain) TOOLCHAIN="$2"; shift 2;;
    --summarise) SUMMARISE=1; shift; break;;
    --) shift; EXTRA=("$@"); break;;
    *) echo "unknown arg: $1" >&2; exit 2;;
  esac
done

# ---- the summariser -------------------------------------------------------------------------
# Counts:
#   running   = suites cargo STARTED  (stderr: "     Running <path>" and "   Doc-tests <crate>")
#   readable  = COMPLETE libtest result lines in stdout
#               ("test result: ok|FAILED. N passed; N failed; N ignored; ...")
#   mangled   = running - readable  (a started suite whose verdict cannot be read)
# A mangled line is not merely a formatting nuisance: it is a suite silently dropped from every
# count taken off the log (§7.61), so it is INCONCLUSIVE, never subtracted quietly.
summarise() {
  local so="$1" se="$2" label="${3:-arm}" exitcode="${4:-?}" wall="${5:-?}"
  local running readable mangled passed failed ignored ggml stubmsgs verdict rc=0
  running=$(grep -cE '^[[:space:]]+(Running|Doc-tests) ' "$se" || true)
  readable=$(grep -cE '^test result: (ok|FAILED)\. [0-9]+ passed; [0-9]+ failed; [0-9]+ ignored; [0-9]+ measured; [0-9]+ filtered out' "$so" || true)
  running=${running:-0}; readable=${readable:-0}
  mangled=$(( running - readable ))
  read -r passed failed ignored <<<"$(grep -oE '^test result: (ok|FAILED)\. [0-9]+ passed; [0-9]+ failed; [0-9]+ ignored' "$so" \
      | awk '{p+=$4; f+=$6; i+=$8} END {print p+0, f+0, i+0}')"
  ggml=$(grep -c 'GGML_ASSERT' "$se" || true); ggml=${ggml:-0}
  stubmsgs=$(grep -c 'building a stub\|building the STUB' "$se" || true); stubmsgs=${stubmsgs:-0}

  if [ "$running" -eq 0 ]; then verdict="INCONCLUSIVE (running=0 — no suite was started; nothing was measured)"; rc=1
  elif [ "$readable" -eq 0 ]; then verdict="INCONCLUSIVE (readable=0 — no complete result line; nothing can be counted)"; rc=1
  elif [ "$mangled" -ne 0 ]; then verdict="INCONCLUSIVE (mangled=$mangled — $running suites started, $readable readable verdicts; a suite's verdict is unreadable)"; rc=1
  elif [ "$failed" -ne 0 ] || grep -qE '^test result: FAILED' "$so"; then verdict="RED (failed=$failed)"; rc=1
  elif [ "$exitcode" != "0" ] && [ "$exitcode" != "?" ]; then verdict="RED (cargo exit=$exitcode with no failing result line — read the stderr)"; rc=1
  else verdict="GREEN"; fi

  echo "arm=$label exit=$exitcode running=$running readable=$readable mangled=$mangled passed=$passed failed=$failed ignored=$ignored ggml_assert=$ggml stub_msgs=$stubmsgs wall=$wall verdict=$verdict"
  if [ "$failed" -ne 0 ]; then grep -E '^test .* \.\.\. FAILED$' "$so" | sed 's/^/  failed: /'; fi
  if [ "$ggml" -ne 0 ]; then grep 'GGML_ASSERT' "$se" | head -3 | sed 's/^/  ggml_assert: /'; fi
  return $rc
}

if [ "$SUMMARISE" -eq 1 ]; then
  [ $# -ge 2 ] || { echo "usage: --summarise STDOUT STDERR [LABEL]" >&2; exit 2; }
  summarise "$1" "$2" "${3:-arm}" "?" "?"
  exit $?
fi

# ---- the run ----------------------------------------------------------------------------------
cd "$(dirname "$0")/.."
OUT="${OUT:-target/test-receipt/$(date -u +%Y%m%dT%H%M%SZ)}"
mkdir -p "$OUT"
CARGO=(cargo); [ -n "$TOOLCHAIN" ] && CARGO=(cargo "+$TOOLCHAIN")
if [ -n "$TOOLCHAIN" ]; then echo "test-receipt: $(date -u +%Y-%m-%dT%H:%M:%SZ) · toolchain=$(rustup run "$TOOLCHAIN" rustc --version) · $(rustup run "$TOOLCHAIN" cargo --version)"; else echo "test-receipt: $(date -u +%Y-%m-%dT%H:%M:%SZ) · toolchain=$(rustc --version) (default) · $(cargo --version)"; fi
# Package selection: `--workspace` unless the caller passed an explicit package filter (`-p`), in
# which case that filter REPLACES it — `--workspace -p X` re-resolves features and silently rebuilds.
SCOPE=(--workspace); for a in ${EXTRA[@]+"${EXTRA[@]}"}; do [ "$a" = "-p" ] || [ "$a" = "--package" ] && SCOPE=(); done
FILTER=(); [ -n "${HYDRA_TEST_FILTER:-}" ] && FILTER=("$HYDRA_TEST_FILTER")
echo "test-receipt: scope=${SCOPE[*]:-(explicit)} extra=${EXTRA[*]:-(none)} filter=${HYDRA_TEST_FILTER:-(none)} env=${HYDRA_TEST_ENV:-(none)} out=$OUT"
echo "test-receipt: command per arm: cargo test ${SCOPE[*]:-} --no-fail-fast ${EXTRA[*]:-} -- --test-threads=1 ${FILTER[*]:-}   (> ARM.stdout 2> ARM.stderr)"

overall=0
IFS=',' read -ra ARMLIST <<<"$ARMS"
for arm in "${ARMLIST[@]}"; do
  so="$OUT/$arm.stdout"; se="$OUT/$arm.stderr"
  start=$(date +%s)
  if [ "$arm" = "stub" ]; then
    env ${HYDRA_TEST_ENV:-} HYDRA_FORCE_ENGINE_STUB=1 "${CARGO[@]}" test ${SCOPE[@]+"${SCOPE[@]}"} --no-fail-fast ${EXTRA[@]+"${EXTRA[@]}"} -- --test-threads=1 ${FILTER[@]+"${FILTER[@]}"} >"$so" 2>"$se"; ec=$?
  else
    env ${HYDRA_TEST_ENV:-} "${CARGO[@]}" test ${SCOPE[@]+"${SCOPE[@]}"} --no-fail-fast ${EXTRA[@]+"${EXTRA[@]}"} -- --test-threads=1 ${FILTER[@]+"${FILTER[@]}"} >"$so" 2>"$se"; ec=$?
  fi
  end=$(date +%s); wall="$(( (end-start)/60 ))m$(( (end-start)%60 ))s"
  summarise "$so" "$se" "$arm" "$ec" "$wall" || overall=1
done
if [ "$overall" -eq 0 ]; then echo "verdict=GREEN (every arm GREEN; counts cross-checked: running == readable on each)"; else echo "verdict=NOT-GREEN (at least one arm is INCONCLUSIVE or RED — see the arm lines above)"; fi
exit $overall
