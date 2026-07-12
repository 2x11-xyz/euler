# Causal DAG

The causal DAG extension turns a session log into a branching attempt graph.
It is user-facing, but the schema is still moving; rely on the `schema` strings
and command output fields, not prose labels.

## Concept

Internally, graph nodes use these statuses:

- `open`
- `blocked`
- `dead_end`
- `inconclusive`
- `success`
- `verified`
- `superseded`
- `abandoned`

Visualization preserves all eight states. Status is encoded redundantly by
color and glyph; kind (`root`, `attempt`, `claim`, `checkpoint`, `synthesis`)
is encoded by outline shape, scale, and weight. The canonical tokens live in
`crates/euler-extension-causal-dag/assets/palette.json` and are injected into
every generated viewer.

## Enable it

```sh
euler extension enable causal-dag
```

All offline commands use the same shape:

```sh
euler extension run causal-dag.<command> <session.jsonl|session-id|session-name> [flags]
```

## Commands

### `export`

Export the active `euler.causal_dag.v2` artifact. If no active semantic graph
exists, the command first projects the requested bounded provenance window and
uses that immutable artifact as the source.

```sh
euler extension run causal-dag.export ./session.jsonl --format html
euler extension run causal-dag.export ./session.jsonl --format json
euler extension run causal-dag.export ./session.jsonl --format markdown --out reports/dag.md
```

Formats:

- `html`: self-contained interactive 2D/3D viewer with inline code and data;
  it performs no external requests.
- `json`: the raw `euler.causal_dag.v2` artifact for analysis and training
  pipelines.
- `svg`: deterministic static vector view for documents and slides.
- `dot`: Graphviz source for external layout tooling.
- `markdown`: backbone outline, dead ends, open frontier, and cross-arcs.
- `summary`: the compact `GRAPH:` context-slot text.

Flags: `--format`, `--out`, `--limit`, `--scan-limit`, `--after-event-id`,
repeatable `--kind`. The default format is `json`. `--out` accepts a
workspace-relative, no-clobber file path; the provenance-backed extension
artifact is still written first.

### `view`

Render the active path, open frontier, and dead ends into the transcript
without writing a derived export file:

```sh
euler extension run causal-dag.view ./session.jsonl
```

The command requires an active graph, normally created by the round observer
or `refresh`.

### TUI surface

`/causal-dag` opens an action picker with `view`, `export`, and `refresh`.
`export` drills into all six formats and Backspace returns to the action
picker. Direct flagged invocation remains available in the composer:

```text
/causal-dag export --format html --out reports/dag.html
/causal-dag refresh --operation reframe
```

### `update`

Run one checkpointed projection tick. It reads checkpoint `main`, writes a graph
artifact if new source events exist, publishes the `graph` context slot, and
stores the checkpoint at the page watermark.

```sh
euler extension run causal-dag.update ./session.jsonl --limit 128
```

Flags: `--limit`, `--scan-limit`.

### `catch-up`

Run bounded update ticks until caught up or the tick budget is exhausted.

```sh
euler extension run causal-dag.catch-up ./session.jsonl --limit 128 --max-ticks 4
```

Flags: `--limit`, `--scan-limit`, `--max-ticks`. Default max ticks is `16`; max
accepted value is `128`.

### `observer-brief`

Build a one-turn companion-agent task from the compact active graph plus the
next bounded event window. The companion returns raw
`euler.causal_dag.hints.v1` JSON.

```sh
euler extension run causal-dag.observer-brief ./session.jsonl --limit 64 --max-tokens 24576
```

Flags: `--limit`, `--scan-limit`, `--after-event-id`, `--max-tokens`.

The brief output carries an `apply` object (the observe window, active-graph
predecessor, and session assertion) that the in-session round observer echoes
untouched into `observer-apply`. The private feed cursor advances across pages
that contain only extension-owned or otherwise unobservable events, so those
pages cannot repeatedly fill the bounded window. If a bounded page ends in the
middle of a prior observer companion run, the cursor remains before that span
and the command asks for a larger limit instead of treating companion output
as driver cognition.

### `observer-apply`

Apply half of the in-session round-observer loop; not meant for direct CLI
use. Core invokes it after the observer companion turn with the envelope

```json
{ "apply": <observer-brief apply object>,
  "companion": { "ok": true, "output": "<raw hints JSON>", "...": "..." } }
```

It parses the companion output as raw `euler.causal_dag.hints.v1` JSON (a
single surrounding markdown code fence is tolerated), folds the hints over
the brief's bounded window (cut at the brief watermark), writes a complete
graph artifact, advances the active pointer, and publishes the `graph` context
slot. The first observation creates a replacement graph; subsequent rolling
observations are incremental, so omitted prior nodes and edges remain in the
new artifact. Stale predecessor or cursor assertions are rejected. A failed
companion or non-hints output is a command error; the driver turn continues
fail-open.

### `observe`

Fold an operator-provided hints JSON file over a bounded provenance page and
write a replacement graph artifact. This is an explicit manual reframe: it may
change roots and parentage, while the prior artifact remains immutable.

```sh
euler extension run causal-dag.observe ./session.jsonl \
  --hints ./observer-hints.json \
  --limit 128
```

Flags: `--hints` (required JSON object file, max 64 KiB), `--limit`,
`--scan-limit`, `--after-event-id`, `--watermark-event-id`.

The hints file is the raw `causal_dag` object, not `{ "causal_dag": ... }`.

### `refresh`

Run a one-turn observer against the active graph and unobserved provenance,
then create an incremental, reframe, or final graph revision.

```text
/causal-dag refresh --operation incremental
/causal-dag refresh {"operation":"reframe","policy":"rolling_and_final"}
/causal-dag refresh {"operation":"final","policy":"final_only"}
```

Arguments: `operation` (`incremental`, `reframe`, or `final`), `policy`
(`manual`, `rolling_only`, `rolling_and_final`, or `final_only`), `limit`,
`scan_limit`, paired `provider` and `model`, and `max_tokens`.

`incremental` upserts returned records and preserves omitted records. Every
returned incremental record must cite at least one newly observed event; prior
evidence is retained and semantically duplicate source refs are coalesced.
`reframe` and `final` replace the active interpretation, so they may introduce
new roots, change parentage, or omit superseded structure. Replacement is
rejected while the bounded feed reports an unobserved backlog; run incremental
refreshes until caught up first. Every revision writes a new immutable
artifact and links to its predecessor. The active pointer selects the latest
revision without overwriting history.

When no active graph exists, an incremental refresh may bootstrap the first
complete graph prefix even when more provenance remains. Refresh output keeps
artifact completeness and feed progress separate: `truncated` describes the
exact observed graph window, while `feed.truncated` and
`feed.next_after_event_id` report whether another incremental refresh is
needed.

`refresh` requires a live session because it uses the generic `agent-spawn`
host capability. Offline `euler extension run` hosts can execute deterministic
projection commands, but they cannot run the semantic observer. Invoke
`/causal-dag refresh` from the TUI or a resumed live session.

### `record-observation`

Record post-hoc observer audit metadata for an existing causal-DAG graph
artifact. This appends extension-owned `agent.spawn` / `agent.result` audit
events; it does not write another graph artifact.

```sh
euler extension run causal-dag.record-observation ./session.jsonl \
  --artifact-event-id <extension.artifact-event-id> \
  --observer-provider anthropic \
  --observer-model claude-sonnet-fixture \
  --limit 256
```

Flags: `--artifact-event-id` (required), `--limit`, `--scan-limit`,
`--after-event-id`, `--observer-provider`, `--observer-model`.

## Hints schema: `euler.causal_dag.hints.v1`

Top level:

```json
{"schema":"euler.causal_dag.hints.v1","nodes":[],"edges":[]}
```

Node keys are exactly:

```text
id, root_id, kind, status, title, summary, source_refs, confidence, basis, metadata
```

Allowed node kinds:

```text
root, attempt, claim, checkpoint, synthesis
```

Allowed statuses:

```text
open, blocked, dead_end, inconclusive, success, verified, superseded, abandoned
```

Edge keys are exactly:

```text
id, from, to, class, kind, canonical_backbone, source_refs, confidence, basis, metadata
```

Allowed edge classes and kinds:

- `structural`: `continuation`, `refinement`, `repair`, `fork`,
  `decomposition`, `integration`, `verification`
- `annotation`: `evidence`, `refutation`, `artifact_use`, `pivot`, `related`,
  `supersedes`

Do not emit chronology edges in semantic hints. Chronology `sequence` edges are
only used by the degraded fallback projection.

Every `source_ref` in the hints input uses exactly:

```text
id, event_id, payload_pointer
```

`payload_pointer` is either `null` or a JSON Pointer against the whole event
object, usually `/payload/content` or `/payload/output`. Artifact source refs
must use `null`.

Every `confidence` uses exactly:

```json
{"level":"high|medium|low","score":0.0}
```

with `score` in `0.0..=1.0`.

Every `basis` uses exactly:

```json
{"kind":"direct|cluster|inferred|chronology|operator","summary":"..."}
```

The projection adds `source_ref_ids` when it persists the artifact.

Backbone rule:

- Every non-root node must have exactly one incoming `canonical_backbone: true`
  edge.
- Canonical backbone edges must be `class: "structural"`.
- Root nodes must use their own `id` as `root_id` and have no backbone parent.
- Backbone edges must not cross roots or form cycles.

Use `metadata: {}` unless a bounded derived annotation is necessary.

## Workflows

### In-session automated observer

Run the round-boundary observer during the session itself:

```sh
euler exec --extensions causal-dag --observe causal-dag --observe-cadence 8 \
  "Read BRIEF.md and carry it out."
```

At every `--observe-cadence` completed driver rounds (default 8), core runs
`observer-brief`, spawns a one-turn zero-capability observer companion with
the brief's task and system prompt, and hands the companion's raw hints
output to `observer-apply`, which appends a rolling graph revision and
publishes the `graph` context slot into the driver's own context. The chain is
fail-open: any brief/companion/apply failure is recorded to diagnostics
(`round_observer_end`) and never fails the driver turn.

### Post-hoc graph from a completed run

Run with provenance:

```sh
euler exec --provenance ./session.jsonl --extensions causal-dag \
  "Read BRIEF.md and carry it out."
```

Then export or catch up deterministically:

```sh
euler extension enable causal-dag
euler extension run causal-dag.export ./session.jsonl --limit 512
euler extension run causal-dag.catch-up ./session.jsonl --limit 128 --max-ticks 16
```

Without an active graph, `export` performs a stateless structural projection.
With one, every format is derived from the selected active revision.
`catch-up` is checkpointed and suitable for repeated incremental projection.
Neither asks a model to reinterpret the completed process. For a semantic
retrospective graph, resume the session and run a `final` refresh. That final
graph is another immutable revision, not a rewrite of rolling history.

### Agent-in-the-loop hints

Keep a raw hints file as the worker's current hypothesis:

```json
{"schema":"euler.causal_dag.hints.v1","nodes":[],"edges":[]}
```

As the session grows, fold it into a graph:

```sh
euler extension run causal-dag.observe ./session.jsonl \
  --hints ./observer-hints.json \
  --limit 128
```

Before choosing the next approach, query the current graph:

```sh
euler extension run causal-dag.export ./session.jsonl --limit 512
```

Use the artifact to avoid already-dead branches and to continue from verified or
promising paths.

## Output artifact

Graph artifacts use schema `euler.causal_dag.v2` and media type
`application/vnd.euler.causal-dag.v2+json`.

Top-level artifact shape:

- `schema`
- `media_type`
- `generated_at`
- `session.id`
- `session.event_range.start/end/complete`
- `construction.operation`
- `construction.policy`
- `construction.trigger`
- `construction.predecessor_artifact_event_id`
- `construction.predecessor_watermark_event_id`
- `construction.observer_result_event_id`
- `projection.extension_id`
- `projection.watermark_event_id`
- `projection.basis`
- `projection.degraded`
- `forest.roots`
- `forest.active_root`
- `forest.nodes`
- `forest.edges`
- `diagnostics`

Artifacts are content-addressed by SHA-256 under the events-file directory. For
a home-session event log, the event payload records this relative path:

```text
sessions/<session-id>/extensions/causal-dag/artifacts/<sha256>
```

For a bare events file outside the home session store, the relative path is:

```text
extensions/causal-dag/artifacts/<sha256>
```

The CLI prints JSON with `relative_path`, `persisted_event_id`, `sha256`, and
counts. The same artifact write appends an `extension.artifact` event to the
session log.

The artifact's projection watermark is semantic: it identifies the newest
source event represented by the graph. The extension also keeps a private
observed-through cursor so extension artifacts, context-slot updates,
permission records, and other filtered events are not scanned forever. That
cursor is operational state, not graph evidence, and is intentionally absent
from the portable artifact.

The JSON artifact is the high-fidelity scientific record: complete nodes and
edges, evidence references, confidence and basis, diagnostics, construction
method, and immutable lineage. HTML, SVG, DOT, Markdown, and summary exports
are views over that artifact. They may omit or progressively reveal detail for
human legibility, but must not invent graph semantics or become the source of
truth. The interactive 2D/3D renderer belongs to the visualization/export
consumer; this extension contract supplies the versioned graph and lineage it
renders.

The HTML export contains four switchable views: 2D top-down, 2D indented
spine, 3D constellation, and 3.5D constellation with event order on the
central axis. Annotation cross-arcs rest at `#7f97a8` with `0.45` opacity and
switch to relationship-kind color on selection. Structural backbone edges
remain neutral.

The committed example is generated from the semantic fixture through the
same command path. Run against a copy because offline extension execution
appends provenance beside its input log:

```sh
mkdir -p /tmp/euler-causal-dag-example
cp crates/euler-extension-causal-dag/tests/fixtures/causal_dag/knuth_style_search/events.jsonl \
  /tmp/euler-causal-dag-example/events.jsonl
mv docs/examples/knuth-gpt55-xhigh.html /tmp/euler-causal-dag-example/previous.html
euler extension run causal-dag.export /tmp/euler-causal-dag-example/events.jsonl \
  --format html --out docs/examples/knuth-gpt55-xhigh.html
```
