# Provenance Contract

Provenance is append-only ground truth.

It records model calls, tool calls, permission decisions, extension calls, agent spawns/results, artifacts, and canvas assembly metadata.

Provenance is complete but cheap. Large payloads should be stored as blobs referenced by hash.

Derived research structures, such as causal DAGs, are projections or extension artifacts. They do not mutate primary provenance events.

Provenance uses the canonical session event envelope in `docs/contracts/events.md`. Persistence policy, durability semantics (emitted/appended/durable), and schema versioning are defined in `docs/contracts/persistence.md`.

## Writer Ownership

A live session log has one owning `ProvenanceWriter`. Creating a second writer
for the same log path while the owner is live must fail with a session-lock
error. Background or extension work that needs to append to a live session log
must use the owning writer through an explicit product-neutral host boundary;
it must not open another writer as a bypass.

A single `ProvenanceWriter` serializes concurrent append calls from the same
process. This is an append integrity guarantee, not an observer lifecycle API:
it does not provide scheduling, cancellation, durable subscriptions, checkpoint
compare-and-swap, or automatic recovery of background work.

The owning writer is also the sole owner of the durable parent tail. For every
post-D2 append, an event without an explicit semantic parent is parented to the
previously persisted event in the same session log, or to null when there is no
persisted predecessor. Batched appends are linear: the first event parents the
durable tail observed when the writer lock is acquired, and each subsequent
event parents the previous persisted event in that same batch.

The semantic-parent exception list is closed: `permission.decision` may parent
its `permission.prompt`; `tool.result` may parent its `tool.call`;
`agent.result` may parent its `agent.spawn`; extension error events may parent
the triggering extension decision/command event. Adding another exception
requires updating this contract and adding tests. A semantic-parent event still
advances the linear spine: its successor in the batch (or the next append)
parents the semantic event's id, not the event before it. This linear parent
chain is an honesty spine, not the causal DAG; richer causal structure belongs
in extension artifacts that cite event ids as evidence.

Legacy parent ids are immutable historical record. Opening an existing log seeds
the writer tail from the final accepted persisted event id only; it does not
validate, rewrite, or repair historical parent fields. Readers and lineage
consumers must tolerate pre-D2 logs whose persisted parents reference
runtime-only event ids such as `model.delta`. The strict durable-parent rule
binds new appends only.

Model reasoning is recorded as `model.reasoning` events at the maximum
fidelity the provider exposes (raw thinking, signed/encrypted items, or
summaries). Euler is a research agent: reasoning chains are part of the
reproducibility record. Large reasoning payloads are externalized as blobs
like any other large payload. Reasoning events are subject to the same
secret-taint rules as all provenance.
