# Project Context Contract (binding shape)

ADR 0017 governs project context; this contract binds its concrete shape.
Implementation status: the phase-2 dormant substrate and the phase-3
`EULER.md` exposure are implemented (issue #180). Implemented truth:
`EULER.md` discovery/containment/bounds, preflight redaction ordering, the
candidate manifest and all four digest domains, the
`project.context.snapshot`/`project.context.diagnostic`/`project.context.relocated`
events and durable bootstrap, core framing and the pinned provider-neutral
input with its budget accounting at both admission and request time,
provenance-only resume with workspace-identity enforcement, child
`none | inherit` filtering at request assembly, the `--project-context
auto|on|off` policy resolution, the user-owned acknowledgment record and
store ("Acknowledgment record" section in full), the interactive
acknowledgment card and non-interactive fail-closed behavior, and resume
relocation consent ("Resume relocation and consent" in full: the relocation
card, `--accept-relocation`, the `project.context.relocated` event, the
permission epoch, and the `new_root` projection). NOT yet implemented (still
bound shape): skills and `skill_read` ("Skills" section, the skill rows of
the bounds table, and skill fields of the snapshot), the always-on catalog,
explicit reload, and guardian/worker `inherit` wiring (the child policy
field exists, but today every child uses the `none` default). Issue #180
tracks the remaining slices, and this paragraph shrinks as they land.

## Definition and non-authority

Project context is repository-authored guidance (`EULER.md` files and
`.agents/skills/` skills) admitted to a root driver session under
core-controlled discovery, framing, bounds, redaction, persistence, and
replay. It is data, never authority: no project-context text or frontmatter
can grant or widen a capability, approve a tool invocation, install a grant,
enable an extension, expose a secret, bypass the sandbox, widen a child
envelope, or suppress permission or provenance events. `allowed-tools` in a
skill has no authorization meaning. Every induced action passes the same
permission machinery as one suggested by an ordinary user message.

## Discovery and precedence

- Recognized paths: `EULER.md` (exact case) and `.agents/skills/<name>/SKILL.md`
  along the ordered directory chain from the project discovery root (nearest
  ancestor with an exact regular `.git` file or directory; a symlinked `.git`
  is not a marker) through `SessionConfig.root`, inclusive. No Git marker
  means the chain is `SessionConfig.root` alone.
- Rendering order is general-to-specific (root first). Nested repositories
  and submodules start a new boundary. Worktrees are distinct workspaces: a
  linked worktree's `.git` file defines its own boundary, and discovery never
  walks into a sibling worktree.
- Discovery reads the working tree and is independent of version-control
  state: tracked, untracked, and ignored files are admitted identically. An
  ignored `EULER.md` or skill directory is the supported private per-project
  overlay; snapshots record and frame it like any other source.
- Containment: the workspace anchor is established by a component-wise
  no-follow `openat` walk from the filesystem root (a check-then-open of an
  absolute path is not sufficient); a component that resolves to a symlink
  fails discovery closed with a typed diagnostic. Sources then open relative
  to those held handles with no-follow/beneath semantics; symlinks, reparse
  points, and non-regular files are rejected; a platform that cannot
  no-follow-read omits the source. Directory enumeration is bounded: a level
  with more entries than the frozen cap is unknowable — its `.git` presence
  and its contents equally — so the boundary search may neither claim it as
  a marker nor continue past it, because an indeterminate level must never
  widen discovery upward across a possible nested-repository boundary. The
  whole preflight fails closed instead: zero sources, a typed
  `dir_entries_exceeded` diagnostic with the observed count plus a
  `marker_indeterminate` record, and a disabled snapshot
  (`resolution_reason: boundary_indeterminate`). A level whose enumeration
  fails outright during the boundary search closes the preflight the same
  way (`io_error` plus `marker_indeterminate`). These records carry no path:
  no discovery root exists yet to relativize against, and ancestor path
  fragments are outside-workspace data the manifest must not carry.
  Admission requires two bounded reads from
  independently verified handles with byte-identical results (per-handle
  stable-metadata comparison is only a fast-path reject); an unstable
  verification retries once, then omits with a `changed_during_read`
  diagnostic. Malformed, unsafe, or over-limit sources are omitted whole
  with typed diagnostics, never truncated.

Bounds (frozen by this contract; changing one is a contract change):

| Resource | Limit |
|---|---:|
| Directory levels from project root to workspace | 32 |
| Accepted `EULER.md` sources | 16 |
| One `EULER.md` | 32 KiB |
| Combined accepted `EULER.md` content | 64 KiB |
| Directory entries examined per directory level | 4096 |
| Skill traversal depth below each skills root | 6 |
| Skill directories examined across all roots | 512 |
| Accepted skills | 64 |
| One `SKILL.md` | 64 KiB |
| Combined frozen skill bodies | 1 MiB |
| Rendered always-on skill catalog | 16 KiB |

Selection and diagnostics are deterministic: lexicographic ordering of
normalized relative paths, no dependence on filesystem iteration order,
duplicate detection before catalog admission, and more-specific sources win
admission priority when the aggregate instruction budget forces a choice.

## Preflight and redaction

Project-context preflight is local and occurs before acknowledgment because
Euler must compute the digest the acknowledgment names. That preflight is not
general filesystem authority: it uses only the discovery paths and bounds
above, reads each verification from independently verified handles, retries
an unstable source at most once as specified above, and freezes only the
byte-stable result so acknowledgment, persistence, and prompt assembly
cannot observe different versions of a file.

Preflight failure is never silent degradation. A fresh session is never
composed without its bootstrap: a preflight whose diagnostics exceed the
manifest bound, or that cannot assemble a valid manifest for any other
reason, collapses into a disabled result whose manifest contains exactly one
typed record (`diagnostic_overflow` with the observed count, or
`preflight_invalid`) and admits nothing — the collapse participates in the
candidate digest like any other manifest content. The one unrecoverable
case is a workspace root that cannot be canonicalized: a session whose root
cannot be resolved cannot enforce any path-keyed rule, so fresh session
start fails with a plain-language error instead of inventing an
identity-less snapshot or starting a legacy-shaped session.

The host constructs one startup redactor before preflight, seeds it with the
environment, the selected auth store, and every other credential value known
at startup, and gives that same redactor to the session. Values that do not
exist until request time remain subject to the secrets contract's token-shape
and request-time tainting rules; this contract does not claim to detect an
unknown value retroactively. Raw candidate bytes and pre-redaction digests
never enter events, logs, diagnostics, the acknowledgment store, or provider
requests.

The **candidate manifest** is the complete bounded preflight result: accepted
normalized relative source identities and their frozen post-redaction bytes,
plus ordered content-free diagnostic records and per-reason total counts. The
portable candidate digest commits to a versioned, domain-separated,
length-prefixed encoding of that manifest. Files and directory entries that
the frozen scan limits prevent Euler from examining cannot reach a model and
are outside the manifest; a limit diagnostic and its observed count are
inside it. Any change to content that could be admitted, or to deterministic
selection among examined candidates, changes the digest.

## Acknowledgment record

Admission under `--project-context auto` requires a recorded project
acknowledgment: user-owned, stored under the user Euler home outside
repository control, keyed to the canonical workspace root and the portable
candidate digest. Two-party rule, exactly as project grants: the repository
supplies content, the user-side record supplies the decision, and neither
alone admits anything. A changed digest requires a fresh acknowledgment at
the next fresh session; unchanged content never re-prompts. Declining is
recorded in that session and disables admission for that session; it does not
write a durable refusal. Only affirmative acceptance through the interactive
acknowledgment surface writes durable acknowledgment. Explicit `on` is
session-only and never writes one. Headless runs never
prompt: without a matching acknowledgment they run with project context
disabled and say so in the startup summary. Under `auto`, `exec --auto-approve
trusted-local` disables admission regardless of acknowledgment. Explicit
`--project-context on` supplied by the current invocation is a separate,
session-only dual opt-in and may override that automatic resolution; the
combination is disclosed before the first provider request and recorded in
provenance. It cannot come from repository configuration, stored
acknowledgment, or resumed state. Explicit `off` disables without touching
stored acknowledgments.

The acknowledgment store contains only a format version, canonical workspace
identity, portable candidate digest, and minimal acceptance metadata. It
contains no source bodies, diagnostics, per-source hashes, or permission
state. The host accesses it through user-owned private directories, rejects
symlinks and non-regular files, validates ownership and non-public
writability where the platform exposes them, and replaces records atomically.
Failure to verify or write durable acceptance fails closed; it never enables
an unrecorded `auto` admission. Acknowledgment admits guidance into model
context and nothing else; it is never capability approval. Any future general
project-trust surface must unify this store with the extension project tier's
install consent (extension SDK contract) rather than adding a third
per-project trust store.

## Snapshot, events, and replay

- One immutable snapshot per fresh session, assembled before the first model
  request. Mid-session filesystem edits change nothing; there is no implicit
  reload. A future explicit reload appends a new snapshot event and never
  rewrites the old one.
- `project.context.snapshot` (durable, versioned) carries at least: load
  policy and resolution reason, acknowledgment basis, portable candidate
  digest, local workspace identity, deterministic ordering, diagnostic counts,
  and schema version. An admitted snapshot additionally carries accepted
  `EULER.md` sources (relative path, effective byte length, domain-separated
  SHA-256 digest, effective content) and accepted skills (name, description,
  relative path, body length, body digest, frozen body). A disabled, declined,
  or unacknowledged snapshot persists no candidate body, per-source content
  hash, exact content length, parser excerpt, or other reversible content;
  only the portable candidate digest, bounded normalized identities, counts,
  and content-free reason codes remain.
- The admitted manifest is serialized once as versioned UTF-8 snapshot JSON in
  one top-level payload string. When it exceeds the provenance threshold that
  complete string is one content-addressed blob; individual bodies are not
  externalized independently. The persisted bytes are authoritative and are
  not regenerated during resume. Rehydration verifies the blob address and
  length, rejects invalid UTF-8, duplicate keys, trailing data, unsupported
  versions, limit violations, and internal digest mismatches, and never falls
  back to current project files. Recorded snapshot and diagnostic payloads
  are untrusted input on fold: every field of both the admitted and the
  disabled shapes is re-validated against the rules the encoder obeys —
  unknown payload fields, malformed digest shapes, non-normalized or
  traversal identities, unknown workspace-identity algorithm/version pairs,
  reason codes outside the stable grammar, and per-reason counts that do not
  match the recorded diagnostics all reject. Field-by-field validity is not
  sufficient: the (status, policy, resolution_reason, acknowledgment_basis)
  combination must be one this Euler version can produce. The permitted
  tuples are frozen per phase and extended — never rediscovered — by later
  slices:

  | status | policy | resolution_reason | acknowledgment_basis |
  |---|---|---|---|
  | disabled | off | exposure_forced_off | none |
  | disabled | off | preflight_collapsed | none |
  | disabled | off | boundary_indeterminate | none |
  | admitted | on | test_hook | none |

  (`test_hook` is writable only by the crate-internal phase-2 test hook; no
  public path produces an admitted tuple. Phase 3 replaces it with the
  acknowledgment-side tuples and adds the `declined`/`unacknowledged`
  statuses with theirs; until a status has a permitted tuple it rejects.)
  The `session.start` project-context summary is validated with the same
  rigor — key whitelist and grammar — and every overlapping field (status,
  policy, resolution reason, acknowledgment basis, candidate digest, source
  and diagnostic counts) must agree exactly with the snapshot it announces;
  any mismatch fails the bootstrap shape.
- `project.context.diagnostic` events record omissions without embedding
  unsafe content. Their payload is limited to a stable reason code, bounded
  normalized relative identity when one exists, and non-content numeric
  metadata such as an offset or observed count. It contains no excerpts, raw
  parser errors, outside-workspace paths, or exception strings derived from a
  candidate.
- The durable bootstrap order is exactly `session.start`, one
  `project.context.snapshot`, then the snapshot's declared number of
  `project.context.diagnostic` events. `session.start` records that a snapshot
  is expected and a compact policy/count/digest summary; each diagnostic cites
  the snapshot event. The complete sequence must persist before any provider
  dispatch. Append or blob failure is session-fatal and cannot fall through to
  a provider call. Resume rejects a missing, partial, duplicated, or
  inconsistent bootstrap rather than silently disabling context.
- Every root-driver `model.call` records the rendered-context digest only when
  those exact bytes occur in the provider-neutral request (no TOCTOU between
  snapshot and prompt assembly). Candidate, source-content,
  rendered-context, and workspace-identity digests use distinct versioned
  domain tags and length-prefixed fields; the provenance blob address remains
  SHA-256 over the exact blob bytes.
- Candidate digests are portable: canonical project-relative identities and
  effective contents, not absolute paths. The local workspace identity is
  deliberately not portable.
- Resume folds the snapshot from the accepted event prefix and performs no
  filesystem discovery; legacy sessions resume with project context disabled;
  resume verifies the canonical live workspace root against the recorded one.
  A mismatch is never silently followed. Interactive resume offers relocation
  consent (see "Resume relocation and consent" below); headless resume and the
  phase-2 interim fail closed with the plain-language remediation. Older Euler
  versions fail safe on the unknown event kind and cannot resume such sessions.
- The latest snapshot event in durable sequence is authoritative. An admitted
  latest snapshot yields exactly one pinned item; a later disabled or declined
  snapshot is a tombstone and yields none. A malformed latest snapshot rejects
  resume or request assembly and never resurrects an older admitted snapshot.
- Independent sessions get independent snapshots; there is no process-global
  or workspace-global mutable project-context cache.

The workspace identity payload carries an algorithm and platform version. It
hashes a domain tag plus the length-prefixed exact platform representation
returned by canonicalizing `SessionConfig.root`. The first implementation
targets Euler's supported Unix hosts and hashes raw `OsStr` bytes with no lossy
display conversion or Unicode normalization. A future host requires a distinct
algorithm version and test vectors before it can resume project-context
sessions. Canonicalization failure and unknown algorithms reject.
Cross-platform resume rejects, because a different host algorithm cannot be
compared; false rejection is preferred to merging distinct roots. Within one
host a path relocation or a different worktree is a recorded mismatch, resolved
through the relocation-consent flow below rather than a silent merge. This
identity detects location mismatch, not replacement of content at the same
path, and is not workspace authentication. Legacy fallback applies only when
both the project-context summary and snapshot are absent; a mixed shape with
context events but no identity is invalid.

## Resume relocation and consent

Implementation status: implemented (issue #180 phase 3), alongside the
acknowledgment store and its interactive surface.

An interactive resume whose live canonical workspace does not match the
recorded workspace identity neither fails closed nor silently adopts the new
location. Euler shows a relocation-consent card and asks one question: resume
this session here, or not.

The card states facts and never a guessed reason. Euler cannot know why a path
changed (a rename, a move, a fresh clone at a new location, a different
checkout) and must not speculate. The card carries exactly:

- the recorded workspace path (where this session last ran);
- the current workspace path (where the resume is being attempted);
- when the session was last active.

The card discloses the consequences in plain language before the choice.
Resuming adopts the current folder for this session going forward, and
approvals keyed to the old location do not carry over. Project grants and
project-context acknowledgments are two-party records keyed to a canonical
workspace root, so the new root has its own records: project grants at the new
root require the new root's own consent intersection, and the next fresh
session under the new root re-asks acknowledgment for the new
(root, candidate digest) pair. Nothing from the old root is copied, widened, or
assumed. That non-carry-over is enforced by a permission epoch, not by wording
(see "Permission epoch" below).

The session also keeps the project guidance it started with. Resume performs no
filesystem discovery, so after relocation the old folder's frozen `EULER.md`
sources and skills remain in model input while tools operate on the new folder,
and the new folder's `EULER.md` is not read; only a fresh session under the new
root discovers and (after acknowledgment) admits the new folder's context. The
card states this in one line.

Permission epoch. Accepting relocation establishes a permission epoch at the
relocation event. Session-scoped permission grants recorded before that event
are invalidated: the resume permission fold ignores session grants that precede
the governing `project.context.relocated` event, so an earlier `shell-exec` or
`fs-write` session grant cannot silently authorize an operation in the newly
adopted folder. Project grants do not transfer; they reload from the new root's
two-party consent intersection (`docs/contracts/capabilities.md`, "Project
grants"), which is keyed to the canonical root and is empty for the new root
until the user approves there. Only durable user rules survive, because they are
workspace-independent by design (`docs/contracts/capabilities.md`, "User
rules"). This mechanism, not the card's wording, is what makes the
no-carry-over line literally true.

Affirmative acceptance appends one explicit `project.context.relocated` event
and then proceeds with the resume. Declining changes nothing (no event, no
identity change, no epoch) and the resume does not proceed; the session remains
exactly as it was, resumable at its recorded location.

The event's canonical payload is:

- `schema_version` (integer).
- `prior_identity`: `{ algorithm, version, digest }`, the workspace identity
  folded at the accepted event prefix (the identity governing immediately
  before this event).
- `new_identity`: `{ algorithm, version, digest }`, the identity of the live
  canonical root at decision time, computed exactly as a fresh snapshot computes
  it from the canonicalized `SessionConfig.root`.
- `new_root`: the new canonical workspace root in the same bounded, normalized,
  lossy display form `session.start` records for its `root`. This field is
  display and projection metadata only, never identity authority; identity
  comparison uses `new_identity`. It exists because the identity digests are
  irreversible, so without a recorded display path Euler could not render the
  recorded path on a later relocation card or in the session picker.
- `decided_at`: an audit wall-clock stamp of acceptance, audit metadata only.
  It never orders events or establishes authority; the event's durable append
  position and parentage are the causal facts.

The event embeds no repository content and no guessed reason, and the original
snapshot and every prior event are never rewritten.

Parentage and durability. The event parents the accepted tail event the resume
folded to (the same event a first continued turn attaches to). It is an in-chain
durable event, not a log-leaf: it persists (append and any blob) before any
resumed activity proceeds, and it becomes the frontier the first continued turn
and the emitted `session.resumed` attach to. Append or blob failure is fatal and
cannot fall through to a provider call, exactly as the bootstrap sequence
requires.

Validation (any breach fails closed and rejects the resume): `prior_identity`
MUST equal the identity folded at the accepted prefix; `new_identity` MUST equal
the live canonical root's identity at decision time; `new_root` MUST re-derive
to `new_identity` under the identity algorithm; the event MUST parent the
accepted tail event (the durable event immediately preceding it), so a missing
or wrong parent rejects; and a relocation whose `prior_identity` does not match
the current governing identity (a stale fold or a branched acceptance) is
rejected and never supersedes. The acceptance path validates the candidate event
against the folded prefix with this exact check before appending it, so nothing
the fold would reject can ever reach the log. Because the workspace identity
hashes the raw canonical path bytes while `new_root` is a lossy display string,
a workspace root whose canonical path bytes are not valid UTF-8 cannot relocate
in v1: it is refused with a plain-language error rather than recorded, because
the display form could never re-derive to its identity.
`docs/contracts/events.md` carries the same field, parentage, and validation
rules for the event kind.

Identity and projection supersession: the latest `project.context.relocated` in
durable sequence governs both the identity used for resume comparison and the
projected workspace root. Its `new_identity` becomes the identity later resumes
compare the live root against, and its `new_root` governs the projected root
everywhere the first `session.start` root is used today (session listing and
picker, current-directory grouping, resume checks, and the recorded path the
next relocation card renders). After a relocation event, later resumes at the
new path succeed without re-asking, and a resume back at the old path is itself
a mismatch and gets the same card. Successive relocations chain the same way,
each appending its own event, and the most recent one wins. A malformed or
unsupported relocation event rejects resume rather than falling back to an older
identity or root.

Headless resume never prompts. Without the explicit flag it keeps failing
closed with the plain-language remediation. `--accept-relocation` supplied by
the current invocation is the scripted equivalent of answering yes: it accepts
the specific live-versus-recorded mismatch present at this resume, appends the
same `project.context.relocated` event, and is recorded identically in
provenance. The flag is a single-invocation decision. It cannot come from
repository configuration, stored acknowledgment, or resumed state, it accepts
only the mismatch actually present (never a future or different relocation),
and it carries no old-root approvals forward.

## Framing and canvas admission

Repository bytes never enter `ModelRequest.instructions`; that field is
byte-identical with or without project context. Project context is a typed,
provider-neutral input with a pinned canvas projection, rendered below
system/developer policy as attributed context. Core owns framing on all
three admission paths, and the rules are identical for each:

1. `EULER.md` sources: core-generated header with normalized path and
   repository-guidance classification; every content line indented/escaped so
   source text can never occupy a core marker position.
2. The always-on skill catalog: compact core-framed name, description, and
   source identity only.
3. `skill_read` results: the frozen body returns through ordinary
   `tool.call`/`tool.result` events with the same core-framed header (skill
   name, source identity) and indentation rules as startup sources.

The provider-neutral order is fixed Euler instructions, at most one
`ProjectContext` item, then every existing input item in its original relative
order. Adapters may wrap the item in provider-specific role or envelope data
but must not trim, normalize, combine, silently omit, or reorder its content.
An adapter unable to represent the item fails before dispatch. Core framing is
versioned and performed once before the rendered-context digest is computed.

Pinned project context counts against both the canvas byte budget and a known
model context limit and does not silently vanish under compaction. The
deterministic context-limit proxy is four rendered UTF-8 bytes per token. At
snapshot admission, required tokens are
`ceil((fixed_instruction_bytes + framed_project_context_bytes) / 4) + 1024 +
output_reserve`, where `output_reserve` is configured `max_output_tokens` or,
when absent, `compaction_reserve_tokens`. At request time the same checked
proxy includes fixed instructions, every provider-neutral input item, and
serialized tool definitions, plus `output_reserve`. Equality with the known
limit fits; one token over does not. Unknown context limits still enforce the
canvas byte budget. Arithmetic overflow, an unrepresentable configured value,
or a project-context item larger than the byte budget fails before provider
invocation with an honest context-budget event. Pinned context is never
truncated or demoted to make either equation pass.

## Skills

- Grammar: `name` is 1-64 ASCII lowercase letters, digits, and hyphens (no
  leading/trailing/consecutive hyphens); parent directory basename must equal
  `name`; `description` is non-empty, at most 1024 UTF-8 bytes; frontmatter
  must parse; body is bounded UTF-8. Known cross-agent fields (`license`,
  `compatibility`, `metadata`, `allowed-tools`) are accepted but inert;
  unknown fields are inert.
- Duplicate normalized names exclude every claimant with an ambiguity
  diagnostic naming their paths; no first-wins or nearest-wins.
- `skill_read` accepts a catalogued name (never a path) and returns the
  session's frozen body and source identity. It re-reads nothing, executes
  nothing, grants nothing, and is permission-ungated because it returns
  already-admitted snapshot bytes; call and result are ordinary provenance
  events. Supporting files stay governed by existing tools and permissions.

## Child agents and the guardian

`project_context: none | inherit` is recorded on `agent.spawn`, distinct from
`include_parent_canvas`; the default is `none`. `inherit` shares the parent's
exact frozen snapshot and digest (no new snapshot, no file reads, no widened
declared capabilities) and is the only path to the child receiving the catalog
or being eligible for `skill_read`. `project_context: inherit` supplies that
snapshot even when `include_parent_canvas` is false; the latter controls only
non-project parent items. Tool advertisement remains subject to the child's
ordinary tool-call budget: `skill_read` is advertised only to the root driver
or an inheriting child with a nonzero tool budget, while a zero-tool child
receives the inherited framed evidence but no definition.

Every startup instruction item, catalog item, and `skill_read` result carries
a project-context classification and snapshot digest through its event and
canvas projection. Child request assembly filters that complete class unless
`project_context` is `inherit`, even when `include_parent_canvas` is true. A
missing policy field in an event written before this field existed decodes as
`none`; an unknown value is invalid and never falls through to inheritance.
Parallel inheriting children share one immutable pre-fan-out snapshot.

Guardian tasks use `inherit`, preserving ADR 0011's same-canvas guarantee so
the reviewer can attribute permission asks to repository-authored text; the
framing rules above, the guardian's empty capability envelope, and its
deny-biased thresholds bound the poisoning risk. Guardians retain their
zero-tool budget, so they see any `skill_read` result already present in the
parent canvas but cannot invoke the tool themselves (ADR 0017 amends ADR 0011
accordingly). CodeSwarm reviewers and observers use `none`.
