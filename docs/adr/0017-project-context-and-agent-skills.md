# ADR 0017: Repository project context and shareable agent skills

## Status

Proposed (2026-07-20). Not implemented.

This ADR records the intended architecture. The contracts in `docs/contracts/`
describe current implemented behavior and will be amended with the first
implementation slice, not ahead of it.

Implementation is tracked in [issue #180](https://github.com/2x11-xyz/euler/issues/180).

## Context

Euler needs two kinds of repository-owned context that a team can commit and
share through Git:

1. `EULER.md`: always-available project guidance such as build commands,
   conventions, safety rules, and contribution protocols.
2. Reusable skills: discoverable instruction packages whose full procedures
   are loaded only when a task needs them.

These are related but not interchangeable. `EULER.md` is scoped, always-on
project guidance. A skill is named, selectively loaded procedure content with
optional supporting files.

The existing `.euler/` workspace directory is not a suitable security boundary
for deciding whether content is shareable. It currently contains several kinds
of data with different ownership:

- `.euler/grants.json` participates in project permission grants, but becomes
  authority only when intersected with matching user-owned consent outside the
  repository;
- `.euler/checkpoints/` contains machine-local rollback pre-images;
- `.euler/code-swarm.json` and `.euler/extensions.json` are project-scoped
  configuration that remains subject to ordinary capability enforcement.

A repository can commit an ignored path deliberately, and users can choose a
different ignore policy. Therefore `.gitignore` is convenience, never an
authorization boundary. This decision does not redesign `.euler/`.

Codex and Pi both discover the cross-agent `.agents/skills/` convention as well
as product-specific skill directories. Both use `SKILL.md` packages and
progressive disclosure: compact name/description metadata is always available,
while the complete procedure is loaded on demand. Using the shared convention
lets one committed skill work across Euler, Codex, and Pi.

Euler has stronger replay requirements than a conventional prompt loader. The
next model request should be reconstructable from the canonical session event
stream. Re-reading mutable repository files on each turn or resume would make
the same history produce different requests. Repository content is also
untrusted input: it may claim to authorize tools, contain prompt injection,
point through symlinks to data outside the repository, or combine dangerously
with broad headless auto-approval.

The relevant architectural constraints are:

- core owns model-input integrity, provenance, replay, permissions, and agent
  isolation;
- repository content may influence model behavior but must never grant
  authority;
- the provider `instructions` field is Euler-owned, high-priority policy, not a
  repository prompt channel;
- always-on context is a bounded product budget;
- independent agents may share a checkout, and several worktrees may contain
  different versions of the same project files;
- spawned children require explicit context inheritance rather than ambient
  filesystem discovery.

## Decision

### 1. Define project context as non-authoritative input

Euler will define **project context** as repository-authored guidance admitted
to a root driver session under core-controlled discovery, framing, bounds,
redaction, persistence, and replay rules.

Project context is data, not authority. No text or frontmatter in `EULER.md`, a
skill, a reference, or a helper script can:

- grant or widen a capability;
- approve a tool invocation;
- install a session, project, or user grant;
- enable an extension;
- expose a secret;
- bypass the subprocess sandbox;
- widen a child agent's capability envelope;
- suppress permission or provenance events.

Every resulting action still passes through the same tool registry, permission
gate, sandbox, secret boundary, budget, and provenance machinery as an action
suggested by an ordinary user message. In particular, an `allowed-tools` skill
field has no authorization meaning in Euler.

The fixed Euler-owned model instructions will state this precedence compactly.
Enforcement remains mechanical; prompt wording is not the security boundary.

### 2. Use `EULER.md` and `.agents/skills/`

The first release will recognize:

```text
EULER.md
.agents/
  skills/
    <skill-name>/
      SKILL.md
      scripts/
      references/
      assets/
```

`EULER.md` is the canonical, exact-case repository instruction filename.
`.agents/skills/` is the canonical shareable project-skill location.

The first release will not discover:

- `.euler/skills/`;
- `.pi/skills/`, `.codex/skills/`, or other product-native roots;
- `AGENTS.md` or `CLAUDE.md` as instruction fallbacks;
- a user-global `EULER.md`;
- user-global `~/.agents/skills/`.

Those are compatibility or user-configuration decisions that can be added
later without changing the repository format selected here.

### 3. Separate the tool workspace from the discovery root

`SessionConfig.root` remains the tool workspace and permission root. Project
context discovery must not replace it with a Git root or otherwise broaden
filesystem authority.

Euler finds the nearest ancestor containing an exact `.git` entry that is a
regular file or directory. A `.git` file identifies a linked Git worktree or
submodule root; Euler does not follow its contents to the common Git metadata
directory. A symlinked `.git` entry is not a root marker.

That nearest marker defines the **project discovery root**. Euler considers the
ordered directory chain from that root through `SessionConfig.root`, inclusive.
If there is no Git marker, the chain contains only `SessionConfig.root`.

This produces the following instruction order, from general to specific:

```text
<project-root>/EULER.md
<project-root>/crates/EULER.md
<project-root>/crates/euler-core/EULER.md
```

Only existing files are admitted, and each source is rendered once in that
order. A nested Git repository or submodule starts a new boundary and does not
implicitly inherit the containing repository's project context.

The same root-to-workspace chain supplies `.agents/skills/` discovery roots.
Skills never use directory proximity as silent override authority; collision
rules are defined below.

Absolute machine paths are not part of portable project-context identity.
Accepted source paths are normalized, bounded, UTF-8, project-root-relative
paths. The canonical workspace and project roots are still recorded separately
for local session ownership and resume checks.

### 4. Admit only stable, contained regular files

Discovery is a narrow read privilege for the exact project-context paths; it is
not general filesystem read authority above `SessionConfig.root`.

Euler will enumerate directory entries and compare names exactly. Opening a
candidate by a case-insensitive lookup is insufficient because it could admit
`euler.md` as `EULER.md` on some filesystems. Near-miss casing may produce a
diagnostic but is not loaded.

Every traversed skill directory and admitted file must remain contained in its
declared discovery root. Euler rejects symlinks, reparse points, devices,
sockets, FIFOs, and other non-regular files. The implementation must open
relative to a held directory/root handle using no-follow/beneath semantics when
the platform provides them, then verify the opened handle is regular and
bounded. A check-then-open sequence alone is not sufficient. If a platform
cannot perform a safe no-follow read, Euler omits that source rather than
following it.

For concurrent edits, Euler reads from one handle, compares stable metadata
before and after the bounded read, and retries at most once. If the source is
still changing, it is omitted with a typed `changed_during_read` diagnostic.
Euler never admits a torn or partially truncated source merely to keep startup
moving.

Malformed, unsafe, and over-limit sources are omitted whole. Startup continues
with visible typed diagnostics; content is never silently truncated because a
partial instruction can invert meaning.

### 5. Keep repository content out of provider system instructions

Repository-authored bytes will not be concatenated into
`ModelRequest.instructions`. That field remains Euler-owned and should be
byte-identical whether or not project context exists.

Core will add a typed, provider-neutral project-context input and corresponding
pinned canvas projection. Provider adapters render it below system/developer
policy as attributed context, before the conversational frontier. It is not
presented as an ordinary human-authored transcript message.

Core owns framing. Every source has a core-generated header carrying its
normalized path and repository-guidance classification, and every content line
is indented or otherwise escaped so source text cannot occupy the same framing
position as a core marker. Skill catalog metadata receives the same treatment.
Framing reduces structural spoofing; it does not make repository prose trusted.

Project instructions and the rendered skill catalog count against the context
budget. They are pinned while their session snapshot is active, rather than
silently disappearing during ordinary compaction. If pinned context plus the
minimum request cannot fit, Euler fails before provider invocation with an
honest context-budget error.

### 6. Freeze one replayable snapshot per fresh session

Fresh-session startup discovers, validates, bounds, and redacts project context
once, before the first model request. The effective result becomes an immutable
session snapshot. Mid-session filesystem edits do not alter that snapshot.

The implementation will add a durable `project.context.snapshot` event. Its
versioned canonical snapshot contains at least:

- load policy and the reason it resolved enabled or disabled;
- accepted `EULER.md` sources with relative paths, effective byte lengths,
  SHA-256 digests, and effective content;
- accepted skills with names, descriptions, relative `SKILL.md` paths, body
  lengths, body digests, and frozen bodies;
- deterministic ordering and schema version.

Large canonical snapshot content is stored through the session provenance blob
store. Digests cover the effective redacted bytes that can reach a model, not a
raw secret-bearing pre-image. Project context is external input, so it passes
through the same known-value and token-shape redaction boundary as tool results
and context slots before persistence or provider exposure. Redaction is
heuristic for unknown secret formats, as documented by the secrets contract.

Typed `project.context.diagnostic` events record omissions without embedding
unsafe content. `session.start` records a compact policy/count/digest summary,
and every root-driver `model.call` records the digest of the exact rendered
project context included in that request.

A snapshot digest is based on canonical project-relative identities and
effective contents, not absolute checkout paths. Two worktrees or two users can
therefore obtain the same digest when their selected and redacted project
content is identical.

There is no implicit reload in the first release. A future explicit reload may
append a new snapshot event; it must never rewrite the earlier snapshot.

### 7. Resume from provenance, never from current project files

Resume folds `project.context.snapshot` from the accepted event prefix and
performs no project-context filesystem discovery. Editing, deleting, or adding
an `EULER.md` or `SKILL.md` after session creation does not change a resumed
session's model input.

Legacy sessions without a project-context snapshot resume with project context
disabled. They do not acquire new repository instructions retroactively.

Resume also verifies the canonical live `SessionConfig.root` against the
workspace root recorded by the session. A different nested directory or a
different worktree can change tool authority and the applicable instruction
chain, so mismatch fails with a clear remediation. Euler does not silently:

- apply worktree A's frozen instructions while modifying worktree B;
- rediscover worktree B's instructions inside worktree A's history;
- switch the live tool root back to an old path without the user's knowledge.

Moving or forking a historical session into another worktree requires a future
explicit operation with new workspace and project-context events. Starting a
new session is the first-release remediation.

### 8. Give independent sessions independent snapshots

Two top-level Euler sessions may use the same checkout concurrently. Each has
its own event stream, project-context snapshot, model canvas, permission state,
and snapshot digest. There is no process-global or workspace-global mutable
project-context cache.

If sessions A and B start from identical context, their portable content
digests match. If `EULER.md` changes and session C starts later, C receives the
new snapshot while A and B retain the old one.

Existing provenance writer locks continue to prevent two processes from
writing the same session log. This feature does not add workspace transactions
or coordinate independent file edits. Two writing agents in one checkout can
still race on source files, generated files, Git operations, and rollback.
Separate Git worktrees are the recommended isolation for independently writing
agents; sharing a checkout is primarily suitable for read-only agents or
explicitly coordinated work.

### 9. Treat each Git worktree as a distinct live workspace

A linked worktree's `.git` file defines its discovery boundary. Euler loads the
`EULER.md` and `.agents/skills/` versions checked out in that worktree and never
walks into a sibling worktree.

Worktrees can intentionally have different project context on different
branches. Their local canonical workspace paths remain distinct for permission
consent and resume ownership even when their portable project-context digests
match. Consent associated with one canonical worktree path does not become
consent for another worktree merely because both share a Git object database.

### 10. Discover skills with progressive disclosure

Euler recursively discovers exact `SKILL.md` files beneath each selected
`.agents/skills/` root, without following directory links. A valid first-release
skill requires:

- a `name` frontmatter field using 1–64 ASCII lowercase letters, digits, and
  hyphens, with no leading, trailing, or consecutive hyphens;
- a parent directory basename exactly equal to `name`;
- a non-empty `description` of at most 1,024 UTF-8 bytes;
- parseable frontmatter and a bounded UTF-8 body;
- a safely opened regular `SKILL.md` contained beneath its skills root.

Known cross-agent fields such as `license`, `compatibility`, `metadata`, and
`allowed-tools` may be accepted for interoperability, but only `name` and
`description` enter the always-on catalog. Unknown fields do not affect Euler
runtime behavior. `allowed-tools` is never rendered as authority and never
changes permission evaluation.

The always-on project-context input contains only compact, core-framed name,
description, and source identity metadata. The complete effective `SKILL.md`
body is frozen in the session snapshot but stays out of model input until
selected.

A new core model tool, provisionally `skill_read`, accepts a catalogued skill
name, never an arbitrary path. It returns that session's frozen body and source
identity through ordinary `tool.call` and `tool.result` events. It does not
re-read the filesystem, grant a capability, execute a script, install a
dependency, or automatically read references and assets.

Supporting files remain ordinary workspace files and are governed by existing
tools and permissions. Project-context discovery does not grant general access
to a skill's sibling files above `SessionConfig.root`; a skill that depends on
an inaccessible helper fails honestly. A future narrow skill-resource API may
be designed separately.

If two accepted files claim the same normalized skill name, every claimant is
excluded and a typed ambiguity diagnostic names their bounded relative paths.
There is no first-wins, nearest-wins, or filesystem-order override.

### 11. Bound discovery and prompt cost

The initial implementation targets these contract limits:

| Resource | Limit |
|---|---:|
| Directory levels from project root to workspace | 32 |
| Accepted `EULER.md` sources | 16 |
| One `EULER.md` | 32 KiB |
| Combined accepted `EULER.md` content | 64 KiB |
| Skill traversal depth below each skills root | 6 |
| Skill directories examined across all roots | 512 |
| Accepted skills | 64 |
| One `SKILL.md` | 64 KiB |
| Combined frozen skill bodies | 1 MiB |
| Rendered always-on skill catalog | 16 KiB |

The implementation contract will freeze these values before the corresponding
slice ships. Selection and diagnostics are deterministic: directory entries
and normalized relative paths are ordered lexicographically, no limit depends
on filesystem iteration order, and duplicate detection occurs before final
catalog admission. Aggregate-limit behavior omits whole sources and reports the
omission rather than slicing their contents.

More-specific instruction files receive admission priority when the aggregate
instruction-byte limit requires a choice, then accepted sources are rendered
root-to-workspace. This preserves the most locally applicable rules while
keeping their interpretation order general-to-specific.

### 12. Make child inheritance explicit and snapshot-based

A spawned child never discovers project context from the filesystem. Project
context inheritance is distinct from the existing `include_parent_canvas`
choice and is recorded on `agent.spawn` as an explicit policy:

```text
project_context: none | inherit
```

`inherit` gives the child the parent's exact frozen snapshot and digest. It
does not produce a new snapshot, widen capabilities, or re-read files. The
child receives the skill catalog and `skill_read` only when this policy is
`inherit`.

The default for existing and generic child tasks is `none`, avoiding an ambient
behavior change. A coding-worker workflow may deliberately request `inherit`.
Guardian tasks always use `none`. CodeSwarm reviewers always use `none` and see
only their explicit bounded review packet. Observers and other companions do
not inherit unless their future protocol explicitly opts in.

Even when `include_parent_canvas` is true, project-context canvas items are
filtered unless `project_context` is `inherit`. This makes agent isolation an
enforced data-flow property rather than a prompt convention.

Parallel children that inherit receive one shared immutable parent snapshot,
assembled before fan-out. They cannot diverge because a file changed during the
batch.

### 13. Disclose and control project-context loading

Fresh sessions gain a policy control, provisionally:

```text
--project-context auto|on|off
```

`auto` resolves as follows:

- enabled for interactive `run` and `tui` sessions;
- enabled for ordinary headless/read-only `exec`;
- disabled for `exec --auto-approve trusted-local`.

A user may explicitly choose `on` with trusted-local auto-approval, but Euler
must disclose that repository-authored instructions are being paired with
pre-approved write and shell capabilities. The policy, resolution reason,
loaded source list, skill count, diagnostic count, and digest are visible at
startup and recorded in provenance.

This policy does not imply that enabled project context is trusted or that
disabled project context makes an otherwise hostile repository safe. It only
controls automatic model exposure. Resume uses the frozen original decision;
project-context flags do not mutate an existing session.

A durable per-project trust store may be considered later. It is not required
for the first interactive slice and must not be conflated with capability
approval if added.

### 14. Implement the substrate in core

Secure discovery, snapshot persistence, canvas admission, resume folding,
model-input framing, context accounting, child filtering, and `skill_read` are
core invariants. They belong in `euler-core` and provider-neutral request types,
not in a workflow extension.

This does not make skill semantics domain-specific core behavior. Core knows
only bounded instruction packages and their non-authority rules. Domain
workflows remain in skills or extensions. Extension-provided skill roots are a
future host capability and must inherit the same bounds, framing, provenance,
and authority rules when designed.

The current extension SDK cannot register general model-facing tools, so
building this first slice as an extension would require a larger SDK change and
would still leave replay and permission invariants in core.

## Delivery sequence

Implementation proceeds in independently reviewable vertical slices.

1. **Architecture and contract**
   - ratify this ADR;
   - add `docs/contracts/project-context.md` with exact event schemas,
     diagnostics, limits, and failure semantics;
   - update the events, canvas, capabilities, secrets, tools, multi-agent,
     boundaries, and extension-SDK contracts with the implementation that makes
     each statement true.
2. **`EULER.md` vertical slice**
   - secure Git/worktree discovery and stable reads;
   - typed snapshot and diagnostic events;
   - core-framed pinned model input and context accounting;
   - fresh-session policy/disclosure;
   - provenance-only resume and same-workspace enforcement.
3. **`.agents/skills/` vertical slice**
   - bounded deterministic discovery and validation;
   - frozen bodies and compact catalog;
   - duplicate exclusion;
   - core `skill_read` tool and provenance.
4. **Multi-agent inheritance**
   - explicit `none | inherit` task field;
   - child canvas filtering and inherited skill-tool wiring;
   - guardian, observer, and reviewer isolation tests.
5. **Documentation and dogfooding**
   - user guide and security guidance;
   - tracked Euler-repository `EULER.md`;
   - one or more useful shared development skills where they provide real
     progressive-disclosure value.

The project-context contract is intentionally not added by this ADR-only
change. Euler contracts state current truth; it lands with the first code slice
that enforces it.

## Required validation

The implementation is not complete without tests covering:

- exact-case discovery, Git directories, worktree `.git` files, submodules,
  nested repositories, and no-Git fallback;
- symlink/reparse escape, linked directories, non-regular files, FIFOs, loops,
  containment, concurrent mutation, invalid UTF-8, and every numeric boundary;
- deterministic ordering independent of directory iteration;
- malformed frontmatter, name grammar, basename mismatch, duplicate names,
  case variants, and Unicode-confusable rejection through the ASCII grammar;
- adversarial source text containing fake core framing markers;
- provider request capture proving `ModelRequest.instructions` is unchanged by
  repository content;
- canvas/context accounting and failure before an oversized provider request;
- mid-session file mutation and deletion with immutable live behavior;
- byte-equivalent model project context after resume, including a resume
  attempt from a different current directory or worktree;
- `skill_read` name-only access, frozen-body behavior, output bounds, and normal
  tool provenance parentage;
- `allowed-tools` and instruction claims producing no permission delta;
- helper scripts never executing merely because a skill was discovered or
  read;
- project-content redaction and exclusion of unrelated `.euler/`, consent,
  checkpoint, and auth bytes;
- independent sessions receiving independent snapshots;
- child `none | inherit` behavior at the final provider-request seam;
- guardian, CodeSwarm reviewer, and observer prompt isolation;
- trusted-local headless default-off behavior, explicit opt-in, disclosure, and
  permission events naming the real policy basis;
- regression that repository `.euler/grants.json` remains inert without
  matching user-owned consent.

## Consequences

- Teams can commit one Euler-specific instruction hierarchy and cross-agent
  skills that travel with a repository.
- Repository guidance is visible and useful without becoming an authorization
  mechanism.
- Active sessions deliberately retain older guidance after repository changes.
  Starting a new session, and later an explicit reload, are the honest ways to
  adopt changes.
- Snapshotted skill bodies increase session storage, bounded to 1 MiB before
  provenance overhead, in exchange for catalog/body coherence and replay.
- Project context consumes measurable model context and may prevent a request
  when it cannot fit; it is never hidden outside budget accounting.
- Worktrees isolate both source edits and checked-out project guidance, while
  portable snapshot digests still permit comparison.
- Same-checkout agents remain capable of filesystem races. This ADR documents
  worktrees as the recommendation rather than pretending project context is a
  workspace transaction system.
- Exact-workspace resume is stricter than current behavior but prevents a
  session from applying one checkout's frozen policy to another checkout's
  files.
- Companion behavior becomes more explicit: parent conversation inheritance
  and project-guidance inheritance are separate data-flow choices.

## Alternatives considered

### Put shareable skills under `.euler/skills/`

Deferred. It would be Euler-specific and would mix shareable content into a
directory already used for permission-related and machine-local state.
`.agents/skills/` provides immediate interoperability without preventing a
future Euler-native root.

### Put `EULER.md` in `.euler/`

Rejected. A top-level named file is visible, conventional, easy to review, and
does not suggest that instructions share ownership with runtime state.

### Append repository text to provider system/developer instructions

Rejected. It would give cloned repository content Euler's highest prompt
priority and bypass canvas accounting and ordinary attributed-input framing.
Mechanical permission enforcement would remain, but the trust and replay story
would be unnecessarily misleading.

### Re-read files every turn or resume

Rejected. It makes model requests depend on mutable ambient state rather than
the canonical session record and allows concurrent edits to change an agent's
instructions invisibly.

### Let the existing `read_file` tool load skills

Insufficient as the only mechanism. It cannot guarantee that the body matches
the catalog snapshot, and user-global or ancestor skills may sit outside the
tool workspace. A narrow snapshot-backed `skill_read` preserves progressive
disclosure without broadening path authority.

### Give every child the parent's project context

Rejected. Ambient inheritance contaminates independent reviewers and guardians
and couples child behavior to context it did not request. Explicit snapshot
inheritance preserves isolation and remains auditable.

### Add a mandatory project-trust prompt first

Deferred. Interactive disclosure plus non-system framing and unchanged
mechanical permissions provide a coherent first slice. Broadly auto-approved
headless runs default project context off. A future trust decision must be
separate from authority and stored outside repository control.

### Redesign `.euler/` and its ignore policy in this work

Rejected as unrelated scope. Existing stores already have distinct security
semantics, including the user-owned consent intersection for project grants.
This feature neither relies on nor changes repository ignore rules.

## Deferred work

- user-global `EULER.md` and `~/.agents/skills/`;
- `.euler/skills/` and product-native compatibility roots;
- `AGENTS.md` or `CLAUDE.md` fallback behavior;
- explicit reload and session relocation/fork semantics;
- `/skill:name` user commands;
- skill installation, registries, and management UX;
- automatic dependency installation or helper-script execution;
- a bounded skill-resource API for supporting files outside the tool root;
- extension-provided skill roots;
- a general project-trust store;
- workspace transactions or cross-session edit coordination.
