# Capability Contract

Capabilities govern tools, extensions, and child agents.

Child agents may only receive capabilities that are a subset of the parent
session's capabilities. In v0, agent capability attenuation uses exact set
semantics over the flat `Capability` strings below. Equality is allowed.
Path-aware, host-aware, provider-aware, model-aware, and secret-name-aware
narrowing are deferred until capabilities gain structured scope fields.

There are no trusted bypass flags for normal workflows. If a first-party extension cannot do its job without a bypass, the capability model is wrong.

## Trust Model Honesty

Native Rust extensions run in-process. They are trusted, audited code: the
capability model constrains what the *host APIs* will do for them and records
every decision in provenance, but it is not a sandbox — in-process native code
that ignores the host APIs is limited only by the operating system. What
capabilities buy for native extensions is least-privilege discipline, a
reviewable declared-authority surface, and an audit trail; containment of
actually-untrusted code requires the out-of-process transport (roadmap) with
OS-level isolation. Do not describe native extension capabilities as a
security boundary against malicious extensions.

## V0 Capability Scopes

Minimum v0 scopes:

- `fs-read`
- `fs-write`
- `provenance-read`
- `diagnostics-read`
- `artifact-write`
- `agent-record`
- `agent-spawn`
- `context-slot`
- `shell-exec`
- `network`
- `config-write`
- `secret-resolve`

## Approval Modes

A capability decision is one of:

- `ask` — prompt the user at use time.
- `session-allow` — allow for the current session/scope.
- `always-deny` — deny without prompting.

Permission prompts and decisions are session events and are recorded in provenance. Privileged secret/config edits always require explicit approval even if broader write access was granted.

## Extension capability approval

A command descriptor's `required_capabilities` is a *declaration*, never a
grant. On surfaces that can ask the user (the TUI), each declared capability
becomes a real permission decision before the command executes: explicit
`session-allow` grants silently, explicit `always-deny` denies without a
prompt, and `ask` or unconfigured capabilities prompt the user — recorded as
`permission.prompt`/`permission.decision` events carrying `extension_id` and
`command`. Session-scoped approvals cover later runs (covered requests run
under the original decision, with no fresh record). Piped headless runs
cannot prompt (stdin is the command protocol): there, explicitly invoking a
named command grants its declared capabilities for that run, announced on
stderr — visible, never silent.

## Scoped Grants

Capability modes are the coarse gate. **Scoped grants** sit above `ask`: when a
request matches an active session or project grant, the gate allows it without
re-prompting. `always-deny` still denies even if a grant exists.
`session-allow` remains capability-wide and does not require a grant match.

Grant lifetime and pattern:

| Scope | Lifetime | Pattern |
|-------|----------|---------|
| `once` | this request only | none |
| `session` | current session | optional `ScopePattern` |
| `project` | workspace project config | optional `ScopePattern` |

`ScopePattern` is an opaque bounded string:

- **Unscoped** (empty pattern): whole capability (legacy `AllowSession`).
- **`shell-exec`**: command first token (`cargo`, `git`).
- **`fs-write`**: workspace-relative directory prefix (typically the path's
  top-level directory). Matching is prefix: `src` covers `src` and `src/lib.rs`.

Derivation of a display/default pattern from a live request (first token, top
level dir) is a caller concern; core stores and matches opaque patterns.

### Decisions

A decider may return:

- allow once (`once`);
- allow session-scoped (`session` + pattern, possibly unscoped);
- allow project-scoped (`project` + pattern);
- deny;
- deny with **instruction** text — guidance the UI passes back as a user turn.

Legacy verdicts map as: `Allow` → `once`, `AllowSession` → `session` unscoped,
`Deny` → deny without instruction.

### Project grants

Project grants persist under the workspace at `.euler/grants.json` (see
`docs/contracts/persistence.md`). Installing a project grant is an explicit
config write: the approval that grants project scope **must** be recorded as a
`permission.decision` event with `grant_scope: "project"` (and pattern when
set). Silent project-config mutation is forbidden.

### Revocation and listing

Core exposes list and revoke APIs over session and project grant stores for
surfaces such as `/permissions`. Revoking a project grant rewrites
`.euler/grants.json`. Child-agent capability attenuation remains exact flat
subset semantics; scoped grants do not change child capability sets.

`provenance-read` gates host-mediated bounded provenance queries. It is not
raw filesystem read access. The v0 pull-based event feed uses this same
capability because it reads the accepted durable provenance prefix through the
same bounded host API. The current process-local wake primitive is not exposed
to extension or child-agent code and therefore adds no new capability. Future
live push/background subscription may require a separate capability if it
exposes materially different privilege or timing semantics.

`diagnostics-read` gates host-mediated bounded reads of the current session's
diagnostics log lines. It is not raw filesystem read access.

`artifact-write` gates host-mediated extension artifact writes. It is not raw
filesystem write access and does not permit arbitrary extension state writes.
Extension private state directory access remains `fs-write`-gated.

`agent-record` gates host-mediated immediate child-agent completion records.
It lets an extension command ask the host to append one validated `agent.spawn`
event followed by one terminal `agent.result` event through the owning session
writer. It is not live model invocation, a scheduler, a durable background
worker, an observer daemon, or an arbitrary child-process launcher. Child
capabilities use the same exact flat subset semantics as core child agents:
the child set may be empty or equal to the command grant, duplicates are
normalized, and escalation fails before the host appends any agent events.

`agent-spawn` gates host-mediated live child-agent execution
(`HostApi::spawn_agent`): the host runs one child session to completion for a
validated `AgentTask` and records the same `agent.spawn`/`agent.result` pair
the session companion path records. Everything `agent-record` is not, this is
also not — except live model invocation of exactly one child per call. Child
capability attenuation uses the same exact flat subset semantics; the child
set must be a subset of the invoking command's granted capabilities. Children
do not receive an extension host and cannot spawn (depth one in v0.1).
Synchronous per call; background/parallel extension spawns remain future work
(see the multi-agent contract).

Extension event-feed checkpoints are private extension state:

- `HostApi::load_event_feed_checkpoint` requires `fs-read`.
- `HostApi::store_event_feed_checkpoint` requires `fs-write`.
- `fs-write` permits the host to perform internal directory reads needed for
  safe checkpoint replacement, quota checks, and file-type validation, but it
  does not grant the extension read access to existing checkpoint contents.
- Bundled extensions that use durable cursors, such as `causal-dag.update`,
  must declare these capabilities even when they access checkpoints only
  through host APIs. The authority remains session-private checkpoint state, not
  arbitrary path access.

- Native extension manifests declare the maximum capability envelope for the
  extension. Each command declares its own required-capability set on its
  `CommandDescriptor`; that is the sole source of command capabilities. An
  empty set means no capabilities — there is no inheritance from the manifest
  envelope. A command's set must be a subset of the manifest envelope,
  enforced at registration.

`context-slot` gates host-mediated extension context slot updates. It permits an
extension command to append bounded `context.slot.updated` events for slots
namespaced to that extension id. It does not grant raw provenance reads,
arbitrary canvas control, or cross-extension slot writes.

`fs-read` defaults to `session-allow`: read tools execute without prompting, but every execution records a permission decision event. `fs-write` and `shell-exec` default to `ask`.
