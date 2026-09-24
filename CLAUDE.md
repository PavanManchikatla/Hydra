# CLAUDE.md — a pointer, not a rulebook

**The binding rules are `PROJECT_STATE.md` §0(a), standing rules 1–27 and their amendments.**
This file never restates them. A copy would be a second source of truth, and README treats any
disagreement between two records as a defect. If this file and PROJECT_STATE disagree,
PROJECT_STATE wins and this file is the defect to fix.

Authority order: the spec (`docs/hydra-session-protocol.md`) decides correctness, `BLUEPRINT.md`
decides process and scope, and `PROJECT_STATE.md` decides status.

## Reading order

1. `PROJECT_STATE.md` §0, all of it (the rules, what is in flight, what comes next, environment facts).
2. `BLUEPRINT.md`.
3. `scripts/state-commit.sh`: the only way to make a commit that changes project reality.
4. `scripts/test-receipt.sh`: the only way to produce a test receipt.
5. Then whatever PROJECT_STATE §2 points to for the task in hand.

## Setup (report each outcome verbatim; do not work around a failure)

a. **Git identity = the owner, repo-local.** Read `git log -5 --format='%an <%ae> | %s' origin/main`,
   confirm the author is the owner and not a bot, then set `git config user.name` / `user.email`
   from it (no `--global`, no `-c`). Trailers follow rule 1. If this is not possible, STOP.
b. `git submodule update --init --recursive`, then compare `git submodule status` with the pin
   PROJECT_STATE names (§0(d) / §5).
c. Toolchain as the repo pins it: there is no `rust-toolchain` file; `Cargo.toml` states
   `rust-version`, and CI uses `stable`. Quote `rustc -V` and `cargo -V`.

## Rule 3 on a branch (one line; the text is PROJECT_STATE §0(a) rule 3)

Off `main`, record reality in `verification/branch-deltas/<branch>.md` (format in its `README.md`)
and never edit PROJECT_STATE.md. `scripts/state-commit.sh` enforces this. Reach `main` by merge
commit only.

## What a cloud session cannot prove

A cloud container has no GGUF model, no real-engine arm, and none of the owner's machines. These
are **not provable here**:

- engine-gated product oracles (anything that needs the real `hydra-engine-sys` build);
- the real arm (`scripts/real-arm-nightly.sh` and the `arm=real` receipts);
- anything that needs a GGUF model file.

Report such a claim as **NOT RUN HERE**. Never infer it from a stub-arm pass, a neighbouring
test, or a CI job's status.

## Receipts

Produce test evidence only through `scripts/test-receipt.sh`. Quote its `verdict=` line or the
`test result:` lines. A job status, a green check-mark or a matrix label is never evidence (rule 16).

## Session reports

A cloud session writes its report to `verification/session-reports/<YYYY-MM-DD>-<branch>.md`
(branch name with `/` replaced by `-`) and commits it on the branch.
