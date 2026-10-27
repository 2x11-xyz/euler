# ADR 0024: Materialized projections next to the event log

## Status

Proposed (2026-10-27). Sequenced behind the adversarial persistence tests in
rollout step 0. Independent of the ADR 0022 and ADR 0023 rollouts. No contract
changes until the rollout PRs land; each PR updates its owning contract.

## Context

The event log is the only authority. "Provenance is append-only ground truth",
and derived structures "are projections" (`docs/contracts/provenance.md`).
Today each consumer derives its state by folding the accepted prefix from the
first event:

- **Live session, every batch.** Each accepted batch goes through
  `reconcile_accepted_events` (`crates/euler-core/src/session.rs`). It copies
  the accepted bus prefix (`bus.events[..split].to_vec()`), checks each new id
  against that copy with a linear search, and refolds the whole candidate
  prefix with `fold_run_lifecycle`. Inside the fold
  (`crates/euler-core/src/session/run_lifecycle.rs`), every root-driver event
  finds the open run by scanning all run records
  (`validate_attributed_root_driver`, `validate_runless_non_lifecycle_event`),
  each admission or terminal transaction folds into a full clone of the
  projection (`fold_started_admission`, `fold_terminal_admission`,
  `fold_steering_admission`), and a model recovery closure rescans the prefix
  (`model_call_is_open`). One batch costs O(events x runs), so total work grows
  quadratically with history. ADR 0020 records this: "The fold scans O(events
  x runs) per batch and clones the projection. Acceptable for current
  histories; an open-run index is the expected follow-up for long logs."
- **Resume.** The mutation boundary re-reads the prefix under the writer lock
  and runs `fold_session` and `fold_run_lifecycle` over all of it
  (`resume_session_from_folded_prefix`, `crates/euler-core/src/resume.rs`).
  Caller-supplied projections are never trusted (`provenance.md`). Resume
  parses every accepted event regardless, because the live bus holds them all.
- **Session listing.** `session.json` already holds a materialized
  projection: status, name, title, root, and kind, valid under the
  `projected_events` key of `accepted_byte_len` and `tail_event_id`
  (`EventProjectionKey` in `crates/euler-core/src/session_store.rs`;
  `docs/contracts/events.md`, `session.renamed`). It is rebuildable, bound to
  an exact accepted prefix, never authority, and never cached for an invalid
  session. The full projection behind it also verifies referenced blobs,
  runtime identity, and the root. But the turn-boundary touch
  (`touch_session_updated_at`) drops the key once a turn has appended, so the
  next listing refolds the active session's whole log
  (`session_projection_from_events_or_sidecar`).

The 2026-09-05 repository audit asks for the same discipline: rebuildable
indexes that "advance only through accepted records" and "Do not treat a stale
index as authoritative provenance" (O1, O2), a test row "Projections agree
with canonical history", and "deterministic fault injection for writes, scrub,
and shutdown" (`docs/reviews/2026-09-05-repository-audit.md`). Seeds exist:
`append_surfaces_injected_log_sync_failure_and_keeps_accepted_prefix`
(`provenance_test.rs`), and
`injected_sidecar_sync_failure_surfaces_and_previous_sidecar_survives` and
`torn_final_line_still_projects_accepted_prefix` (`session_store_test.rs`).

pi-durable (`earendil-works/pi` at `136b87e`, `packages/durable` v1.4.0)
keeps typed state next to its entries:

1. **Documents** (`docs/spec.md` section 3). A definition token declares
   `kind`, `version`, `initial`, an optional `migrate`, and a required
   `checkpointWhen`, with session, conversation, or task scope (3.1, 3.2).
   Storage keeps complete bases and operation deltas; `checkpointWhen` picks a
   base to bound replay, and a base lets older records of a non-rewindable
   document be reclaimed (3.5). A stored version newer than the token rejects
   typed access (3.6).
2. **One commit** (section 4 and invariant 1 of section 1). "A Session
   atomically commits immutable entries, full task records, and Chord-tracked
   documents. Only committed state is observable." Adoption runs inside
   `Storage.commit()` once the batch is durable, "so memory always shows a
   complete prefix of the log". The JSONL backend writes document sidecars and
   then one main marker per commit, and recovery applies confirmed records only
   (11.3).
3. **Built-in documents** `pi.live`, `pi.inbox`, and `pi.usage` change in the
   same commit as the entries that change them (sections 6, 8.2, 8.6).

One difference decides the design. pi documents are state in their own right:
`pi.live` holds committed partial output and retry times that no entry
records, so documents must commit atomically with entries. Euler projections
are functions of the log. A materialized Euler projection never needs to
commit with the log; it only needs to prove which prefix it describes.

## Decision

### 1. Authority

- A projection is a deterministic fold of an accepted prefix. Materialized
  state, in memory or on disk, caches that fold at a named prefix. It is never
  written to the event log.
- Only a projection folded in the same process from confirmed events gates a
  mutation: the live session's in-memory projection (Decision 2) and resume's
  fold of the writer-bound prefix. A file may seed the fold of a read-only
  consumer such as a listing (Decision 3); it never seeds the live session or
  resume, and never overrides the log.
- Every materialized projection is rebuildable by folding the log from the
  first event. Deleting every projection file loses no information and changes
  nothing but time.
- A new projection file holds no authored content: only generated identities
  (event, run, queue, and agent ids), closed lifecycle grammar, counts, and log
  byte offsets. Content, such as queued input, stays in the log and is read
  from it when needed; the in-memory fold may hold it, but the file writer
  emits only ids, order, and offsets. A new projection file is therefore
  neither a scrub surface (`docs/contracts/secrets.md`) nor a copy of private
  pending input (`provenance.md`, queue content). `session.json` keeps its
  existing contract and stays a listed scrub surface.

### 2. Incremental run lifecycle projection

- `RunLifecycleProjection` gains one apply step: validate one accepted batch
  against the current projection and fold it. `fold_run_lifecycle` becomes
  that step applied from an empty projection, so there is one fold
  implementation and no live path beside a reference path.
- Beside the run records and pending queue entries it holds today, the
  projection carries the indexes the scans rebuild today: the open run per
  (session, agent) owner (ADR 0020's open-run index), the open model calls the
  recovery-closure check needs, the queue ids seen, and any transaction group
  still incomplete at the end of the applied prefix (a crash-partial admission
  or terminal batch).
- A batch commits all or nothing without cloning the projection or the bus
  prefix: it is validated completely, or its changes are staged and swapped,
  so an error leaves the projection unchanged, as feed reconciliation requires
  today (`provenance.md`). The duplicate-id check uses an id set. Per-batch
  cost follows the batch and the runs it names, not the history. Memory still
  grows with the history (the id set and the bus), as it does today.
- Batch boundaries never change results: for every accepted prefix and every
  split of it into batches, applying the batches in order gives the same
  projection, or an error of the same kind, as one fold of the whole prefix.
- A log rewrite (scrub) rebuilds the live projection from the rewritten prefix
  before any projection is written, as the lifecycle refold after scrub does
  today (`provenance.md`). A session whose accepted state is invalid, and so
  reopen-only, writes no projection.
- ADR 0023's `run.continued` and derived restart count, if accepted, are fold
  fields like any other. Caching them does not store them; the log still
  derives them.
- Other folds, such as usage and permission grants in `fold_session`, stay as
  they are; each moves to this model in its own change when measured (open
  question 6).

### 3. Projection files next to the log

These rules govern every new projection file, including later ones such as
the audit's O1 offset index or an ADR 0023 spawn index. `session.json` keeps
its own contract (last bullet):

- **Place.** Only store-managed sessions (`~/.euler/sessions/<id>/`) keep
  projection files, in a `projections/` directory next to `events.jsonl`
  (open question 4). Deleting a session deletes its directory, files
  included.
- **Header.** Every file under `projections/` records the projection kind,
  projection schema version, the writing binary's package version and build
  revision, session id, and the prefix key: `accepted_byte_len` and
  `tail_event_id`, as `session.json` uses. A binary without a known build
  revision writes no file under `projections/` and accepts none.
- **Writer.** Only the process holding the session's writer lock writes, and
  only for a prefix the accepted-event feed has confirmed (write, file sync,
  directory sync). It uses `session.json`'s atomic replace. A failed write
  costs only the cache and does not fail the session. The log append path
  gains no second file and no extra sync; this is where Euler departs from
  pi's single commit, because a stale Euler projection is detectable by its
  key.
- **Reader.** A reader accepts a file only when every header field matches
  its own binary and session and the key names an event boundary of the
  current accepted prefix: `accepted_byte_len` is within the accepted prefix,
  and the last non-blank line at or before it is an accepted event with id
  `tail_event_id`, found with the bounded backward scan `session.json` key
  observation uses (a tail event larger than that window gets no cache, as
  today). It observes the accepted end once, as `session.json` key
  observation does, and folds only up to it. It then applies the suffix
  through the same apply step and checks as the live fold (Decisions 2 and
  5). Any mismatch, parse failure, or I/O error means a full fold. The trust
  bound is `session.json`'s: a same-length rewrite that keeps the tail id
  stays inside the session-directory trust boundary. Resume's comparison
  (Decision 4) catches it at the next resume by the same binary.
- **Rewrites.** Scrub, the only log rewrite, deletes the session's projection
  files before it replaces the log, because a length-preserving rewrite would
  keep the key while byte offsets inside the file moved. A scrub error leaves
  them deleted.
- **`session.json`** keeps its place, its key, its writers, and its existing
  contract; the header rule above does not apply to it. It gains the schema
  version, package version, and build revision as siblings of
  `projected_events`, not inside it. A listing cache hit still compares only
  `accepted_byte_len` and `tail_event_id`, as today, so a listing may serve a
  projection another binary folded until the next append or resume. The
  post-resume comparison (Decision 5) runs only when the siblings match its
  own binary, so an upgrade never warns.

### 4. Validation on resume

- Resume keeps folding the writer-bound prefix from the first event (proposed
  answer to open question 2). For each file under `projections/` whose header
  matches and whose key names a boundary of that prefix, resume compares the
  file with its own fold at that boundary.
- Equal: the file stays. Different: resume rewrites the file from its fold and
  adds a `ResumeWarning` naming the projection and the boundary, because with
  the same binary a disagreement means a fold bug, a damaged file, or a
  rewrite inside the trust boundary (open question 3). A key that names a
  longer log than the accepted prefix gets the same warning, because appends
  never shorten the log. Stale or unreadable: resume rewrites it, without a
  warning. If a rewrite fails, resume deletes the file.
- A projection never changes a resume decision, and a mismatch never fails
  resume. The log decides.

### 5. Writer-refreshed listing projection

Proposed for this rollout (open question 5).

- The live session extends its incremental projection with the listing fields
  `session.json` caches (today's `status_from_events`, `name_from_events`,
  `title_from_events`, `root_from_events`, and `kind_from_events`), as one fold
  shared with the listing. For each batch it applies, that fold performs the
  checks the full listing projection performs: runtime identity, the root, and
  the blobs the batch references. An invalid result is never written under a
  key, as today; an invalid live fold stays invalid until the session is
  reopened, and listings project it fully. Blobs of earlier events are not
  re-verified, which is the trust a listing cache hit already has.
- At the turn boundary the host writes `session.json`'s projection from that
  live fold under the key of the prefix it describes, instead of dropping the
  key. Both are read under the Session's feed reconciliation, so no feed drain
  falls between them. A later append makes the key stale, which only costs a
  fold at the next listing. Listing the active session after a turn then
  observes the key but does not fold the log.
- After resume, the host calls `refresh_session_metadata`, which refolds the
  log because resume's appends left the key stale. Before replacing the
  cached projection, it applies Decision 4's comparison at the cached key and
  reports a mismatch beside the resume warnings. This host step, not core
  resume, owns the `session.json` comparison.

### 6. No run lifecycle file yet

Decision 2 removes the cost ADR 0020 records, and resume parses the whole log
anyway, so a run lifecycle file would speed up nothing today. The first on-disk
projection beyond `session.json` lands with a consumer that does not parse the
whole log, plus a benchmark showing that its fold matters (open question 1).
The session index (`index.jsonl`, ids and timestamps) stays as it is.

## Compatibility

- **Legacy logs** need no migration: every projection folds from the log, and
  missing files mean a full fold.
- **Rollback.** An older binary ignores the `projections/` directory and the
  new `session.json` sibling fields (`session.json` does not deny unknown
  fields), and reads `session.json` as today. A newer binary ignores files
  from another version by header. Old `session.json` keys stay valid: the key
  format is unchanged.
- **Scrub** deletes projection files (Decision 3) and rebuilds the live
  projection (Decision 2), so no projection describes the pre-scrub bytes.
  `session.json` stays a scrub surface as today.
- **ADR 0020** stands: `RunLifecycleProjection` stays the sole owner of run and
  queue state; it becomes incremental. ADR 0020 gains a note that its open-run
  follow-up landed here.

## Rollout

One change per PR, each with tests that fail without it and its owning
contract update:

0. **Adversarial persistence tests**, against today's code and before any
   projection changes. They fix the baseline that each later step must keep:
   - deterministic fault injection at each persistence step of an append (blob
     write, blob and directory sync, log write, flush, file sync, directory
     sync), of a scrub rewrite, and of a `session.json` replace (including a
     crash after the temporary file is written), then a simulated crash and
     reopen: the reopened state equals a full fold of the accepted prefix, and
     the exact-retry fence holds;
   - damaged tails: a torn final line, a torn UTF-8 sequence, a zero-filled
     final page, and trailing blank lines;
   - damaged caches: garbage or truncated `session.json`, a key that names a
     longer log, a key whose tail id is not at its offset, a key from another
     session, and a scrub that changes the byte length;
   - concurrency: a listing during appends never serves a key from one instant
     with a projection from another;
   - property tests over generated lifecycle streams: listing status, resume,
     and the live projection agree with a full fold of the same prefix.
   Contracts: none, unless a test exposes drift.
1. **Incremental run lifecycle projection** (Decision 2), with the
   split-invariance property test and a long-log benchmark (for example 10k,
   50k, and 100k events) that shows per-batch cost no longer grows with
   history. Contracts: `provenance.md` (feed reconciliation), ADR 0020 note.
2. **Projection rules and resume validation** (Decisions 1, 3, 4): the rules
   as contract text, the `session.json` version fields, scrub deleting
   projection files, and the post-resume `session.json` comparison.
   Contracts: `provenance.md`, `events.md` (`session.renamed` projection
   caching), `secrets.md`.
3. **Writer-refreshed listing projection** (Decision 5). Contracts:
   `events.md`.
4. **Later projection files**, each with its consumer and benchmark (Decision
   6).

Required regression tests across the steps: incremental and full folds agree
for every split of generated streams, including crash-partial admission and
terminal batches; a projection file with any mismatched header field or key
is ignored and the result equals a full fold; a key naming a longer log
warns on resume; a binary without a build revision writes and accepts no file
under `projections/`; an older `session.json` reader ignores the new sibling
fields; a corrupted file under a matching key yields a resume warning and a
rewritten file, and resume appends the same events as without it; a crash
between a log append and a projection write leaves an older key whose suffix
fold equals the full fold; a projection file holds no authored content after
a session with queued input; a scrub deletes the projection files, and the
next turn-boundary `session.json` carries the scrubbed title; a turn boundary
leaves `session.json` with a fresh key, and the next listing does not fold the
log; a missing blob in an applied batch leaves no key in `session.json`.

## Open questions for Eli

1. Should a run lifecycle projection file land now for long-log resume, or
   wait for a consumer that does not parse the whole log (proposed)?
2. On resume, should the full fold stay authoritative and check each file
   (proposed), or should resume trust a header- and key-matching file and fold
   only the suffix? The second is faster on long logs, but a file would then
   drive recovery closures, which are mutations, under a trust bound that
   today only governs display. It would amend Decision 1 and `provenance.md`'s
   resume boundary ("Every public `FoldedSession` projection is recomputed").
3. On a mismatch under a matching header and key, should resume warn and
   rebuild (proposed), rebuild silently, or refuse to resume?
4. Should logs at explicit `--provenance` paths get no projection files
   (proposed), or a sibling `<log>.projections/` directory?
5. Should Decision 5 (writer-refreshed `session.json`) ship in this rollout
   (proposed), or should the turn-boundary touch keep dropping the key?
6. Should later projections such as usage totals and permission grants
   (`fold_session`) move to the incremental model in this ADR, or each in its
   own change when measured (proposed)?

## Consequences

- Per-batch lifecycle time stops growing with history, which closes ADR
  0020's open-run follow-up. Memory still grows with history, as today.
- New projection files next to the log have one set of rules: rebuildable,
  keyed to an accepted prefix, free of authored content, written only by the
  writer, and checked on resume. `session.json` gains the resume check.
- Listing the active session after a turn stops refolding its log, if Decision
  5 is accepted.
- The log append path, its durability, and its exact-retry fence do not
  change.
- Euler does not adopt pi's document-as-authority model or its single
  multi-file commit. State that cannot be derived from the log stays out of
  projections.
