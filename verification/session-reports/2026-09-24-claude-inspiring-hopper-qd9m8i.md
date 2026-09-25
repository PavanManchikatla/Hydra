# Session report — 2026-09-24/25 — branch `claude/inspiring-hopper-qd9m8i`

Cloud session. Task: probe f, then **lane C: the real arm in CI**. It ran under rule 3 **as amended
for parallel branches**: every reality-changing commit went through `scripts/state-commit.sh` and
recorded its change in `verification/branch-deltas/claude-inspiring-hopper-qd9m8i.md`, and
PROJECT_STATE.md was never edited. No Rust source changed, no v2 work, no history rewrite, and no
existing workflow, `scripts/real-arm-nightly.sh` or the launchd plist was touched. PR:
[PavanManchikatla/Hydra#2](https://github.com/PavanManchikatla/Hydra/pull/2). It is **not merged**;
the integrator merges it locally through the guard.

## Setup (verbatim)

**a. Git identity.** `git log -5 --format='%an <%ae> | %s' origin/main`: all five commits by
`Pavan Manchikatla <91258136+PavanManchikatla@users.noreply.github.com>`, the owner named in rule 1,
not a bot. Set repo-local (no `--global`, no `-c`):
```
$ git config --local --get user.name
Pavan Manchikatla
$ git config --local --get user.email
91258136+PavanManchikatla@users.noreply.github.com
```
The branch started at `origin/main` = `2f52bd8` (merge of PR #1).

**b. Submodule.**
```
Submodule 'vendor/llama.cpp' (https://github.com/PavanManchikatla/llama.cpp.git) registered for path 'vendor/llama.cpp'
Cloning into '/home/user/Hydra/vendor/llama.cpp'...
Submodule path 'vendor/llama.cpp': checked out 'c00bcebf79cbf95be22aa711d485a287914672c5'
exit=0
 c00bcebf79cbf95be22aa711d485a287914672c5 vendor/llama.cpp (remotes/origin/hydra-layer-window-f280b26)
```
PROJECT_STATE §0(d)/§5 pin: `c00bcebf` on `PavanManchikatla/llama.cpp`, branch `hydra-layer-window-f280b26`. **Match.**

**c. Toolchain.** There is no `rust-toolchain` file (`ls: cannot access 'rust-toolchain*': No such file or directory`);
`Cargo.toml:25: rust-version = "1.80"`; CI uses `stable`.
```
rustc 1.94.1 (e408947bf 2026-03-25)
cargo 1.94.1 (29ea6fb6a 2026-03-24)
```
(CI's `stable` on 2026-09-24 printed `stable-x86_64-unknown-linux-gnu unchanged - rustc 1.98.1 (48a229cea 2026-09-01)`.)

## PROBE f (report only)

`rustup toolchain install 1.98.1 --profile minimal -c clippy`:
```
info: syncing channel updates for 1.98.1-x86_64-unknown-linux-gnu
info: latest update on 2026-09-03 for version 1.98.1 (48a229cea 2026-09-01)
info: downloading 4 components

  1.98.1-x86_64-unknown-linux-gnu installed - rustc 1.98.1 (48a229cea 2026-09-01)

info: checking for self-update (current version: 1.29.0)
info: downloading self-update (new version: 1.29.1)
exit=0
```
It succeeded. Note the side effect: the command also downloaded a rustup self-update (1.29.0 → 1.29.1),
because it has no `--no-self-update`. The 1.98.1 toolchain was not used for anything else.

## Lane C, step 1 — what was read, reported before writing

- **fuzz.yml** already builds the engine: `actions/checkout@v4` with `submodules: true`, then
  `rustup toolchain install stable --profile minimal --no-self-update`, then `actions/cache@v4` on
  `vendor/llama.cpp/build` keyed `engine-build-v1-${{ runner.os }}-<pin>-cpu-shared-nonative`, then
  `cmake … -DBUILD_SHARED_LIBS=ON -DGGML_NATIVE=OFF …`, then `--target llama`. It checks the build
  log for a stub. Convention: actions pinned by **major tag** (`@v4`), `permissions: contents: read`,
  no secrets. The new workflow reuses these steps and **the same cache key**, and CI restored fuzz's
  cache (`Cache hit for: engine-build-v1-Linux-c00bcebf…-cpu-shared-nonative`).
- **scripts/real-arm-nightly.sh** + **packaging/launchd/com.hydra.real-arm-nightly.plist**: at 03:00
  local on the Mac, under `nice -n 15`, the script refuses to overlap cargo/TLC. It runs
  `scripts/test-receipt.sh --arms real` (the **full** workspace real arm) and writes
  `verification/ci-results/real-arm-<date>.md`. Per §0(b) it was never installed.
- **scripts/test-receipt.sh**: one `cargo test … -- --test-threads=1` per arm, stdout and stderr
  split, and a checked `arm=` line. `HYDRA_TEST_ENV` supplies env and `HYDRA_TEST_FILTER` is appended
  after libtest's `--`. It rejects `running=0`, `readable=0`, `mangled>0` and zero executed tests.
  **It cannot see a skipped test** (the finding below).
- **The model:** every engine-gated test defaults to `models/qwen2.5-0.5b-instruct-fp16.gguf`
  (`hydra_worker::pair::dev_model_path`, which honours `HYDRA_TEST_MODEL` and returns `None` without
  the engine). The tree records **no URL and no hash** for it. §0(d) says only "re-download
  Qwen2.5-0.5B fp16". The workflow fetches it from
  `Qwen/Qwen2.5-0.5B-Instruct-GGUF` at revision `12145bd1d629190a4d44254073650877954d02c9`. That
  revision came from a search-result title, and HF's own `x-repo-commit` header confirmed it in CI.
- **GitHub's published limits:** `docs.github.com` and `huggingface.co` are **unreachable from this
  container** (`CONNECT tunnel failed, response 403`; WebFetch `EGRESS_BLOCKED`). What could be
  quoted, from a web-search snippet of <https://docs.github.com/en/actions/concepts/security/github_token>
  (listed alongside <https://docs.github.com/en/actions/reference/limits>): *"The maximum job execution
  time for GitHub-hosted runners is 6 hours"*. The **RAM table**
  (<https://docs.github.com/en/actions/reference/runners/github-hosted-runners>) could **not** be
  quoted from here. Per rule 24 the workflow measures the runner instead:
  `nproc=4`, `MemTotal: 16373452 kB`, `SwapTotal: 3145724 kB`, 145 G disk with 86 G free, image `ubuntu-24.04`
  `20260920.314.1`, `Linux … 6.17.0-1022-azure … x86_64`. The repository is **public**.
  **Fit:** each scope is its own matrix job with `timeout-minutes: 330`, under the 6 h limit. The
  observed walls were 1m33s and 0m54s. Four model-loading processes fit in 16 GB with no paging.
  **Left out:** the full workspace real arm, the rest of hydra-worker's engine suites, the modelsvc
  real-split and shard-anchor suites (they need `models/shards2`), and all GPU/Metal paths. The
  reason is the lane's scope (the named oracles plus the targeted arm), not the limits: the observed
  times leave room to widen it later.

## What landed (commits)

| Commit | What |
|---|---|
| `378ddcc` | `.github/workflows/real-arm-nightly.yml`, `scripts/real-arm-model.sh`, `scripts/real-arm-classify.sh`; the pin deliberately `UNPINNED` |
| `56f1208` | model SHA-256 pinned from the first run (measured = HF-advertised); `defaults.run.shell: bash` (pipefail) |
| `223de2c` | the evidence behind each verdict is copied into the job log |
| (this commit) | final delta entry + this report |

## The workflow's oracle (rule 19)

- **Dispatchable:** `corrupt_model_hash: true` flips the last hex digit of the pin. Required output:
  `verdict=INCONCLUSIVE (model SHA-256 mismatch: expected …, observed … — the file was deleted unused)`
  then `scope=<scope> verdict=INCONCLUSIVE (the model was not VERIFIED …; no test was run)`, in
  **every** scope. **NOT RUN HERE:** `workflow_dispatch` exists only once the file is on `main`.
- **Observed anyway, on every run:** the helper self-tests (`selftest verdict=GREEN` on the runner),
  and a live check against the real 1.27 GB file with a flipped pin:
  `verdict=INCONCLUSIVE (model SHA-256 mismatch: expected 8e0ae260…3f0, observed 8e0ae260…3fc — the file was deleted unused)`
  followed by `hash-guard=OK (flipped pin refused as a mismatch on the real file; the verified file is intact)`.
- **Mutation checks (local):** removing the hash comparison sends `real-arm-model.sh --selftest` to
  `selftest verdict=RED (3 case(s) …)`; removing the SKIP guard sends `real-arm-classify.sh --selftest`
  to `selftest verdict=RED (1 case(s) …)`.
- **Also observed:** run 36073107311 (pin `UNPINNED`) ended both scopes with
  `scope=… verdict=INCONCLUSIVE (the model was not VERIFIED …; no test was run)`. The unpinned-fetch
  path failed closed, and no test ran.

## Finding (rule 25): a skipped engine-gated test is counted as PASSED

The engine-gated tests `eprintln!("SKIP: …"); return;` when `dev_model_path()` is `None`, and libtest
reports them `ok`. In this container (engine built, no model), `test-receipt.sh` printed
`arm=real exit=0 running=3 readable=3 mangled=0 passed=8 failed=0 ignored=0 ggml_assert=0 stub_msgs=0 wall=0m25s verdict=GREEN`
for the three oracles, and all 8 of their tests had skipped. The targeted scope printed `passed=52 … verdict=GREEN`
with 4 skips. The workflow guards it: `--show-output` via `HYDRA_TEST_FILTER`, and the classify script
refuses any `SKIP:` line or a missing `successes:` section. On these logs it prints
`scope=oracles verdict=INCONCLUSIVE (skipped=8 …)` and `scope=targeted verdict=INCONCLUSIVE (skipped=4 …)`.
**`test-receipt.sh` itself is unchanged**, which is out of this lane's scope. A §8 row is proposed in the
delta. Whether any earlier receipt was taken with the model absent is **not assessed here**; a receipt
that has no `--show-output` log cannot tell, which is the point of the finding.

## A second defect, found in the CI log (rule 12)

`run:` steps executed as `bash -e {0}`, without pipefail, so `classify | tee … || rc=1` could never
fail the job. It was fixed in `56f1208`. No verdict line was wrong; only the job status could have
contradicted one.

## The GREEN that was held and then banked

Run 36074016299 printed `scope=oracles verdict=GREEN` with `wall=1m32s` for a cold compile plus 8
model-loading tests, a figure that looked implausible against the Mac's 7m11s for the stage-loss
windows alone. The artifact was unreadable from here (its blob host gets proxy 403), so the result
was **held**. `223de2c` put the evidence into the job log. Run 36076105524 then showed a cold compile
of `28.97s`, suites of `4.65s` / `19.42s` / `38.61s`, the product's stream `Hello! How can I assist
you today?` ending `finish: stop`, and four `reached the continuation … finish=Some("stop")` lines.
The time is real: 4 Linux cores with 16 GB do not page the way the 8 GB Mac did.

## Lane C, step 4 — the run's verdict lines (run 36076105524, `pull_request`, head `223de2c`)

**Linux x86_64 (ubuntu-24.04, 4 vCPU, 16 GB), engine `c00bcebf` CPU-only, rustc 1.98.1, model
sha256 `8e0ae26000627ed62de0e78e41860af70094558b9d2913385c842a6aa06cf3fc`. Not a macOS claim.**
```
scope=oracles receipt: arm=real exit=0 running=3 readable=3 mangled=0 passed=8 failed=0 ignored=0 ggml_assert=0 stub_msgs=0 wall=1m33s verdict=GREEN
scope=oracles receipt: verdict=GREEN (every arm GREEN; counts cross-checked: running == readable on each)
scope=oracles verdict=GREEN (receipt GREEN, skipped=0, stub_msgs=0)
scope=targeted receipt: arm=real exit=0 running=13 readable=13 mangled=0 passed=52 failed=0 ignored=0 ggml_assert=0 stub_msgs=0 wall=0m54s verdict=GREEN
scope=targeted receipt: verdict=GREEN (every arm GREEN; counts cross-checked: running == readable on each)
scope=targeted verdict=GREEN (receipt GREEN, skipped=0, stub_msgs=0)
```
Test-result sums: (a) `1 passed` + `3 passed` + `4 passed` = 8; (b) 2+1+2+4+6+5+9+11+3+1+3+2+3 = 52.
Every per-suite `test result:` line is quoted in the delta file.

## What this does NOT establish (NOT RUN HERE / not claimed)

- Anything on **macOS** or **Metal**. Anything with GPU layers.
- That the pinned file is **byte-identical to the owner's local model**. One `shasum -a 256
  models/qwen2.5-0.5b-instruct-fp16.gguf` on the Mac settles it.
- The **full** real arm. This is two scopes, and scope (b) is a named reconstruction, not the Mac's
  unrecorded 10-suite selection.
- The **dispatched corruption oracle** (owed after merge).
- **The plist's retirement:** 0 of the 3 consecutive **nightly** GREEN receipts exist. The schedule
  starts firing only once the workflow is on `main`.

## Owed, for the integrator / next session

1. Merge PR #2 locally through the guard. Fold the delta (§0(b), §0(d), §6/§6.R, §7, §8, §12) and delete it in the merge commit.
2. After merge: dispatch `real-arm-nightly` with `corrupt_model_hash: true` and quote both scopes' `verdict=INCONCLUSIVE (model SHA-256 mismatch …)` lines.
3. Quote three consecutive nightly GREEN receipts. Only then may the ruling retire the launchd plist.
4. §8 proposal: `test-receipt.sh` counts `SKIP:` (under `--show-output`) and refuses GREEN on a real arm with any skip.
5. Owner: confirm the model hash against the Mac's copy.
