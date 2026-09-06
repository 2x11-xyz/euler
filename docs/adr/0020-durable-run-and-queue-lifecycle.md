# ADR 0020: Durable Run and Queue Lifecycle

## Status

Accepted (2026-09-05).

Numbered 0020 rather than 0019 because ADR 0019 is being introduced on a
sibling branch at the same time; the gap is deliberate and 0019 is not
retired.

## Context

Run boundaries and queued user input (mid-turn steering and follow-ups) lived
only in Session memory. A crash between admitting a queued message and closing
its run could lose the guidance or replay it twice, `SessionStore` listings
could not tell whether a run had ended, and resume reconstructed run state
from heuristics over `model.result` and `error` events. The provenance writer
also conflated the last physically appended event with the logical parent a
new event should link to, so audit leaves such as `session.resumed` skewed
parent chains.

## Decision

- Seven durable event kinds record the lifecycle: `run.started`,
  `run.terminal`, `queue.enqueued`, `queue.replaced`, `queue.cancelled`,
  `queue.delivered`, and `queue.recovered`. Payloads and batch shapes are
  normative in `docs/contracts/events.md`.
- `RunLifecycleProjection` (`crates/euler-core/src/session/run_lifecycle.rs`)
  is a deterministic fold over the event log and the sole owner of run and
  queue state. The live Session, resume, `SessionStore` status, and the TUI
  all read run openness, terminal status, and pending queue entries from this
  fold; no component keeps a parallel run or queue model.
- `SteeringQueue` becomes durable: every enqueue, replace, cancel, and
  delivery is a writer transaction, so a queued entry survives a crash and is
  either recovered (`queue.recovered`) or delivered exactly once.
- Admission and terminal batches are atomic writer transactions
  (`run.started + user.message`, `run.started + queue.delivered +
  user.message`, `queue.cancelled* + run.terminal`). An ambiguous append
  retains the exact envelope batch and fences every non-identical write until
  the identical retry settles: the retained exact result, admission, or
  terminal is the sole repair owner. A deferred run terminal fences new user
  admissions, but not the orphaned `agent.result` retry that the terminal
  itself waits on; otherwise the two would fence each other permanently.
- Resume treats a crash-partial batch (`run.started` alone, `run.started +
  queue.delivered`, or a lone steering `queue.delivered`) as inert and closes
  it with an exact-parent recovery row rather than replaying the input.
- The provenance writer splits its physical durable tail from the logical
  parent frontier. Marker leaves such as `session.resumed` advance the tail and
  byte length only; ordinary events parent the frontier, and batched events
  parent their predecessor in the same batch.

## Consequences

- Session listing validates the lifecycle before selecting status. An open
  run is active; otherwise the latest `run.terminal` is authoritative: failed
  runs report failed, while completed, cancelled, and interrupted runs leave
  the session active. Ordinary run-less root errors after lifecycle activity
  are invalid streams, not late overrides of a completed terminal. Legacy
  streams without run terminals retain the model/error status fallback.
- The fold scans O(events x runs) per batch and clones the projection.
  Acceptable for current histories; an open-run index is the expected follow-up
  for long logs.
- Every new admission path must emit lifecycle events through the fold or
  resume will reject the log as invalid; there is no in-memory escape hatch.
- Separable concerns landed together and should be read as distinct
  decisions even though they share this record: the event kinds and fold, the
  durable queue and exact-retry fencing, the TUI queue mutations moving to a
  background boundary, and the writer tail/frontier split.
- Superseding any part of this record requires updating
  `docs/contracts/events.md` and `docs/contracts/provenance.md` first; the
  contracts remain normative over this ADR.
