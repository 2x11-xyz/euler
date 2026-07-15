# ADR 0013: Operation-scoped permission approval

## Status

Accepted (owner-directed implementation, 2026-07-14).

## Context

Euler's permission and grant model is capability-scoped, which is the right
shape for matching, revocation, provenance, and resume. Its presentation used
the same shape: an extension command with several declared capabilities caused
one sequential approval panel per capability. A user experiences
`causal-dag.refresh` as one action, not six unrelated actions, so the repeated
panels obscure rather than improve consent.

Codex separates an operation's approval policy from its sandbox boundary. Euler
does not yet have an enforced workspace sandbox. This change therefore improves
the approval interaction without claiming containment that does not exist.

## Decision

### One operation, one approval panel

The live-session extension bridge groups the *uncovered, static* capabilities
of one extension command into one permission request batch. The panel names the
extension command and lists every capability that the decision will cover.

Before it presents a batch, the bridge evaluates every declared capability:

- an explicit `always-deny` rejects the whole operation before any approval;
- configured `session-allow` and an already matching grant need no new
  decision;
- remaining `ask` and unconfigured capabilities form the batch.

The batch cannot make a partially approved operation run. Allow applies to all
of its listed capabilities; deny applies to all of them.

### Capability grants and provenance remain individual

The existing `PermissionGate`, `ApprovalMode`, matching rules, and grant stores
remain the authority. A batch response produces one `permission.decision` per
capability, and every decision records its own capability and grant scope.
Resume consequently continues to fold session grants by capability.

The canonical `permission.prompt` event remains the event kind. A batched
prompt retains its required primary `capability` field for compatibility and
adds the complete ordered `capabilities` list plus the operation attribution.
All of the batch's individual decisions parent that one prompt. Readers must
consider a batched prompt settled only after it has a decision for every listed
capability. Session-wide grants are installed only after the complete decision
set is durably appended; an interrupted batch cannot revive a partial session
grant during resume.

Batch UI offers only:

- allow once;
- allow all listed capabilities for this session; and
- deny.

It deliberately does not offer project or user-wide rules. Those scopes need
their own explicit capability/path or command subject and durable-store
transaction semantics; grouping them would make a broad rule look narrower
than it is.

### Permission postures prepare, but do not imitate, sandboxing

`/permissions` offers concise session postures alongside the existing advanced
per-capability controls:

- **Read only** — permit file, provenance, and diagnostics reads; deny the
  remaining capabilities.
- **Ask every time** — put every capability behind explicit user approval.
- **Full access** — allow capabilities for this session only, with an explicit
  unsandboxed label.
- **Auto in sandbox** — visible but unavailable until a verified Linux
  workspace-sandbox backend exists. It has no unsandboxed fallback.

The posture UI is a convenience mapping over the existing capability gate, not
a new grant or sandbox model. It does not override the secret/config-edit
guardrail in `docs/contracts/secrets.md`.

The existing Guardian remains an individual-request reviewer under ADR 0011.
This ADR does not claim that a guardian can safely decide a multi-capability
operation without a separately specified operation-review contract.

## Consequences

- A static extension operation has one comprehensible user interruption and a
  concise, truthful capability list.
- The event log and resumed session preserve per-capability authority rather
  than creating an opaque aggregate grant.
- Tool calls with one dynamically derived capability retain their current
  exact command/path approval panel.
- The terminal UI can expose the final sandbox-oriented choices now without
  allowing a mode that is not technically enforced.
- A later Bubblewrap implementation changes the availability of `Auto in
  sandbox`; it does not redefine authorization or introduce a second
  permission UI.
