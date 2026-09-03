#!/usr/bin/env bash
# scripts/state-commit.sh — commit ONLY after the PROJECT_STATE edit step succeeded (rule 3 / §11).
#
# The aa7ab70 shape: an edit script died before writing PROJECT_STATE.md and the commit ran anyway,
# so a commit changed project reality without the file. This wrapper makes that impossible: the
# edit script runs under `set -euo pipefail`; if it exits non-zero, or PROJECT_STATE.md is unchanged
# afterwards, nothing is committed and the exit is loud.
#
#   scripts/state-commit.sh <edit-script> [edit-script args...] -- <paths to add...> -m "<message>"
#   scripts/state-commit.sh --no-edit -- <paths...> -m "<message>"    # PROJECT_STATE already edited
#
# The paths must include PROJECT_STATE.md (it is added regardless). Trailers per §0(a) rule 1 are
# appended by the caller inside the message.
set -euo pipefail
cd "$(dirname "$0")/.."
EDIT=(); NOEDIT=0
if [ "${1:-}" = "--no-edit" ]; then NOEDIT=1; shift; else while [ $# -gt 0 ] && [ "$1" != "--" ]; do EDIT+=("$1"); shift; done; fi
[ "${1:-}" = "--" ] || { echo "state-commit: expected -- before the paths" >&2; exit 2; }; shift
PATHS=(); MSG=""
while [ $# -gt 0 ]; do case "$1" in -m) MSG="$2"; shift 2;; *) PATHS+=("$1"); shift;; esac; done
[ -n "$MSG" ] || { echo "state-commit: -m message required" >&2; exit 2; }
# The reference PROJECT_STATE.md is compared against is what is COMMITTED (HEAD), not the working
# tree before the edit: with --no-edit the working tree is already edited, and comparing a hash
# against itself would refuse every correct commit (found on the guard's first --no-edit use).
if [ "$NOEDIT" -eq 0 ]; then
  [ "${#EDIT[@]}" -gt 0 ] || { echo "state-commit: no edit script given" >&2; exit 2; }
  "${EDIT[@]}" || { echo "state-commit: REFUSED — the edit step exited non-zero; nothing committed" >&2; exit 1; }
fi
git diff --quiet HEAD -- PROJECT_STATE.md && { echo "state-commit: REFUSED — PROJECT_STATE.md is unchanged against HEAD (§11 same-commit rule); nothing committed" >&2; exit 1; }
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
# An empty extra-path list is legal (a PROJECT_STATE-only commit); `${PATHS[@]+...}` is the bash-3.2-safe spelling under set -u.
git add PROJECT_STATE.md ${PATHS[@]+"${PATHS[@]}"}
git commit -q -m "$MSG"
echo "state-commit: committed $(git rev-parse --short HEAD)"
