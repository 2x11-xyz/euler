# PR stack review evidence

This package supports [PR_STACK_LOG.md](../../PR_STACK_LOG.md). It is separate
from the original repository audit at `9dfb881`; each probe names its own PR
baseline. It contains synthetic-provider observations and validation records,
not real session data or credentials.

## Confirmed fixes

- **#213**, baseline `aa7c478`, fixed at `ff70bb6`: [before](pr213-before.txt)
  and [after](pr213-after.log) show shadow compaction semantic-idle retries
  falling from three dispatches to one while FirstByte keeps its retry budget.
  [Worker tests](pr213-compaction-tests.log), [round-loop tests](pr213-round-loop-tests.log),
  and [Clippy](pr213-clippy.log) passed. Desired-behavior regressions are in the
  PR commit; the public Session probe source remains in
  `/private/tmp/euler-shadow-retry-probe`.
- **#216**, baseline `8d56dae`, fixed at `67abc78` and refined at `ff37987`: [failing observation](pr216-before.txt)
  demonstrates an unrelated second dispatch after checkpoint sync failure.
  The [baseline test patch](pr216-baseline-test.patch) applies to a disposable
  archive of that baseline and intentionally fails. The committed regression
  instead requires a fence, then verifies lifecycle recovery and positive
  ordinary retry paths. [Validation record](pr216-validation.md) and
  [Clippy output](pr216-clippy.log) cover the final implementation.
- **#216 PTY synchronization**, fixed at `8d56dae`: [targeted result](pr216-fold-test.log).
  The original test passed three macOS baseline reruns; those passes did not
  reproduce or disprove the original Linux timing failure.

The validation record describes the agent's handoff before commit/push; the
root PR log records the subsequent commit and GitHub CI outcome. CI is tied to
the named head only; rebases and later stack integration require new checks.
The [CI snapshot](ci-results.json) records eight successful runs, including
#216's final `ff37987` run `34011528944`. The [PR snapshot](pr-state.json)
records remote heads, bases, and merged/open states at handoff.

## Independent Claude review

The initial [read-only review prompt](claude-review-prompt.md) targeted #213's
correction and an immutable #216 baseline; that slow attempt was stopped.
A bounded, tool-free review of the [final source and patches](claude-final-patch-prompt.md)
completed successfully. Read the [raw review](claude-review.md) alongside its
[adjudication](claude-adjudication.md): two proposed triggering scenarios are
unreachable, and the ownership condition was simplified. No new filesystem
failure was reproduced by the reviewer.
