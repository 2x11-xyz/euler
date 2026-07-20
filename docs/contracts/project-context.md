# Project Context Contract (binding shape)

ADR 0017 governs project context; this contract binds its concrete shape.
Implementation status: NOTHING in this contract is implemented. Every section
binds the eventual shape, not present behavior; issue #180 tracks the slices.
As each slice lands, its statements convert from bound shape to implemented
truth, and this line shrinks accordingly.

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
- Containment: sources open relative to held root handles with
  no-follow/beneath semantics; symlinks, reparse points, and non-regular
  files are rejected; a platform that cannot no-follow-read omits the source.
  Unstable reads retry once, then omit with a `changed_during_read`
  diagnostic. Malformed, unsafe, or over-limit sources are omitted whole with
  typed diagnostics, never truncated.

Bounds (frozen by this contract; changing one is a contract change):

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

Selection and diagnostics are deterministic: lexicographic ordering of
normalized relative paths, no dependence on filesystem iteration order,
duplicate detection before catalog admission, and more-specific sources win
admission priority when the aggregate instruction budget forces a choice.

## Acknowledgment record

Admission under `--project-context auto` requires a recorded project
acknowledgment: user-owned, stored under the user Euler home outside
repository control, keyed to the canonical workspace root and the portable
snapshot digest. Two-party rule, exactly as project grants: the repository
supplies content, the user-side record supplies the decision, and neither
alone admits anything. A changed digest requires a fresh acknowledgment at
the next fresh session; unchanged content never re-prompts. Declining is
recorded and disables admission for that session. Headless runs never
prompt: without a matching acknowledgment they run with project context
disabled and say so in the startup summary. `exec --auto-approve
trusted-local` disables admission regardless. Explicit `--project-context on`
is a per-session override recorded in provenance; explicit `off` disables
without touching stored acknowledgments. Acknowledgment admits guidance into
model context and nothing else; it is never capability approval. Any future
general project-trust surface must unify this store with the extension
project tier's install consent (extension SDK contract) rather than adding a
third per-project trust store.

## Snapshot, events, and replay

- One immutable snapshot per fresh session, assembled before the first model
  request. Mid-session filesystem edits change nothing; there is no implicit
  reload. A future explicit reload appends a new snapshot event and never
  rewrites the old one.
- `project.context.snapshot` (durable, versioned) carries at least: load
  policy and resolution reason, acknowledgment basis, accepted `EULER.md`
  sources (relative path, effective byte length, SHA-256 digest, effective
  content), accepted skills (name, description, relative path, body length,
  body digest, frozen body), deterministic ordering, and schema version.
  Large content goes through the provenance blob store. Digests cover
  effective redacted bytes (the same known-value and token-shape redaction
  boundary as tool results), never a raw pre-image.
- `project.context.diagnostic` events record omissions without embedding
  unsafe content. `session.start` records a compact policy/count/digest
  summary. Every root-driver `model.call` records the digest of the exact
  rendered project context in that request (no TOCTOU between snapshot and
  prompt assembly).
- Snapshot digests are portable: canonical project-relative identities and
  effective contents, not absolute paths.
- Resume folds the snapshot from the accepted event prefix and performs no
  filesystem discovery; legacy sessions resume with project context disabled;
  resume verifies the canonical live workspace root against the recorded one
  and fails mismatches with remediation. Older Euler versions fail safe on
  the unknown event kind and cannot resume such sessions.
- Independent sessions get independent snapshots; there is no process-global
  or workspace-global mutable project-context cache.

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

Pinned project context counts against the context budget and does not
silently vanish under compaction; if the pinned context plus a minimum
request cannot fit, the session fails before provider invocation with an
honest budget error.

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
capabilities) and is the only path to the child receiving the catalog and
`skill_read`. Even with `include_parent_canvas: true`, project-context canvas
items are filtered unless `project_context` is `inherit`. Parallel inheriting
children share one immutable pre-fan-out snapshot.

Guardian tasks use `inherit`, preserving ADR 0011's same-canvas guarantee so
the reviewer can attribute permission asks to repository-authored text; the
framing rules above, the guardian's empty capability envelope, and its
deny-biased thresholds bound the poisoning risk (ADR 0017 amends ADR 0011
accordingly). CodeSwarm reviewers and observers use `none`.
