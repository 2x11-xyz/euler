# Extension SDK Contract

Extensions register tools, commands, context slots, and workflows through a stable host API. Observer and companion agents are not a core registration category; they are extension compositions of core primitives (agent spawn/result, bounded event subscription, inter-agent message channels).

Implementation status: commands, explicitly declared model tools, one
terminal-idle contribution per root session, deterministic root request ticks,
the bounded event feed, bounded
diagnostics reads, artifact writes, agent task records, checkpoints, context
slot updates, typed plan presentation, the local wake primitive, and a generic
managed-process adapter exist today.
Explicitly enabled local packages run through offline and live command surfaces
and may participate in the generic round-observer composition by declaring an
 observer command pair in their manifest. The observer chain runs only at a
 cadence boundary that can be followed by another permitted driver request; an
 explicit round cap falls through to its limit outcome without running brief,
 companion, or apply work that no subsequent request could consume.

Core must provide enough SDK surface that extensions do not need to shadow runtime state, parse raw logs directly, or bypass permissions.
Powerful extensions should be easy because the SDK exposes the right generic
substrates. If a removable workflow such as Causal DAG projection is hard to
build without workflow-specific core APIs, improve the product-neutral SDK or
host boundary before adding DAG-specific core behavior.

Host-authored extension control provenance is part of command truth, not
best-effort telemetry. Registration cannot succeed until its static capability
decisions are durable. A returned command error or panic cannot be reported as
normally recorded until its fixed sanitized `error` event is durable. If
either append fails, the host returns an infrastructure failure, retains the
writer's exact-append fence, and never substitutes the extension's ordinary
result for the missing provenance.

## Bounded Event Feed v0

`HostApi::query_provenance` is the v0 pull-based event feed for extensions.
It reads the accepted durable prefix only. It is not a passive subscription,
push stream, background runtime, wakeup mechanism, lease, or backpressure API.

Cursor semantics:

- `after_event_id` is a stable event-id cursor in global accepted-prefix order.
- Optional `through_event_id` is an inclusive accepted-prefix upper bound. It
  is independent of filters and must remain unchanged across every page of one
  stable historical view. No event after it may be scanned, returned, or named
  as a watermark.
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
- A missing `after_event_id` or `through_event_id` is a typed failure. A bound
  that occurs before the cursor is a typed invalid range. Equal cursor and
  bound returns an empty, untruncated page whose watermark is that id.

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

## Live Agent Spawn (v0.1) and Parallel Batch Spawn (v0.2)

`HostApi::spawn_agent(task) -> AgentOutcome` runs one child agent to
completion within the command execution (capability `agent-spawn`; depth
one; per-command quota). `HostApi::spawn_agents(tasks) -> Vec<AgentOutcome>`
runs a batch of single-round, tool-free, empty-capability child briefs
**concurrently** and returns outcomes in task order under the same gate and
quota. Determinism, event ordering, and failure honesty for both live in the
multi-agent contract; hosts without live spawn support reject both calls.

`SpawnAgentTask::include_parent_canvas` is an explicit context boundary,
honoured on **both** spawn paths: `spawn_agent` and `spawn_agents`. A task
that sets it to `false` receives no parent canvas and its `canvas.snapshot`
records zero retained items, so provenance shows what the child actually got.
Native extensions set it to `true` only when their child workflow requires
the active parent canvas; self-contained workflows such as CodeSwarm set it
to `false` and carry all bounded context in `task`/`explicit_context`. The
default is `true`, so existing companion workflows keep the canvas they have
always had. This field was added to the pre-1.0 SDK as a source-breaking
struct-field change rather than hiding a privacy-sensitive default in the
host bridge.

`SpawnAgentTask::explicit_context` carries up to 256 KiB of caller-assembled
context as a separate child input item for both single and parallel spawn.
Spawn provenance records its byte count, not its contents, so a multi-reviewer
batch does not duplicate the review subject in every `agent.spawn` event.

## Context Slot Update v0

`HostApi::update_context_slot(slot, content)` appends a canonical
`context.slot.updated` event through the owning session writer. It requires the
`context-slot` capability. The host derives `extension_id` from the calling
extension; extensions cannot write another extension's slots.

Slot names reuse the event-feed checkpoint grammar below. Content is UTF-8 text
capped at 4096 bytes; control characters other than newline, Unicode format
characters (`Cf`), and Unicode line/paragraph separators (`Zl`/`Zp`) are
rejected. Empty content deletes the slot. At most eight active
`(extension_id, slot)` pairs are allowed per session; a ninth active slot fails
without eviction. An identical update to the current active content is a no-op
and appends no event.

Canvas assembly folds the last update per namespaced slot before compaction
frontier filtering, renders active slots with core-generated framing, and
includes the selected slot event ids in `canvas.snapshot`. Live request
assembly projects a slot only while its owning extension id is currently
enabled. Disable/removal suppresses it from the next snapshot without erasing
durable state; re-enabling the same id restores the latest value.

## Plan Presentation v0

`HostApi::update_plan_presentation(presentation)` publishes bounded typed plan
state without granting arbitrary event-emission or canvas authority. It
requires `plan-presentation`; the host derives `extension_id` and `command`
from the active invocation and appends one canonical `plan.update`.

```json
{
  "revision": 1,
  "status": "active",
  "explanation": "Optional single-line explanation",
  "items": [
    {"step": "Inspect the boundary", "status": "completed"},
    {"step": "Implement the workflow", "status": "in_progress"}
  ]
}
```

`revision` is `1..=i64::MAX`; status is `active`, `blocked`, `waiting`, or
`completed`; and `items` contains 1..=16 entries whose status is `pending`,
`in_progress`, or `completed`. Each nonblank step is at most 1024 UTF-8 bytes;
a non-null explanation is nonblank and at most 4096 bytes. Both are
single-line and reject ordinary controls, Unicode `Cf`, and `Zl`/`Zp`.
The host redacts text before validating/persisting it, rejects unknown fields,
and derives compatibility summary
`r<revision> · <status> · <completed>/<total> completed`.

Core validates only presentation shape. Revision transitions, in-progress
cardinality, completion rules, persistence, and whether a plan exists remain
extension policy. `plan.update` is transcript/provenance presentation, not
model-canvas state; model-facing workflow state uses an independently gated
context slot. The TUI renders one `Updated Plan` checklist and, when it is the
causally attributed side effect of an extension model tool, suppresses only
that tool's successful generic JSON result row. Provenance retains every event.

An exact retry is idempotent after redaction and validation. Before appending,
the host folds durable canonical extension `plan.update` events and compares
the latest update for the same `extension_id` by revision, status,
explanation, items, and derived summary. An exact match succeeds without
appending or adding to the live queue, even when a different command retries
it, provided the writer has no unresolved append. A same-writer ambiguous
durability failure stays honestly fenced because only the exact failed event
batch can reconcile it; after lifecycle reopen establishes a settled durable
tail, the canonical physical event may satisfy deduplication. Command
attribution is provenance for a real transition, not presentation identity.
The same revision with changed content appends, and the same payload from
another extension appends. This fold-then-append rule assumes the host's
single-threaded command execution, as context-slot deduplication does.

## Private Extension State v0

`HostApi::state_dir()` returns
`<session-dir>/extensions/<extension-id>` and requires `extension-state`.
The host creates the directory with private permissions where supported.
Returning a raw directory is one honest read/write scope: extension runtimes
are trusted code, not OS sandboxes, so the host cannot enforce separate reads
and writes after returning the path. This capability does not authorize
workspace reads or writes.

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

Command capability rules:

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

## Model Tool Registration v0

`CommandDescriptor.model_tool` explicitly exposes an existing command to the
root session's model. It does not register another executable or bypass command
capabilities.

- The command must be `agent-only`.
- The model-visible name is 1..=64 lowercase ASCII bytes using letters,
  digits, `_`, or `-`, beginning with a letter or `_`.
- Description, schema descriptions, property names, and string enum values
  reject controls, Unicode 17 `Cf`, and `Zl`/`Zp`; their respective host
  bounds apply before advertisement.
- The input schema uses the host-supported JSON Schema subset. Every object
  schema is closed with `additionalProperties: false`; unsupported keywords
  fail registration.
- Schema bytes, nesting, property counts, and model-supplied input bytes are
  host-bounded. Input is validated before capability approval or extension
  execution.
- Numeric `minimum`/`maximum` ordering and input checks compare the exact
  canonical JSON decimals represented by `serde_json::Number`, without
  converting integers through `f64`; adjacent integers above `2^53` therefore
  remain distinct.
- Core rejects, rather than rewrites, a descriptor when its model-visible name,
  description, or schema contains a registered secret value or recognized
  credential shape. The check runs both at wiring and immediately before
  advertisement, keeping the advertised schema identical to the schema used
  for input validation.
- A successful model-tool result is a bounded JSON object. Core first redacts
  every string value and object key, retains post-redaction key collisions
  deterministically with `#N` suffixes, then format-validates, serializes, and
  bounds that exact model-facing value. The canonical `tool.result` therefore
  remains valid JSON and active-canvas preview limits retain a
  `tool_result_get` recovery handle.
- Failed model-tool results expose only host-generated error text through one
  bounded projection. Controls and Unicode 17 `Cf`/`Zl`/`Zp` are rendered as
  visible escapes before the error reaches provenance or model context; raw
  extension error bodies remain unavailable.
- Model-tool names must not collide with core tools or another enabled
  extension tool. Advertisement and execution both require the extension to be
  wired and enabled.
- Root sessions alone receive extension model tools. Companions and spawned
  agents retain their bounded core tool palettes.
- Root-session contribution wiring is fixed at launch/resume in v0. Disabling
  a wired extension hides its model tool, idle hook, and request tick
  immediately, and
  re-enabling that same wired extension restores them. Enabling or installing
  a package that was not wired when the session started updates live
  enablement, but its model tool, idle hook, and request tick appear only after
  restart or resume; the TUI names that limitation instead of implying a hot
  load.

Native and managed-process extensions use the same descriptor and execution
path. Managed manifests place `model_tool` on the declaring command.

## Terminal Idle Contribution v0

`Extension::idle_contribution()` may nominate one registered `agent-only`
command. Managed manifests declare the same shape at top level:

```json
{"idle_contribution":{"command":"command-id"}}
```

At most one enabled extension owns this point in a root session. After a normal
terminal model response, core may invoke the nominated command with `{}`.
Implicit idle work never prompts: every required capability must be
`session-allow`, or be `ask`/unconfigured and covered by an existing grant;
`always-deny` is final. Otherwise core records a rejected stop contribution
with reason `authority-unavailable`, starts no command or child agent, and
emits no error. Explicit model-tool calls keep the ordinary permission braid.
The result of an executed idle command must be exactly one of these closed
envelopes:

```json
{"action":"stop"}
{"action":"continue","input":"bounded UTF-8 text"}
```

The extension owns why work is complete, what private state it reads, and what
the continuation text means. Core never infers a goal or workflow from ordinary
conversation. Core redacts continuation text first, then validates the exact
accepted content: nonblank UTF-8 capped at 8192 bytes, allowing newline/tab but
rejecting other ordinary controls, Unicode 17 `Cf`, and `Zl`/`Zp`. That
once-redacted value is persisted and modeled verbatim; emission does not run a
second, potentially non-idempotent redaction pass.

Root sessions default `extension-state` and the bounded, extension-namespaced
`context-slot` and `plan-presentation` capabilities to `session-allow`. This
permits absent-state/resume probing and bounded presentation without a prompt;
it does not create a plan, goal, or workflow in core.

Pending user input wins both before and after command execution. Cancellation
is checked at both boundaries. An accepted continuation is recorded as
`extension.contribution` and projected with core-generated extension
attribution into a fresh root `RoundLoop`; it is not a `user.message`. The
continuation is one-shot: it remains eligible across persistence and resume
until an accepted same-agent root-driver `model.call` binds the exact
purpose-free `canvas.snapshot` that selected it, then leaves all later
canvases. A snapshot-only crash consumes nothing; the next admitted request may
select the contribution again. Child and parallel-reviewer canvases, snapshots,
provider requests, and pre-request context-budget checks exclude it entirely:
child models cannot observe or select the text, and root-only input cannot
exhaust a child request's budget. A full `canvas.swap` cannot hide it either:
core folds
pending contributions over the full accepted log and pins any pre-frontier
contribution ahead of ordered frontier replay. Shadow compaction likewise omits
pending contributions from its captured canvas and provider request, preventing
opaque projection text from persisting or duplicating the one-shot driver input.
Stop and unaccepted outputs remain provenance-only. The hook is
skipped after errors, context-limit stops, guardian interruption, explicit
round ceilings, and cancellation. Core adds no second, hidden continuation
ceiling: the configured `RoundLoop` round limit and cancellation are the
generic owners of resource termination. Acceptance commits the continuation
to the current user turn. A later registry disable cannot retroactively hide
it; disablement prevents only new contributions.

Immediate cancellation of an already-running command is governed by the
generic extension-command cancellation seam; the idle API does not define a
second mechanism.

## Root Request Tick v0

`Extension::request_tick()` may nominate one registered `agent-only` command.
Managed manifests declare the same shape at top level:

```json
{"request_tick":{"command":"command-id"}}
```

After all compaction decisions and immediately before each logical root-driver
model request, core runs every enabled tick in stable extension-id order. It
persists the current accepted prefix once and supplies every contributor the
same exact closed input:

```json
{"through_event_id":"<accepted durable tail event id>"}
```

Core discovers one immutable snapshot of tick entries and owner ids for the
request, then reuses that exact snapshot for both admission and execution.
Dynamic or failed registration cannot nominate an owner to gain admission and
then avoid its execution/failure outcome. The ordinary full canvas first gets
all configured compaction handling. If it still exceeds the byte budget, a
provisional pre-tick view may omit only the snapshotted owners' durable slots
so they can refresh, clear, or latch. The provisional view is never a
`canvas.snapshot` and never reaches a provider. The authoritative post-tick
assembly restores successful/no-op owners, suppresses failed owners, and must
pass every ordinary final budget check. Its request-growth comparison retains
the settled full pre-tick request as the compatibility baseline.

During that invocation every `HostApi::query_provenance` call is pinned to the
shared inclusive cutoff. Omitting `through_event_id` injects it; supplying the
same id is accepted; supplying any other id fails the query. A contributor
therefore cannot observe permission decisions or other durable side effects
emitted by an earlier tick in the same boundary. It must keep the same bound
while paging.

The returned value must be a JSON object, but core otherwise ignores it. Tick
results, raw provenance, and extension reasoning never enter transcript or
model canvas. Extensions use existing capability-gated context slots for
bounded model-facing state and typed plan presentation for UI state. After the
tick sequence, core assembles the final canvas and emits its ordinary
purpose-free `canvas.snapshot`; no high-volume tick/heartbeat event is added.

Ticks are root-only. Shadow compaction, companions, reviewers, and background
work do not run them. They are implicit lifecycle work and never prompt:
required capabilities need standing authority under the same rule as terminal
idle. Registration, missing authority, ordinary command, or result-shape
failure records exactly one canonical `error` with fixed host text and
`failure: "command_error"` or `failure: "panic"`, disables that contributor
for the remainder of the live Session, and does not suppress later
contributors or the root request. The latch applies only to the request tick;
otherwise valid model tools and terminal-idle work from that extension remain
available. Live root request assembly does withhold that contributor's durable
context slots after the latch: a contributor that cannot refresh or clear its
state cannot leave stale state model-facing. This suppression does not delete
slot events. Resume retries contributors with a fresh process-local failure
latch and restores normal latest-slot projection so a successful resumed tick
can refresh it before provider invocation. A command error or panic already
recorded by the ordinary command host is not duplicated by the tick latch.
Cancellation stops the boundary.
Authoritative provenance failure retains the ordinary fatal writer fence. If
no live writer or durable tail is available, the optional tick point is
skipped before its contributor discovery rather than making the root request
depend on observer read I/O.

## Managed Process Runtime v0

`runtime_kind: "managed-process"` is a language-neutral package runtime. Its
manifest includes `entrypoint.command`: a nonempty, bounded argv array. Euler
starts that argv directly, with the package directory as the working directory;
it does not invoke a shell or interpolate environment values. The package's
static id, version, capability envelope, command names, and per-command
capabilities remain canonical: a child process cannot redefine them at runtime.

The transport is newline-delimited JSON-RPC 2.0 messages on stdin/stdout. A
peer can be implemented by any language; Python is an SDK client, not a wire
variant. Version `euler-managed-process/1` has this lifecycle:

1. Euler sends an `initialize` request with the offered protocol versions,
   static extension identity, and host output limit. The peer responds with the
   selected `protocol_version`.
2. Euler sends `initialized`, then one `euler/command` request containing the
   declared command name and its JSON input value.
3. While that command is active, the peer may send bounded
   `euler/progress` notifications and JSON-RPC requests for the host methods
   below. The command's terminal response must be a JSON object.
4. On either a successful terminal result or a terminal JSON-RPC command error,
   Euler sends `shutdown`, requires an object result, then sends `exit` and
   reaps a zero-exit-status child. A non-object shutdown result or non-zero
   final process status is a generic extension failure. On timeout or protocol
   failure Euler sends `$/cancelRequest` for the command, allows a short grace
   period, then terminates and reaps the child and its normal descendants.

The current host request methods map one-for-one to `HostApi`:

| Method | Params | Result |
| --- | --- | --- |
| `euler/host/query-provenance` | `ProvenanceQuery` JSON | `ProvenancePage` JSON |
| `euler/host/read-diagnostics` | `DiagnosticsQuery` JSON | `DiagnosticsPage` JSON |
| `euler/host/state-dir` | `{}` | `{ "path": string }` |
| `euler/host/write-artifact` | `{ display_name, media_type, bytes_base64, source_event_ids?, metadata? }` | `ArtifactRecord` JSON |
| `euler/host/load-checkpoint` | `{ name }` | `EventFeedCheckpoint` JSON or `null` |
| `euler/host/store-checkpoint` | `{ name, checkpoint }` | `{}` |
| `euler/host/record-agent-task-result` | `{ task: HostAgentTask, result: HostAgentResult }` | `HostAgentRecord` JSON |
| `euler/host/update-context-slot` | `{ slot, content }` | `{}` |
| `euler/host/update-plan-presentation` | `PlanPresentation` JSON | `{}` |
| `euler/host/spawn-agent` | `SpawnAgentTask` JSON | `AgentOutcome` JSON |
| `euler/host/spawn-agents` | `{ tasks: SpawnAgentTask[] }` | `AgentOutcome[]` JSON |

The DTO field names are the `serde` names in `euler-sdk`; the maintained
[Python SDK](https://github.com/2x11-xyz/euler-extensions/tree/main/sdks/python/euler-managed-process-sdk)
in `euler-extensions` is the canonical client implementation, while Euler's
raw JSON-RPC tests are the protocol conformance fixture. Progress uses the
notification `euler/progress` with
`{ message: string, fraction?: number }`; the message is 1–4096 UTF-8 bytes
and the optional fraction is finite and in `[0, 1]`. All host requests and
progress notifications are valid only after `euler/command` and before its
terminal response.

Every one of these calls reaches the existing host implementation, so bounds,
capability checks and prompts, quotas, redaction, provenance attribution, and
live-agent policy remain host-owned. A host rejection is returned as a safe
JSON-RPC error; raw host failure details are not serialized to the process.

The runtime bounds every protocol message, aggregate protocol messages and
bytes, pending inbound/outbound queues, host request count, progress budget,
invocation, shutdown, and stderr byte budget. Stderr is discarded without
entering provenance or canvas; crossing its host byte limit aborts the
invocation. Non-protocol stdout, malformed protocol messages, and process error
bodies similarly become safe generic extension failures. Structured progress is
validated and bounded, but is not implicitly admitted to the model canvas or
transcript. Default ceilings are 1 MiB per JSON payload, 4 MiB/512 messages of
total peer output, 64 host requests, 128 progress messages, and 64 KiB stderr.

Process transport writes are isolated from the session loop, so a peer that
stops reading stdin cannot block the command deadline. Every admitted command
receives a host-owned `CancellationToken`. The default native-command method
checks it before calling the legacy synchronous `execute`; native extensions
with long work override `execute_cancellable` and cooperate at their own safe
boundaries. The managed-process adapter observes the token while waiting on
handshake, invocation, and shutdown: it makes a best-effort
`$/cancelRequest` notification, then kills and reaps the process group after
the bounded grace period. Notification delivery is not a cancellation
precondition. Host calls remain synchronous and cannot be forcibly preempted;
the host stops admitting their late output after cancellation. Child-agent
calls launched through the live session inherit the same token.

The child environment is deliberately minimal: package directory as current
directory, inherited `PATH`, and `EULER_MANAGED_PROCESS_PROTOCOL`. No ambient
home, locale, certificate, Python-path, or secret environment is inherited.
Packages that need dependencies must invoke a package-local interpreter or
otherwise carry their own explicit runtime configuration.

`link` alone never launches a package. A linked managed-process package begins
in `needs-review`; `validate`, `link`, and `info` expose its exact argv, and
`enable` echoes that argv while recording explicit local launch consent.
`disable` and `reload` return it to `needs-review`. Installed packages remain
inert in this slice (Extension Distribution v1 below binds the flow that will
make them enableable). This consent is separate from capabilities: each
invocation is still checked against its command's declared capability subset.

This runtime is a process-management boundary, not OS-level containment. It is
for trusted local packages and currently runs only on Unix hosts (including
macOS); non-Unix hosts reject launch before starting a child. On Unix, Euler
launches the peer in its own process group and, on cancellation or failure,
terminates that group before reaping its leader so ordinary descendants cannot
retain protocol pipes. After a clean peer exit Euler reaps only the direct
child—it never signals a process group after reaping its leader, because that
numeric id could be reused. A successful package must therefore clean up its
own children. Sandboxing untrusted third-party code, including filesystem or
network isolation, remains a separate security milestone.

## Extension Distribution v1 (binding shape)

ADR 0015 governs distribution; this section binds its concrete shape.
Implementation status: the link lane is complete (link, review, enable with
launch consent, run). `extension install <path>` records an `installed-inert`
entry that `enable` refuses because the install consent flow below does not
exist yet. Git sources, the store reconciler, materialization, update, and the
project tier are unimplemented; this section binds their eventual shape, not
their present existence (issue #159).

### Sources and pins

A source names where extension content comes from:

- `git:<host>/<path>@<ref>` — cloned into the store. `<ref>` may be a tag,
  a commit, or a branch name.
- `path:<dir>` — referenced in place, for local development. `extension link`
  remains sugar for a single-extension `path:` source.

Every install resolves the ref to an exact commit — the **pin** — and records
it. A tag or commit ref is a *pinned spec*: `update` skips it entirely. A
branch ref is a *tracking spec*: it is an update candidate, but only under the
explicit `update` verb. Nothing in Euler ever moves a pin without an explicit
user command; reproducibility of installed extensions is a contract property,
not a configuration option.

One source may provide several extensions. A source-level manifest
(`Euler.source.json`) lists provided extension directories, themes, and
templates; absent that manifest, discovery is conventional:
`extensions/*/Euler.extension.json`. Extension identity remains the extension
manifest id. The same id offered by two sources is a surfaced conflict, never
a silent override. (Euler ships no bundled extensions; the former bundled-id
reservations ended when the last bundled crate was removed per the ADR 0015
amendment.)

### Store

`~/.euler/extensions/` holds installed content, source-addressed: one
directory per (source, pin), immutable after materialization. Reinstalling
the same (source, pin) reuses the directory; `remove` deletes it and its
registry entry. The registry (enablement log, fingerprints, consent records)
remains the single authority over what is enabled; store presence alone
grants nothing.

### Materialization

A source declares how its entrypoints are built (bounded argv steps, e.g.
`cargo build --release`) and which toolchains those steps require. Install
verifies the toolchains first and fails naming the missing tool. Builds run
at install time only — never at load, enable, or session start — and only
after install consent (capability contract). A failed build fails the
install; nothing is registered or enabled. Entrypoints remain argv; the
managed-process protocol stays the only load boundary.

### Update semantics

`extension update` is always explicit and interactive. It skips pinned specs,
and for tracking specs it presents what would move — old pin, new pin, and a
manifest diff when the manifest changed — before touching anything. Consent is
keyed to the pinned content fingerprint (ADR 0015 decision 4), so any content
movement requires re-consent: an unchanged manifest does not make new code
safe, because declared builds execute source-controlled build logic. A moved
pin whose manifest also changed must render the manifest diff prominently in
the consent card.

### Project tier

`.euler/` in a workspace may declare sources and activation deltas
(`.euler/extensions.json`). The file is repo-controlled content and is never
authority on its own — the same two-party rule as project grants: entering a
directory never fetches, builds, or runs anything. In a trusted project,
startup reconciles declarations against the store and *offers* missing
installs through the ordinary install consent flow; there is no silent
auto-install. Project deltas apply over the user's global set; the project
entry wins on conflict, and conflicts are surfaced.

### Dev lane and provenance honesty

Linked (`path:`) extensions run unpinned working-tree code. Their lightweight
grant is the existing launch consent: `enable` echoes the exact argv and
records consent for that path once; rebuilds and re-runs never re-prompt.
Extension-attributed events and artifacts must record distribution identity:
source and pin for installed extensions, and an explicit linked-path marker
(never a fabricated pin) for linked ones, so a reader can always distinguish
a result produced by a released extension from one produced by a dev tree.
Exact event field names bind with the implementing slice.

## Command Invocation v0

`CommandDescriptor.invocation` declares who may drive a command:

- `Invocation::User` (the default): the command earns a slash token, a
  headless `extension_run` control line, and `euler extension run`.
- `Invocation::AgentOnly`: the command is a step an agent takes on the user's
  behalf, reachable only through a session tool. `build_extension_slash_commands`
  mints no token for it; the headless control line and the CLI refuse it by
  name. The `code-swarm` extension's `review` command is the first of these.

Rules:

- **It is a product boundary, not a security one.** `AgentOnly` says a command
  is not a verb the user drives; it grants and withholds nothing. Authority is
  `required_capabilities` and only that, whoever reaches the command. Do not
  use `invocation` to contain a dangerous command — declare fewer capabilities.
- **Refusals name the way in.** A surface that refuses an agent-only command
  must say how to reach it (ask the agent), not merely that it cannot run.
  "Unknown command" is a lie: the command exists.
- **Enforced at the chokepoint, not only at the surfaces.**
  `execute_extension_command_gated` — the path every user-driven run takes —
  refuses agent-only commands itself, before any approval is spent. Surfaces
  still refuse in their own words (they can name a better next step), but the
  boundary does not depend on the set of surfaces that happens to exist.
  `execute_extension_command` is the agent's ungated path and is deliberately
  exempt: guarding it too would make agent-only mean unreachable.
- **Listed, not hidden.** `/extension` still shows agent-only commands, marked
  `(agent-only)`. Hiding them would trade one wrong answer for another.
- **Absent means `user`.** The manifest field and the persisted link inventory
  both default to `user`, which is what every manifest written before this
  field existed meant. A missing `invocation` must decode, never fail the
  extension.
- Remediation text anywhere in the system must name only invocations that
  work; an agent-only command must not be advertised as a user command.

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
2. Out-of-process extensions over the generic managed-process JSON-RPC stdio
   protocol (implemented for explicit linked-package runs). Protocol-specific
   adapters such as MCP are first-party extensions built on this transport, not
   core. See the extension-composition principle above.

Rhai is not the primary extension mechanism.
