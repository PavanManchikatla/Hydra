# The Metal teardown abort at the new pin — and why NO upstream report was drafted

**The question (owner, step 4):** the bump candidate carries heavy Metal churn, so does it resolve
`ggml-metal-device.m:622: GGML_ASSERT([rsets->data count] == 0)`? *"If it persists, draft the
upstream report with a duplicate search first per rule 8."*

## It persists

Re-run on the new pin `c00bcebf` (base `f280b269`), `HYDRA_TEST_NGL=99`, `local_pair`:

```
test direct_worker_to_worker_fwd_is_bit_exact ... ok
test two_worker_teacher_forced_no_sample_bit_exact ... ok
test subprocess_worker_survives_kill_9_and_restart ... ok
test result: ok. 3 passed; 0 failed; 0 ignored
  ... then:
ggml-metal-device.m:952: GGML_ASSERT([rsets->data count] == 0) failed   (SIGABRT)
```

The assert **moved from line 622 to line 952** — the file changed substantially — and still fires.
**Both bit-exact anchors pass on Metal at the new pin**, so the GPU gate row's result carries over.

## The duplicate search — and what it found

Two prior upstream issues, both **closed**:

| Issue | State | How it closed |
|---|---|---|
| [#19137](https://github.com/ggml-org/llama.cpp/issues/19137) *"Eval bug: GGML_ASSERT([rsets->data count] == 0)"* | CLOSED | **COMPLETED** 2026-03-09 — a fix landed |
| [#22593](https://github.com/ggml-org/llama.cpp/issues/22593) *"Metal: ggml_metal_rsets_free assertion fires deterministically on device free (missing rsets_remove in buffer rset free path)"* | CLOSED | **NOT_PLANNED** — *closed by the stale bot after 14 days of inactivity*, despite two independent reproductions in its comments |

**This is not §7.48's situation.** There, the abort had been reported four times and **fixed on
master**, so nothing was filed. Here a precise root-cause report was **auto-closed for inactivity**
while the symptom survived. That asymmetry is exactly the kind of thing worth reporting.

## ⛔ But no report was drafted, because its premise does not hold

**#22593's root cause is stale, and rule 20 forbids repeating a summary without checking the
source against the tree.** Its claim was that `ggml_metal_device_rsets_rm` is *"defined and never
called"*. At `f280b269` that is **false**:

```
ggml_metal_device_rsets_add  -> called at ggml-metal-device.m:2014 and :2111   (two constructors)
ggml_metal_device_rsets_rm   -> called at ggml-metal-device.m:2117             (ggml_metal_buffer_free)
```

Add and remove **are** paired, by buffer lifetime. So the assert firing does not mean "rm is never
called" — it means **a Metal buffer outlived `ggml_metal_device_free`**. That is a *leak or ordering*
question, and the leak is not necessarily upstream's.

**The control says it is probably ours.** Suites that load a model onto the GPU and **drop
`Model`/`Context` explicitly do not abort** — `bit_exact` and `shard_load` both run on Metal, both
allocate GPU buffers, both exit cleanly. The aborting suites are the ones that build workers holding
models and let the process exit with them alive. **Clean teardown demonstrably works at this pin.**

**Therefore the premise for an upstream filing is not established**, and under rule 8b — *permission
to do X is permission to do X-as-described* — an approval to report an engine defect is not approval
to report a defect that may be the reporter's own. §7.48's discipline is to **report back rather than
proceed**, and that is what this file does.

## What is owed instead

1. **Exclude Hydra-side lifetime first.** Determine which `Model`/`Context` instances are alive at
   process exit in the aborting suites, and whether dropping them removes the abort. If it does,
   this is a Hydra defect — worth fixing, since a Metal-backed worker aborting at shutdown is real
   either way — and nothing goes upstream.
2. **Only if a leak-free case still aborts** does a report become warranted. It would then be a
   comment on **#22593 asking for reopen** — with a corrected root cause, since "never called" no
   longer describes the code — rather than a new issue duplicating it.

**Nothing has been posted, and nothing has been drafted for posting.** Rule 8 requires owner approval
before posting; this records why there is not yet anything worth approving.
