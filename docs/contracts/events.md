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
- `canvas.snapshot`
- `canvas.policy.changed`
- `canvas.swap`
- `canvas.candidate.discarded`
- `secret.redacted`
- `secret.exposure.detected`
- `secret.scrubbed`
- `extension.artifact`
- `agent.spawn`
- `agent.message`
- `agent.result`
- `session.start`
- `session.resumed`
- `session.renamed`
- `session.summary`
- `project.context.snapshot`
- `project.context.diagnostic`
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
  boundaries — after a completed round's tool results, always between
  rounds, never inside a streamed assistant message. Request assembly
  positions them like any other event, and readers must not assume a turn
  has exactly one leading user message.
- `assistant.message`: `content`.
- `tool.call`: `id`, `name`, `input` (structured JSON).
- `tool.result`: `id`, `name`, `ok`; `output` (+ optional `exit_code`) on
  success, `error` on failure (optional `output` and `exit_code` may
  accompany `error` when the tool produced partial output before failing).
  Optional `recovery_closure: true` marks a resume-time canonical closure for
  an interrupted tail `tool.call`; it records the resume observation, not the
  original tool outcome.
  Optional `grant_source` (`"session"` | `"project"`) marks a run covered by
  an existing scoped grant; optional `static_safe: true` marks a run
  auto-approved by static command-safety analysis (see
  `docs/contracts/capabilities.md`). Both are ledger provenance tags rendered
  on the tool header, not fresh decisions.
  This payload is the canonical tool-result shape; provider adapters map
  exactly this shape onto their wire formats.
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
  `requested_reasoning_effort`, optional `reasoning_effort`. Optional
  `project_context_digest` (ADR 0017) is the versioned rendered-context
  digest, recorded only when those exact core-framed bytes occur in the
  provider-neutral request being dispatched (no TOCTOU between snapshot and
  prompt assembly); absent whenever the request carries no project context.
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
  blob-externalized when large).
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
  session then stops cleanly (survivability first; automatic compaction follows). If a
  provider stops with `max_tokens` mid-call, that is recorded in
  `model.result.stop_reason`; `context.limit` may still follow at the
  boundary.
- `context.slot.updated`: `extension_id`, `slot`, `content`. Records a
  host-mediated extension context slot update. `extension_id` is assigned by the
  host from the calling extension, `slot` uses the event-feed checkpoint name
  grammar, and `content` is UTF-8 text capped at 4096 bytes. Control characters
  other than newline are rejected. Empty `content` deletes the slot. Slot
  payloads are below the blob externalization threshold and remain inline.
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
  `{ "expected": true, "schema_version": 1, "status", "policy",
  "candidate_digest", "source_count", "diagnostic_count" }`. Present exactly
  when the session was created with a project-context bootstrap; it announces
  that one `project.context.snapshot` follows immediately. Absent means the
  legacy shape: no snapshot events exist and resume treats project context as
  disabled. A summary without its snapshot (or vice versa) is an invalid
  mixed shape and resume fails closed.
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
  event-log projection (status/name/title/root/kind) keyed by the event
  log's `(byte length, mtime)`. While the key matches the live log, listings
  serve the cached projection verbatim instead of re-deriving it — the
  events remain the sole naming authority, but their authority is enforced
  at projection time, not on every read. A hand-edited sidecar can therefore
  misreport display fields until the event log next changes; that is inside
  the store's trust boundary (the same actor could edit the log itself) and
  outside its integrity model. Any event append or log rewrite moves the key
  and forces re-projection, and integrity failures (`invalid` status) are
  never cached, so they are re-checked on every listing.
- `project.context.snapshot` (schema version 1; ADR 0017,
  `docs/contracts/project-context.md`): `schema_version`, `status`
  (`admitted` | `disabled`; phase 3 adds `declined`/`unacknowledged`),
  `policy`, `resolution_reason`, `acknowledgment_basis`, `candidate_digest`
  (versioned, domain-separated, length-prefixed digest of the canonical
  candidate manifest), `workspace_identity`
  (`{ "algorithm": "unix-raw-osstr", "version": 1, "digest" }` over the raw
  canonicalized workspace-root bytes), `ordering` (`lexicographic-v1`),
  `source_identities` (bounded normalized project-root-relative paths),
  `diagnostic_count`, and `diagnostic_reason_counts`. An admitted snapshot
  additionally carries `framing_version`, `manifest_len`, and `manifest` —
  the complete canonical UTF-8 manifest JSON as one top-level payload string,
  externalized as one content-addressed blob above the provenance threshold.
  A disabled snapshot persists NO source body, per-source content hash, exact
  content length, or parser excerpt. The durable bootstrap order is exactly
  `session.start`, one snapshot, then the declared diagnostics, all persisted
  before any provider dispatch; the latest snapshot in durable sequence is
  authoritative, and a disabled snapshot is a tombstone. Rehydration verifies
  the blob address and length and rejects invalid UTF-8, duplicate keys,
  trailing data, unsupported versions, limit violations, and digest
  mismatches; it never falls back to current project files.
- `project.context.diagnostic` (schema version 1): `schema_version`,
  `snapshot_event_id`, `reason` (stable content-free code), optional bounded
  `path` (normalized relative identity), optional numeric `observed`. Never
  carries excerpts, raw parser errors, outside-workspace paths, or exception
  strings derived from a candidate.
- `canvas.snapshot`: `selected_event_ids`, `counts`, retention telemetry
  `retained_items`, `retained_bytes`, `demoted_items`, `automatic`, `stubs`,
  `tier`, `budget_bytes`,
  `over_budget`, and `pressure` (`none`|`byte`|`token`|`both`). Optional
  `used_tokens` and `limit_tokens` are included when provider usage and a
  configured context limit are known. Snapshot fields are assembly telemetry
  for the next model request; they do not rewrite provenance history.
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
  short validation outcome.
- `canvas.candidate.discarded`: `reason`, `policy_version`. It records a
  rejected shadow compaction candidate at the turn boundary; `reason` is a
  short non-secret validation failure and `policy_version` names the
  compaction policy that produced the candidate.
- `error`: `source`, `message`, optional `category` (`auth` |
  `transport` | `rate_limit` | `rejected` | `stream_truncation` |
  `internal`) carrying the provider error taxonomy from
  `docs/contracts/provider.md` when the source is a provider. When
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
- `model.result`, `model.reasoning`, and `model.delta` parent their
  `model.call`.
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
- `error` parents the `model.call` when the source is a provider failure
  during that call; otherwise the previous persisted event.
- Events with no specific causal parent (e.g. `user.message`) parent the
  previous persisted event in the session, or null at session start.
- A persisted event must never parent a runtime-only event (e.g.
  `model.delta`); the persisted stream's DAG must be closed under the
  persisted stream.

Cardinality and ordering invariants:

- exactly one `session.start` per session, always the first persisted
  event;
- exactly one `model.result` per `model.call`;
- zero or more `model.reasoning` events per `model.call`, emitted in
  provider order before their `model.result`;
- `assistant.message` is emitted after its `model.result`, and only for
  turns that end without tool calls.
- an accepted `model.switched` is emitted after the previous turn's final
  persisted event and before the next `user.message` is accepted. A switch
  after a new `user.message` starts the next turn is rejected. The next
  accepted `user.message`/`model.call` sequence uses the switch target,
  and its `model.call` must carry the target `provider` and `model`. A
  switch event must never be interleaved inside a provider stream,
  tool-execution round, or already-started user turn.
- zero or more `canvas.swap` events may appear per session; each marks a
  compaction boundary and is replay-critical for reconstructing which canvas
  range was active.
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
