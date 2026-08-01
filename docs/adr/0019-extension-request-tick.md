# ADR 0019: Extension Request Tick

## Status

Accepted (2026-08-01).

## Context

Some removable workflows need to refresh bounded extension-owned projections
immediately before a root model request. A daemon, workflow-specific health
state in core, or direct provenance-to-canvas path would put interpretation in
the runtime and create a second session authority. Running ordinary extension
commands at an explicit safe point provides the missing substrate without
doing so.

The safe point must also have a deterministic historical view. Sequential
contributors can append permission, context-slot, plan, artifact, or error
events; a later contributor must not accidentally see a different input
frontier merely because it runs later.

## Decision

An extension may nominate one existing registered `agent-only` command as its
request tick:

```json
{"request_tick":{"command":"command-id"}}
```

Immediately before each logical root-driver model request, core completes all
compaction decisions, persists the accepted session prefix, samples one
durable tail event id, and invokes enabled contributors in stable extension-id
order. The exact closed input is:

```json
{"through_event_id":"<accepted durable tail event id>"}
```

Contributor discovery is one immutable per-request snapshot of entries and
their owning extension ids. Pre-tick admission and execution reuse it; dynamic
or failed registration cannot nominate an owner for admission and then evade
the corresponding execution or failure latch. The ordinary full canvas gets
the first opportunity to fit or settle eligible shadow compaction. Only when
that settled canvas remains over its byte budget may admission temporarily
omit durable slots owned by the snapshotted contributors so the boundary that
must refresh, clear, or suppress them can run. This provisional canvas is
never snapshotted or sent to a provider. Final full assembly and all byte/token
checks remain authoritative. A successful or no-op owner is included normally;
a failed owner is omitted by its latch. The settled full pre-tick request stays
the growth-comparison baseline, so an already-present slot is not relabeled as
tick-created growth.

The command's result must be a JSON object but is ignored and not persisted.
Durable effects use existing capability-gated host APIs. Model-facing effects
use bounded context slots; typed UI effects use existing plan presentation;
all other side effects retain their existing event and capability contracts.
Because active slots are folded independently across compaction frontiers,
shadow-compaction snapshots and their provider requests omit them rather than
persisting a potentially superseded slot value inside a projection.
After all contributors, core assembles the final canvas and emits the ordinary
purpose-free `canvas.snapshot`. A tick never runs for shadow compaction,
companions, parallel reviewers, or background work.

`ProvenanceQuery` gains optional inclusive `through_event_id`. A query with a
bound cannot scan or return beyond it, keeps the same bound across pages, and
reports typed missing-bound and reversed-range failures. During a request
tick, the host injects the shared bound when omitted and rejects a different
explicit bound. Thus every contributor sees the same accepted history even
when earlier contributors append events.

Request ticks are implicit lifecycle work. They use only standing authority
and never prompt. Registration, authority, ordinary command, or result-shape
failure produces exactly one canonical extension `error` with fixed host text
and `failure` equal to `command_error` or `panic`, latches that contributor for
the rest of the live Session, and does not suppress later contributors or the
root request. The latch affects only the request tick; otherwise valid model
tools and terminal-idle work from that extension remain available. Because a
latched contributor can no longer refresh or clear its model-facing state,
live root request assembly withholds its durable context slots for the rest of
the Session rather than presenting stale state. The events are not deleted.
Resume starts with a fresh process-local latch, restores normal latest-slot
projection, and lets the resumed tick refresh it before the provider request.
An ordinary command-host error is not duplicated by the latch. Cancellation
stops the boundary. An unresolved
authoritative provenance append retains the existing fatal writer fence; the
tick adds no alternate recovery policy. If no live provenance writer or
durable tail is available, this optional observer point is skipped before tick
contributor discovery rather than becoming a new root-request availability
dependency.

## Consequences

- Native and managed-process extensions share one deterministic request-time
  command surface.
- Multiple extensions compose without ordering races or moving provenance
  cutoffs.
- Core learns no workflow health schema, thresholds, strategy policy, or model
  reasoning interpretation.
- There is no timer, heartbeat, daemon, new durable tick event, or direct raw
  provenance projection into the canvas.
- Tick side effects may make final request assembly exceed its normal context
  budget; growth from a fitting pre-tick request then fails honestly without
  starting another post-tick compaction cycle. Provisional admission cannot
  bypass that final check. A no-op or failed tick does not replace the legacy
  admission decision for an unchanged settled full baseline request.
