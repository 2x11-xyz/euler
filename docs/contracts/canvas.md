# Canvas Contract

Euler separates three surfaces:

- **Provenance**: everything that happened. Append-only, cheap, complete.
- **Transcript**: what the user and assistant meaningfully said or did.
- **Canvas**: what the next model turn is allowed to reason over.

Provider retries, partial streams, raw finish metadata, failed repair
attempts, and diagnostic facts belong in provenance, not in the transcript
or canvas. The model must not eat the log.

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
size, never by fixed item counts. Auto-compaction fires late (threshold on
context usage), emits an explicit swap event to transcript and provenance,
and produces structure before prose: artifact index, action ledger, and
dead-end preservation (attempts and their failure reasons are never compacted
away). The policy ladder is `off`/`stubs` today, with `structured` and an
extension-owned `assisted` tier designed to follow.

Write-shaped facts (edits, patches, artifact creations) demote last, and
their stubs always carry the artifact path.

Extensions may contribute bounded context through named slots. Slot content is
rendered under core-generated `[slot <extension-id>:<slot>]` headers with every
content line indented, so extension text cannot spoof canvas section markers.
Raw provenance must not be dumped into the canvas.

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
