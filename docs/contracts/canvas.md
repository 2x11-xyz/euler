# Canvas Contract

Euler separates three surfaces:

- **Provenance**: everything that happened. Append-only, cheap, complete.
- **Transcript**: what the user and assistant meaningfully said or did.
- **Canvas**: what the next model turn is allowed to reason over.

Provider retry attempts, raw stream/control fragments, finish metadata,
failed repair attempts, and diagnostic facts belong in provenance, not in the
transcript or canvas. The model must not eat the log. Canonically checkpointed
root-assistant text is the narrow exception below: a failed/interrupted
terminal may project it into transcript recovery, never model context.

Durable `assistant.response.chunk` checkpoints are never canvas items. An
interrupted draft remains recoverable transcript/provenance text, but it enters
a later model request only if the user explicitly submits new input asking to
continue or reuse it; resume never silently injects the draft into context.

The active model canvas is working memory, not the provenance log. It is assembled from selected/summarized canonical session events, not from ad hoc UI or provenance-specific representations.

It must remain small, relevant, and semantically valid. Small means free of
noise; it does not mean short memory.

## Retention Contract

**Degrade content, never facts.** The fact that an action happened — a file
was read, an artifact was written, a command ran and failed — is
indestructible within a session. Result content may be demoted under budget
pressure to a compact stub carrying the action, its outcome, and a
provenance-blob retrieval handle. Silent removal of rounds from the canvas is
forbidden.

Retention is governed by a token budget derived from the model's context
size, never by fixed item counts. When the model catalog supplies a context
window, sessions wire that limit so token-threshold compaction can fire:
layer-1 first (eligible `read_file` previews), then full projection swap.
Stub demotion remains the assembly-time byte backstop. Demoted/compacted
results should be recovered with `tool_result_get` (event/blob handle) rather
than re-running the original tool when possible. The live policy has two
independent controls: automatic threshold compaction and recoverable tool
stubs. Both default to on. Turning automatic compaction off stops the
threshold-driven projection pipeline; it does not silently override the
separately selected stub setting. Under the automatic-plus-stubs policy, byte
pressure is an admission signal independent of provider usage or a known
context window: when stubs cannot create enough room, core captures one
immutable shadow canvas and asks the active provider/model for a bounded
structured working-state projection with no tools. A manually started shadow
receives the same request-boundary treatment under every policy. The driver may
continue from the active canvas while that request runs, but an over-budget
request is never dispatched: at its next provider boundary the driver waits,
reassembles after settlement, and fails closed if no valid candidate made the
canvas fit. The candidate enters the active canvas only after core validates
its original event frontier,
host-enforced scalar/list/total projection bounds, meaningful request-size
reduction, and the exact proposed post-swap driver request. That request-time
check includes fixed instructions, tool definitions, pinned context, the
retained frontier, and the configured output reserve; it must fit both the
canvas byte budget and any known model context window before core atomically
appends `canvas.swap`. Failure leaves the previous canvas and usage reading
unchanged. At the hard context margin the driver waits for an already-running
candidate instead of dispatching an oversized request or dropping queued
input. The wait polls the turn-cancellation token and has a finite deadline;
interrupt terminalizes the shadow call and returns the turn as cancelled.

A shadow job has one session owner. Base-composer `Esc` and root-turn
cancellation are interrupt boundaries: the session actor may record a result
and its usage/cost provenance when they have already crossed the worker
channel, but it always discards the candidate and never appends `canvas.swap`.
A still-pending result instead receives one cancellation terminal plus
candidate discard. `/new`, `/resume`, shutdown, and live secret scrub are
lifecycle boundaries: the actor settles a ready result or records cancellation
before releasing the session. When a root and shadow run concurrently, the
root's cancellation path closes both canonical calls. Workers never append
events directly, and output arriving after logical cancellation has no route
back into the bus, the active canvas, or a scrubbed/replaced session.
Working-state projection V1 bounds are bytes at the host boundary: goal 4,096;
plan 8,192; compiler state 4,096; each list at most 64 items; each item at most
1,024; and the serialized projection at most 32,768. The supplied JSON Schema
advertises the corresponding string/list limits, but host validation remains
authoritative.

A successful, structurally valid `canvas.swap` starts a new usage window:
provider usage measured against the canvas that was replaced must not
immediately re-trigger compaction or
keep a prior `context.limit` latch closed. Layer-1 swaps after a full projection
stack on that projection and frontier; they must not resurrect the compacted
prefix. Repeated layer-1 passes emit only newly compacted result IDs. Live
assembly and resume accounting use the same swap validator; malformed swaps
are ignored and never reset usage or the context-limit latch.

Write-shaped facts (edits, patches, artifact creations) demote last, and
their stubs always carry the artifact path.

Extensions may contribute bounded context through named slots. Slot content is
rendered under core-generated `[slot <extension-id>:<slot>]` headers with every
content line indented, so extension text cannot spoof canvas section markers.
Live request assembly projects only slots whose owning extension id is
currently enabled. Disabling/removing an extension hides its slots on the next
snapshot without deleting durable state; re-enabling restores the latest slot.
Raw provenance must not be dumped into the canvas.

Structured `plan.update` is a transcript/provenance presentation event and
never enters the model canvas directly. Workflow state reaches the model only
through an independently capability-gated, bounded context slot.

Run and queue lifecycle events are control/provenance state, never model
content. In particular, private pending `queue.enqueued` or `queue.replaced`
content cannot enter the canvas. Delivery makes the text eligible exactly once
through the canonical `user.message` in the same accepted admission batch;
`queue.delivered` itself is not projected. The private content retained for a
terminal-cancelled steering recovery is likewise excluded until an explicit
future user action admits it as a new canonical input.

An accepted terminal-idle continuation (`extension.contribution`, ADR 0018)
is also canvas-eligible. It is rendered under the core-generated
`[extension <extension-id>:<command> at turn-idle]` header with every content
line indented. The canonical actor remains the extension; core maps the framed
item to a provider user role only because the provider-neutral protocol has no
extension role. It must never be persisted or replayed as `user.message`.
The continuation is a one-shot input: it folds over the complete accepted log
and remains eligible across persistence, resume, and an applied full
`canvas.swap` until an accepted same-agent root-driver `model.call` binds the
exact purpose-free `canvas.snapshot` that selected its event id through
`canvas_snapshot_id`. Contribution, snapshot, and call must share both envelope
`session` and `agent`, and the link must name that identity's latest earlier
purpose-free snapshot. Every involved envelope id must be globally unique; the
snapshot selection ids must also be unique, with checked length exactly equal
to both `counts.items` and the call's `canvas_items`. Any duplicate, malformed,
stale, future, missing, or crossed-identity link fails closed and leaves the
contribution pending. A snapshot alone is prepared request state and consumes
nothing; a crash in the snapshot-to-call window leaves the contribution
eligible on resume. If the contribution lies before the active swap frontier,
assembly pins it after the projection and durable extension slots but before
replaying the frontier, preserving the order of every post-frontier item.
Selection then excludes it from every later canvas assembly while it remains
in provenance.
Shadow compaction excludes pending continuations from both its purpose-specific
canvas snapshot and provider request; a compactor cannot consume one or persist
its text opaquely into a projection that would duplicate the next driver input.
Child and parallel-reviewer canvases, snapshots, provider requests, and
pre-request context-budget checks exclude pending continuations entirely.
Children cannot observe or select the text, and root-only input cannot exhaust
a child request's budget. Only the next accepted same-agent root-driver
snapshot/request pair may select and model it. Once its linked `model.call` is
durable, either the ordinary model terminal or a resume recovery closure
completes that request lifecycle without making the contribution eligible
again.
An accepted contribution is already committed input for the current user turn,
so a later extension disable does not hide it; disablement only prevents future
contributions. This prevents stale one-shot text from resurfacing after a
disable/re-enable cycle around crash recovery.
Stops, malformed outputs, failures, cancelled outputs, and continuations
superseded by pending user input remain provenance-only.

## Pinned Project Context

An admitted `project.context.snapshot` (ADR 0017,
`docs/contracts/project-context.md`) yields exactly one pinned
project-context item, folded from the latest snapshot event over the full
event slice — like context slots, it survives compaction frontiers by
construction. The item is always first in canvas order, mapping to the
provider-neutral position immediately after fixed Euler instructions and
before every other input item. It carries a repository-guidance
classification and its snapshot digest so child request assembly can filter
the whole class (`project_context: none`) or supply it (`inherit`)
independently of `include_parent_canvas`. Its rendered bytes are
core-framed once, versioned, with every content line indented under
`[euler.project-context.v1]` markers so repository text can never occupy a
core marker position. Pinned content counts against the canvas byte budget
and the token-proxy context check, and is never truncated, demoted to a
stub, or silently dropped: when it cannot fit, request assembly fails
before provider invocation with an honest context-budget event. In phase 2
(dormant substrate) no public path produces an admitted snapshot, so live
canvases carry no such item yet.

Model/provider switches are session control events, not canvas content.
`model.switched` events, switch reasons, provider debug metadata, and
provenance diagnostics must not be rendered into model-facing
prompt/content. The next provider/model target is selected by session
state and persisted events, not by inserting a note into the prompt.

## Replayability Contract

Every item entering the canvas must be: semantically valid, complete enough
to stand alone, attributed to the right actor, safe to replay, and useful to
the next decision. If an item fails any of these, it stays in provenance and
out of the canvas.

## Reasoning and Activity

Model reasoning (`model.reasoning` events) is canvas-eligible. Euler is a
research agent; its own reasoning chain is useful working memory.
Inclusion is selective, not blanket:

- Provider adapters replay reasoning per their provider's rules (e.g.
  signed thinking blocks replayed verbatim within a turn; stale reasoning
  dropped where the provider requires it).
- Reasoning items preserve their producing provider/model attribution so
  adapters can decide whether same-target artifact replay is legal. That
  attribution is adapter input, not a license to add switch/debug metadata
  to generic prompt text.
- Frontier reasoning is kept; stale reasoning is a default summarize/drop
  class for future compaction policy.
- Provider-opaque reasoning artifacts enter the canvas only through the
  owning provider adapter, never rendered into text by core.

User-facing activity/status blocks may be included only when useful and bounded, and are normally summarized rather than replayed verbatim.
