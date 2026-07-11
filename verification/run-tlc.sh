#!/usr/bin/env bash
# Run the standing TLC gate (BLUEPRINT §4.3). Records raw output under verification/results/.
set -uo pipefail
cd "$(dirname "$0")"
JAVA=/opt/homebrew/opt/openjdk/bin/java
JAR=tools/tla2tools.jar
mkdir -p results

run() {
  local name="$1"; shift
  echo "=== $name ($*) ==="
  # -deadlock: TERMINAL is deliberately absorbing (VERIFICATION-README)
  "$JAVA" -cp "$JAR" tlc2.TLC -workers auto -deadlock "$@" HydraActivationCore.tla \
      > "results/$name.out" 2>&1
  local rc=$?
  echo "$name exit=$rc"
  grep -E "Model checking completed|violated|Error:|is violated|states generated|distinct states|The behavior up to|Temporal properties were violated|No errors|deadlock" \
      "results/$name.out" | head -12
  echo
}

run baseline-safety   -config BaselineSafety.cfg
run baseline-liveness -config BaselineLiveness.cfg
run mut1-unservable   -config Mut1Unservable.cfg
run mut3-attemptfence -config Mut3AttemptFence.cfg
echo "ALL DONE"
