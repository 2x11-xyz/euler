# ADR 0018: Extension Model Tools and Idle Contributions

## Status

Accepted (2026-07-27).

## Context

Euler's extension host can execute native and managed-process commands, but a
session cannot expose those commands to its model. Extensions therefore cannot
build removable, model-driven workflows without either adding a special tool
to core or teaching core the workflow.

The session also ends whenever the model returns a terminal response. Some
workflows need to decide, from extension-owned state, whether another model
turn is needed. Core must provide the idle boundary and resource governance
without learning what "done" means.

CodeSwarm's dedicated review tool is a transitional exception. Generalizing
its workflow or moving its policy into this API would preserve the wrong
boundary.

## Decision 1: commands remain the executable primitive

There is one extension execution path. A command may carry an optional,
explicit model-tool descriptor:

- model-visible name;
- concise description;
- closed, bounded JSON input schema.

The descriptor is advertisement, not a second command implementation. A model
tool call maps back to its declaring command and runs through the existing
extension host, operation-scoped capability approval, panic isolation,
redaction, and managed-process protocol.

Only `agent-only` commands may declare model tools. User command surfaces
continue to refuse those commands. Model-tool names must not collide with core
tools or another enabled extension tool. Companions do not inherit root-session
extension tools.

Core validates both the schema and each model-supplied input. Schemas are a
small supported JSON Schema subset, objects are closed, and schema/input/output
sizes are host-bounded. Unsupported schema vocabulary fails registration
instead of being advertised with semantics core does not enforce.

## Decision 2: one enabled extension may contribute at terminal idle

An extension may nominate one registered `agent-only` command as its
terminal-idle contributor. After an otherwise successful terminal model round,
core may invoke that command with an empty object. Its closed result envelope
is either:

```json
{"action":"stop"}
```

or:

```json
{"action":"continue","input":"bounded model-facing text"}
```

The extension owns the meaning and state behind that decision. Core owns only
the boundary, validation, standing-authority check, provenance, cancellation
checks, user-input priority, and a host resource ceiling. Because this is
implicit lifecycle work, it never opens a permission prompt: all command
capabilities must already be session-allowed, or covered by an existing grant
when their mode is `ask` or unconfigured; `always-deny` remains final. Missing
authority records a rejected stop contribution without starting the command or
emitting an error. Explicit model tools continue to use the ordinary
operation-scoped permission braid.

At most one enabled contributor may own a root session's idle boundary.
Pending user input wins before and after contributor execution. An accepted
continuation starts another root `RoundLoop` without forging a `user.message`;
it is recorded as an attributed `extension.contribution` event and projected
into the next request with core-generated framing. It remains eligible across
persistence and resume until selected by that request's `canvas.snapshot`,
then becomes provenance-only so later requests cannot accumulate old
continuations. A stop, rejected continuation, malformed result, or failure
does not enter the model canvas.

The hook runs only after a normal terminal model response. It does not run
after provider failure, context-limit stop, guardian interruption, explicit
tool-round limit, or cancellation. Automatic continuations are capped per
user-driven run so a faulty extension cannot create unbounded model spend.
Root sessions default the session-private `extension-state` and bounded,
extension-namespaced `context-slot` and `plan-presentation` capabilities to
`session-allow`, allowing a plan extension to probe absent or resumed state and
publish bounded presentation without a prompt. The extension still owns
whether any plan exists; core does not infer one.

An accepted continuation is committed input for the current user turn, not
live extension state. A later disable therefore cannot retroactively hide it;
the next snapshot consumes it once. This keeps crash/resume from turning a
temporary disable into stale continuation resurrection.

## Decision 3: session wiring is generic and revalidated

Fresh and resumed root sessions wire enabled linked managed-process extensions
that declare either contribution. Wiring launches no process and grants no
capability. The linked package fingerprint and launch consent are revalidated
when declarations are read and again immediately before command execution.

The existing observer and CodeSwarm wiring may coexist during migration, but
they do not define this API. New removable workflows use the generic surface.

## Decision 4: typed plan presentation is a host substrate

Extensions may request one bounded typed plan presentation through
`HostApi::update_plan_presentation`, gated by `plan-presentation`. Core owns the
closed DTO, redaction, structural bounds, canonical `plan.update` emission, and
TUI checklist projection. The host derives extension/command attribution and a
compatibility summary; extensions cannot emit arbitrary events.

The extension owns all workflow semantics: whether a plan exists, revision
transitions, item consistency, persistence, completion policy, and the
model-facing context slot. A plan event is transcript/provenance presentation,
not direct canvas state. This keeps plan/todo removable while giving every
language runtime a first-class canonical UI.

## Cancellation boundary

This slice observes cancellation before invoking an idle contributor and
before accepting its returned continuation. Immediate interruption of an
already-running extension command, including termination of a managed child,
belongs to the single generic extension-command cancellation seam owned by the
Escape/cancellation work. This ADR does not introduce a parallel cancellation
API.

## Consequences

- Native Rust and out-of-process extensions have descriptor and behavior
  parity.
- Core gains product-neutral hosting and lifecycle scaffolding, not workflow
  state, goals, plans, or todo policy.
- A plan/todo extension can own its schema, persistence, interpretation, and
  completion decision while using ordinary model tools, typed plan
  presentation, and the idle boundary.
- `extension.contribution` becomes canonical provenance and resumable event
  vocabulary; only accepted `continue` events are model-canvas inputs.
- CodeSwarm remains an explicitly transitional special path until separately
  migrated or removed.
