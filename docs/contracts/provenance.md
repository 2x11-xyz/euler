# Provenance Contract

Provenance is append-only ground truth.

It records model calls, tool calls, permission decisions, extension calls, agent spawns/results, artifacts, and canvas assembly metadata.

Provenance is complete but cheap. Large payloads should be stored as blobs referenced by hash.

Derived research structures, such as causal DAGs, are projections or extension artifacts. They do not mutate primary provenance events.

Bounded provenance queries may name an inclusive `through_event_id`. The bound
is a physical position in the accepted durable prefix, independent of filters;
pages may stop earlier because of match or scan limits but no page may scan,
return, or watermark an event after the bound. The same bound is retained
across pages. Missing bounds and ranges whose bound precedes their cursor fail
with typed errors rather than widening to the live tail. The reader physically
stops as soon as it decodes the bound. If a requested cursor has not appeared
by then, the bounded range is invalid; the reader does not inspect the suffix
to distinguish a later cursor from one absent from the whole log. Unbounded
missing-cursor queries retain the typed cursor-not-found error.

At the root request-tick boundary (ADR 0019), core first settles compaction and
persists current live events, then samples one writer-confirmed durable tail
for all contributors. Tick-phase capability decisions and side effects occur
after that cutoff and cannot enter any contributor's bounded read, including a
later contributor's read. Tick execution adds no parallel provenance stream:
commands, permission decisions, context slots, plans, artifacts, and fixed
errors keep their existing canonical event shapes and writer fence.

Provenance uses the canonical session event envelope in `docs/contracts/events.md`. Persistence policy, durability semantics (emitted/appended/durable), and schema versioning are defined in `docs/contracts/persistence.md`.

Every fresh `session.start` records the exact compile-time runtime identity
defined by the event contract: package/binary version, build-time Git revision
and tracked dirty state when knowable, sorted build features, the digest of the
recorded non-runtime `session.start` projection, provider-client version, and
the currently attached root. That digest does not claim to cover unrecorded
`SessionConfig` state. Legacy omission projects and serializes as explicit
`legacy_unknown` provenance; a recorded value serializes as `recorded`. Neither
report nor resume inherits the identity of its reader.

Accepted event ids are globally unique within a session stream. A duplicate
makes request links and causal references ambiguous, so resume rejects the
prefix during canonical preflight before recovery closures, resume markers, or
continued activity can mutate the log. Inspection projections remain readable
where possible but must fail closed instead of granting authority to the
colliding id.

## Writer Ownership

A live session log has one owning `ProvenanceWriter`. Creating a second writer
for the same log path while the owner is live must fail with a session-lock
error. Background or extension work that needs to append to a live session log
must use the owning writer through an explicit product-neutral host boundary;
it must not open another writer as a bypass.

Writer ownership is the non-blocking exclusive OS advisory lock held on the
persistent `events.jsonl.lock` file descriptor for the writer lifetime. The
lock pathname's existence and its best-effort diagnostic metadata are never
authority. A legacy PID-only lock file, malformed metadata, or metadata left by
a crashed process is overwritten after the OS grants the lock; users must not
delete the pathname to force access while another writer may be active.
The session directory is part of the trust boundary: while a writer is live,
other actors must not unlink, rename, or replace its lock pathname. Advisory
locks attach to open files rather than names, so replacing that pathname can
create a different file with an independent lock. On network filesystems
(NFS, SMB) advisory-lock semantics vary by protocol and mount options; a
session directory on such a mount weakens the single-writer guarantee to
whatever the filesystem actually enforces.

Mixed versions: a lock file whose payload is a legacy bare PID belongs to a
pre-advisory-lock Euler, which owns sessions by pathname existence and holds
no OS lock. New writers refuse such files instead of claiming them — an old
writer may be live and unobservable — and recover only by the user deleting
the file after confirming no older Euler is running. In the other direction,
older Euler versions cannot parse the persistent lock file this version
leaves behind: rolling back across this version requires deleting
`events.jsonl.lock` files while no Euler process is running.

A single `ProvenanceWriter` serializes concurrent append calls from the same
process. This is an append integrity guarantee, not an observer lifecycle API:
it does not provide scheduling, cancellation, durable subscriptions, checkpoint
compare-and-swap, or automatic recovery of background work.

One live `Session` may opt into the writer's process-local accepted-event feed.
The feed has one owner and publishes only events whose write, file sync, and
directory sync have all completed. Commit publishes only into this passive
in-memory feed while still holding the append lock; it invokes no Session,
host, or user callback. Concurrent queue, extension, and companion producers
therefore reach the feed in canonical writer order, and a later scrub audit is
an exact, already-contiguous cutoff. The feed is not another log or
durable subscription: it retains no history before
attachment, disappears when its owner drops, and the provenance log remains
the authority. The Session maintains a confirmed prefix from this feed;
unconfirmed local suffixes, where legacy append paths still retain them, are
never folded as durable run/queue state.

Feed reconciliation is transactional over the live projection. The Session
first constructs a candidate accepted prefix, rejects duplicate accepted ids
or a pending same-id envelope disagreement, and folds the complete candidate
lifecycle before changing the event bus, accepted cursor, or run/queue
projection. Runtime-only events have no feed row; reconciliation preserves
their original position before the next matching durable event and fails
closed if a feed event would overtake an unmatched durable local row. This
prevents an earlier `model.delta` from being moved behind its run terminal. A
validation failure changes none of the bus, cursor, or lifecycle projection.
Because draining the process-local feed is not reversible, that live Session
and its bound queue then fail closed for every authoritative write until the
durable log is reopened and replayed. Public lifecycle getters reconcile this
feed before returning, rather than exposing a stale process-local projection.

The writer tracks three distinct positions: its confirmed byte length, the id
of the final physically accepted event, and the logical parent frontier for
the next writer-linear event. They are normally the same event. A
`session.resumed` audit leaf advances the physical tail and byte length but
does not advance the logical parent frontier; reopening a marker-terminated
log reconstructs both positions independently.

While holding the append lock, the writer installs a logical reservation
before the first fallible persistence step. That reservation fixes the
original logical parent frontier, every assigned event id and timestamp, every
assigned parent, and the exact payload/blob-reference bytes of the logical
envelopes, including any pending resume marker. A digest makes this full
logical identity cheap to compare; it does not weaken the byte identity. Any
later persistence failure -- including directory or blob preparation, log
open or metadata access, write, flush, file sync, or directory sync -- leaves
that exact logical batch pending. Only a retry carrying those same envelopes
may proceed. Once the physical log offset and serialized suffix are known, the
reservation also records their identity. A complete matching suffix is
re-synced and committed without being appended again; an absent suffix is
written again only at the unchanged confirmed byte offset. A partial, changed,
or extra suffix fails closed without truncation. Until any reserved append is
reconciled, unrelated appends, resume-marker changes, and log rewrites such as
scrub are rejected.

A fresh user admission preflights this writer reservation before installing
its own pending transaction. If a retained agent report, extension event,
queue operation, or other exact candidate already owns the writer retry, the
admission returns a typed unresolved-append error without creating a competing
pending run. The original owner can then reconcile its exact event batch.

An authoritative queued `user.message` installs its exact pending envelope and
globally unique queue-row ULID before flushing any older accepted bus
suffix. Thus an ambiguous backlog sync has an owner just as an ambiguous
candidate sync does. Until that exact retry reconciles, unrelated admissions
and control writes are fenced, the row cannot be edited or cleared, and a live
session cannot be replaced by `/new` or `/resume`.

Parent-authored `agent.result` follows the same exact-owner rule. The live
Session installs the result envelope, including the originating run, before
flushing older accepted events or attempting the result append. Failure keeps
that envelope attached to the open spawn, rejects a different result retry,
and fences competing authoritative Session writes and lifecycle replacement.
A launch failure that cannot persist its fixed sanitized result leaves an
orphaned exact candidate; the owning Session retries that candidate before its
next authoritative write. A prepared background `agent.message` likewise
retains its exact envelope on append failure and remains ahead of later reports
from that handle.

Child-authored companion/reviewer events use a stricter restart-only failure
boundary. The Session installs the exact parented envelope before append. If
the writer outcome, accepted-feed fold, or confirmed-event lookup fails, that
envelope remains the live owner where applicable and the Session becomes
reopen-required. It never guesses the child's remaining model/tool stack:
every later authoritative write, queued auto-dispatch, and lifecycle owner
swap is fenced. The remediation is to stop and restart Euler, then reopen the
session; `/new` or `/resume` inside the poisoned live Session cannot bypass
the fence.

Durable queue enqueue, cancel, replace, and recovery operations follow the
same rule:
the operation is acknowledged in memory only after its canonical lifecycle
event is confirmed. A failed sync retains the exact assigned envelope and
globally fences dispatch and mutation until that envelope reconciles; retry
never synthesizes a new id, timestamp, parent, or payload. Run admission owns
one exact atomic batch (`run.started` where applicable, `queue.delivered` where
applicable, then `user.message`). Run termination owns one exact atomic batch
(`queue.cancelled` for pending steering, then `run.terminal`). An ambiguous
terminal batch remains outside the accepted live bus even if its complete bytes
are visible on disk, and becomes live exactly once only after exact retry
confirms durability. Replay likewise leaves every crash-prefix `run_*`
`queue.cancelled` row inert until a matching `run.terminal` completes the
logical batch; a later complete retry supersedes those physical fragments.
The terminal cutoff closes steering and samples the already-classified enqueue
generation under the queue lock. Earlier generations settle first; the
terminal batch then owns the writer before any later follow-up enqueue. The
cutoff clears the active source identity: an explicit operation carrying that
run fails stale, while a newly classified follow-up is source-less and cannot
persist ahead of, delay, or starve the terminal event. If the terminal attempt
aborts before persistence and restores the open run, a parked enqueue must
revalidate its classification; it cannot persist the cutoff's stale
source-less interpretation into the restored run.
Recoverable requeue owns a two-row marker-first transaction:
`queue.recovered` then the linked `queue.enqueued`. Replay validates every
marker's original target and reserves its replacement id, but a marker prefix
is projection-inert. Only the adjacent parent-linked matching enqueue commits
the replacement and removes the recovery row. An ambiguous live append keeps
the exact recovery target, dismiss-versus-replacement shape, envelopes, and
replacement metadata globally fenced until the owning worker retries that
same batch; a mismatched retry cannot write or release the fence. A later
restart attempt uses fresh event, queue, and planned-run ids.

Lifecycle replacement is a writer-ownership boundary, not merely an admission
check. `/new` and `/resume` first reconcile accepted events and then refuse to
detach while any admission, enqueue, cancellation/replacement, terminal, or
scrub operation is in flight, queued for its writer turn, or retained for exact
retry. They also refuse after an invalid accepted projection or unresolved
provenance append. Once the owning operation commits and its feed event is
folded, the transition may proceed. The shared queue remains submission-fenced
from its old-owner clear through installation of the new session's durable
writer. One live `Session` has exactly one authoritative queue `Arc`, and an
ordinary bind may neither replace that `Arc` nor change a queue's durable
writer/session/agent owner. Owner replacement is permitted only while the
matching `QueueLifecycleTransition` holds the queue's submission fence. The
new writer/session identity is bound before the fence reopens. Binding
serializes behind every classified enqueue generation and queue-owned
mutation. Dispatch validation and canonical hydration are one transaction
under the queue lock: a missing reservation, stale row, or incompatible owner
changes neither entries, reservation, nor durable authority. A row that
settled under the old authority makes an incompatible bind fail instead of
being inserted into new-owner state. A bind failure before the application
state swap drops the transition and reopens the still-current, durably cleared
old owner. Failure after the authority or application state swap becomes
uncertain stays closed rather than allowing a row to reach a detached log.

An interactive host may compare a displayed FIFO head by stable `queue_id`
before reservation. A different head fails without reserving its successor;
the same head behind an in-flight queue transaction remains unreserved and is
reported as temporarily unavailable. After reservation, Session still binds
and hydrates that id from canonical queue authority before admitting content;
the host never supplies authoritative prompt bytes.

Durable bind reconstructs row bytes and FIFO order from the current lifecycle
projection, including after resume or scrub; matching in-memory ids do not
authorize stale cached content. A reserved queued turn is resolved again by
its globally unique id after bind, and no transcript, history, or model use may
observe its pre-bind bytes.

Queue message content is private pending input. It may be externalized as a
content-addressed blob and is rewritten by the ordinary recursive secret scrub
without changing queue/run identity. It must not be copied into session
sidecars, discovery caches, transcript, or model canvas before delivery.
Live scrub uses its accepted `secret.scrubbed` audit event as the exact feed
cutoff: it rewrites only the accepted prefix through that audit, then drains
later writer generations normally and without masking them. Queue mutation is
fenced through durable read, bus reconciliation, and lifecycle refold. After
the durable rewrite succeeds, the Session masks its entire pre-cutoff live bus
before any fallible reread or projection work. If that reconciliation then
fails, secret bytes stay masked and the Session marks its accepted state
invalid; both headless writes and the bound queue fail closed until the log is
reopened and replayed. `scrub_and_audit` cannot report whether an error happened
before or after the atomic log replacement. Therefore any error it returns is
handled by the same conservative rule: Session events and pending queue bytes
are masked immediately, matching scrub candidates are removed, and both
headless and queued operation become reopen-only even when no audit event was
confirmed.

The physical durable tail, append diagnostics, and event-wake notification
advance only after the matching bytes have passed both file and
containing-directory sync. The logical parent frontier advances to the final
non-`session.resumed` event in that commit, or remains unchanged for a
marker-only commit.
An append that externalizes a blob does not open the log until the blob file
and blob directory have both synced. A matching content-addressed blob is
re-synced together with its directory on every retry: matching bytes establish
identity but cannot prove that an earlier rename survived a failed directory
sync.

Streamed root-assistant text uses ordinary durable
`assistant.response.chunk` appends. Root response ownership lasts until its
canonical terminal is durably accepted. An unresolved checkpoint, reasoning,
or terminal append before that boundary fences unrelated live-session activity;
lifecycle reopen is the recovery boundary. A checkpoint failure stops further
stream forwarding. Reopen accepts a physically complete checkpoint or terminal
once, preserves any recorded terminal outcome, and gives a still-open call an
interrupted recovery terminal. After an accepted canonical terminal, later
appends retain their ordinary exact-batch reconciliation rules. Chunk content
above the blob threshold is content-addressed
and rehydrated for replay. Secret scrub rewrites chunk and terminal
`retained_content_bytes` together so the scrubbed stream remains
protocol-valid; immutable `observed_output_bytes` remains the original local
observation. Chunks are one logical scrub surface: if an explicit secret spans
a chunk boundary, or ordinary marker expansion would exceed the per-chunk byte
bound, every chunk in that response is conservatively replaced by the scrub
marker. Event ids, ordering, chunk count, and size bounds remain valid without
retaining a reconstructable secret. Seam detection retains only a suffix
bounded by the longest literal or JSON-escaped scrub value; it never copies an
unbounded response into a second aggregate buffer. Externalized chunks in a
collapsed response are staged through the ordinary content-addressed rewrite
transaction: every reference moves to the durable scrub marker before each old
hash is sanitized and retired.
Opening a log with a readable final fragment may recover its newline-terminated
prefix for inspection, but the raw-length mismatch fences every new append.

A shadow compaction captures its exact run origin when its request starts.
Every later reasoning/result/error, candidate discard, and canvas swap emitted
when that worker drains uses the captured value. This remains true after the
origin run terminates or another run starts; a captured runless origin remains
runless rather than inheriting the then-active run.

If a shadow worker is detached while an unresolved authoritative admission
prevents its terminal event, that live session rejects every later
authoritative write. Reopening is the recovery boundary: before arming the
resume marker or admitting new activity, Euler appends a parented
`recovery_closure` error for every accepted `model.call` without a semantic
terminal association. Semantic terminals are `model.result`, provider errors,
and session errors explicitly marked as cancellation or recovery; an extension
or ordinary session error whose linear parent happens to be an asynchronous
call does not close it. This also covers a shadow call followed by later
accepted events or by a physically complete user admission whose final sync
was ambiguous. The closure reports an unknown outcome rather than replaying
the request or claiming a confirmed cancellation. Terminal-to-call association
is defined authoritatively by the actor/order rule in
`docs/contracts/events.md`; provenance readers must not treat a crossed-agent
writer-linear parent as terminal authority, and resume rejects a direct or
otherwise unambiguous writer-linear duplicate terminal before appending any
recovery mutation.

A validated session-owned model recovery closure settles already-accepted
model work; it does not start new root-driver work. It may therefore retain the
originating run after that run's terminal event. This exception is narrow: the
closure must be an `error` with `source: "session"` and
`recovery_closure: true`, its direct parent must name a still-open
`model.call`, its session/agent/run origin must equal that call, and its
purpose plus any provider/model fields it carries must satisfy the canonical
terminal-association rule. Resume also emits an exact-parent failed
`tool.result` for each unmatched accepted tool call belonging to an incomplete
child spawn, in call order, while preserving already-settled calls. This is a
narrow restart closure for accepted work, not ordinary late tool execution;
it never invents an `agent.result`, and the historical spawn remains
incomplete. Ordinary late model or tool work remains invalid.
Before resume appends any recovery mutation, it preflights the exact candidate
prefix -- durable events plus every proposed closure -- through full session,
run-lifecycle, and model-terminal validation, then verifies that another
recovery fold proposes no residual closure. Rejection leaves the log
untouched.

Resume also folds run and queue lifecycle state before arming its marker. For
each open run it appends writer-linear recovery `queue.cancelled` events for
that run's pending steering, followed by one `run.terminal` with status
`interrupted`; pending follow-ups are preserved. Refolding the recovered prefix
must yield no open run, must preserve follow-up FIFO, and must make a second
recovery pass idempotent.

Resume's mutation boundary re-reads the accepted prefix from the locked
writer, requires it to match the supplied event vector byte-for-byte in parsed
form, and requires its final id (including an empty prefix) to equal the
writer's durable tail. It then enforces the configured session id and, when
`session.start` or root lifecycle supplies one, the configured root-agent id.
Legacy streams with neither owner signal keep root identity unknown. Every
public `FoldedSession` projection is recomputed from those writer-bound events;
caller-supplied target, reasoning, compaction, usage, warning, and permission
values are never trusted. Recovery decisions use that fresh projection, so a
newer terminal cannot receive a duplicate closure and a stale, forged, or
different prefix cannot append into another writer.

The owning writer is also the sole owner of the logical parent frontier. For
every post-D2 append, an event without an explicit semantic parent is parented
to that frontier, or to null when no persisted writer-linear predecessor
exists. Batched appends are linear: the first event parents the logical
frontier observed when the writer lock is acquired, and each subsequent
non-marker event parents the preceding event in that same batch. The
`session.resumed` sibling exception is validated separately and never becomes
the frontier for continued activity.

The semantic-parent exception list is closed: `permission.decision` may parent
its `permission.prompt`; `tool.result` may parent its `tool.call`;
`agent.result` may parent its `agent.spawn`; extension error events may parent
the triggering extension decision/command event. Adding another exception
requires updating this contract and adding tests. A semantic-parent event still
advances the linear spine: its successor in the batch (or the next append)
parents the semantic event's id, not the event before it. This linear parent
chain is an honesty spine, not the causal DAG; richer causal structure belongs
in extension artifacts that cite event ids as evidence. Consequently,
sequential-companion and parallel-reviewer `model.reasoning`, `model.result`,
and model-terminal `error` events remain writer-linear; they do not gain an
implicit semantic-parent exception. Run and queue lifecycle identity likewise
lives in the `run` envelope field and queue payload fields; lifecycle events
remain writer-linear, including inside atomic admission and terminal batches.

Legacy parent ids are immutable historical record. Opening an existing log
seeds the physical durable tail from the final accepted event id and the
logical parent frontier from the final accepted non-`session.resumed` event;
it does not rewrite or repair historical parent fields. Readers and lineage
consumers must tolerate pre-D2 logs whose persisted parents reference
runtime-only event ids such as `model.delta`. The strict durable-parent rule
binds new appends only.

Model reasoning is recorded as `model.reasoning` events at the maximum
fidelity the provider exposes (raw thinking, signed/encrypted items, or
summaries). Euler is a research agent: reasoning chains are part of the
reproducibility record. Large reasoning payloads are externalized as blobs
like any other large payload. Reasoning events are subject to the same
secret-taint rules as all provenance. Recording opaque artifacts does not
authorize core UI to render them; display policy is `docs/contracts/ui.md`
and ADR 0007.
