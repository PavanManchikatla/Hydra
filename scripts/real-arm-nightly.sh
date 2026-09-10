#!/bin/bash
# scripts/real-arm-nightly.sh — the NIGHTLY full real-arm workspace run (design authority, 2026-09-09, item 5).
# Receipt discipline: sessions do NOT run the full real arm interactively any more (an 8 GB machine pages a
# model-loading suite out to 68–98 minutes when anything else runs beside it). This job runs alone, under
# `nice`, and writes verification/ci-results/real-arm-<date>.md on completion; sessions QUOTE the latest one
# and run targeted real-arm suites for the crates they touch plus the full stub arm.
# Install: packaging/launchd/com.hydra.real-arm-nightly.plist (see its comment); runs 03:00 local.
set -u
HERE="$(cd "$(dirname "$0")/.." && pwd)"; cd "$HERE"
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
DATE="$(date -u +%Y-%m-%d)"; STAMP="$(date -u +%FT%TZ)"; OUT="$(mktemp -d)"
# Refuse to overlap anything heavy (rule: nothing heavy overlaps a receipt run).
if pgrep -f "cargo (test|build|clippy)|tlc2.TLC" >/dev/null; then
  printf '# Real-arm nightly — %s\n\n**verdict=SKIPPED** — another cargo/TLC process was running at %s; nothing heavy may overlap a receipt run.\n' "$DATE" "$STAMP" > "verification/ci-results/real-arm-$DATE.md"; exit 0
fi
nice -n 15 scripts/test-receipt.sh --arms real --out "$OUT" > "$OUT/receipt.log" 2>&1; rc=$?
ARM="$(grep -E '^arm=real' "$OUT/receipt.log" | tail -1)"
TOOL="$(grep -E '^test-receipt: 20' "$OUT/receipt.log" | head -1 | sed 's/^test-receipt: //')"
FAILED="$(grep -E '^test .* FAILED$' "$OUT/real.stdout" 2>/dev/null | head -40)"
{
  echo "# Real-arm nightly receipt — $DATE"
  echo
  echo "**Tree:** \`$(git rev-parse --short HEAD)\` ($(git status --porcelain | wc -l | tr -d ' ') dirty files) · started $STAMP · ended $(date -u +%FT%TZ) · \`nice -n 15\`, alone on the box · $TOOL"
  echo
  echo "**Receipt (rule 16, the summariser's own line):**"; echo; echo '```'; echo "${ARM:-arm=real (no arm line — the run did not finish; exit=$rc)}"; echo '```'
  if [ -n "$FAILED" ]; then echo; echo "**Failed tests:**"; echo; echo '```'; echo "$FAILED"; echo '```'; fi
  echo; echo "Logs kept at \`$OUT\` on the machine that ran it (not committed)."
} > "verification/ci-results/real-arm-$DATE.md"
echo "real-arm-nightly: wrote verification/ci-results/real-arm-$DATE.md (rc=$rc)"
