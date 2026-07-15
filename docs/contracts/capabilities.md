# Capability Contract

Capabilities govern tools, extensions, and child agents.

Child agents may only receive capabilities that are a subset of the parent
session's capabilities. In v0, agent capability attenuation uses exact set
semantics over the flat `Capability` strings below. Equality is allowed.
Path-aware, host-aware, provider-aware, model-aware, and secret-name-aware
narrowing are deferred until capabilities gain structured scope fields.

There are no trusted bypass flags for normal workflows. If a first-party extension cannot do its job without a bypass, the capability model is wrong.

## Trust Model Honesty

Native Rust extensions run in-process. Managed-process extensions run as a
child process and the host owns their protocol, lifecycle, capability-gated
host APIs, provenance events, and canvas admission. Both are trusted-code
surfaces: the capability model constrains what the *host APIs* will do and
records every decision in provenance, but neither runtime is an OS sandbox. A
native extension or child process that ignores host APIs is limited only by the
operating system. What capabilities buy is least-privilege discipline, a
reviewable declared-authority surface, and an audit trail. Containment of
actually-untrusted code requires a separate OS-level isolation milestone; a
subprocess alone is not that boundary. Do not describe extension capabilities
as a security boundary against malicious extension code.

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

## Permission reviewer (guardian)

The session has one **permission reviewer** for uncovered `ask` decisions:
`user` (default — the configured decider, e.g. the TUI approval panel) or
`guardian` (ADR 0011). With `guardian`, an uncovered ask is reviewed by a
flag-gated companion agent spawned with an **empty capability set** and a
one-round, zero-tool budget, on the same decision channel the human would
use. Rules (normative; enforced in code, not only in the guardian prompt):

- Verdict shape: `{risk_level: low|medium|high|critical, user_authorization:
  unknown|low|medium|high, outcome: allow|deny|abstain, rationale}`.
- Thresholds: low/medium risk → allow; high risk → allow only when
  `user_authorization` ≥ medium; critical → deny, not overridable.
- Fail closed: guardian spawn failure, companion failure, or an unparseable
  verdict is a deny.
- `abstain` falls back to the configured decider (the human). `deny` is
  final for the ask; it never falls back.
- Guardian allows are once-scoped; the guardian never installs session or
  project grants. Requests covered by existing grants run under those grants
  and are not guardian-reviewed.
- The guardian adjudicates only requests it can see **verbatim** (ADR 0011
  amendment): if the command was truncated at the retention bound, or the
  task brief's own field bound would alter the command or path, the guardian
  is never consulted — the ask goes directly to the human decider
  (fail-to-human, enforced in code).
- Denials inject guidance into the failed tool result telling the model not
  to work around the block. Three consecutive guardian denials in one turn
  interrupt the turn (circuit breaker).
- Every guardian decision is a `permission.decision` event tagged
  `decision_source: "guardian"` with the verdict fields (events contract);
  automated decisions are always distinguishable from user decisions.
- Headless `exec`: auto-approve tiers leave no capability in `ask` mode, so
  configuring the guardian returns `fs-write` and `shell-exec` to `ask` —
  every use is guardian-reviewed, overriding the tier for those two
  capabilities in both directions (read-only's always-deny and
  trusted-local's session-allow). A guardian abstain then hits the headless
  decider's unconditional deny (fail closed; no prompt exists).

## Extension capability approval

A command descriptor's `required_capabilities` is a *declaration*, never a
grant. On surfaces that can ask the user (the TUI), one extension command
gets one operation-level prompt listing every uncovered static capability:
explicit `session-allow` grants silently, explicit `always-deny` rejects the
whole operation before a prompt, and remaining `ask` or unconfigured
capabilities form the batch. An allow or deny applies to the operation as a
whole, but the ledger records one `permission.decision` per listed capability,
all parented to the shared `permission.prompt` and carrying `extension_id` and
`command`. Session-scoped approvals cover later runs (covered requests run
under the original decision, with no fresh record). Batch approval offers only
once and unscoped session scope; it never turns a multi-capability operation
into a project or durable user rule. Piped headless runs cannot prompt (stdin
is the command protocol): there, explicitly invoking a named command grants
its declared capabilities for that run, announced on stderr — visible, never
silent.

## Scoped Grants

Capability modes are the coarse gate. **Scoped grants** sit above `ask`: when a
request matches an active session, project, or user grant, the gate allows it
without re-prompting. `always-deny` still denies even if a grant exists.
`session-allow` remains capability-wide and does not require a grant match.

Grant lifetime and pattern:

| Scope | Lifetime | Pattern |
|-------|----------|---------|
| `once` | this request only | none |
| `session` | current session | optional `ScopePattern` |
| `project` | workspace project config | optional `ScopePattern` |
| `user` | every session, every project (durable) | `ScopePattern` (prefix rule) |

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
- allow user-scoped (`user` + pattern — a durable prefix rule);
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

The workspace file is repo-controlled content and is **never authority on its
own**. A project grant is active only when it appears in BOTH the workspace
file AND the user's consent store — a per-root file under the user-owned
euler home (`<home>/project-grants/<sha256(canonical root)>.json`) written
when the user approves the grant on this machine. A cloned repository that
ships `.euler/grants.json` therefore grants nothing until this user approves
each entry; deleting either side deactivates the grant. Sessions opened
without a resolvable consent directory disable project grants entirely —
reads and writes both fail closed.

### User rules (durable prefix rules)

User rules are the "don't ask again for commands starting with `cargo`"
tier: they persist across sessions AND projects, in a single store at
`<home>/user-grants.json` under the user-owned euler home (same atomic-write
and 0600 discipline as the other grant stores). Unlike project grants they
need **no consent intersection** — the store is user-authored in the user's
own home and is never repo-controlled content, so there is no second party
whose entries could preseed authority. Sessions opened without a resolvable
user grant dir disable user rules entirely — reads and writes both fail
closed.

Installing a user rule is an explicit durable-config write and **must** be
recorded as a `permission.decision` event with `grant_scope: "user"` (and
the pattern). Silent user-store mutation is forbidden.

Pattern semantics for `shell-exec` are a **command prefix over the parsed
first token** — a rule `cargo` covers any command whose first token is
`cargo`, exactly as session/project token scopes match. Coverage composes
per segment (issue #78, see "Static command safety"): a prefix rule covers
a compound command iff it parses into plain segments and every segment is
either statically safe or prefix-covered. Unparseable commands (redirects,
substitution, subshells) are never covered and always re-ask.

A run covered by an existing user rule executes under that original
decision: no fresh `permission.decision` event, and the tool result carries
`grant_source: "user"` so the ledger can tag the run `· user rule`.

The approval panel offers the rule as `u  Allow <prefix> * always`,
alongside once/session/project — and only when it is honest: a prefix must
derive from a simple shell command AND the session must hold a loaded user
store. Unscoped or compound asks never show the option, and a session
without a resolvable user grant dir hides it entirely.

### Static command safety

Core performs static analysis of `shell-exec` command lines
(`euler-core/src/command_safety.rs`). Execution is `sh -c <command>`, so the
analysis reasons about the whole line:

- **Parsing.** A command line decomposes into plain segments across `&&`,
  `||`, `;`, `|`, and newlines. The tokenizer honors single/double quotes
  (quoted metacharacters are literal text, never operators). Any redirect
  (`>`, `<`, `>>`, `<<`), subshell/grouping/brace form (`(`, `)`, `{`, `}`),
  substitution or expansion (`$`, backtick — including inside double
  quotes), background `&`, comment, unterminated quote, or empty segment
  makes the whole command **not statically analyzable**. Unparseable
  commands are never auto-approved and never covered by scoped grants; they
  fall to the ask path. False negatives cost a prompt; false positives are
  forbidden.
- **Classification.** Each segment's argv is checked against a behavioral
  allowlist of read-only binaries: `cat cd cut echo expr false grep head id
  ls nl paste pwd rev seq stat tail tr true uname uniq wc which whoami`,
  plus flag-inspected binaries that are safe only in read-only form: `find`
  (no `-exec`/`-execdir`/`-ok`/`-okdir`/`-delete`/`-fls`/
  `-fprint`/`-fprint0`/`-fprintf`), `rg` (no `--pre`/`--hostname-bin`/
  `--search-zip`/`-z`, including bundled shorts), `base64` (no
  `-o`/`--output`), `sed` (only `sed -n Np` / `sed -n M,Np` print-range
  form), `git` (only `status`/`log`/`diff`/`show`/`branch` as the token
  immediately after `git` — any global flag rejects — with no
  `--output`/`--ext-diff`/`--textconv`/`--exec` args, and `branch` only as
  a pure listing query). Binary names match the first token exactly
  (`/bin/ls` and `env ls` do not match); unquoted globs reject the
  flag-inspected binaries because runtime expansion could inject
  flag-shaped tokens.
- **Workspace confinement.** Read-only is not harmless: `cat
  ~/.aws/credentials` writes nothing and still exfiltrates. Every argument
  of a safe segment that may name a filesystem path must stay inside the
  workspace root the command executes in: an existing path must
  canonicalize (symlinks resolved) under the canonicalized root; a
  non-existing argument must be relative with no `..` component, no leading
  `~`, and no `$`/backtick. A sensitive-basename denylist (`.env*`, names
  containing `secret`/`credential`, `id_rsa`, `id_ed25519`, `*.pem`,
  `*.key`) rejects even inside the workspace. Argument positions are
  classified conservatively — only the grep/rg pattern position is exempt,
  and only when no `-e`/`-f`-style flag can shift it; `--flag=value` values
  are checked, and flags that could carry an attached path reject. A
  rejected segment is simply not statically safe: the command falls back to
  the ordinary ask path (fail open to ask, never a new denial surface).
- A command is **statically safe** iff it parses AND every segment is safe
  AND every segment's path arguments are confined to the workspace.

**Auto-approval under `ask`.** When `shell-exec` is in `ask` mode, a
statically-safe command runs without a prompt. The run is recorded as a
fresh `permission.decision` with `mode: "static-safe"`, `allowed: true`,
`grant_scope: "once"`, parented to the `tool.call` — allowed-once
semantics; **no grant is installed** and no prompt event is emitted. The
static-safe check precedes grant-coverage matching, so the ledger
attributes such runs to the analysis rather than to an unrelated grant.
Static safety never bypasses `always-deny`, and a capability denial earlier
in the same turn still short-circuits the tool call. A command truncated at
the retention bound is never analyzed (and never matches scoped grants):
any permission decision must be based on exactly what will execute, or fail
closed to the ask path.

**Ledger treatment.** The decision event keeps provenance honest, but the
transcript does not render it as a standalone record — the
standalone-record-per-call noise is exactly what covered grants eliminated
(review v2 §8). Instead the `tool.result` carries `static_safe: true` and
the tool header shows a dim `· safe` tag, matching the covered-grant
`· session grant` header treatment.

**Segment-aware grant coverage.** Scoped `shell-exec` grant matching uses
the same segment analysis: a command is covered iff it parses into plain
segments and EVERY segment either has a granted first token or is
statically safe — with at least one segment actually matching a granted
token (an all-safe command is attributed to the static-safe path, never to
an unrelated grant). Tokens pool within one store: `cargo test && npm run
lint` is covered when the session store (or the project store) grants both
`cargo` and `npm`; a compound whose segments straddle the two stores falls
back to ask so the ledger's single `grant_source` tag stays honest.
Unparseable commands are never covered. The approval panel offers a token
scope only when the gate could actually grant it for the live command:
when every non-statically-safe segment shares one first token, that token
is offered; otherwise (distinct unsafe tokens, unparseable command) only
allow-once / unscoped / deny are offered.

### Revocation and listing

Core exposes list and revoke APIs over session, project, and user grant
stores for surfaces such as `/permissions`. Revoking a project grant rewrites
`.euler/grants.json`; revoking a user rule rewrites `<home>/user-grants.json`.
Child-agent capability attenuation remains exact flat subset semantics;
scoped grants do not change child capability sets.

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
`spawn_agent` is synchronous per call. `HostApi::spawn_agents` (v0.2) runs a
batch of single-round, tool-free, empty-capability child briefs concurrently
under the same gate and per-command quota; see the multi-agent contract for
its determinism and event-ordering invariants. The session-level
`code_swarm_review` tool is gated on this same capability through the
ordinary tool permission machinery. Headless `euler exec --auto-approve`
tiers set `agent-spawn` to session-allow in both tiers: children's own tool
calls remain gated by the parent's tier-configured modes, so allowing spawn
cannot escalate beyond the tier.

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

`fs-read` defaults to `session-allow`: read tools execute without prompting,
but every execution records a permission decision event. `fs-write`,
`shell-exec`, and root-session `agent-spawn` default to `ask`. Unconfigured
capabilities remain `always-deny`; child-agent gates start deny-all and inherit
only their explicit attenuated envelope. Headless auto-approve tiers override
these root defaults with the explicit mapping above.
