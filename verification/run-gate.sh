#!/usr/bin/env bash
# Remaining TLC gate runs (Mut1/Mut2/Mut4 already confirmed). Sequential with unique metadirs
# to avoid collisions and RAM contention on an 8 GB machine. Ordered shortest-expected first.
set -uo pipefail
cd "$(dirname "$0")"
JAVA=/opt/homebrew/opt/openjdk/bin/java
JAR=tools/tla2tools.jar
mkdir -p results

run() {
  local name="$1"; shift
  echo "=== $name ($*) start $(date +%H:%M:%S) ==="
  "$JAVA" -Xmx6g -cp "$JAR" tlc2.TLC -workers auto -deadlock -metadir "results/meta-$name" "$@" \
      HydraActivationCore.tla > "results/$name.out" 2>&1
  echo "$name exit=$? $(date +%H:%M:%S)"
  grep -E "is violated|Temporal properties were violated|Model checking completed|No error|Error: The behavior|distinct states found" \
      "results/$name.out" | tail -4
  echo
}

run mut3          -config Mut3AttemptFence.cfg
run baseline-live -config BaselineLiveness.cfg
run baseline-safe -config BaselineSafety.cfg
echo "GATE-REMAINING DONE $(date +%H:%M:%S)"
