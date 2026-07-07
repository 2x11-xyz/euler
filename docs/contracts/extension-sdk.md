# Extension SDK Contract

Extensions register tools, commands, context slots, and workflows through a stable host API. Observer and companion agents are not a core registration category; they are extension compositions of core primitives (agent spawn/result, bounded event subscription, inter-agent message channels).

Implementation status: commands, the bounded event feed, bounded diagnostics
reads, artifact writes, agent task records, checkpoints, context slot updates,
and the local wake primitive exist today. Extension-registered tools are roadmap
(Phase 2 of the SDK consolidation); nothing in core registers them yet, and this
contract's mention of them binds their eventual shape, not their present
existence.

Core must provide enough SDK surface that extensions do not need to shadow runtime state, parse raw logs directly, or bypass permissions.
Powerful extensions should be easy because the SDK exposes the right generic
substrates. If a removable workflow such as Causal DAG projection is hard to
build without workflow-specific core APIs, improve the product-neutral SDK or
host boundary before adding DAG-specific core behavior.

## Bounded Event Feed v0

`HostApi::query_provenance` is the v0 pull-based event feed for extensions.
It reads the accepted durable prefix only. It is not a passive subscription,
push stream, background runtime, wakeup mechanism, lease, or backpressure API.

Cursor semantics:

- `after_event_id` is a stable event-id cursor in global accepted-prefix order.
- Cursors are independent of filters. A cursor means "strictly after this
  session event", not "after this matching event".
- Pages are ordered exactly as events appear in the accepted durable prefix.
- `limit` bounds returned matching events.
- `scan_limit` bounds accepted-prefix events inspected after the cursor, so
  sparse filters cannot force unbounded synchronous scans.
- `applied_limit` and `applied_scan_limit` report host clamps.
- `watermark_event_id` is the last accepted-prefix event the host scanned, or
  the input cursor when the caller is already at the durable head.
- `next_after_event_id` is present only on truncated pages and equals the
  cursor the caller should use to continue the same feed.

Malformed accepted-prefix events are deterministic storage-corruption failures.
They are not empty-feed results. Blob payloads are not expanded unless the
caller explicitly requests bounded blob expansion, and this path introduces no
new redaction or raw filesystem surface beyond the bounded provenance query.

Compatibility note: Euler's native SDK is still pre-1 and first-party. This
slice intentionally changes the `ProvenanceQuery`/`ProvenancePage` source
shape and the meaning of `next_after_event_id` for filtered pages. Consumers
must treat `next_after_event_id` as a feed continuation cursor, not as "the
last returned event id"; it may name a non-matching scanned event.

## Diagnostics Read v0

`HostApi::read_diagnostics` returns bounded raw lines from the current session's
diagnostics log. It requires `diagnostics-read`, is scoped to the session log
file chosen by the host, and is not arbitrary filesystem access. Core returns
lines only; extensions own any parsing or interpretation.

## Artifact Write v0

`HostApi::write_artifact` persists extension-owned bytes and appends the
corresponding `extension.artifact` event to the accepted durable prefix. The
returned `ArtifactRecord.persisted_event_id` is the event id of that appended
`extension.artifact` event. A later `HostApi::query_provenance` page that reads
that artifact event must expose the same id as `event.id`.

This id is a feed-position handle, not a content hash and not an artifact path.
Extensions may use it to checkpoint past their own durable side effects, but
they must not infer semantic graph/content identity from it.

For live sessions, artifact writes must go through the owning session writer.
Opening a second `ProvenanceWriter` for the same locked log is not an extension
host strategy. A future live extension-host bridge must define same-process
lifetime, concurrency, permission, shutdown, and partial-failure semantics
before extensions may use it for observer-like background work.

## Agent Task Completion Record v0

`HostApi::record_agent_task_result` appends a completed child-agent task as a
canonical `agent.spawn` event immediately followed by its terminal
`agent.result` event. This is the v0 host-mediated record path for extensions
that orchestrate observer or companion work outside core policy. It records
what happened; it does not invoke a provider, run a live child loop, schedule a
background worker, return a handle, or keep durable observer lifecycle state.

The method requires `agent-record`. The requested child capabilities are
validated with the same flat exact subset rule as `Session::spawn_agent`:
empty child capabilities are valid, equality with the command grant is valid,
duplicates are normalized, and any capability outside the command grant fails
before either agent event is appended.

Before writing, the host validates the task fields, budget, optional result
schema, and terminal result through the shared `euler-agents` DTO rules. A
successful result must not include `error`; a failed result must include one.
After validation, the host appends the spawn/result pair through the owning
`ProvenanceWriter` and returns the child agent id plus both event ids. The
spawn event is parented to the current accepted durable session head; v0 does
not create a separate extension-command invocation event for this API. Live
sessions queue the same appended events for publication into the session bus.

The host builds and validates both events before calling the writer, and it
queues live-session events only after `ProvenanceWriter::append` returns
success. This prevents ordinary host validation from orphaning a spawn without
its result. It is not a filesystem transaction: crash or low-level I/O failure
during the underlying append is governed by the provenance writer durability
contract and accepted-prefix recovery.

Both events include extension attribution fields:

- `source: "extension"`
- `extension_id`
- `command`

The host does not automatically redact arbitrary extension-supplied task,
summary, output, error, or schema strings. Extensions must not pass secrets to
this API. Core still keeps these provenance/control events out of model canvas
assembly unless a future canvas contract explicitly admits them.

## Context Slot Update v0

`HostApi::update_context_slot(slot, content)` appends a canonical
`context.slot.updated` event through the owning session writer. It requires the
`context-slot` capability. The host derives `extension_id` from the calling
extension; extensions cannot write another extension's slots.

Slot names reuse the event-feed checkpoint grammar below. Content is UTF-8 text
capped at 4096 bytes; control characters other than newline are rejected. Empty
content deletes the slot. At most eight active `(extension_id, slot)` pairs are
allowed per session; a ninth active slot fails without eviction. An identical
update to the current active content is a no-op and appends no event.

Canvas assembly folds the last update per namespaced slot before compaction
frontier filtering, renders active slots with core-generated framing, and
includes the selected slot event ids in `canvas.snapshot`.

## Event Feed Checkpoint v0

`HostApi::load_event_feed_checkpoint` and
`HostApi::store_event_feed_checkpoint` provide a durable, product-neutral
cursor store for long-running extension projections. A checkpoint stores only a
schema version and an `after_event_id` cursor. It must not contain event
payloads, canvas content, secrets, or extension artifacts.

Checkpoint names are session-local extension identifiers, not paths. The v0
grammar is frozen independently of command IDs: ASCII lowercase letters,
digits, and `-`; length 1..=64 bytes; first and last byte must be lowercase
alphanumeric.

Checkpoint files live under the session-scoped extension private state
directory:

`<session-dir>/extensions/<extension-id>/checkpoints/<name>.json`

Cursor semantics:

- `after_event_id` means extension-owned effects through that event are already
  durable.
- Extensions must store the checkpoint only after their derived state/artifacts
  are durable.
- Missing checkpoint returns `Ok(None)`.
- Corrupt or unsupported checkpoint files fail clearly and never silently reset
  to `None`.
- Valid but stale/missing cursors are not checkpoint corruption; the next
  provenance query returns `CursorNotFound`.
- Processing is at-least-once unless extension effects are idempotent or
  jointly committed with the checkpoint.
- Recovery correctness requires a single logical writer per checkpoint name.
  The host serializes file replacement and quota checks, but it does not provide
  compare-and-swap, monotonicity, or stale-writer protection.

V0 shape and bounds:

- `schema_version` is exactly `1`; unknown fields and future versions fail.
- `after_event_id` is 1..=128 visible ASCII bytes.
- load reads at most 4096 bytes before JSON decoding.
- at most 64 logical checkpoint names are allowed per extension.
- no host list/delete/cleanup API exists in v0; dynamic checkpoint names can
  exhaust the quota until manual cleanup.

Capability rules:

- load requires `fs-read`;
- store requires `fs-write`;
- store may perform internal directory reads needed for safe overwrite, quota,
  and file-type validation, but it does not return prior checkpoint contents.

Native command capability rules:

- `ExtensionManifest.capabilities` is the extension's maximum capability
  envelope.
- `CommandDescriptor.required_capabilities` is the sole source of a command's
  capability set. There is no trait-level declaration and no inheritance from
  the manifest: an empty descriptor set means the command holds no
  capabilities (least privilege), even if the manifest envelope is broad.
- Every command's declared set must be a subset of the manifest envelope;
  violations fail at registration, before any command executes.
- Full extension enablement requires the full manifest envelope. One-shot
  command execution may register only the selected command and grant only that
  command's declared set.
- V0 command-scoped registration still calls the extension's normal
  `register()` method to discover commands, and validates the command names it
  reports. Extension registration must remain side-effect-free.

## Local Event Wake v0

Core provides a process-local wake primitive on `ProvenanceWriter` /
`Session` for current-process background workers. It is a payload-free signal
that the accepted durable provenance prefix may have advanced. The wake
contains no event data, no watermark, and no canvas content; consumers must
retrieve payloads through `HostApi::query_provenance` / `query_provenance`.
The shared state-machine types live in `euler-sdk` so host crates can use the
same primitive, but no `HostApi` method currently returns a wake handle to
extension or child-agent code.

Consumer algorithm:

1. Open the wake receiver and record `baseline_event_id`.
2. Query provenance from the consumer's durable cursor until caught up.
3. Block on `recv()` from a background OS thread, or poll `try_recv()`.
4. After `Advanced`, query provenance again until caught up.

Non-guarantees:

- no per-event delivery;
- no durable notification;
- no replay of historical wakes;
- no wake after crash-recovered ambiguous append failures;
- no fairness or timeout guarantee for slow consumers;
- no background scheduler, lease, or observer lifecycle.

`recv()` is a synchronous blocking API. It must not run on a thread that must
keep driving the parent session loop or an async executor. In v0, no host API
exposes wake receivers directly to untrusted extension code, so this adds no
new capability. If a later slice exposes wake handles through a host API, that
surface must require `provenance-read` or a separately justified
product-neutral wake capability.

Primary extension paths:

1. Native Rust crates implementing `euler-sdk` traits (implemented today).
2. Out-of-process extensions over a generic stdio subprocess protocol
   (product-neutral; core names no protocol). Roadmap: no stdio transport
   exists yet. Protocol-specific adapters such as MCP are first-party
   extensions built on this transport, not core. See
   the extension-composition principle above.

Rhai is not the primary extension mechanism.
