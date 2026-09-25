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

## 2026-09-24 — lane C: first CI run read (INCONCLUSIVE by design: unpinned); model hash pinned; pipefail

- **Commits:** `378ddcc` (previous entry); this commit
- **What changed in reality:**
  - The first run of the workflow ([run 36073107311](https://github.com/PavanManchikatla/Hydra/actions/runs/36073107311),
    `pull_request` on `378ddcc`, merge ref `026748f`) ran on `ubuntu-24.04` image `20260920.314.1`,
    **Linux x86_64**, `nproc=4`, `MemTotal: 16373452 kB`, `SwapTotal: 3145724 kB`, 86 G free disk;
    `stable` = `rustc 1.98.1 (48a229cea 2026-09-01)`; engine pin `c00bcebf…` restored from fuzz.yml's
    cache key. Both scopes ended INCONCLUSIVE at the model step, as the unpinned commit required; no
    test ran. The helper self-tests (all three) printed `selftest verdict=GREEN` on the runner.
  - **The model is pinned:** `MODEL_SHA256=8e0ae26000627ed62de0e78e41860af70094558b9d2913385c842a6aa06cf3fc`
    (1 266 425 696 bytes) at HF revision `12145bd1d629190a4d44254073650877954d02c9`. Basis: the hash the
    runner measured equals the hash HF advertised for that revision (`x-linked-etag`), in both jobs.
    One origin, two readings, so not independent evidence. **Identity with the owner's local model is
    still unestablished**; the owner can settle it with one `shasum -a 256`.
  - **Workflow defect found in the log and fixed:** `run:` steps executed as `bash -e {0}` (no pipefail),
    so the classify step's `… | tee classify.log || rc=1` could never fail the job. `defaults.run.shell:
    bash` now gives `-eo pipefail`. No verdict line was wrong; only the job status could have
    contradicted it.
- **Lands in:** §0(b), §6 (the workflow's "cannot see" line), §12.
- **Receipts (verbatim, from the job logs):**
  ```
  runner.os=Linux runner.arch=X64
  Linux runnervmtr4k5 6.17.0-1022-azure #22-Ubuntu SMP Mon Jul 27 17:24:03 UTC 2026 x86_64 x86_64 x86_64 GNU/Linux
  nproc=4
  MemTotal:       16373452 kB
  model: advertised x-repo-commit: 12145bd1d629190a4d44254073650877954d02c9
  model: advertised x-linked-size: 1266425696
  model: advertised x-linked-etag: "8e0ae26000627ed62de0e78e41860af70094558b9d2913385c842a6aa06cf3fc"
  observed-sha256=8e0ae26000627ed62de0e78e41860af70094558b9d2913385c842a6aa06cf3fc bytes=1266425696
  verdict=INCONCLUSIVE (no pinned SHA-256 — expected 'UNPINNED' is not 64 hex digits; the file was deleted unused)
  scope=oracles verdict=INCONCLUSIVE (the model was not VERIFIED — see the model verdict above; no test was run)
  scope=targeted verdict=INCONCLUSIVE (the model was not VERIFIED — see the model verdict above; no test was run)
  ```

## 2026-09-25 — lane C: the second run's GREEN is not yet banked; the evidence goes into the job log

- **Commits:** `56f1208` (previous entry); this commit
- **What changed in reality:** Run [36074016299](https://github.com/PavanManchikatla/Hydra/actions/runs/36074016299)
  (`pull_request`, head `56f1208`, merge `9f70b23`) printed a GREEN for the oracle scope. That
  result is **held, not banked**, because its `wall=1m32s` would have to cover a cold compile plus 8
  model-loading tests, including the four stage-loss windows that took `wall=7m11s` on the Mac. The
  raw stdout exists only as a run artifact, and this container cannot download it (the blob host
  gets proxy 403). Under rule 12 the classifier's GREEN gets no presumption until the evidence behind
  it can be read. The classify step now copies cargo's `Finished` line, every `test … ...` line,
  each suite's `finished in` time and the oracles' own success lines (`reached the continuation`,
  `[oracle] stream tail`) into the job log.
- **Lands in:** §0(b), §12.
- **Receipts (verbatim, run 36074016299, oracles job):**
  ```
  observed-sha256=8e0ae26000627ed62de0e78e41860af70094558b9d2913385c842a6aa06cf3fc bytes=1266425696
  verdict=INCONCLUSIVE (model SHA-256 mismatch: expected 8e0ae26000627ed62de0e78e41860af70094558b9d2913385c842a6aa06cf3f0, observed 8e0ae26000627ed62de0e78e41860af70094558b9d2913385c842a6aa06cf3fc — the file was deleted unused)
  hash-guard=OK (flipped pin refused as a mismatch on the real file; the verified file is intact)
  test-receipt: 2026-09-24T23:42:26Z · toolchain=rustc 1.98.1 (48a229cea 2026-09-01) (default) · cargo 1.98.1 (797e8a9bc 2026-08-05)
  arm=real exit=0 running=3 readable=3 mangled=0 passed=8 failed=0 ignored=0 ggml_assert=0 stub_msgs=0 wall=1m32s verdict=GREEN
  scope=oracles verdict=GREEN (receipt GREEN, skipped=0, stub_msgs=0)
  ```
  (The mismatch line above is the **live hash-guard check** on the real file, with a deliberately
  flipped pin. The real verification in that job passed.)

## 2026-09-25 — lane C: first GREEN receipts of the real arm in CI (Linux x86_64, CPU-only), with evidence

- **Commits:** `223de2c` (previous entry); this commit (the session report)
- **What changed in reality:** Run [36076105524](https://github.com/PavanManchikatla/Hydra/actions/runs/36076105524)
  (`pull_request`, head `223de2c`; the targeted job's summary line names merge commit `436e882`) produced both scopes
  GREEN **on Linux x86_64, CPU-only engine `c00bcebf`, rustc 1.98.1, model sha256 `8e0ae260…cf3fc`
  re-hashed after a cache restore**, with the evidence in the job log. **This is the first time the
  three product oracles and the engine-gated worker recovery suites have run anywhere except the
  owner's Mac.** It is **not a macOS claim** (no Metal, not the 8 GB machine), and it covers two
  scopes, not the full real arm. The generation oracle's stream on this runner is `Hello! How can I
  assist you today?` with `finish: stop`, the same text §0(b) quotes from the Mac quickstart.
  **Counts toward the plist retirement:** this is a `pull_request` run, not a nightly. The ruling asks
  for three consecutive **nightly** GREEN receipts, so the count is **0 / 3** until the workflow is on
  `main` and the schedule fires.
  **Not run:** the dispatched `corrupt_model_hash: true` oracle, because `workflow_dispatch` only
  exists for a workflow on the default branch. Its required outcome (every scope
  `verdict=INCONCLUSIVE (model SHA-256 mismatch …)` then `scope=… verdict=INCONCLUSIVE (the model was
  not VERIFIED …)`, no test run) is **NOT RUN HERE** and is owed after merge. The same guard was
  observed refusing a flipped pin on the real file in every job (`hash-guard=OK`).
- **Lands in:** §0(b) (nightly real arm: in CI, pending three nightly receipts), §0(d) (receipt
  discipline: sessions quote this workflow's receipts), §6 / §6.R (the three oracle rows gain a
  Linux/CPU CI receipt; "cannot see": macOS/Metal, GPU layers, model identity with the Mac's file,
  the untargeted real-arm suites), §7 (the SKIP-as-passed finding), §8 (test-receipt SKIP counting;
  the dispatch oracle after merge; three nightly receipts, then retire the plist), §12.
- **Receipts (verbatim, run 36076105524):**
  ```
  # oracles job 107887535516
  hash-guard=OK (flipped pin refused as a mismatch on the real file; the verified file is intact)
  scope=oracles receipt: arm=real exit=0 running=3 readable=3 mangled=0 passed=8 failed=0 ignored=0 ggml_assert=0 stub_msgs=0 wall=1m33s verdict=GREEN
  scope=oracles receipt: verdict=GREEN (every arm GREEN; counts cross-checked: running == readable on each)
  scope=oracles verdict=GREEN (receipt GREEN, skipped=0, stub_msgs=0)
      Finished `test` profile [unoptimized + debuginfo] target(s) in 28.97s
  test the_shipped_binary_generates_the_pair_drivers_tokens_byte_for_byte ... ok
  test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.65s
  test w1_killed_after_the_first_event_resumes_gapless_and_byte_identical ... ok
  test w2_killed_mid_stream_resumes_gapless_and_byte_identical ... ok
  test w3_intent_durable_commit_unsent_fences_forward_and_a_stale_epoch_is_refused ... ok
  test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 19.42s
  test w1_final_stage_killed_after_the_first_event_is_replaced_under_empty_and_the_stream_resumes_byte_identical ... ok
  test w1_first_stage_killed_after_the_first_event_is_replaced_and_the_stream_resumes_byte_identical ... ok
  test w2_final_stage_killed_mid_stream_is_replaced_under_empty_and_the_stream_resumes_byte_identical ... ok
  test w2_first_stage_killed_mid_stream_is_replaced_and_the_stream_resumes_byte_identical ... ok
  [sl-final-w1] reached the continuation after 2 reconnect(s); finish=Some("stop")
  [sl-w1] reached the continuation after 2 reconnect(s); finish=Some("stop")
  [sl-final-w2] reached the continuation after 2 reconnect(s); finish=Some("stop")
  [sl-w2] reached the continuation after 2 reconnect(s); finish=Some("stop")
  test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 38.61s

  # targeted job 107887535834
  model: models/qwen2.5-0.5b-instruct-fp16.gguf already present (cache restore) — re-hashing, not trusting
  observed-sha256=8e0ae26000627ed62de0e78e41860af70094558b9d2913385c842a6aa06cf3fc bytes=1266425696
  verdict=VERIFIED (sha256=8e0ae26000627ed62de0e78e41860af70094558b9d2913385c842a6aa06cf3fc bytes=1266425696)
  hash-guard=OK (flipped pin refused as a mismatch on the real file; the verified file is intact)
  scope=targeted receipt: arm=real exit=0 running=13 readable=13 mangled=0 passed=52 failed=0 ignored=0 ggml_assert=0 stub_msgs=0 wall=0m54s verdict=GREEN
  scope=targeted receipt: verdict=GREEN (every arm GREEN; counts cross-checked: running == readable on each)
  scope=targeted verdict=GREEN (receipt GREEN, skipped=0, stub_msgs=0)
      Finished `test` profile [unoptimized + debuginfo] target(s) in 33.23s
  test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.52s   (hydra-node binary_auth)
  test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.62s   (hydra-node binary_restart)
  test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.51s   (hydra-node token_file)
  test result: ok. 4 passed; … finished in 0.00s · 6 passed · 5 passed · 9 passed · 11 passed      (hydra-state coordinator/ledger/recovery/stage/stage_case_a_guard)
  test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s   (hydra-state supersede)
  test d1_recovery_three_kill_windows_are_byte_identical_to_an_uninterrupted_seeded_run ... ok
  test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 10.88s  (hydra-worker d1_recovery)
  test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s   (hydra-worker recovery)
  test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s   (hydra-worker survivor_reactivate)
  test three_node_kill_middle_s2_rebuilds_from_upstream_durable_boundaries_byte_identical ... ok
  test three_node_kill_middle_with_sampled_ahead_survivor_truncates_byte_identical ... ok
  test three_node_kill_s_p_rebuilds_from_durable_boundaries_and_relinks_byte_identical ... ok
  test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 7.88s   (hydra-worker three_node_recovery)
  ```
  (Per-suite labels in parentheses are the session's reading of the `Running` order; the log lines
  themselves are unlabelled. The four hydra-state lines on one row are condensed for width; each is
  `ok … 0 failed; 0 ignored` in the log.) Sums: (a) 1+3+4 = 8; (b) 2+1+2+4+6+5+9+11+3+1+3+2+3 = 52,
  equal to the receipts' `passed=`.
