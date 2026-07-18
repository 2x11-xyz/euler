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

Writer ownership is the non-blocking exclusive OS advisory lock held on the
persistent `events.jsonl.lock` file descriptor for the writer lifetime. The
lock pathname's existence and its best-effort diagnostic metadata are never
authority. A legacy PID-only lock file, malformed metadata, or metadata left by
a crashed process is overwritten after the OS grants the lock; users must not
delete the pathname to force access while another writer may be active.
The session directory is part of the trust boundary: while a writer is live,
other actors must not unlink, rename, or replace its lock pathname. Advisory
locks attach to open files rather than names, so replacing that pathname can
create a different file with an independent lock. On network filesystems
(NFS, SMB) advisory-lock semantics vary by protocol and mount options; a
session directory on such a mount weakens the single-writer guarantee to
whatever the filesystem actually enforces.

Mixed versions: a lock file whose payload is a legacy bare PID belongs to a
pre-advisory-lock Euler, which owns sessions by pathname existence and holds
no OS lock. New writers refuse such files instead of claiming them — an old
writer may be live and unobservable — and recover only by the user deleting
the file after confirming no older Euler is running. In the other direction,
older Euler versions cannot parse the persistent lock file this version
leaves behind: rolling back across this version requires deleting
`events.jsonl.lock` files while no Euler process is running.

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
secret-taint rules as all provenance. Recording opaque artifacts does not
authorize core UI to render them; display policy is `docs/contracts/ui.md`
and ADR 0007.
