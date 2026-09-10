# Real-arm receipt — `hydra-worker`, in isolation (ruling 2026-09-09, item 4)

**Tree:** `74561f2` (the binaries compiled at 01:42Z before any edit of this session touched a source; the run's own header line is quoted below) · **alone on the box**, `nice -n 10`, nothing else scheduled · started 2026-09-10T01:42:39Z · ended 02:16:52Z.

**Why this receipt exists:** the 2026-09-03 full real-arm run was stopped after 8 h 49 min with the worker's engine-gated suites paged out (68–98 minutes each beside other work) and was quoted as INCONCLUSIVE; the one-line recovery-ack-frontier change of that session lives in `hydra-worker`, and these suites are its oracle.

**Receipt (rule 16, the summariser's own line):**

```
test-receipt: 2026-09-10T01:42:39Z · toolchain=rustc 1.93.1 (01f6ddf75 2026-02-11) (default) · cargo 1.93.1 (083ac5135 2025-12-15)
arm=real exit=0 running=36 readable=36 mangled=0 passed=102 failed=0 ignored=5 ggml_assert=0 stub_msgs=0 wall=34m13s verdict=GREEN
```

36 suites started, 36 readable; 102 passed, 0 failed, 5 ignored; 34 min 13 s — the whole crate, including every suite the stopped run never reached (`survivor_reactivate`, `three_node`, `three_node_recovery`, `wire_durability`, `wire_limits`, …).
