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

For UI and planning, read them as five visible states:

- **open**: `open`
- **promising**: `blocked`, `inconclusive`, `success`
- **verified**: `verified`
- **dead end**: `dead_end`, `abandoned`
- **superseded**: `superseded`

Euler's slot summary currently highlights open nodes and dead-end-class nodes;
the full artifact keeps the exact internal status.

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

Project a bounded provenance window to a `euler.causal_dag.v1` artifact.

```sh
euler extension run causal-dag.export ./session.jsonl --limit 128
euler extension run causal-dag.export ./session.jsonl --after-event-id <event-id>
euler extension run causal-dag.export ./session.jsonl --kind user.message --kind tool.result
```

Flags: `--limit`, `--scan-limit`, `--after-event-id`, repeatable `--kind`.

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

Build a one-turn companion-agent task for observing a complete event window and
returning raw `euler.causal_dag.hints.v1` JSON.

```sh
euler extension run causal-dag.observer-brief ./session.jsonl --limit 64 --max-tokens 24576
```

Flags: `--limit`, `--scan-limit`, `--after-event-id`, `--max-tokens`.

The brief output carries an `apply` object (the observe window plus the
session assertion) that the in-session round observer echoes untouched into
`observer-apply`.

### `observer-apply`

Apply half of the in-session round-observer loop; not meant for direct CLI
use. Core invokes it after the observer companion turn with the envelope

```json
{ "apply": <observer-brief apply object>,
  "companion": { "ok": true, "output": "<raw hints JSON>", "...": "..." } }
```

It parses the companion output as raw `euler.causal_dag.hints.v1` JSON (a
single surrounding markdown code fence is tolerated), folds the hints over
the brief's bounded window (cut at the brief watermark), writes a graph
artifact, and publishes the `graph` context slot. A failed companion or
non-hints output is a command error; the driver turn continues fail-open.

### `observe`

Fold an observer-produced hints JSON file over a bounded provenance page and
write a graph artifact.

```sh
euler extension run causal-dag.observe ./session.jsonl \
  --hints ./observer-hints.json \
  --limit 128
```

Flags: `--hints` (required JSON object file, max 64 KiB), `--limit`,
`--scan-limit`, `--after-event-id`, `--watermark-event-id`.

The hints file is the raw `causal_dag` object, not `{ "causal_dag": ... }`.

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
output to `observer-apply`, which writes the graph artifact and publishes
the `graph` context slot into the driver's own context. The chain is
fail-open: any brief/companion/apply failure is recorded to diagnostics
(`round_observer_end`) and never fails the driver turn.

### Post-hoc graph from a completed run

Run with provenance:

```sh
euler exec --provenance ./session.jsonl --extensions causal-dag \
  "Read BRIEF.md and carry it out."
```

Then export or catch up:

```sh
euler extension enable causal-dag
euler extension run causal-dag.export ./session.jsonl --limit 512
euler extension run causal-dag.catch-up ./session.jsonl --limit 128 --max-ticks 16
```

`export` is stateless. `catch-up` is checkpointed and suitable for repeated
incremental projection.

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

Graph artifacts use schema `euler.causal_dag.v1` and media type
`application/vnd.euler.causal-dag.v1+json`.

Top-level artifact shape:

- `schema`
- `media_type`
- `generated_at`
- `session.id`
- `session.event_range.start/end/complete`
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
