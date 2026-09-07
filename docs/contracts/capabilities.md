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
- `extension-state`
- `provenance-read`
- `diagnostics-read`
- `artifact-write`
- `agent-record`
- `agent-spawn`
- `context-slot`
- `plan-presentation`
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

Cancelling an operation while it awaits an `ask` is neither an allow nor a
deny: Euler records no permission decision or grant, closes the owning
operation through its canonical cancellation path, and invalidates that
prompt's reply route so a late answer cannot decide a later request.

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

The extension host records each resolved registration grant or denial as a
`static-grant` decision before it reports registration success or the ordinary
capability-denied result. That provenance is mandatory: an append failure is
an infrastructure failure, not an implicit grant or denial, and a failed
allowed-decision append leaves no partially registered extension or command in
the host. A command cannot catch an SDK-level denial/provenance error and turn
that host failure into success; the host checks its own failure latch after
the command returns. Diagnostics are emitted only after the decision append
succeeds.

Terminal-idle contributions are implicit lifecycle work and therefore never
open this operation prompt. Every required capability must be
`session-allow`, or be `ask`/unconfigured and covered by an existing grant.
`always-deny` always rejects. Missing standing authority stops the contribution
before its command starts; the rejected stop is recorded without an `error`
event. Explicit model tools retain the ordinary operation-level prompt above.

Root request ticks (ADR 0019) use the same standing-authority rule and never
open an operation prompt. Missing authority latches only that tick contributor
for the remainder of the live Session; later contributors and the root request
continue. During an authorized tick, the ordinary command descriptor remains
the sole capability source. The request boundary's injected provenance cutoff
narrows an already-authorized `provenance-read` query; it grants no capability
and cannot be changed by the extension. Cancellation and mandatory permission
or command provenance failures retain their ordinary session-wide semantics.

## Install consent (extension distribution)

Installing an extension involves three distinct consents that must never be
merged into one record:

1. **Install consent** (this section): agreeing to fetch a pinned source and
   run its declared build steps. An install-time build is arbitrary code
   execution and is treated as such (ADR 0015).
2. **Launch consent**: the existing per-extension enable step that echoes the
   exact argv (managed-process runtime contract).
3. **Capability approval**: the per-command operation prompts above, at use
   time.

Install consent is recorded against the pinned content fingerprint of the
source. A changed fingerprint — any content movement, not only a manifest
change — requires fresh consent, because declared builds execute
source-controlled build logic. Consent for one fingerprint never covers
another.

The consent card must show, before anything runs: the source and resolved
pin, every declared build argv, the toolchains it requires, the extension
ids provided with their capability envelopes, and (on update) the manifest
diff when the manifest changed. Consent decisions are recorded as provenance
events like every other permission decision.

Presentation has two profiles, a user-level setting:

- `standard` (default): one combined card covering fetch, build, and
  execution grant; a single approval completes the install.
- `granular`: separate confirmation at each stage (fetch, build, grant) for
  users who want to inspect intermediate results.

Both profiles gather the same consents; the profile changes prompt
granularity, never authority. **First contact is never automatic**: no
profile, tier, posture, or project trust may skip consent for a fingerprint
the user has not approved on this machine. Automation is permissible only
for re-materializing an already-consented fingerprint (for example,
reinstalling on a store wipe).

## Scoped Grants

Capability modes are the coarse gate. **Scoped grants** sit above `ask`: when a
request matches an active session, project, or user grant, the gate allows it
without re-prompting. `always-deny` still denies even if a grant exists.
`session-allow` remains capability-wide and does not require a grant match —
except for a request the danger walk flags or a sensitive path, which
`mode_for_request` escalates to `ask` (see "Static command safety").

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
- deny with **instruction** text — guidance for the exact active run blocked
  by this permission request.

In the interactive UI, deny-with-instruction is a narrow permission response,
not ordinary composer submission. The UI captures the permission's active
durable run identity when the ask opens and admits the instruction only as
same-run steering for that identity. It never infers steering versus follow-up
from timing, and it never converts the instruction into a later run when the
blocked run has moved or terminalized. If the run identity cannot be proven,
the prompt and instruction remain visible and no permission reply is sent.
The instruction may be acknowledged to the decider only after that exact
steering enqueue is durable.

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

A malformed or unreadable grants file — on either the workspace side or the
consent side — fails closed exactly like a missing consent directory: no
project grants load, project-grant writes are disabled for the session, and
a reload over a corrupted file clears any previously active project grants.
A corrupt file is never broader authority than a missing one. Load rejects
oversize files, unknown versions, unknown capabilities, and control-bearing
patterns; unknown JSON fields are ignored; duplicate entries collapse to
one.

### User rules (durable prefix rules)

User rules are the "don't ask again for commands starting with `cargo`"
tier: they persist across sessions AND projects, in a single store at
`<home>/user-grants.json` under the user-owned euler home (same atomic-write
and 0600 discipline as the other grant stores). Unlike project grants they
need **no consent intersection** — the store is user-authored in the user's
own home and is never repo-controlled content, so there is no second party
whose entries could preseed authority. Sessions opened without a resolvable
user grant dir disable user rules entirely — reads and writes both fail
closed. A malformed or unreadable user-grants file is treated the same way:
no rules load, the store stays unloaded (durable installs fail), and a
corrupt file never yields broader permissions than a missing one.

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
analysis reasons about the whole line. It parses with **tree-sitter-bash**
and uses **two parsers over that syntax tree**: one conservative parser that
may prove a command safe, and one permissive walk that may only find danger.
Neither can do the other's job, and a lexical approximation of shell can do
neither: redirections glued to a word, `${...}`, `$'...'`, brace expansion,
and here-documents all evade a tokenizer while an AST reports them as
distinct nodes.

**Parser 1 — prove-safe grammar (conservative).**

- **Parsing.** The parse must succeed with no error node, and every node in
  the tree must be one of `program`, `list`, `pipeline`, `command`,
  `command_name`, `word`, `string`, `string_content`, `raw_string`,
  `number`, `concatenation`, joined only by `&&`, `||`, `;`, `|`. Anything
  else — a redirection, here-document, here-string, substitution,
  expansion, subshell, brace group, control flow, background `&`, variable
  assignment — makes the command **not statically analyzable**. Such
  commands are never auto-approved and never covered by scoped grants; they
  fall to the ask path. False negatives cost a prompt; false positives are
  forbidden.
- **Literal words.** Every word must be exactly what the binary will
  receive: no `* ? [ ] { } ~ $ ` \ ^ #`, and no word beginning with `=`.
  A word the shell may rewrite is never proof, for any binary. A backslash
  ANYWHERE in the line makes it unprovable: `tree-sitter-bash` treats a
  backslash-newline as whitespace while `sh` treats it as a line
  continuation, so `cat .en\<newline>v` parses as three harmless words and
  executes as `cat .env`.
- **Options are an allowlist.** Each binary in the read-only set declares
  the exact option spellings it accepts. An unknown option, a GNU long
  abbreviation (`--recu`, `--dereference-rec`, `--fol`), an attached short
  value (`-oout.bin`, `-i.env`, `-n50`), or a bundle containing an unlisted
  letter (`-rS`, `-Do`) is simply not provable. A denylist has to enumerate
  every harmful spelling on every platform and misses one; this polarity
  cannot.
- **Read-only set.** `true false pwd whoami id uname echo expr seq which
  cat head tail wc ls nl paste rev cut tr stat uniq grep base64`, plus
  `find` (only the enumerated read-only predicates, so `-exec`, `-delete`,
  `-L`, `-follow` are rejected by omission) and `sed` (only the print-range
  form `sed -n Np [file]`). Recursive readers are excluded: no `grep -r`, and no
  `rg` at all, since `rg` recurses by default. A tree walk reads files the
  per-operand sensitive check never saw — `grep -r PASSWORD .` printed
  `.env`, and `rg PASSWORD` reads `deploy.pem`, `id_rsa`, and
  `credentials.json`. Under ADR 0021 row P the sandbox auto-allows these
  later; until then they prompt. `uniq` accepts at most one operand, because the
  second operand is an output file it truncates. Binaries outside the set
  are never provable — `sort` (`-o` writes a file), `tee`, and every
  interpreter included.
- **`git` is not in the set**, even for `status`/`log`/`diff`/`show`. Those
  subcommands execute repository-controlled programs through
  `diff.external`, `core.fsmonitor`, `core.pager`, and clean and smudge
  filters, all selected by `.git/config` — the very file audit F34 showed a
  "read-only" command could write. Proving `git` safe needs the sandbox,
  not a parser; it returns in Unit 2 (ADR 0021 row P).
- **`cd` is not in the set**: it moves the directory later commands resolve
  against while confinement keeps checking the root the command started in.
- **Wrappers.** `[sh|bash|zsh] -c|-lc <script>` is provable only by
  recursively proving `<script>`, depth-capped at eight. No other wrapper
  form (`env`, `/bin/sh`, extra flags) is provable.
- **Workspace confinement.** Read-only is not harmless: `cat
  ~/.aws/credentials` writes nothing and still exfiltrates. Every operand
  and every option value — there is no exempt position, so a regex operand
  is checked like any other word — must stay inside the workspace root the
  command executes in: an existing path must canonicalize (symlinks
  resolved) under the canonicalized root; a non-existing argument must be
  relative with no `..` component and no leading `~`.
- **Sensitive paths.** One denylist, used by static shell analysis and by
  the fs-tool "Sensitive-basename ask" below (one list, not two), applied
  to the literal spelling AND to the canonicalized resolution — so an
  innocently named in-workspace symlink cannot read through it:
  - anything with a `.git` path component — the directory, the worktree
    pointer file, and everything under it;
  - `.gitmodules`, `.gitattributes`, `.gitconfig`, `.npmrc`, `.netrc`,
    `.cargo/config.toml` (and `.cargo/config`) — configuration an
    interpreter or build tool honors on its next run;
  - shell startup files: `.bashrc`, `.bash_profile`, `.bash_login`,
    `.bash_logout`, `.profile`, `.zshrc`, `.zshenv`, `.zprofile`,
    `.zlogin`, `.zlogout`;
  - `.env*`, names containing `secret`/`credential`, `id_rsa`,
    `id_ed25519`, `*.pem`, `*.key`.
- A command is **statically safe** iff it parses under this grammar AND
  every command in it is read-only with rule-conforming options AND every
  operand and option value is confined and non-sensitive AND the danger
  walk below finds nothing.

**Parser 2 — find-danger walk (permissive).** A separate function visits
*every* command node in the tree — inside control flow, command and process
substitutions, expansions, redirection arguments, and the scripts carried by
`sh -c`, `eval`, `env -S`, and `trap` — and unwraps `sudo`, `doas`, `su`,
`env`, `nohup`, `time`, `timeout`, `nice`, `ionice`, `chrt`, `stdbuf`,
`setsid`, `flock`, `xargs`, `command`, `builtin`, and `exec`. It **must
never be used to prove safety**. It fails closed on a parse error, on a
dynamic command name, on a dynamic argument of a dangerous or wrapper
command, on an unrecognized `env` option, and past a wrapper depth of eight;
both walks are iterative and node-bounded, so nesting cannot exhaust the
stack.

Its danger predicate is an intentionally **extensible table** of commands
that destroy data with no undo: `rm` with `-r`/`-R`/`-f`/`--recursive`/
`--force` or any GNU abbreviation of those (`run_shell` closes stdin, so
`rm -r` never gets its interactive confirmation and is as destructive as
`rm -rf`), `find` with `-delete`/`-exec`/`-execdir`/`-ok`/`-okdir`, `git
clean -f`, `reset --hard`, `rm -f`, `checkout` that forces or names a path,
`restore <path>`, `branch -D` (and `-d` with `-f`), `stash drop`/`clear`,
`shred`, `truncate`, `wipefs`, `dd of=`, the `mkfs*` family, and any
interpreter invocation (`sed`, `awk`, `perl`, `python`, `ruby`, `node`,
`patch`, `ed`) whose operands — program text included — mention a sensitive
path. Command names are matched
case-insensitively, because the default macOS filesystem is.
It also flags **writes an interpreter later honors**: a redirect target or a
`cp`/`mv`/`tee`/`install`/`ln`/`rsync` destination on the sensitive list, so
`printf x > .git/hooks/pre-commit` and `echo x > .bashrc` ask exactly as
`write_file` on those paths does (audit F34, write side), including when
the redirection hangs off a compound statement
(`{ printf x; } > .bashrc`).

An extension-declared `shell-exec` request is walked like `run_shell` when
the invocation names a command, and treated as unreadable — never
grant-covered, always prompted — when it does not. The walk runs before the
capability mode is consulted, so a blanket `session-allow` does not skip
it.

A **truncated** command still blocks scoped grant matching, but the walk
itself reads the full command text, so an ordinary multi-kilobyte command is
not flagged merely for being long.

A flagged command is **never auto-approved, in any mode** (ADR 0021 decision
D), and is never routed to the guardian reviewer: it must reach a human. No grant covers it — scoped or unscoped, session, project, or user —
and every capability mode short of `always-deny` is escalated to `ask` for
that one request, so a forced `rm` prompts even under a blanket
`session-allow`. `always-deny` still denies without prompting, and a
never-prompt decider denies on the same path.

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

`extension-state` gates `HostApi::state_dir`, which returns the calling
extension's session-private directory. The directory is namespaced by
extension id, but it is intentionally one read/write scope: native and
managed-process extensions are trusted code rather than OS-sandboxed peers, so
claiming separate host-enforced read and write authority after returning a raw
path would be dishonest. This does not grant workspace file access.

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
- Extensions that use durable cursors must declare these capabilities even
  when they access checkpoints only through host APIs. The authority remains session-private checkpoint state, not
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

`plan-presentation` gates host-mediated typed plan presentation. It permits an
extension command to append bounded, attributed `plan.update` events. It does
not grant arbitrary event emission, context-slot writes, direct canvas
admission, or authority over another extension's workflow state.

`fs-read` defaults to `session-allow`: read tools execute without prompting,
but every execution records a permission decision event. Root-session
`extension-state`, `context-slot`, and `plan-presentation` also default to
`session-allow`; each is extension-attributed and bounded as described above.
`fs-write`, `shell-exec`, and root-session `agent-spawn` default to `ask`.
Unconfigured capabilities remain `always-deny`; child-agent gates start
deny-all and inherit only their explicit attenuated envelope. Headless
auto-approve tiers override these root defaults with the explicit mapping
above.

**Sensitive-basename ask.** A blanket `session-allow` never covers a tool
request whose path names a categorically sensitive file. When a path-taking
tool request (`read_file`, and the write tools' paths equally) targets a
path on the sensitive list — the same list static command safety enforces
(anything under a `.git` component, git/npm/cargo/shell configuration,
`.env*`, names containing `secret`/`credential`, `id_rsa`, `id_ed25519`,
`*.pem`, `*.key`; see "Static command safety" above for the full list) —
the gate escalates that single request from `session-allow` to `ask`. The check applies to the literal argument AND its
canonicalized workspace resolution, so an innocently named symlink cannot
evade it. The escalated ask flows through the ordinary permission braid: a
covering session/project/user grant satisfies it silently (the tool result
carries the usual `grant_source` tag), otherwise a `permission.prompt` is
emitted whose reason names the file and why it is sensitive, and the
configured decider resolves it — allow-once, a session grant (which then
covers later sensitive reads), or deny. This is an ask, not a deny:
`always-deny` is never weakened and no new denial surface exists. Both
permission gates — root session and companion loop — apply the same
escalation. In headless auto-approve tiers the escalated ask reaches the
fail-closed headless decider and is therefore denied unless a durable grant
covers it.
