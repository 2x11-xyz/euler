# ADR 0012: Parallel CodeSwarm Fan-Out and Deterministic Event Ordering

## Status

Accepted for implementation on branch `feat/swarm-v2` (2026-07-11).

## Context

CodeSwarm review (issues #33, #32) spawned reviewer companions sequentially:
each `HostApi::spawn_agent` blocked the session thread through a full
provider round trip. Five reviewers meant five serial model calls. The
product requirement is concurrent reviewer execution, without giving up two
invariants:

1. **Durable parent chain**: every provenance append flows through the
   owning `ProvenanceWriter`, and queued extension events publish into the
   bus before spawn-path events (extension-bridge contract).
2. **Replayable, deterministic event logs**: fixture-driven sessions must
   produce byte-comparable logs across runs; event order must never depend
   on provider completion timing.

A companion loop owns `&mut` session state (bus, writer counters, permission
gate with the interactive decider). None of that is shareable across
threads, and multiplexing permission prompts from concurrent children has no
honest UX today.

## Decision

**Phase-split fan-out: concurrent provider I/O, single-threaded provenance.**

`Session::spawn_reviewers_parallel` (surfaced to extensions as
`HostApi::spawn_agents`) restricts batch children to single-round, tool-free,
empty-capability briefs, then runs three phases:

- **Phase 1 (session thread, batch order)**: assemble the parent canvas once
  only when a task explicitly requests it; for each task record
  `agent.spawn`, `canvas.snapshot`, `model.call`, and build the
  `ModelRequest`. Canvas-enabled children share that snapshot and never see
  earlier reviewers' events. Self-contained CodeSwarm briefs disable parent
  canvas inheritance and receive only their explicit task context.
- **Phase 2 (worker threads, concurrent)**: one scoped thread per task
  invokes the provider and drains its stream through the shared
  `RoundLoop` (keeping transport-retry semantics). Workers append **no**
  events; they buffer `ModelRoundData` or the terminal provider error.
  This requires `ModelProvider: Send + Sync` (providers are stateless HTTP
  adapters; the scripted test provider moved from `RefCell` to `Mutex`).
- **Phase 3 (session thread, batch order)**: join workers in batch order
  regardless of completion order; per task, append the round events
  (`model.reasoning*`, `model.result`, `assistant.message` or `error`) and
  the terminal `agent.result` with the same honesty checks as the
  sequential companion path (stop-reason, token budget, output bound).

## Why this keeps the log replayable

Every event append happens on the session thread, in an order that is a
pure function of the batch order. Worker completion timing influences only
wall-clock latency, never event order or content. With deterministic
(fixture/scripted) providers, repeated runs yield identical event sequences,
so fixtures and resume-equivalence tooling keep working unchanged.

## Consequences and limitations

- Reviewer events land in the log **after** the batch's phase 1 — reviewer
  round events are not live-streamed while the provider call is in flight.
  Phase 3 flushes incrementally (reviewer 1's events append as soon as
  reviewer 1 joins), but a slow reviewer 1 delays the recording (not the
  execution) of reviewers 2..N. This is the price of determinism and is
  acceptable for review briefs.
- Parallel **tool-running** children remain future work: they would need
  event appends and permission decisions mid-flight, which this design
  deliberately keeps off worker threads.
- Provider `invoke` and stream draining both run on worker threads, so
  model generation genuinely overlaps (true concurrency of provider calls),
  proven by a barrier-synchronized provider test that would deadlock under
  sequential execution.
