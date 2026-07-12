# verification/ci-results/

Completed TLC runs from `.github/workflows/tlc.yml` land here (logs + `.status` files, and
downloaded checkpoint artifacts for long runs). When a long run reaches a conclusive result
(fixpoint / designed violation), fold it into `../VERIFICATION-README.md` §results **and**
`../../PROJECT_STATE.md` §6 in the **same commit** (PROJECT_STATE §11).

Local machine runs **smoke only** (parse, Mut2, Mut4) per the thermal policy (PROJECT_STATE §9);
baseline-safety→fixpoint, baseline-liveness, Mut1, Mut3 are **CI-owned** (the `long` job).
The Mut3 drain-clean contingency (|msgs|≤30 → MaxAttempt=3 → escalate) triggers only on a run
that *completes* its bounded space without firing — never on a timed-out or cancelled run.
