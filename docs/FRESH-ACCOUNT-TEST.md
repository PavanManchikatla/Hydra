# The fresh-account test

M4's definition of done contains one claim the people who built Hydra **cannot** verify:

> a non-author can set up a 3-machine cluster from the README in under 30 minutes

Everyone who has worked on this project knows where the model file goes, which flag the coordinator
wants, and what the error messages mean. That knowledge is exactly what the test is trying to
measure the absence of. So this document is a **protocol**, not a checklist: follow it literally,
and the result is a measurement rather than an impression.

## Why it is written this way

The M4·3 quickstart was carefully written, reviewed, and wrong in three places — none of which a
reading found and all of which one execution did. This test is the same instrument pointed at a
person instead of a shell.

**Nothing here is a test of the tester.** Every point of confusion is a defect in the
documentation. If you find yourself thinking "I should have known that", write it down: that
thought is the finding.

## Before you start

- [ ] A **fresh user account** on the machine, or a fresh VM. Not your normal account: a stray
      `~/.cargo`, an already-built `vendor/llama.cpp`, or a `HYDRA_API_TOKEN` still exported from
      last week all invalidate the result.
- [ ] A **stopwatch**, started at the moment you open the README and not paused.
- [ ] A **text file open**, for the running log below.
- [ ] No access to this repository's maintainers during the run. If you get stuck, record it and
      keep going or stop — do not ask.

## The protocol

1. Start the stopwatch.
2. Open `README.md` at the top. Read it in order — do not skip to Quickstart. If the reading itself
   is the confusing part, that is a finding.
3. Follow the Quickstart exactly as written. **Type the commands as printed.** If a command needs
   changing to work, that is a defect: record the original, the change, and why.
4. Stop the stopwatch at the moment a `curl` to the API returns a `200`.
5. Fill in the record below **before** discussing it with anyone.

## The running log (fill in as you go, not afterwards)

For every moment of hesitation, however small:

| Elapsed | What I was doing | What confused me, **verbatim** | What I did about it |
|---|---|---|---|
| | | | |

Record it **verbatim**. "It wasn't clear whether the token goes in the file or the environment" is
a usable finding; "minor confusion about config" is not. The wording of your confusion is the data.

## The record

- **Total elapsed to a `200`:** ______ (or: **did not complete**, and where you stopped)
- **Points of confusion:** ______ (count the rows above)
- **Commands that did not work as printed:** ______
- **Anything you had to look up outside the README:** ______
- **The first moment you thought "this is going wrong":** ______

## What counts as a pass

**Under 30 minutes, from opening the README to a `200`, without asking anyone anything.**

A run that takes longer is not a failure of the tester — it is the measurement, and the number is
the finding. A run that completes in 25 minutes **with nine points of confusion** is worth more
than one that completes in 12 with none, because the nine are what get fixed.

## Afterwards

The result goes into `PROJECT_STATE.md` §6 as a dated line with the elapsed time and the confusion
count, and the confusions become owed items in §8. **Until that line exists, the README does not
claim the 30-minute figure** — and it currently does not.

If the run fails, that is the honest state of the product, and it is recorded as such rather than
retried until it passes. Repeating the test with the same person is not a second measurement: they
know the answers now.
