## 2026-09-24 — lane C: the real arm in CI (workflow written; not yet observed GREEN)

- **Commits:** this commit
- **What changed in reality:**
  - New workflow `.github/workflows/real-arm-nightly.yml` (nightly 02:23 UTC, `workflow_dispatch`,
    `pull_request` filtered to the workflow file). It runs on GitHub-hosted `ubuntu-latest`
    (**Linux x86_64**; a result here is not a macOS claim), builds the pinned engine **CPU-only** with
    fuzz.yml's steps and cache key, fetches the dev model (`qwen2.5-0.5b-instruct-fp16.gguf`) by a
    revision-pinned Hugging Face URL, verifies its SHA-256 **before** use, caches it keyed on the hash,
    and produces **two receipts through `scripts/test-receipt.sh`**: (a) the three product oracles
    (`generation_e2e`, `restart_e2e`, `stage_loss_e2e`, crate `hydra-node`, against the shipped
    binaries); (b) a targeted real arm (hydra-state's six suites; hydra-node's `binary_auth`,
    `binary_restart`, `token_file`; hydra-worker's `recovery`, `d1_recovery`, `three_node_recovery`,
    `survivor_reactivate`). The owner's 2026-09-10 10-suite selection is not recorded in the tree, so
    (b) is a named reconstruction whose counts are not comparable with that receipt.
  - **The model pin is `UNPINNED` in this commit.** Hugging Face and docs.github.com are unreachable
    from this container (proxy 403 / `EGRESS_BLOCKED`), and the repo records no hash for the dev model.
    The first run therefore must end `verdict=INCONCLUSIVE (no pinned SHA-256 …)` and prints the
    served hash; pinning is the next commit. That the served file is byte-identical to the owner's
    local model is **not established** by any of this.
  - New helpers, each with a `--selftest`: `scripts/real-arm-model.sh` (fetch + verify; a mismatch,
    a failed fetch or no pin → `verdict=INCONCLUSIVE`, and the file is deleted unused) and
    `scripts/real-arm-classify.sh` (quotes the receipt's own `arm=`/`verdict=` lines, then refuses a
    GREEN with any `SKIP:` line, without the `successes:` section `--show-output` prints, or with
    `stub_msgs≠0`).
  - **Finding (rule 25, for §7/§8): `scripts/test-receipt.sh` counts a skipped engine-gated test as
    PASSED.** The engine-gated tests `eprintln!("SKIP: …")` and `return` when `dev_model_path()` is
    `None`; libtest reports them `ok`. Observed here with the engine built and **no model**: the
    receipt printed GREEN for 8 oracle tests that had all skipped. Nothing in the receipt line can
    express it. The workflow guards it (classify script); the receipt itself is unchanged in this lane
    and the §8 row is proposed: *make `test-receipt.sh` count `SKIP:` lines (with `--show-output`) and
    refuse GREEN when any is present on an arm that claims `real`*.
  - Nothing retired: `scripts/real-arm-nightly.sh` and the launchd plist are untouched (the ruling
    retires the plist only after three consecutive nightly GREEN receipts from this workflow, quoted).
- **Lands in:** §0(b) (nightly real arm), §0(d) (receipt discipline), §6 (a rule-19 "cannot see"
  line for the workflow: Linux/CPU only; model identity with the Mac's file), §7 (new: the SKIP-as-
  passed finding), §8 (new row: test-receipt SKIP counting; the plist retirement condition), §12.
- **Receipts (verbatim):**
  ```
  # this container (Linux x86_64, 4 cores, engine built CPU-only with fuzz.yml's flags, NO model):
  arm=real exit=0 running=3 readable=3 mangled=0 passed=8 failed=0 ignored=0 ggml_assert=0 stub_msgs=0 wall=0m25s verdict=GREEN
  scope=oracles verdict=INCONCLUSIVE (skipped=8 — 8 test(s) printed SKIP: and were counted as passed; the engine or the model was not reachable from the test)
  arm=real exit=0 running=13 readable=13 mangled=0 passed=52 failed=0 ignored=0 ggml_assert=0 stub_msgs=0 wall=0m21s verdict=GREEN
  scope=targeted verdict=INCONCLUSIVE (skipped=4 — 4 test(s) printed SKIP: and were counted as passed; the engine or the model was not reachable from the test)
  scripts/real-arm-model.sh --selftest    → selftest verdict=GREEN (all cases produced the verdict they guard)
    mutant (hash comparison removed)      → selftest verdict=RED (3 case(s) did not produce the verdict they guard)
  scripts/real-arm-classify.sh --selftest → selftest verdict=GREEN (all cases produced the verdict they guard)
    mutant (SKIP guard removed)           → selftest verdict=RED (1 case(s) did not produce the verdict they guard)
  scripts/test-receipt.sh --selftest      → selftest verdict=GREEN (all cases produced the verdict they guard)
  ```
  The engine-gated product oracles and the real arm themselves: **NOT RUN HERE** (no model in a cloud
  container). The CI run's verdicts are quoted in the next entry.
