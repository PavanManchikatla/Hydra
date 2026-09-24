#!/usr/bin/env bash
# scripts/state-commit.sh — commit ONLY after the state-record edit step succeeded (rule 3 / §11).
#
# The aa7ab70 shape: an edit script died before writing PROJECT_STATE.md and the commit ran anyway,
# so a commit changed project reality without the file. This wrapper makes that impossible: the
# edit script runs under `set -euo pipefail`; if it exits non-zero, or the state record is unchanged
# afterwards, nothing is committed and the exit is loud.
#
#   scripts/state-commit.sh <edit-script> [edit-script args...] -- <paths to add...> -m "<message>"
#   scripts/state-commit.sh --no-edit -- <paths...> -m "<message>"    # state record already edited
#   scripts/state-commit.sh --selftest                                # this guard's own oracle
#
# THE STATE RECORD depends on the branch (rule 3, amended 2026-09-24 for parallel branches):
#   - on `main` it is PROJECT_STATE.md (added regardless), exactly as before;
#   - on any other branch — and on a DETACHED HEAD, which is not main — it is
#     verification/branch-deltas/<branch>.md (`/` → `-`; detached: `detached-<short HEAD sha>`),
#     added regardless, and ANY change to PROJECT_STATE.md (staged or not) is refused: the
#     integrator folds the delta into PROJECT_STATE.md in the merge commit.
# Trailers per §0(a) rule 1 are appended by the caller inside the message.
set -euo pipefail

selftest() {
  # No errexit here: a setup step broken by a mutant must surface as FAIL lines and verdict=RED,
  # never as an abort without a verdict (rule 25). Each case asserts its own outcome.
  set +e
  local src; src="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
  local d; d=$(mktemp -d); local fails=0
  local r="$d/repo"
  mkdir -p "$r/scripts" "$r/verification/branch-deltas"
  cp "$src" "$r/scripts/state-commit.sh"
  g() { git -C "$r" "$@"; }
  g init -q -b main
  g config user.name selftest; g config user.email selftest@invalid; g config commit.gpgsign false
  echo "state v0" > "$r/PROJECT_STATE.md"; echo "code v0" > "$r/code.txt"
  g add -A; g commit -q -m init
  # expect <label> <want-exit: 0|1> <needle in combined output> -- <state-commit args...>
  expect() {
    local label="$1" want="$2" needle="$3"; shift 4
    local out rc=0
    out=$("$r/scripts/state-commit.sh" "$@" 2>&1) || rc=$?
    if [ "$rc" -eq "$want" ] && [[ "$out" == *"$needle"* ]]; then
      echo "selftest OK   [$label] exit=$rc: $(printf '%s' "$out" | head -1)"
    else
      echo "selftest FAIL [$label] expected exit=$want with \"$needle\", got exit=$rc: $out"; fails=$((fails+1))
    fi
  }
  # check <label> <condition-description> <command...> — a post-condition on the repo
  check() {
    local label="$1" what="$2"; shift 2
    if "$@"; then echo "selftest OK   [$label] $what"; else echo "selftest FAIL [$label] $what"; fails=$((fails+1)); fi
  }
  committed_files() { g show --name-only --format= HEAD | sort | tr '\n' ' '; }

  # 1. main, PROJECT_STATE edited → accepted, and the commit carries it.
  echo "state v1" > "$r/PROJECT_STATE.md"; echo "code v1" > "$r/code.txt"
  expect "main + PROJECT_STATE" 0 "state-commit: committed" -- --no-edit -- code.txt -m "c1"
  check "main + PROJECT_STATE" "commit holds PROJECT_STATE.md and code.txt" \
    test "$(committed_files)" = "PROJECT_STATE.md code.txt "

  # 2. main, PROJECT_STATE not edited → refused, nothing committed.
  local before; before=$(g rev-parse HEAD)
  echo "code v2" > "$r/code.txt"
  expect "main without PROJECT_STATE" 1 "REFUSED — PROJECT_STATE.md is unchanged against HEAD (§11 same-commit rule)" -- --no-edit -- code.txt -m "c2"
  check "main without PROJECT_STATE" "HEAD did not move" test "$(g rev-parse HEAD)" = "$before"

  # 2b. main, the edit script fails → refused as before.
  expect "main edit script fails" 1 "REFUSED — the edit step exited non-zero" -- false -- code.txt -m "c2b"
  g checkout -q -- code.txt

  # 3. branch feat/x with its delta → accepted; the commit carries the delta, never PROJECT_STATE.
  g checkout -q -b feat/x
  echo "delta entry 1" > "$r/verification/branch-deltas/feat-x.md"; echo "code v3" > "$r/code.txt"
  expect "branch + delta" 0 "state-commit: committed" -- --no-edit -- code.txt -m "c3"
  check "branch + delta" "commit holds the delta and code.txt, not PROJECT_STATE.md" \
    test "$(committed_files)" = "code.txt verification/branch-deltas/feat-x.md "

  # 3b. branch, delta written by an edit script (the non --no-edit form) → accepted.
  expect "branch + delta via edit script" 0 "state-commit: committed" -- \
    sh -c 'echo "delta entry 2" >> verification/branch-deltas/feat-x.md' -- -m "c3b"

  # 4. branch without a delta change → refused, and the message names the delta path.
  before=$(g rev-parse HEAD)
  echo "code v4" > "$r/code.txt"
  expect "branch without delta" 1 "REFUSED — verification/branch-deltas/feat-x.md is unchanged against HEAD (rule 3, parallel branches: a branch records reality in its delta file)" -- --no-edit -- code.txt -m "c4"
  check "branch without delta" "HEAD did not move" test "$(g rev-parse HEAD)" = "$before"

  # 5. branch touching PROJECT_STATE → refused: unstaged edit, staged edit, and named as a path.
  echo "delta entry 3" >> "$r/verification/branch-deltas/feat-x.md"
  echo "state on branch" > "$r/PROJECT_STATE.md"
  expect "branch edits PROJECT_STATE (unstaged)" 1 "REFUSED — PROJECT_STATE.md is changed on branch 'feat/x'" -- --no-edit -- code.txt -m "c5"
  g add PROJECT_STATE.md
  expect "branch edits PROJECT_STATE (staged)" 1 "REFUSED — PROJECT_STATE.md is changed on branch 'feat/x'" -- --no-edit -- code.txt -m "c5b"
  check "branch edits PROJECT_STATE" "HEAD did not move" test "$(g rev-parse HEAD)" = "$before"
  g reset -q -- PROJECT_STATE.md; g checkout -q -- PROJECT_STATE.md
  expect "branch names PROJECT_STATE as a path" 1 "REFUSED — PROJECT_STATE.md is named as a path on branch 'feat/x'" -- --no-edit -- PROJECT_STATE.md code.txt -m "c5c"

  # 6. dirty index (something staged, not named) → refused as before, on the branch and on main.
  echo "other" > "$r/other.txt"; g add other.txt
  expect "dirty index on branch" 1 "REFUSED — the index already holds staged changes not named on this command line" -- --no-edit -- code.txt -m "c6"
  check "dirty index on branch" "HEAD did not move" test "$(g rev-parse HEAD)" = "$before"
  g reset -q -- other.txt; rm -f "$r/other.txt"; g checkout -q -- code.txt verification/branch-deltas/feat-x.md
  g checkout -q main
  echo "other" > "$r/other.txt"; g add other.txt; echo "state v6" > "$r/PROJECT_STATE.md"
  before=$(g rev-parse HEAD)
  expect "dirty index on main" 1 "REFUSED — the index already holds staged changes not named on this command line" -- --no-edit -- -m "c6b"
  check "dirty index on main" "HEAD did not move" test "$(g rev-parse HEAD)" = "$before"
  g reset -q -- other.txt; rm -f "$r/other.txt"; g checkout -q -- PROJECT_STATE.md

  # 7. detached HEAD is a branch, not main: PROJECT_STATE refused; its delta accepted.
  g checkout -q --detach main
  local short; short=$(g rev-parse --short HEAD)
  echo "state detached" > "$r/PROJECT_STATE.md"
  expect "detached + PROJECT_STATE" 1 "REFUSED — PROJECT_STATE.md is changed on branch '(detached HEAD)'" -- --no-edit -- -m "c7"
  g checkout -q -- PROJECT_STATE.md
  expect "detached without delta" 1 "REFUSED — verification/branch-deltas/detached-$short.md is unchanged against HEAD" -- --no-edit -- -m "c7b"
  mkdir -p "$r/verification/branch-deltas"; echo "detached delta" > "$r/verification/branch-deltas/detached-$short.md"
  expect "detached + delta" 0 "state-commit: committed" -- --no-edit -- -m "c7c"
  check "detached + delta" "commit holds only the detached delta" \
    test "$(committed_files)" = "verification/branch-deltas/detached-$short.md "

  rm -rf "$d"
  [ "$fails" -eq 0 ] && { echo "selftest verdict=GREEN (all cases produced the refusal or commit they guard)"; return 0; }
  echo "selftest verdict=RED ($fails case(s) did not produce the refusal or commit they guard)"; return 1
}
if [ "${1:-}" = "--selftest" ]; then selftest; exit $?; fi

cd "$(dirname "$0")/.."
EDIT=(); NOEDIT=0
if [ "${1:-}" = "--no-edit" ]; then NOEDIT=1; shift; else while [ $# -gt 0 ] && [ "$1" != "--" ]; do EDIT+=("$1"); shift; done; fi
[ "${1:-}" = "--" ] || { echo "state-commit: expected -- before the paths" >&2; exit 2; }; shift
PATHS=(); MSG=""
while [ $# -gt 0 ]; do case "$1" in -m) MSG="$2"; shift 2;; *) PATHS+=("$1"); shift;; esac; done
[ -n "$MSG" ] || { echo "state-commit: -m message required" >&2; exit 2; }
# Which record this commit must change (rule 3). A detached HEAD has no symbolic ref: it is NOT main.
if BRANCH=$(git symbolic-ref --quiet --short HEAD); then :; else BRANCH=""; fi
if [ "$BRANCH" = "main" ]; then
  RECORD=PROJECT_STATE.md
else
  if [ -n "$BRANCH" ]; then BNAME="$BRANCH"; DELTA="verification/branch-deltas/${BRANCH//\//-}.md"
  else BNAME="(detached HEAD)"; DELTA="verification/branch-deltas/detached-$(git rev-parse --short HEAD).md"; fi
  RECORD="$DELTA"
  for p in ${PATHS[@]+"${PATHS[@]}"}; do
    [ "$p" = PROJECT_STATE.md ] && { echo "state-commit: REFUSED — PROJECT_STATE.md is named as a path on branch '$BNAME'; a branch records reality in $DELTA and never edits PROJECT_STATE.md (rule 3, parallel branches); nothing committed" >&2; exit 1; }
  done
fi
# The reference the record is compared against is what is COMMITTED (HEAD), not the working
# tree before the edit: with --no-edit the working tree is already edited, and comparing a hash
# against itself would refuse every correct commit (found on the guard's first --no-edit use).
if [ "$NOEDIT" -eq 0 ]; then
  [ "${#EDIT[@]}" -gt 0 ] || { echo "state-commit: no edit script given" >&2; exit 2; }
  "${EDIT[@]}" || { echo "state-commit: REFUSED — the edit step exited non-zero; nothing committed" >&2; exit 1; }
fi
if [ "$RECORD" = PROJECT_STATE.md ]; then
  git diff --quiet HEAD -- PROJECT_STATE.md && { echo "state-commit: REFUSED — PROJECT_STATE.md is unchanged against HEAD (§11 same-commit rule); nothing committed" >&2; exit 1; }
else
  # Staged or unstaged, a branch never carries a PROJECT_STATE.md change (the integrator folds it).
  git diff --quiet HEAD -- PROJECT_STATE.md || { echo "state-commit: REFUSED — PROJECT_STATE.md is changed on branch '$BNAME'; a branch records reality in $DELTA and never edits PROJECT_STATE.md (rule 3, parallel branches); restore it (git restore --staged --worktree PROJECT_STATE.md); nothing committed" >&2; exit 1; }
  # `git status --porcelain` also sees an untracked (first-entry) delta, which `git diff HEAD` does not.
  [ -n "$(git status --porcelain -- "$DELTA")" ] || { echo "state-commit: REFUSED — $DELTA is unchanged against HEAD (rule 3, parallel branches: a branch records reality in its delta file); nothing committed" >&2; exit 1; }
fi
# The commit's input is the INDEX, not this path list: `git add <paths>` commits whatever else was
# already staged. 06b9649 (2026-09-03) carried three file moves a `git mv` had left in the index and
# put `main` without the coordinator binary's source for 22 minutes (clean-checkout RED). Refuse a
# dirty index up front; the caller unstages or names the paths deliberately.
if ! git diff --cached --quiet; then
  echo "state-commit: REFUSED — the index already holds staged changes not named on this command line:" >&2
  git diff --cached --name-status >&2
  echo "state-commit: unstage them (git restore --staged <path>) or name them as paths; nothing committed" >&2
  exit 1
fi
# An empty extra-path list is legal (a record-only commit); `${PATHS[@]+...}` is the bash-3.2-safe spelling under set -u.
git add "$RECORD" ${PATHS[@]+"${PATHS[@]}"}
git commit -q -m "$MSG"
echo "state-commit: committed $(git rev-parse --short HEAD)"
