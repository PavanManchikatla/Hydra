# CI restoration receipt — `container-2node` and `clean-checkout` (§7.60)

**Why this file exists.** From 2026-08-23T20:28:12Z until 2026-08-25T00:5x UTC, `container-2node`
— which PROJECT_STATE §9 names **the standing multi-node verifier** — and `fuzz` failed on **every**
run, because a clean checkout did not compile (`E0432`, `imp::gguf_probe` absent from the
engine-stub arm). PROJECT_STATE's header meanwhile read *"container-2node ✅ green weekly"*.

**Rule 16 governs the restoration exactly as it governs the break:** a green check-mark is not
evidence that the verifier is working again. These are the quoted `verdict=` lines.

---

## `container-2node` — run [32795673204](https://github.com/PavanManchikatla/Hydra/actions/runs/32795673204), sha `0d4e960`

```
verdict=GREEN (two-node docker-kill recovery held)
verdict=GREEN (two-node docker-kill recovery held under tc netem jitter)
```

Both arms of the workflow report: the plain `docker kill` recovery, and the same recovery re-run
under `tc netem` (50 ms ± 20 ms normal delay, 1 % loss, 25 % reorder on `docker0`). Each verdict is
derived from `CONTAINER_2NODE_RECOVERY_OK` in the container log, not from the runner's exit status —
the workflow's own classify step reads the log, per the rule-16 discipline it was built with.

**Also green on the intermediate commit** `a8fbb5f` (run
[32793964387](https://github.com/PavanManchikatla/Hydra/actions/runs/32793964387)), which isolates
the cause: `a8fbb5f` carried the compile fix alone, so the multi-node verifier was restored by that
fix and by nothing else.

## `clean-checkout` — run [32795673194](https://github.com/PavanManchikatla/Hydra/actions/runs/32795673194), sha `0d4e960`

The new workflow (§7.60) that owns the no-engine configuration. It prints no `verdict=` line, and
this receipt does not invent one: **its evidence is the three step outcomes**, on a checkout with
**no submodule init** — i.e. what `git clone` gives a new contributor.

| Step | Result |
|---|---|
| `cargo check --workspace --all-targets --locked` | pass |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | pass |
| `cargo test --workspace --locked -- --test-threads=1` | pass |

**What it proves:** the workspace compiles, lints clean, and passes its non-engine tests **on a
machine with no `vendor/llama.cpp` at all**. This had never been true in CI as a *named* check
before; it was only ever a side effect of two other workflows, which is why its failure was legible
as "the workflow is broken" rather than "the project does not build".

**What it does NOT prove:** anything whatsoever about the engine. It never builds it. Every
engine-gated test skips itself here (they check `ENGINE_AVAILABLE`), and the bit-exact anchors,
shard anchors, `d1_recovery` and calibration remain **evidence about one developer machine** until
audit **L1**'s fork pin lands (§8).

**Its own first run FAILED** — [32793964366](https://github.com/PavanManchikatla/Hydra/actions/runs/32793964366),
sha `a8fbb5f` — on `clippy -D warnings`, against its author, for a lint the dev box cannot produce
(local `rustc 1.93.1` vs CI `stable` `1.98.0`). That is §7.61, and it is recorded here rather than
tidied away: the workflow's first act was to falsify a standing claim of the project's.

---

## What is still red, or still unobserved

* **No scheduled `fuzz` leg has ever run.** The cron (Tue 04:15 UTC) was added 2026-08-23; the first
  firing is 2026-08-25. Legs 1–3 were dispatch/push. See `fuzz-32795673202.md`.
* **`vendored-gguf` remains UNAVAILABLE in CI** (audit L1) — and as of §7.62 the log finally says
  that word instead of `GREEN`.
* **`TLC` and `DST Marathon` were green throughout the outage** and are unaffected by any of this.
