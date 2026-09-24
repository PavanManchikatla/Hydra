# verification/branch-deltas/ — the parallel-branch record (rule 3, amended 2026-09-24)

PROJECT_STATE.md §0(a) rule 3 is binding; this file only describes the format it names.

On any branch other than `main`, a commit that changes project reality does **not** edit
`PROJECT_STATE.md`. It records the change, in the same commit, in
`verification/branch-deltas/<branch>.md`, where `<branch>` is the branch name with every `/`
replaced by `-` (e.g. `claude/foo-bar` → `claude-foo-bar.md`). A detached HEAD has no branch name;
it uses `detached-<short-sha-of-the-parent-commit>.md`. `scripts/state-commit.sh` enforces this:
on a non-main branch it accepts the delta file as satisfying the reality-change check and refuses
any staged change to `PROJECT_STATE.md`.

The integrator folds each delta into `PROJECT_STATE.md` **in the merge commit** and deletes the
delta file **in that same commit**. Branches reach `main` by merge commit only: no squash, no rebase
of pushed commits. On `main`, rule 3 is unchanged.

## Format

One file per branch, append-only while the branch lives. One entry per reality-changing commit,
newest last:

```markdown
## <YYYY-MM-DD> — <one-line summary>

- **Commits:** <short hash(es)>  <!-- the hash of the entry's own commit is unknown when it is
  written; write `this commit` and name it in the next entry or in the session report -->
- **What changed in reality:** <the fact, stated as it would be stated in PROJECT_STATE — verified
  facts only, never aspirational (§11.4)>
- **Lands in:** <PROJECT_STATE section(s), e.g. §0(b), §6, §7 (new §7.NN), §8 row X, §12>
- **Receipts (verbatim):**
  ```
  <quoted verdict= / test result: / selftest lines — never a job status (rule 16)>
  ```
```

An entry with no receipt says so explicitly (`Receipts: none — <why>`); a claim that could not be
proven in the environment that made it is written **NOT RUN HERE**, never inferred.
