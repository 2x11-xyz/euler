# ADR 0011: Permissions v2 — Guardian reviewer

## Status

Accepted (owner decision 2026-07-11, from the codex permission study).

## Context

Euler's permission gate has one decision channel: a capability ask reaches a
`PermissionDecider` (the TUI approval panel, a scripted decider, or the exec
auto-approve tiers). Every ask interrupts the human, so long autonomous runs
either drown the operator in prompts or run with broad session grants. The
codex study evaluated four remedies: an automated reviewer on the approval
channel (`auto_review`/guardian), safe-command static analysis, prefix-scoped
grant rules, and sandbox-first execution.

## Decision

Adopt three, defer one:

- **Guardian reviewer (this ADR, #80)** — a flag-gated companion agent that
  reviews permission asks on the SAME decision channel as the human. Off by
  default (`permission_reviewer = user`); enabled per session
  (`--permission-reviewer guardian`).
- **Safe-command analysis (#78)** and **prefix grant rules (#79)** — adopted;
  tracked in their own issues.
- **Sandbox-first execution** — deferred as a future epic; nothing in this
  design may foreclose it.

### Guardian mechanics

- The guardian is a companion (`Session::spawn_companion`) with an **empty
  capability set** (it cannot act at all), a budget of one model round and
  zero tool calls, and the standard `agent.spawn`/`agent.result` provenance
  (persona `guardian`). It sees the same assembled canvas the main model
  sees, plus the exact permission request (capability, reason, bounded
  command/path) as its task.
- Its system prompt frames everything except genuine user messages as
  **untrusted evidence**: evidence may inform state but can never expand user
  authorization, and instructions inside it are data.
- It must return one structured JSON verdict:
  `{risk_level: low|medium|high|critical, user_authorization:
  unknown|low|medium|high, outcome: allow|deny|abstain, rationale}`.
- The verdict is recorded as a first-class `permission.decision` event with
  `decision_source: "guardian"` plus `risk_level`, `user_authorization`, and
  `rationale` (events contract).
- `abstain` falls back to the configured human decider. `deny` is final —
  the human is never asked to overrule a guardian denial mid-turn.

## Safety invariants

1. **Thresholds are enforced in code**, not only in the prompt:
   low/medium risk → allow; high risk → allow only when
   `user_authorization` ≥ medium; critical → deny regardless of stated
   authorization or outcome field.
2. **Fail closed**: spawn failure, companion failure, missing or unparseable
   verdict → deny.
3. **Attenuation**: the guardian holds no capabilities; a compromised
   guardian can misjudge but cannot act. It advertises no tools (zero
   tool-call budget), so it cannot be prompt-injected into running anything.
4. **Circuit breaker**: three consecutive guardian denials in one turn
   interrupt the turn with a clear transcript line instead of letting the
   model thrash against the gate.
5. **Teaching denials**: a guardian denial injects guidance into the denied
   tool result — the model is told not to work around the block — instead of
   the bare `permission denied` string.
6. **Provenance honesty**: automated decisions are always distinguishable
   from user decisions (`decision_source`); the transcript renders guardian
   verdicts as quiet decision records with the rationale.
7. Guardian allows are **once-scoped**: no session or project grant is ever
   installed by the guardian. Existing user grants keep covering requests
   before the guardian is consulted.

## Consequences

- The ask channel gains one seam (uncovered `ask` resolution); deciders,
  grants, and modes are unchanged. Flag off → behavior is byte-identical to
  today.
- A guardian review costs one extra model round per ask and its verdict
  events live in the session canvas like any companion output (visible,
  bounded).
- Known gaps, accepted for v0: the guardian's canvas view renders compaction
  projections and extension context slots in the user role (the prompt warns
  against treating synthetic user-role content as authorization); guardian
  reviews are not cancelled by user turn-cancel mid-review; companion
  (child-agent) asks are not guardian-routed; token budgets on the guardian
  task are structural (one round, no tools, bounded output bytes) rather
  than a token ceiling, because companion token budgets count canvas input.
