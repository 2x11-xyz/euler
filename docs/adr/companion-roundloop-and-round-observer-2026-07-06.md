# ADR: Companion RoundLoop Adoption + Round-Boundary Observation Hook

Status: accepted (Eli, 2026-07-06 - "the approved sequence").

## Context

1. session/companion.rs duplicates the model-round loop (~800 lines) that
   session.rs now runs through the extracted RoundLoop seam (PR #191).
   Transport retry (#193) exists only on the RoundLoop side; companions
   still die on single transient failures.
2. The causal-dag live experiment exposed the missing primitive: context
   slots fold from the in-process event stream, but nothing drives
   observation DURING a session. External processes cannot reach a running
   session's memory.

## Decision 1: companion loop adopts the RoundLoop seam

companion.rs becomes the second RoundLoopIo implementor (its Complete type
carries the companion result summary). Duplicated streaming/collection code
is deleted, not preserved.

Frozen contracts (binding, from docs/adr/phase2-primitives-2026-07-05.md):
- denial semantics (TurnState) identical,
- budget accounting identical (AgentBudget max_tokens counts input+output),
- event kinds/ordering on the bus identical.

Amendment (2026-07-11, issue #58): AgentBudget max_tokens now bounds
cumulative OUTPUT (completion) tokens only, not input+output as frozen
above. Reviewers/companions see the whole session canvas as input, which
routinely exceeded any output-scale budget on its own, exhausting budgets
before the first completion token. Owner-approved semantics change:
- each round requests at most the REMAINING output budget (cap minus
  cumulative output so far) as the provider-side max_output_tokens;
- the budget is exhausted when cumulative output strictly EXCEEDS the cap;
  a round landing exactly on the cap succeeds, and a continuation with
  zero remaining budget fails before the next provider call.

Intentional behavior change, documented here: companions inherit bounded
transport retry (#193 semantics: zero-output rounds, transport category
only). This is a deliberate upgrade, not drift; a companion retry test is
required.

## Decision 2: round-boundary observation hook (core primitive)

Amendment (2026-07-07): the hook exists so extensions can construct derived
state while the session unfolds, not so core can prescribe a particular
observer model class. The Causal DAG extension's lesson is that citation
discipline must be structural: an observer operating over the current bounded
event window can only cite the events it was shown, whereas whole-log retro
observation can satisfy distinctness gates with hindsight shortcuts and stale
notebook citations. Realtime windowing makes graph construction an extension
workflow over bounded events; core remains a product-neutral scheduler.

SessionConfig gains:

    round_observer: Option<RoundObserverConfig>
    // extension_id, brief_command, apply_command, every_n_rounds,
    // provider, model, max_tokens

At each configured cadence boundary in RoundLoop::run, core executes:
1. brief_command on the enabled extension via the live extension host
   (host bound to the in-process session; watermark/window threading is the
   extension's checkpoint concern),
2. one companion model call (consolidated Decision-1 path) with the
    brief output as task,
3. apply_command with the companion output.

Cadence is round-driven, with an event-count safety trigger: the default is
every 8 driver tool rounds, and implementations must force a tick before the
unobserved event window can exceed the extension's bounded-page contract. For
the Causal DAG extension this keeps live observation inside the 256-event page
bound by construction. Retro observation remains a useful audit path but is a
degraded construction path for long sessions.

Properties (non-negotiable):
- fail-open: any hook failure logs to diagnostics and never fails the
  driver round;
- budget-capped by max_tokens; cancel-aware;
- provenance-recorded: extension command events + companion attribution;
- default None; enabled per-session only (exec flags --observe <ext>,
  --observer-model <provider/model>, --observe-every <n>);
- model target inherits the active session target unless explicitly
  overridden by configuration; model class is an evaluation question, not an
  ADR default;
- observer outputs are provisional. Extension projections must support later
  status and edge revision as new windows arrive; evals must score revision
  quality, not only final graph shape.

Boundary defense: core schedules and plumbs three things core already owns
(round boundaries, providers, extension host). Interpretation stays in the
extension; the primitive is generic over any (brief, apply) command pair.
causal-dag is the first consumer, not the design.

Amendment (2026-07-12, observer-loop repair): the loop's authority split and
envelope contract are now explicit, fixing the two defects that kept the
in-session loop from ever closing.

- **Observer companion capabilities: none.** The companion is a one-turn
  generation task — it PRODUCES the observation and performs no writes, so
  its `AgentTask` carries an empty capability set. Granting the extension's
  manifest set to the companion (the previous behavior) failed companion
  subset validation against the parent session's tool-permission
  capabilities — extension-host capabilities (artifact-write, context-slot,
  agent-record, provenance-read) are never in that set — and rejected every
  spawn before `agent.spawn`. All writes happen in the apply command, which
  core executes with the extension's manifest grant (the existing
  capability-gated path); the brief and apply commands keep that grant
  unchanged.
- **Brief envelope**: either a task object or the generic no-work object
  `{ "status": "idle" }`. A task object has `task` (required string), optional
  `provider`/`model` (both or neither), optional `system_prompt` (string,
  becomes the companion's system instructions — this is how the extension
  teaches the observer its output schema), optional `budget`
  (`max_turns`/`max_tool_calls`/`max_tokens`), and an opaque `apply` value.
  An idle object may carry extension-owned informational fields, but none of
  the recognized task fields; core records a successful tick and runs neither
  companion nor apply. Unknown fields on a task object are ignored; an unknown
  status or mixed idle/task envelope fails the tick fail-open
  (`failed_stage="envelope"` semantics under the brief stage).
- **Apply envelope**: core calls the apply command with exactly
  `{ "apply": <brief apply value untouched>, "companion": { ok, summary,
  output, error, child_agent_id, spawn_event_id, result_event_id } }`.
  The extension owns both halves: the brief must thread whatever
  window/checkpoint context its apply step needs through `apply`, and the
  apply step extracts the companion's `output` itself.
- **causal-dag pairing**: `(observer-brief, observer-apply)`. The brief's
  `apply` passthrough is its observe window (`limit`, optional
  `scan_limit`/`after_event_id`, `watermark_event_id`, optional
  `session_id`); `observer-apply` parses the companion output as raw
  `euler.causal_dag.hints.v1` JSON (one surrounding markdown fence is
  stripped), folds it over the same bounded window cut at the brief's
  watermark, writes the graph artifact, and publishes the `graph` context
  slot. A failed companion or non-hints output is an apply error — recorded
  fail-open, never laundered into a degraded projection. The previous
  pairing (`record-observation` as apply) could not consume the envelope at
  all: it is a post-hoc audit command and stays one.

## Future (v0.2, explicitly out of scope now)

Manifest-declared extension ticks and HostApi::run_companion as the
extension-facing companion surface. The RoundObserverConfig shape must
graduate to that surface without rework; changes here that would foreclose
it are rejected.

## Stop-condition audit

"Core API expanding for one workflow" - this ADR is the required record;
the primitive is workflow-generic. "Workflow logic entering core" - none:
core learns no causal-dag concepts. "Provenance entering canvas assembly" -
untouched; the hook writes via the existing capability-gated slot path.
