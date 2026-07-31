# Session Event Contract

Euler has one canonical session event stream.

The terminal transcript, provenance records, canvas inputs, and extension observations are projections of this stream. Do not create parallel event vocabularies for UI, provenance, tools, or agents.

## Event Envelope

Every session event has:

```json
{
  "v": 1,
  "id": "ulid",
  "ts": "rfc3339",
  "session": "session-id",
  "agent": "agent-id",
  "parent": "causal-parent-event-id-or-null",
  "kind": "event.kind",
  "payload": {},
  "blobs": {}
}
```

Large payloads are stored as content-addressed blobs and referenced from `blobs`.

Event ids are globally unique within one accepted session stream. Resume
rejects a prefix containing any duplicate id before appending recovery or
continued activity. Projections that can inspect an unresumable stream must
independently refuse to treat a duplicated id as selection or request-link
authority.

## Initial Event Kinds

- `user.message`
- `assistant.message`
- `assistant.activity`
- `plan.update`
- `tool.call`
- `tool.result`
- `permission.prompt`
- `permission.decision`
- `patch.proposed`
- `patch.applied`
- `file.change`
- `file.diff`
- `workspace.restore`
- `check.started`
- `check.result`
- `model.call`
- `model.result`
- `model.reasoning`
- `model.delta` (runtime-only, never persisted; see `docs/contracts/persistence.md`)
- `model.switched`
- `model.effort.changed`
- `context.limit`
- `context.slot.updated`
- `project.context.snapshot`
- `project.context.diagnostic`
- `project.context.relocated`
- `canvas.snapshot`
- `canvas.policy.changed`
- `canvas.swap`
- `canvas.candidate.discarded`
- `secret.redacted`
- `secret.exposure.detected`
- `secret.scrubbed`
- `extension.artifact`
- `extension.contribution`
- `agent.spawn`
- `agent.message`
- `agent.result`
- `session.start`
- `session.resumed`
- `session.renamed`
- `session.summary`
- `error`

Unknown future event kinds are reader-specific. Inspection readers
(replay-for-rendering) skip unknown kinds with a warning. Resume readers
fail safe with a canonical incompatibility error naming the unknown kind,
because resume appends to the same stream and cannot prove a skipped kind is
irrelevant to live state. An unchanged envelope `v` does not imply a stream is
resumable.

## Ratified Payload Fields

Golden tests freeze these fields. Additive optional fields are allowed
without a version bump; renames/removals/semantic changes bump the
envelope `v` per `docs/contracts/persistence.md`.

- `user.message`: `content`. A turn is not limited to one: mid-turn
  steering (issue #146) appends additional `user.message` events at round
  boundaries — after a completed tool round's results or after the committed
  `assistant.message` of a no-tool round. They are always between model
  rounds, never inside a streamed assistant message. Request assembly
  positions them like any other event, and readers must not assume a turn
  has exactly one leading user message. Queue entries are removed only after
  this event is durable: both mid-turn absorption and queued-turn dispatch
  reserve by id first, and append failure leaves the entry queued. A queue-entry
  id is opaque and includes both the queue instance and its row sequence; a
  same-position, same-content row from another queue is never the same
  reservation. Admission installs the pending candidate — including its exact
  envelope id, timestamp, parent, payload, and originating queue-entry id —
  before attempting to persist any older accepted backlog. It then reconciles
  that backlog, appends the candidate, and only then publishes the candidate to
  the live bus. A failure in either append protects the same pending owner, and
  a rejected candidate is never an accepted in-memory event. Only that exact
  queue row with the same payload may retry it. Remove/edit/clear protect the
  unresolved entry, and dispatch selects it before any row inserted later,
  even when another row has identical content. Repair and retry therefore
  reconcile that event exactly once instead of persisting a failed bus copy or
  acknowledging a content-equal duplicate.
- `assistant.message`: `content`. It commits the visible content of a
  no-tool model round. Pending steering may keep that same user turn active,
  append more `user.message` events, and dispatch another model round only
  when the explicit round budget can admit that request. At the final allowed
  round, the terminal transaction closes the steering group without persisting
  queued steering; rows submitted before that close remain deferred, and rows
  submitted after it are ordinary follow-ups.
- `model.call`: `provider`, `model`, `canvas_items`,
  `requested_reasoning_effort`; optional resolved `reasoning_effort`,
  `max_output_tokens`, and `project_context_digest`. A root-driver call also
  carries `canvas_snapshot_id`, naming the exact preceding purpose-free
  `canvas.snapshot` used to build that request. It must name the latest earlier
  purpose-free snapshot for the call's exact envelope `session` and `agent`;
  stale, future, duplicated, or crossed-identity links have no authority. The
  snapshot's `selected_event_ids` are unique, their checked length exactly
  equals `counts.items`, and that count equals `model.call.canvas_items`.
  Shadow-compaction calls use `purpose:
  "compaction"` and do not carry this root-driver link; companion and reviewer
  calls use their own actors and cannot claim a root snapshot.
  Every accepted call has
  exactly one semantic terminal association: `model.result` on a drained
  finished stream, or a terminal `error`. Cancellation before a result records
  the safe error payload `source: "session"`, `message: "model call
  cancelled"`, and `cancelled: true`. Cancellation after a `model.result`
  never adds a second terminal. A model-terminal `error` is specifically a
  provider error, a session error with `cancelled: true`, or a session error with
  `recovery_closure: true`; an extension, guardian, or ordinary session error
  that merely receives a linear parent of an asynchronous call does not settle
  it.

  **Authoritative terminal association rule:** scan accepted events in order
  while tracking open `model.call` events. A terminal whose `parent` names an
  open call from the same envelope `agent` settles that call. Otherwise it may
  settle only the unique open call from the same `agent` whose recorded
  `provider`/`model` match when the terminal carries those fields and whose
  `purpose` matches exactly (including both sides omitting it). With no
  candidate, the terminal settles no call; multiple candidates make the
  history incompatible and resume fails closed. A direct terminal naming an
  already closed same-agent call is a duplicate and makes the history
  incompatible. When no call is open, a writer-linear terminal that uniquely
  matches an already closed same-agent call is likewise rejected as a
  duplicate; it is never ignored or allowed to settle later work. This
  actor/order rule is necessary because sequential companions and parallel
  reviewers use the writer-owned linear spine:
  reasoning and terminal events may durably parent a preceding reasoning event
  or another reviewer's event rather than their logical call. A crossed-agent
  linear parent is never authority.

  Resume applies this rule and closes every call left open
  with a parented `error` carrying `source: "session"` and
  `recovery_closure: true`; the message says that the call was interrupted and
  its outcome is unknown. The closure preserves an originating `purpose`
  (including `"compaction"`), but does not claim `cancelled: true`: restart
  cannot know whether the remote provider completed. All such closures are
  durable before the resume marker is armed or a new user turn is admitted.
- `plan.update`: canonical extension updates carry `source: "extension"`,
  host-derived `extension_id` and `command`, positive `revision`, overall
  `status` (`active` | `blocked` | `waiting` | `completed`), `explanation`
  (bounded string or null), a nonempty bounded `items` array of
  `{ step, status }` (`pending` | `in_progress` | `completed`), and a
   host-derived compatibility `summary`. The owning writer parents it to the
  durable tail at emission. It is transcript presentation, never direct
  canvas input. The host treats an exact normalized retry of the latest
  canonical event for the same extension as success without another event;
  comparison ignores `command` but includes revision, status, explanation,
  items, and summary. This no-op requires a settled provenance writer; an
  unresolved same-writer append remains fenced until exact reconciliation or
  lifecycle reopen. Changed content at the same revision and identical content
   from a different extension remain distinct events. Legacy summary/content-
   only events remain renderable.
- `tool.call`: `id`, `name`, `input` (structured JSON).
- `tool.result`: `id`, `name`, `ok`; `output` (+ optional `exit_code`) on
  success, `error` on failure (optional `output` and `exit_code` may
  accompany `error` when the tool produced partial output before failing).
  `output` is the complete redacted text supplied by the tool (and may be
  partial on the failure path described above). A producer that bounds the
  active display may add `output_preview_max_bytes` and
  `output_preview_max_lines`; the shared projection derives a head/tail preview
  from the redacted output and appends the result event id needed by
  `tool_result_get`. It leaves the output verbatim when the bounded form would
  not be smaller. Producers retain those limits even when the current output
  fits, so a later secret-scrub rewrite cannot bypass the projection bound.
  Large `output` strings are content-addressed in the durable log and
  rehydrated at the session boundary.
  Optional `project_context_snapshot_digest` is the candidate digest of the
  immutable project-context snapshot from which this result derived bytes.
  It classifies `skill_read` and every `tool_result_get` rehydration of a
  classified result; canvas projection preserves it and child request/tool
  execution enforce the recorded `none | inherit` policy against it. This is
  distinct from the rendered-context digest recorded as
  `model.call.project_context_digest`.
  Optional `recovery_closure: true` marks a resume-time canonical closure for
  an interrupted tail `tool.call`; it records the resume observation, not the
  original tool outcome.
  Optional `cancelled: true` marks a live cancellation closure. Every
  `tool.call` already accepted from one provider batch that has no terminal
  result receives exactly one terminal failed result in batch order, including
  calls that had not started and a call cancelled while waiting for permission.
  A running subprocess result retains collected partial output, exit code, and
  any observed file changes completed before its owned process group was
  stopped. The ordinary-shell workspace observation remains bounded by the
  frozen file-snapshot limits in the tool/UI contracts.
  Optional `grant_source` (`"session"` | `"project"`) marks a run covered by
  an existing scoped grant; optional `static_safe: true` marks a run
  auto-approved by static command-safety analysis (see
  `docs/contracts/capabilities.md`). Both are ledger provenance tags rendered
  on the tool header, not fresh decisions.
  This payload is the canonical tool-result shape; provider adapters map
  exactly this shape onto their wire formats.
  Extension-backed model-tool calls/results additionally carry host-derived
  `extension_id` and `command`. A causally descended, identically attributed
  `plan.update` lets the TUI suppress the successful generic JSON result row
  only when the originating call and result also carry the same nonempty
  provider call `id`; provenance retains the complete braid and failures or
  malformed/mismatched results remain visible.
- `permission.prompt`: `capability`, `reason`. An operation-level extension
  prompt retains that primary capability for compatibility and adds
  `capabilities` (the complete, ordered, distinct capability list),
  `operation`, `batch: true`, `extension_id`, and `command`. A batch is
  settled only when it has one child `permission.decision` for every member of
  `capabilities`; readers must not treat its first decision as a complete
  answer.
- `permission.decision`: `capability`, `mode`, `allowed`, `decision`.
  `mode` is the approval mode label (`ask` | `session-allow` |
  `always-deny`), or `static-grant` for extension registration grants, or
  `static-safe` for statically-safe shell auto-approvals
  (`docs/contracts/capabilities.md`).
  Additive optional fields for scoped grants (see
  `docs/contracts/capabilities.md`):
  - `grant_scope`: `once` | `session` | `project` when the decision allowed a
    grant (or recorded an allow under an existing grant / mode).
  - `grant_pattern`: non-empty scope pattern string when the grant is patterned;
    omitted for unscoped grants.
  - `scope`: legacy resume marker; present as `"session"` only for **unscoped**
    session grants so resume can fold capability-wide session allows. Patterned
    session grants use `grant_scope`/`grant_pattern` and do not set this field
    until resume learns patterned fold.
  - `instruction`: non-empty deny-with-guidance text when the user denied with
    instructions; omitted on bare deny and on allows.

  Operation-batch decisions add `batch: true`, `operation`, `extension_id`,
  and `command`. Each remains a separate capability decision and parents the
  shared `permission.prompt`.

  Additive optional fields for guardian-reviewed decisions (ADR 0011 /
  `docs/contracts/capabilities.md`):
  - `decision_source`: `"guardian"` when an automated guardian reviewer made
    the decision. Omitted means the configured decider (the user) decided.
  - `risk_level`: `low` | `medium` | `high` | `critical` — the guardian's
    risk assessment, present when the verdict parsed.
  - `user_authorization`: `unknown` | `low` | `medium` | `high` — the
    guardian's read of user authorization, present when the verdict parsed.
  - `rationale`: short guardian rationale for the outcome (also present on
    fail-closed denials, where it names the failure instead of a verdict).
- `extension.contribution`: `extension_id`, `command`, `point` (currently
  `"turn-idle"`), `action` (`"stop"` or `"continue"`), and `accepted`.
  An accepted continue additionally carries redacted `content`; an unaccepted
  action carries `reason` (`"user-pending"`, `"cancelled"`, or
  `"authority-unavailable"`) and no content. Missing standing authority is an
  expected idle stop, not an `error` event.
  Only an accepted continue projects into the model canvas, with core-generated
  extension framing. It remains eligible until an accepted same-agent
  root-driver `model.call` binds the exact purpose-free `canvas.snapshot` that
  selected it, then becomes provenance-only. A prepared snapshot with no
  accepted call consumes nothing. It is never reclassified as `user.message`.
- `patch.proposed` / `patch.applied`: `path`, `old`, `new`. For
  `modify`-style edits, `old` and `new` are the requested replacement or patch
  hunk text, not guaranteed whole-file before/after content. Whole-file
  identity belongs in `file.change` hashes and byte lengths. These events may
  still contain raw edit text until the patch-event redaction contract is
  revised separately.
- `file.change`: `tool_call_id`, `origin`, `action`, `path`, `old_path`,
  `before_sha256`, `after_sha256`, `before_byte_len`, `after_byte_len`,
  `diff_redaction`; optional `pre_image_blob` (sha256 hex) when a workspace
  checkpoint pre-image was stored for this edit. This event is metadata-only:
  `origin` is descriptive edit metadata with known values `edit_file`,
  `apply_patch`, `run_shell:apply_patch`, and `run_shell`; `action` is `add`,
  `modify`, or `delete`, `old_path` is null, and `diff_redaction` is `omitted`.
  `run_shell:apply_patch` means Euler intercepted a strict apply-patch heredoc
  before shell execution; it does not mean a shell process ran. `run_shell`
  means Euler observed a bounded net filesystem change around an ordinary shell
  process under the workspace root. For delete, `after_sha256` is null and
  `after_byte_len` is `0`.
  No raw file content, before/after content, or unified diff bytes belong in
  this payload. This is only a `file.change` payload rule.
  When present, `pre_image_blob` is a content-addressed hash of the pre-edit
  file body stored under the **workspace-scoped** checkpoint dir
  (`.euler/checkpoints/<sha256>`), not the session provenance blob store.
  Pre-images are never stored for secret-like paths/content, binary content, or
  oversize files (aligned with `file.diff` omission policy); when skipped the
  field is omitted and the transcript shows no checkpoint suffix. v0 stores
  pre-images for safe single-file `edit_file` / `apply_patch` **modify** only;
  adds, deletes, multi-file shell observations, and external disk drift are out
  of scope.
- `workspace.restore`: `path`, `checkpoint_event_id`, `blob_sha256`,
  `restored` (always `true` on success). Appended when the user restores a
  workspace file via `/rollback` to the pre-image of a prior `file.change`.
  The transcript is never rewritten: restore is new provenance; the dead-end
  history stays queryable. Rendered as
  `↩ reverted <path> → ckpt <checkpoint_event_id> · files restored, history intact`.
- `file.diff`: `tool_call_id`, `file_change_id`, `path`, `old_path`,
  `action`, `origin`, `diff`, `truncated`, `truncation`, `omitted_reason`;
  optional `before_sha256`, `after_sha256`, `before_byte_len`,
  `after_byte_len`, `line_count`.
  This is the canonical user-visible code-change artifact for safe edit paths.
  It is emitted for `edit_file`, `apply_patch`, strict intercepted
  `run_shell:apply_patch`, and bounded ordinary `run_shell` workspace
  observations. Emitted actions are `add`, `modify`, and `delete`; `rename`
  remains reserved event vocabulary. Ordinary shell observations do not parse
  shell command strings and do not claim arbitrary writes outside the workspace
  root. They compare bounded pre/post snapshots of regular workspace files,
  skip symlinks and common build/dependency/cache/local-state directories such
  as `.git`, `.euler`, and `target`, and emit no shell file-change events if
  either snapshot is incomplete. Large or binary content can still produce
  metadata-only file events with `diff=null`. Deletes never include deleted
  content and use `omitted_reason="delete-content"`.
  `diff` is a bounded unified diff when safety checks pass and is null when
  omitted; `truncation` is `none` or `tail`. Generated diffs use zero context
  lines. `file.diff` may contain raw code diff text and is for transcript /
  provenance display, not model-canvas input. Large generated diffs are bounded
  including the truncation marker and must set `truncated=true` with a non-null
  `omitted_reason`.
- `model.call`: `provider`, `model`, `canvas_items`,
  `requested_reasoning_effort`, optional `reasoning_effort`. Root-agent calls
  additionally carry `system_instructions_version`,
  `system_instructions_sha256`, and `system_instructions_bytes`; those fields
  name the exact fixed root instructions used for that call. The full text is
  also present when this is the first occurrence of that instruction identity
  in the stream. They are request audit metadata and are not model-canvas
  content. Root-driver calls additionally carry the exact
  `canvas_snapshot_id`; root-agent shadow calls do not. Optional
  `project_context_digest` (ADR 0017) is the versioned rendered-context
  digest, recorded only when those exact core-framed bytes occur in the
  provider-neutral request being dispatched (no TOCTOU between snapshot and
  prompt assembly); absent whenever the request carries no project context.
  A shadow projection request adds `purpose: "compaction"`,
  `tools_enabled: false`, and `shadow_snapshot_end_id`. It is canonical
  provenance and cost-bearing model activity, but is excluded from the driver
  transcript/canvas and active-context usage reading.
- `model.effort.changed`: `from_effort`, `to_effort`, `reason`.
  (provider-scoped string, emitted and stored verbatim — core does not
  normalize; examples non-exhaustive: `"low"` | `"medium"` | `"high"`, with
  some providers extending to `"extra-low"` | `"extra-high"` or numeric
  knobs; omitted when the provider has no reasoning-effort concept for the
  target model; persisted, see ADR 0008).
- `model.result`: `provider`, `model`, `content`, `tool_calls`,
  `stop_reason`, `usage` (object: `input_tokens`, `output_tokens`,
  optional `uncached_input_tokens`, `cached_tokens`,
  `cache_write_5m_tokens`, `cache_write_1h_tokens`, `reasoning_tokens`).
  `input_tokens` is the total request input; when the four input buckets are
  present they are disjoint and their checked sum equals that total. An adapter
  leaves all four buckets absent when the provider reports only an aggregate
  cache-write count whose TTL cannot be established; it must not assign that
  count to a cheaper bucket. Optional
  `purpose: "compaction"` matches the originating shadow `model.call`; its
  usage contributes to session cost but never replaces the driver canvas's
  active-context reading. Optional
  `cost` is a V1 persisted quote with `schema_version: 1`, `currency: "USD"`,
  `unit: "picodollar"`, exact integer `input_picos`, `output_picos`,
  `cache_read_picos`, `cache_write_5m_picos`, `cache_write_1h_picos`, and
  `total_picos`, plus `pricing` provenance (`provider`, `model`, `source`,
  `source_id`, the selected pico-dollar-per-token rates, and the optional tier
  threshold). `source` is `official` for a release-backed catalog or `local`
  for a user-owned schedule; `source_id` is respectively the catalog release
  id or a SHA-256 identity of the exact schedule. The component breakdown is
  authoritative and must sum exactly to `total_picos`; selected rates are
  audit evidence used to validate that saved arithmetic, not instructions to
  price against a live catalog. An absent or invalid `cost` means
  unpriced, while a present all-zero breakdown means known zero. Replay never
  prices an old event from the current catalog. `tool_calls` (each:
  `id`, `name`, `input`) is a denormalized record of what the provider
  returned; the canonical execution truth is the subsequent `tool.call` /
  `tool.result` events, and replay request-building reads those, never
  `model.result.tool_calls`.
- `model.reasoning`: `provider`, `model`, `fidelity`
  (`raw` | `summary` | `opaque`), `content` (empty for opaque),
  optional provider-opaque `artifact` (signature/encrypted item,
  blob-externalized when large), and optional `purpose: "compaction"` when
  parented to a shadow projection call.
- `model.delta`: `kind` (`text` | `reasoning`), `delta`. Runtime-only.
- `model.switched`: `from_provider`, `from_model`, `to_provider`,
  `to_model`, `reason`. Provider fields are stable provider ids; model
  fields are provider-scoped model ids. `reason` is a short non-secret
  label such as `user`, `config`, or `resume`; free-form explanatory text
  belongs in transcript/UI surfaces, not this payload. The event records
  an accepted between-turn next-call switch only. No event is emitted for a
  no-op same-target request or a failed/rejected switch. Same-target
  comparison uses exact canonical `(provider id, provider-scoped model id)`
  strings after caller/provider-selection parsing; aliases are out of
  scope.
- `context.limit`: `provider`, `model`, `used_tokens`, `limit_tokens`,
  `threshold`. A local guardrail, distinct from the provider `max_tokens`
  stop reason: evaluated at the turn boundary, after a `model.result` and
  before what would be the next `model.call`. `used_tokens` comes from the
  latest `model.result.usage`; `limit_tokens` is the model's context
  window from provider/model configuration; `threshold` is the configured
  fraction of `limit_tokens` that triggers the stop. Emitted once; the
  session then stops cleanly. Automatic compaction is attempted before this
  stop; an already-running shadow candidate is awaited at the hard margin,
  and only a failed, invalid, or unavailable candidate falls through to
  `context.limit`. If a provider stops with `max_tokens` mid-call, that is recorded in
  `model.result.stop_reason`; `context.limit` may still follow at the
  boundary.
- `context.slot.updated`: `extension_id`, `slot`, `content`. Records a
  host-mediated extension context slot update. `extension_id` is assigned by the
  host from the calling extension, `slot` uses the event-feed checkpoint name
  grammar, and `content` is UTF-8 text capped at 4096 bytes. Control characters
  other than newline, Unicode `Cf`, and `Zl`/`Zp` are rejected. Empty
  `content` deletes the slot. Slot payloads remain inline. Durable state is
  retained while the owner is disabled, but live snapshots project it only
  while that extension id is enabled.
- `project.context.relocated` (schema version 1; ADR 0017,
  `docs/contracts/project-context.md`, issue #180 phase 3): records an
  accepted resume relocation and carries:
  - `schema_version`: integer.
  - `prior_identity`: `{ "algorithm": <string>, "version": <int>,
    "digest": <hex string> }`, the workspace identity folded at the accepted
    event prefix.
  - `new_identity`: same shape, the identity of the live canonical root at
    decision time, computed as a fresh snapshot computes it.
  - `new_root`: normalized lossy path string in the same bounded display form
    as `session.start` `root`; display and projection metadata only, never
    identity authority (identity comparison uses `new_identity`).
  - `decided_at`: audit wall-clock stamp of acceptance; audit metadata only,
    never ordering or authority (append position and parentage are the causal
    facts).
  Parentage: it parents the accepted tail event the resume folded to (the same
  event a first continued turn attaches to) and is an in-chain durable event,
  not a log-leaf, so the continued turn and the emitted `session.resumed`
  attach to it. Validation (any breach fails closed and rejects the resume):
  `prior_identity` MUST equal the identity folded at the accepted prefix;
  `new_identity` MUST equal the live canonical root's identity at decision time;
  `new_root` MUST re-derive to `new_identity` under the identity algorithm; a
  relocation whose `prior_identity` does not match the current governing
  identity (stale or branched acceptance) is rejected and never supersedes; and
  the event MUST persist durably before any resumed activity proceeds.
  Projection: the latest `project.context.relocated` `new_root` in durable
  sequence governs the session's projected root everywhere the first
  `session.start` `root` is used (listing and picker, current-directory
  grouping, resume checks, and the recorded path a later relocation card
  renders), and its `new_identity` governs resume comparison.
- `session.start`: `provider`, `model`, optional `root`. `root` is only a
  filesystem path string derived from `SessionConfig.root`; it is not an
  arbitrary JSON object or workflow identity token. To emit or compare it,
  Euler applies one normalization policy: if the configured path is relative,
  join it to the process current directory when available; then try
  `std::fs::canonicalize`; if that succeeds, use the resolved path, otherwise
  use the absolute fallback; finally serialize with `Path::to_string_lossy`
  because the event stream is JSON. Root matching compares this normalized
  string form. Non-UTF-8 paths may collapse through lossy conversion; that is
  accepted for local discovery metadata, not for security identity.
  Older streams may omit `root`; omission means unknown, not the reader's
  current directory. The first readable `session.start` is the root authority;
  later duplicate `session.start` events, if present in malformed histories,
  do not update root projection. `session.json.root` is an advisory transition
  fallback only when the event stream is readable and the first `session.start`
  has no usable `root`; if the event stream is unreadable or corrupt, projected
  root is unknown even if the sidecar contains a root. `root` is local discovery
  metadata for grouping and current-directory prioritization, not resume
  authority and not model-canvas content. It is stored in cleartext, is not
  redacted or hashed in v0, and can contain user-identifying path components.
  Current streams also record the fixed root-agent contract as
  `system_instructions`, `system_instructions_version`,
  `system_instructions_sha256`, and `system_instructions_bytes`. The complete
  text is recorded when an instruction identity first appears so a session
  remains reconstructable; each later root `model.call` repeats only its
  version, digest, and byte length. A resumed session records the full text on
  the first call after a binary update changes that identity. Older streams may
  omit these fields. They are provenance metadata, not canvas content or resume
  authority.
  Optional `session_kind` is `interactive` or `non-interactive`. It records
  how the session was launched for discovery/resume UI grouping only. Omitted
  means unknown/legacy and must not affect resume authority or canvas content.
  Optional `permission_reviewer` is `user` or `guardian` (ADR 0011),
  recording which reviewer the session was configured with at start. Omitted
  in older streams means `user`. It is config projection for visibility, not
  resume authority; per-decision truth is `permission.decision`
  `decision_source`.
  Optional `context_limit` is either `null` (unknown/legacy window) or an
  object `{ "limit_tokens": <u64>, "source": "catalog" }` recording the
  catalog-derived context window used for token-threshold compaction and
  hard-stop checks. It is telemetry and config projection only; resume
  authority remains the event stream and active model target. Omitted in older
  streams means unknown.
  Optional `auto_compaction` is an object `{ "automatic": <bool>,
  "stubs": <bool>, "tier": "off"|"stubs", "budget_bytes": <usize> }`.
  `automatic` controls threshold-driven compaction and `stubs` controls
  recoverable tool-result demotion. Both default to `true` in new sessions;
  older streams without the object use the launching configuration. The
  legacy `tier` field remains for compatibility and is normalized at resume.
  Optional `project_context` is the compact bootstrap summary (ADR 0017):
  `{ "expected": true, "schema_version": 2, "status", "policy",
  "resolution_reason", "acknowledgment_basis", "candidate_digest",
  "manifest_admitted", "source_count", "skill_count",
  "diagnostic_count" }`. Version-1 summaries (no `manifest_admitted` or
  `skill_count`) remain resumable; legacy manifests cannot contain skills.
  Present exactly when the session was created with a project-context
  bootstrap; it announces that one `project.context.snapshot` follows
  immediately. Absent means the legacy
  shape: no snapshot events exist and resume treats project context as
  disabled. A summary without its snapshot (or vice versa) is an invalid
  mixed shape and resume fails closed. The summary is validated like the
  snapshot (key whitelist, grammar) and every overlapping field must agree
  exactly with the snapshot it announces; any mismatch fails resume.
  Current streams additionally carry `runtime`, an object with exact keys:
  `schema_version` (currently `1`), `binary_name`, `package_version`, nullable
  `git_sha` and `git_dirty` represented as an all-known or all-null pair,
  sorted unique `build_features`, `session_start_projection_sha256`,
  `provider_client_version`, and nonempty `attached_roots`. The projection
  digest is SHA-256 over the complete `session.start` payload before the
  `runtime` object is inserted. It commits only to that recorded projection,
  not every behavior-affecting field of the live `SessionConfig`; the exact
  committed bytes remain beside their identity without serializing live
  config files or secret-bearing provider settings. Git identity is captured at
  build time; `git_dirty` describes tracked source/index differences from
  `HEAD`. If the build environment has no trustworthy Git checkout, both Git
  fields are null rather than guessed. The current authority model attaches
  exactly the normalized `root` above; additional roots require the explicit
  multi-root authority contract. A missing `runtime` object means unknown
  legacy identity, never the currently running build. Report/export
  projections serialize these states explicitly as `recorded` or
  `legacy_unknown`; they never substitute the reader's build. A present
  malformed or unsupported object is incompatible rather than silently
  treated as legacy. More than one `session.start` is likewise invalid; a
  report or resume must not select one of several claimed identities.
- `session.resumed`: `provider`, `model`, `events_folded`, optional
  `resumed_from_event_id`. A durable audit marker recording that the session
  lifetime was continued, against which target and from which tail event.
  Audit metadata only — never user or model content. Emitted with the first
  durable activity of a resumed session (an open-and-inspect resume that never
  mutates or continues emits none). It is a LOG-LEAF: appended to the log but
  not the in-memory bus, so it never becomes the parent of continued activity.
- `session.renamed`: `name`. Records the latest user-visible session name;
  sidecars and indexes are projections of this event, not naming authority.
  For sessions created by current new-Euler builds before this event existed,
  a valid `session.json.name` may be used only as a display fallback when the
  event stream is readable and contains no `session.renamed`; the next rename
  writes this canonical event and refreshes the sidecar projection.
  Projection caching: `session.json` may additionally carry the cached
  event-log projection (status/name/title/root/kind) under
  `projected_events`, keyed by `accepted_byte_len` and `tail_event_id` for the
  same verified accepted JSONL prefix. The accepted length stops at the final
  newline, excluding a torn final fragment under the persistence contract;
  an empty prefix records a null tail. Each cache-key observation reads only
  the final 1 MiB window, covering any torn fragment, trailing blank lines,
  and the last accepted event; lookup repeats the observation on a fresh file
  handle and rejects disagreement. If that window cannot contain the complete
  accepted tail event, the fast path declines the cache key and performs the
  authoritative full projection. A valid large-tail session remains valid; it
  merely receives no cache hit. No cache-key line allocation exceeds the
  window.
  An append invalidates the old accepted-length/tail key; it need not
  synchronously rewrite the sidecar, and the stale key cannot be a cache hit.
  Mismatch, truncation, an unreadable tail, a legacy `(length, mtime)` key, or
  a missing key forces the complete event/blob projection and an atomic
  sidecar replacement. A turn-boundary metadata touch performs the same key
  comparison before carrying projection fields forward. While the durable key
  matches, listings serve the cached projection verbatim instead of
  re-deriving it — the events remain the sole naming authority, enforced at
  projection time rather than on every read. Same-length hostile rewrites
  preserving the tail id remain inside the session-directory trust boundary.
  Integrity failures (`invalid` status) never receive a projection key, so
  they are re-checked on every listing. Sidecars and indexes remain
  rebuildable caches and never become session authority.
- `project.context.snapshot` (current schema version 2; ADR 0017,
  `docs/contracts/project-context.md`): `schema_version`, `status`
  (`admitted` | `disabled` | `declined` | `unacknowledged`, each gated by the
  permitted policy-tuple table in the project-context contract),
  `policy`, `resolution_reason`, `acknowledgment_basis`, `candidate_digest`
  (versioned, domain-separated, length-prefixed digest of the canonical
  candidate manifest), `workspace_identity`
  (`{ "algorithm": "unix-raw-osstr", "version": 1, "digest" }` over the raw
  canonicalized workspace-root bytes), `ordering` (`lexicographic-v1`),
  `source_identities` (bounded normalized project-root-relative paths),
  `manifest_admitted`, `skill_count`, `diagnostic_count`, and
  `diagnostic_reason_counts`. A snapshot with `manifest_admitted: true`
  additionally carries `framing_version`, `manifest_len`, and `manifest` —
  the complete canonical UTF-8 manifest JSON as one top-level payload string,
  externalized as one content-addressed blob above the provenance threshold.
  Repository-disabled snapshots may retain a manifest containing only
  user-global skills; they persist no repository source body, project-skill
  body, per-source content hash, exact content length, or parser excerpt.
  Version-1 snapshots omit `manifest_admitted` and `skill_count`; their
  admitted status alone controls manifest presence and their legacy manifests
  cannot contain skills. Each schema has an exact key whitelist: v1 records
  carrying v2-only fields, partial v2 records, and mixed-version bootstraps
  reject rather than being normalized. The durable bootstrap order is exactly
  `session.start`, one snapshot, then the declared diagnostics, all persisted
  before any provider dispatch; the latest snapshot in durable sequence is
  authoritative, and a snapshot without an admitted manifest is a tombstone.
  Rehydration verifies
  the blob address and length and rejects invalid UTF-8, duplicate keys,
  trailing data, unsupported versions, limit violations, and digest
  mismatches; it never falls back to current project files. Both shapes are
  fully re-validated on fold as untrusted input: unknown payload fields,
  malformed digests, non-normalized identities, unknown workspace-identity
  algorithms, count inconsistencies, and status/policy/reason/basis
  combinations outside the contract's permitted-tuple table reject resume
  and request assembly.
- `project.context.diagnostic` (current schema version 2): `schema_version`,
  `snapshot_event_id`, `reason` (stable content-free code), optional bounded
  `path` (normalized relative identity), optional numeric `observed`. Never
  carries excerpts, raw parser errors, outside-workspace paths, or exception
  strings derived from a candidate. Its schema version must exactly match its
  owning snapshot; v1 diagnostics remain valid only in a v1 bootstrap.
- `canvas.snapshot`: `selected_event_ids`, `counts`, retention telemetry
  `retained_items`, `retained_bytes`, `demoted_items`, `automatic`, `stubs`,
  `tier`, `budget_bytes`,
  `over_budget`, and `pressure` (`none`|`byte`|`token`|`both`). Optional
  `used_tokens` and `limit_tokens` are included when provider usage and a
  configured context limit are known. Snapshot fields are assembly telemetry
  for the next model request; they do not rewrite provenance history or consume
  one-shot input by themselves. An accepted root-driver `model.call` binds the
  exact request snapshot through `canvas_snapshot_id` only under the unique,
  latest, same-session/agent accounting rule above.
  A fixed shadow-compaction snapshot adds `purpose: "compaction"` and
  `shadow_snapshot_end_id`.
- `canvas.policy.changed`: `automatic`, `stubs`, and `budget_bytes`. It records
  a user/configuration change to the two live retention switches. The event is
  session-level control metadata; it does not change or delete provenance.
- `canvas.swap`: `snapshot_start_id`, `snapshot_end_id`,
  `frontier_start_id`, `policy_version`, `projection_schema_version`,
  `projection_blob`, `validation_result`. It records a compacted canvas
  projection: `snapshot_start_id` is the first event in the compacted range,
  `snapshot_end_id` is the last event in that range, `frontier_start_id` is
  the first event kept verbatim after the projection, policy/schema versions
  name the compaction and projection formats, `projection_blob` carries the
  projection text or hash reference, and `validation_result` is `pass` or a
  short validation outcome. Layer-1 swaps add
  `layer1_compacted_event_ids`; those IDs accumulate after the latest full
  projection. A model-produced full projection adds `summary_source: "model"`,
  `compactor_provider`, `compactor_model`, and `compaction_elapsed_ms`.
  Only a structurally valid swap invalidates the preceding provider-usage
  sample and context-limit latch. Live canvas assembly, live accounting, and
  resume folding share one validator; malformed swaps remain inert provenance.
- `canvas.candidate.discarded`: `reason`, `policy_version`. It records a
  rejected shadow compaction candidate at the turn boundary; `reason` is a
  short non-secret validation failure and `policy_version` names the
  compaction policy that produced the candidate.
- `error`: `source`, `message`, optional `category` (`auth` |
  `transport` | `rate_limit` | `rejected` | `stream_truncation` |
  `internal`) carrying the provider error taxonomy from
  `docs/contracts/provider.md` when the source is a provider. When
  a shadow projection request fails, `purpose: "compaction"` attributes the
  error to that request; it remains provenance-only while the TUI reports the
  compact failure without replacing the driver transcript or driver-failure
  HUD. Session-owned cancellation uses `source: "session"` with the same
  purpose and terminally closes the shadow `model.call`. A resume-time
  `source: "session"` error may instead carry `recovery_closure: true`; it
  terminally records the unknown outcome of an interrupted model call and
  preserves the call's optional `purpose` without asserting `cancelled`. When
  `source` is `extension`, optional `extension_id`, `command`, and
  `failure` (`command_error` | `panic`) fields attribute the host-observed
  failure. Extension error messages in persisted events are host-generated
  summaries, not raw extension error text or panic payloads.
- `extension.artifact`: `extension_id`, `display_name`, `media_type`, `path`,
  `sha256`, `byte_len`, `source_event_ids`, `metadata`. The artifact bytes are
  stored outside the event payload under the session-scoped extension artifact
  directory; this event records only compact metadata and the relative artifact
  path. `path` is host-derived, not extension-provided.
- `agent.spawn`: `child_agent_id`, `task`, `persona`, `provider`, `model`,
  `capabilities`, `budget`, optional `result_schema`. The event is authored by
  the parent session's envelope `agent`; the child id is payload identity in
  v0. `capabilities` are canonical capability strings using exact set
  semantics from `docs/contracts/capabilities.md`. `budget` is bounded metadata
  in v0, not an escrow or accounting record.
  `project_context` is `none` or `inherit` (ADR 0017): whether the child
  request assembly receives the parent's frozen project-context snapshot.
  Missing (events written before the field existed) decodes as `none`; an
  unknown value is invalid and never falls through to inheritance.
- `agent.message`: `from_agent_id`, `to_agent_id`, `spawn_event_id`,
  `queued_ts`, `payload`. This is a parent-drained child-to-parent report from
  a live current-process background child, not transcript content and not a
  durable mailbox. The event is authored by the parent session's envelope
  `agent`; child code supplies only the bounded JSON-object `payload`. Core
  derives `from_agent_id`, `to_agent_id`, and `spawn_event_id` from the live
  background handle. `queued_ts` is core-assigned when the report is accepted
  into volatile runtime memory and is informational; it is not a causal clock
  and is not guaranteed monotonic or less than the envelope `ts`.
- `agent.result`: `child_agent_id`, `spawn_event_id`, `ok`, `summary`,
  optional `output`, optional `error`. The event is authored by the parent
  session's envelope `agent` and parents the matching `agent.spawn` event.
  `ok=true` permits `output` and forbids `error`; `ok=false` permits `error`
  and optional bounded `output`.
- `secret.exposure.detected`: `event` (id of the exposing event), `field`,
  `shapes` (array of non-secret shape labels, e.g. `sk-ant-` or `known-value`),
  `count`. A read-only marker that a credential shape was detected in a faithful
  tool-call argument (see `docs/contracts/secrets.md`). Never carries the value:
  the exposing event stays verbatim; this only records that a scrub is offered.
- `secret.scrubbed`: `requested_values` (count of distinct values requested),
  `replacements` (total occurrences), `surfaces` (`events`, `blobs`,
  `checkpoints`, `extension_artifacts`, and `extension_state_files` counts;
  `sidecar` boolean), `note`. Audit-only record of a user-initiated scrub
  across every session-owned persistent surface. Never carries the value. The
  count-only audit is committed last, after all surfaces are scrubbed, so it is
  a truthful all-surface record.

## Parentage Rules

`parent` is the causal parent, not merely the previous event:

- `tool.result` parents its `tool.call`.
- `permission.decision` parents its `permission.prompt` (or the
  `tool.call` when no prompt was emitted).
- `patch.proposed` parents its `tool.call`; `patch.applied` parents its
  `patch.proposed`.
- Structured `file.change` parents the `patch.applied` event that records the
  edit it summarizes. Bounded ordinary `run_shell` file observations parent the
  originating `tool.call`, because there is no canonical patch event for that
  shell process. The final `tool.result` still parents the original
  `tool.call`, not the `file.change`.
- `file.diff` parents the same event as the matching `file.change`. It is a
  sibling display projection, not the parent of `tool.result`. Its
  `file_change_id` references the matching `file.change`.
- Root-driver `model.result`, `model.reasoning`, and runtime-only
  `model.delta` directly parent their logical `model.call`. Sequential
  companion and parallel-reviewer persisted events instead follow the
  writer-owned linear spine and may parent preceding reasoning or another
  reviewer's event. Model terminal identity is governed only by the
  authoritative association rule in the `model.call` schema above.
- `assistant.message` parents its `model.result`.
- `model.switched`, `model.effort.changed`, `context.limit`,
  `context.slot.updated`, `canvas.policy.changed`, `canvas.swap`, and
  `canvas.candidate.discarded`
  parent the previous persisted event (they are session-level control events).
  When a model switch requires an automatic effort downgrade, the
  `model.effort.changed` event parents that `model.switched` event and both are
  accepted in one durable batch.
- `session.start` has parent null. It is always the session's first
  persisted event.
- `project.context.snapshot` parents `session.start`;
  `project.context.diagnostic` parents its snapshot and also cites it in
  `snapshot_event_id`. The bootstrap sequence is contiguous:
  `session.start`, one snapshot, then exactly the snapshot's declared number
  of diagnostics, before any other persisted event.
- `session.resumed` parents the accepted tail it continued from (the same
  event the first continued turn parents off). It is a sibling LEAF of that
  continuation, never its parent — so a resumed lifetime's causal chain is
  identical to an uninterrupted run.
- `project.context.relocated` parents the accepted tail event the resume folded
  to. Unlike `session.resumed` it is an in-chain durable event, not a leaf: it
  must persist before any resumed activity, and it becomes the frontier the
  first continued turn and the emitted `session.resumed` attach to.
- `extension.artifact` parents the previous persisted event at append time.
  Source attribution belongs in `source_event_ids`; those ids do not choose the
  artifact event's parent.
- `secret.exposure.detected` parents the exposing event it flags (the
  `tool.call` whose argument held a credential shape).
- `secret.scrubbed` parents the log tail at scrub time (a session-level audit
  event). It is appended once all surfaces are scrubbed.
- `agent.spawn` parents the current parent event in the spawning session,
  excluding runtime-only `model.delta` events.
- `agent.message` parents the previous persisted event at parent drain time.
  Reports accepted before a child result may be drained after `agent.result`;
  consumers must not infer child liveness or production chronology from this
  ordering.
- `agent.result` parents its matching `agent.spawn` event. V0 has no child
  session event stream to join.
- A root-driver provider/cancellation `error` directly parents its
  `model.call`. Companion and parallel-reviewer errors follow the writer-owned
  linear spine; the `model.call` association rule above determines whether
  they terminalize a call. Other errors parent the previous persisted event
  unless a closed semantic-parent exception applies.
- Events with no specific causal parent (e.g. `user.message`) parent the
  previous persisted event in the session, or null at session start.
- A persisted event must never parent a runtime-only event (e.g.
  `model.delta`); the persisted stream's DAG must be closed under the
  persisted stream.

Cardinality and ordering invariants:

- exactly one `session.start` per session, always the first persisted
  event;
- exactly one semantically associated terminal `model.result` or `error` per
  `model.call`, under the authoritative actor/order association rule above;
  resume rejects a second semantic terminal instead of normalizing it away;
- zero or more `model.reasoning` events per `model.call`, emitted in
  provider order before its terminal event;
- `assistant.message` is emitted after its `model.result`, and only for
  model rounds that finish without tool calls. It does not by itself prove
  that the user turn ended: pending steering or an accepted same-turn idle
  continuation can continue the turn. The driver closes the steering group
  only at its explicit terminal transaction after all such continuations
  return Stop.
- an accepted `model.switched` is emitted after the previous turn's final
  persisted event and before the next `user.message` is accepted. A switch
  after a new `user.message` starts the next turn is rejected. The next
  accepted `user.message`/`model.call` sequence uses the switch target,
  and its `model.call` must carry the target `provider` and `model`. A
  switch event must never be interleaved inside a provider stream,
  tool-execution round, or already-started user turn.
- zero or more `canvas.swap` events may appear per session; each marks a
  compaction boundary and is replay-critical for reconstructing which canvas
  range was active. The latest valid full projection owns the compacted prefix;
  subsequent layer-1 swaps accumulate over its retained frontier until another
  full projection supersedes it.
- zero or more `agent.message` events may appear for a live background spawn
  while its `BackgroundAgent` handle exists. Queue acceptance is volatile; only
  drained `agent.message` events are durable and queryable after resume.

Ratification note: M1 wrote `parent` as "previous event". This ratification
changes that meaning within envelope `v: 1`: payload fields and parentage
are frozen by golden tests from this point forward, and pre-ratification
M1-era log files are development artifacts, not supported replay inputs.
From now on, semantic changes to ratified fields follow the versioning
rules in `docs/contracts/persistence.md`. The causal DAG extension depends
on honest parents.

## Projection Rules

- The terminal UI renders a bounded, readable transcript from session events.
- Provenance stores the append-only event stream plus blob references.
- Canvas assembly selects and summarizes relevant events; it does not replay raw provenance by default.
- `file.change` is excluded from model-canvas projection in v0. Future canvas
  policy may add a bounded derived summary, but raw file-change payloads are
  not prompt content.
- `file.diff` is excluded from model-canvas projection in v0.
- `agent.message` is excluded from transcript/model-canvas projection in v0.
- Extensions observe events through the SDK, subject to capabilities and result bounds.

## Reasoning

Model reasoning is a first-class session event kind: `model.reasoning`.
Euler is a research agent; reasoning chains are part of the reproducibility
record, not disposable scaffolding.

Rules:

- Reasoning is captured at the maximum fidelity the provider exposes: raw
  thinking blocks, signed/encrypted reasoning items, or summaries. The
  payload records which fidelity was captured.
- Providers that expose nothing produce no `model.reasoning` events; core
  must not require reasoning tokens from providers.
- Provider-opaque reasoning artifacts (signatures, encrypted items) are
  preserved verbatim in the payload/blobs so the owning provider adapter can
  replay them per provider rules.
- **Storage ≠ display ≠ canvas.** Provenance may retain maximum fidelity.
  Core UI renders only adapter-classified user-displayable, taint-safe
  content (see `docs/contracts/ui.md` and ADR 0007). Opaque/encrypted
  artifacts are never rendered as transcript prose by core. Canvas inclusion
  is separate (ADR 0002 / `canvas.md`).
- Reasoning events are taint-checked like all other events: resolved secrets
  never appear in them.
