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
   session's memory. See docs/notes/subagent-wave-ledger-2026-07-05.md.

## Decision 1: companion loop adopts the RoundLoop seam

companion.rs becomes the second RoundLoopIo implementor (its Complete type
carries the companion result summary). Duplicated streaming/collection code
is deleted, not preserved.

Frozen contracts (binding, from docs/adr/phase2-primitives-2026-07-05.md):
- denial semantics (TurnState) identical,
- budget accounting identical (AgentBudget max_tokens counts input+output),
- event kinds/ordering on the bus identical.

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
