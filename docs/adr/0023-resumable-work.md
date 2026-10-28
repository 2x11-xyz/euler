# ADR 0023: Resumable work: replay policy, durable subagent ownership, crash-loop parking

## Status

Accepted (2026-10-27). Sequenced after the ADR 0022 rollout and coordinated
with issue #239. No contract changes until the rollout PRs land; each PR
updates its owning contract.

## Context

Euler persists everything and resumes, but resume continues no work.
Reopening is the recovery boundary (`docs/contracts/provenance.md`, the
`recovery_closure` paragraphs). Today:

- **Tool calls.** The root driver records the whole provider batch of
  `tool.call` events before it executes the first one (`finish_tool_round`
  and `record_tool_call_batch` in `crates/euler-core/src/session.rs`). A
  `tool.call` carries `id`, `name`, `input`, and extension attribution
  (`record_tool_call`, `crates/euler-core/src/session/tool_dispatch.rs`). On
  reopen, `tool_recovery_closure` (`crates/euler-core/src/resume.rs`) appends
  one failed `tool.result` with `recovery_closure: true` for the root's tail
  unmatched call, found by `tail_unmatched_tool_call_index`. Its text says
  either that the tool did not run (permission undecided) or that side effects
  may have occurred. Nothing re-runs (`docs/contracts/events.md`,
  `tool.result`).
- **Model calls.** Every accepted `model.call` without a semantic terminal
  gets a `recovery_closure` error that reports an unknown outcome
  (`model_recovery_closure`).
- **Runs.** For each open run, resume appends `queue.cancelled` for pending
  steering and one `run.terminal` with status `interrupted`; follow-ups are
  preserved (ADR 0020, `run_recovery_closures`). A resumed session can only
  start a new run.
- **Children.** Recording a result requires a current-process `SpawnedAgent`
  handle (`docs/contracts/multi-agent.md`). Background children are
  `FnOnce() -> AgentResult + Send + 'static` thread closures behind a
  current-process `BackgroundAgent` handle that is never joined, cancelled, or
  recovered (`crates/euler-core/src/session/background.rs`). Resume closes an
  incomplete child's unmatched tool calls (`child_tool_recovery_closures`) and
  open model calls, but leaves `agent.spawn` without `agent.result`, and that
  spawn can never be completed.
- **Extension model tools** declare `name`, `description`, and `input_schema`
  (`ModelToolDescriptor` in `crates/euler-sdk/src/lib.rs`, with
  `deny_unknown_fields`). Nothing says whether re-running a call is safe, and
  the extension receives no stable identity for the call.

These closures are honest, and they stay the default. They also mean the
roadmap item "`exec --resume`: continue an interrupted run from its provenance
log instead of re-exploring from scratch" (`docs/roadmap.md`) has no
mechanism. Today's `exec --resume` admits a new prompt into a session whose
interrupted run is already closed.

pi-durable (`earendil-works/pi` at `136b87e`, `packages/durable` v1.4.0)
continues work after a crash. Three of its mechanisms fit Euler without its
storage model:

1. **Per-tool replay policy.** A tool registration declares
   `replay: "safe" | "unsafe"`, default `unsafe`. The tool task commits the
   final arguments and the resolved policy as its `execute` checkpoint before
   it runs the tool (`packages/durable/src/harness/tool.ts`,
   `ToolTaskCheckpoint` and the `call` phase). On reopen, the `execute` phase
   reruns the tool only when the stored and the current policy both say
   `safe`; otherwise it settles an `interrupted` result with the durable
   partial output (`tool.ts`, `execute` phase; `docs/spec.md` section 7.3).
   Nested calls reattach by key and fail on a different tool or arguments
   (`startNestedCall` in `tool.ts`). Replay is not exactly-once: a replay-safe
   tool with external effects needs its own idempotency (section 7.3).
2. **Owned children.** A replay-safe subagent tool creates its child
   conversation with the tool task as owner, finds it on rerun with
   `scanConversations({ ownerTaskId })`, and submits with the deterministic
   request id `subagent:${taskId}` (section 7.3, subagent example). Aborts
   cascade down the ownership tree and abort handlers run bottom-up; a
   background task is a boundary (sections 5.4 and 5.5). A caller that is not
   replay-safe marks its children `abandonOnRestart` (`tool.ts`, `resumes`).
3. **Restart counting.** Open counts each task a crash cut off in its
   `restarts` field; a state change the task commits resets it. At
   `restartLimit` (default 5) the task is parked as blocked with reason
   `restart_limit`, reported once, and `retryTask(id)` runs it again. A clean
   close does not count (section 5.4, "Restarts"; section 5.1, `TaskRecord`).

The 2026-10-27 comparison also named materialized projections (the open-run
index ADR 0020 expects). That is a separate follow-up and not part of this
record. Two pi mechanisms are not adopted: deferred commits, which conflict
with Euler's write, file sync, and directory sync durability, and in-process
hot-reload extensions, which would undo the process isolation of ADR 0018 and
ADR 0021.

## Decision

Honest closure stays the default. Each new path either produces a fresh
result from a re-execution that provenance marks as one, or falls back to
today's closure. Nothing is inferred from a missing event. Clauses marked
"(resolved question N)" record the answer listed under Resolved questions below.

### 1. Run continuation

Replay needs an open run to land in, because late tool work after a run
terminal is invalid (`provenance.md`). So:

- **Continuation is explicit** (resolved question 1). `euler exec --resume
  <path-or-id> --continue` asks resume to continue the open run; a prompt
  argument with `--continue` is a usage error, and `--continue` never reads
  piped stdin (without it, `exec` reads the prompt from stdin when no prompt
  arguments are given). Every other resume keeps today's closures and
  `run.terminal` `interrupted`.
- **Only a cleanly admitted open run continues.** The run's admission
  (`run.started` with its `user.message`) must be complete and it must have no
  partial terminal batch. ADR 0020's crash-partial admission and terminal
  batches keep today's inert recovery. Otherwise, and when there is no open
  run, `--continue` exits nonzero before arming the resume marker and appends
  nothing.
- **One transaction.** Resume appends today's closures for every call that
  will not replay and every unresolved model call, `queue.cancelled` for the
  run's pending steering as today (resolved question 4), then a new lifecycle
  event, `run.continued`, for the open run. It appends no `run.terminal`.
  `run.continued` becomes the eighth lifecycle kind beside ADR 0020's seven.
  `RunLifecycleProjection` folds it as keeping an open run open, and a later
  `run.continued` for the same open run is valid, so ADR 0020's single owner
  of run state holds. Like the other lifecycle events it is writer-linear and
  advances the logical parent frontier; the `session.resumed` marker keeps its
  existing leaf rule and never parents continued activity.
- **Same preflight and failure rule.** The candidate prefix (durable events,
  proposed closures, and `run.continued`) passes full session, run-lifecycle,
  and model-terminal validation before anything is appended. A rejection
  leaves the log untouched. An ambiguous append fails the resume, as a failed
  recovery append does today; the next reopen re-reads the accepted prefix
  and decides again from it.
- **Then the driver resumes.** The live session re-dispatches each
  replay-eligible call (Decision 2) in call order, then starts a new model
  round in the same run. Follow-ups wait for the run's terminal.
- **Only work that was cut off continues.** An open run at reopen does not
  prove its work was cut off. The live session decides a run's outcome before
  it appends the run terminal, and the decided outcome lives only in memory
  until that append succeeds (`deferred_run_terminal` and
  `pending_run_terminal` in `session.rs`), so a crash or an ambiguous append
  can leave a completed, cancelled, or failed run open. `--continue` therefore
  refuses before arming the resume marker, exiting nonzero and appending
  nothing, when the run's root events after its admission show a decided
  outcome: any `cancelled: true` model error or tool result, a provider model
  error (transport retries persist nothing, so a persisted one ended the
  run), the guardian turn-interrupt error, the tool-rounds-limit message, a
  root session error that is not a recovery closure (added 2026-10-28, #265:
  the live session had already failed the run), or
  a `model.result` that requests no tools with no later steering
  `user.message`. It also refuses, failing closed, unless the root work is
  visibly in flight: an unresolved `model.call`, an unmatched `tool.call`, or
  a settled tool batch or steering `user.message` not yet followed by a
  `model.call`. A plain resume still closes such a run as today. A run without a decided outcome is open because the
  process died or was stopped from a restart-only fence (`provenance.md`,
  `multi-agent.md`). Both count as restarts (Decision 4).

### 2. Per-tool replay policy

- **Declaration.** Every tool has a replay policy, `safe` or `unsafe`, and an
  omitted policy is `unsafe`. Core: `read_file`, `git_status`, `git_diff`,
  `tool_result_get`, and `skill_read` are `safe`. `edit_file`, `write_file`,
  `apply_patch`, and `run_shell` are `unsafe`, including statically safe shell
  commands (resolved question 7). Extension model tools declare `replay: "safe"`
  on `ModelToolDescriptor`.
- **Intent.** `record_tool_call` writes `replay: "safe"` on `tool.call` when
  the policy resolved at call time is `safe`; absent means `unsafe`. The call
  is already durable before permission and execution, which is pi's intent
  commit. Euler has no argument-rewriting hook, so the stored `input` is the
  execution input.
- **Eligibility.** During continuation, an unmatched accepted call replays
  only when all of these hold: the stored policy is `safe`; the current
  declaration resolves and is `safe` (the current declaration can veto but
  never upgrade, and a tool that no longer resolves is `unsafe`); and #239's
  root identity check confirms the recorded workspace. Every other call gets
  today's closure. An updated extension that still declares `safe` is trusted;
  the gate on changed extension code is ADR 0018's re-consent when the package
  fingerprint changes. Classification covers every unmatched call of the
  interrupted batch, not only the tail, and keeps today's pending-window rule:
  permission and companion events between a call and its missing result do
  not match it (`is_pending_tool_window_event`).
- **Re-execution.** A replay goes through ordinary dispatch, including the
  permission braid (resolved question 3). It is decided as a new operation under
  the resumed session's modes and grants, which resume already folds without
  reviving a grant from an interrupted batch
  (`fold_session_permission_decision`). No once-decision recorded for the
  crashed attempt is reused. A headless run that would need a prompt denies
  honestly. The result is an ordinary `tool.result` parented to the original
  `tool.call`, plus `replayed: true`. It reports state at replay time, not at
  the crashed attempt. Extension input is redacted again from the stored
  faithful `input` with the current redactor (ADR 0022, substrate 3); the
  idempotency key below does not depend on those bytes.
- **Idempotency key.** The host gives every extension command execution
  reached through a model tool the owning `tool.call` event id, which is
  stable across replays. Declaring `safe` is the declarer's promise; a tool
  with external effects keys its own idempotency on that id.
- **Model calls are not replayed.** An interrupted `model.call` keeps its
  unknown-outcome closure, and continuation issues a new `model.call`.

### 3. Durable subagent ownership

Scope: children spawned through `HostApi::spawn_agent` and
`HostApi::spawn_agents` while an extension command runs as a model tool.
Guardian, companion, round-observer, and `spawn_background_agent` children
keep current-process semantics. A background worker is a Rust closure, so no
durable record can re-run it.

- **Ownership edge.** `agent.spawn` gains `owner_event_id`, the owning
  `tool.call` event id, and `spawn_key`. The key is the spawn's ordinal within
  that tool execution (`1`, `2`, and so on, batch tasks in task order) unless
  the extension passes an explicit key in a new optional
  `SpawnAgentTask.spawn_key`. Explicit keys follow pi's nested-call rule
  (non-empty, no `/`) and must not consist only of ASCII digits, so they
  never collide with ordinals. A key used twice in one execution is a host
  error. `agent.spawn` also gains `explicit_context_sha256`, the SHA-256 of
  the redacted explicit context, because today it records only the context's
  length (`explicit_context_bytes`). Core records these fields from rollout
  step 1, so every log that can replay an extension tool also carries its
  spawn keys.
- **Reattach.** During a replay of a replay-safe owner, a spawn with an
  existing `(owner_event_id, spawn_key)`:
  - with an `agent.result`: returns the recorded `AgentOutcome` and starts
    nothing;
  - without one: continues the same child id. Continuation has already closed
    the child's unmatched tool calls and open model calls. The host rebuilds
    the child's conversation from the child's own accepted events in log
    order (model results, tool calls, and tool results, including those
    closures) and gives it a new model round. The request uses the replayed
    task's system prompt and explicit context, which the mismatch check below
    has matched against the record. Rounds and tool calls already in the log
    count against the recorded budget (`max_turns`, `max_tool_calls`). The
    host rebuilds the open spawn entry from the log, and the child's single
    `agent.result` completes the historical spawn. The continuing process
    builds a new exact envelope for it and follows the existing exact-retry
    rule from there; the crashed process's retained envelope did not survive
    and is not retried.
    For today's batch children (single-round, tool-free briefs) this is one
    new `model.call`;
  - with a task that differs from its recorded `agent.spawn` fields
    (persona, target, brief, system prompt, capabilities, budget, context
    policies, `explicit_context_sha256`): returns a host error to the
    extension and appends nothing. The record holds redacted fields (ADR
    0022, substrate 3), so the replayed task is compared after redaction with
    the current redactor; a redactor change reads as a mismatch and fails
    closed.
- **Closed owners.** When the owner gets a closure instead of a replay, its
  children stay as today: child calls and model calls closed, spawn
  incomplete (resolved question 5). The order is already bottom-up, since
  `recovery_closures` emits child closures before the owner's.
- **Cancellation is unchanged.** Synchronous companion and parallel-review
  spawn paths record terminal `agent.result` events before returning
  cancellation (`multi-agent.md`), so a cancelled owner never leaves a child
  to reattach. pi's background boundary has no Euler counterpart until
  background children are durable.

### 4. Crash-loop parking

- **Count.** A run's restart count is the number of `run.continued` events for
  that run after its latest progress, where a `retry: true` event starts the
  count again at one. Progress is a `tool.result` without `recovery_closure`,
  an `agent.result`, or a root `model.result` that requests no tools, in that
  run. Closures are not progress, and neither is a `model.result` that
  requests tools: otherwise a tool that is unsafe to rerun and crashes every
  time would reset the count on each new model round (continue, close the
  call, the model requests it again, crash) and never park. The run fold
  derives the count from the log; nothing stores it.
- **Limit** (resolved question 2). A run whose count is 5, pi's default, is
  parked: it has had five continuations without progress. Continuation then
  refuses before arming the resume marker: no replay, no model round, and
  nothing appended. It exits nonzero with reason `restart_limit`, the count,
  and two remedies: retry, or a plain resume, which closes the run as today.
  A plain resume's closure of a parked run's replay-safe call says it was not
  replayed because the restart limit was reached.
- **Retry.** `--continue --retry` is accepted only for a parked run. It
  appends `run.continued` with `retry: true` and then continues.
- **One counter per run.** A session has at most one open root run, and owned
  children continue inside their owner's replay, so one counter covers root
  replays, child continuations, and repeated crashes during a model stream.

### 5. Ownership split with #239

| Concern | Owner |
|---|---|
| Recording and validating workspace root identity on resume; overlap with ADR 0017 relocation (`project.context.relocated`, `plan_relocation`); multiple writable and attachment roots; `.worktrees` restrictions | #239 (ADR 0021 Unit 4) |
| Run continuation, replay declaration and eligibility, `replayed` results, idempotency key, spawn keys and reattach, restart count and parking | ADR 0023 |
| Resume preflight order | Shared seam: #239's identity validation runs first, and continuation consumes its verdict. A relocated or unverified root disables replay (closures only); continuation itself is still allowed |

Until #239 lands, replay stays off and continuation closes every call.

Note (2026-10-27): the verdict this seam consumes exists.
`RootIdentityVerdict` on `FoldedSession` and `ResumeOutcome` is `Verified`,
`Relocated`, or `Unverified { reason }` (`docs/contracts/project-context.md`).
Per Decision 5, rollout step 4 replays only on `Verified`.

## Compatibility

- **Legacy logs.** No `replay` field means `unsafe`, no `owner_event_id`
  means no reattach, and no `run.continued` means a zero count. A plain resume
  behaves as today, apart from rollout step 2: a multi-call root batch
  interrupted before all its results gets a closure for every unmatched
  call.
- **Rollback.** Older Euler rejects unknown event kinds during resume
  preflight (`preflight_event`, `ResumeError::UnknownKind`), so a log
  containing `run.continued` fails closed on an older binary. Some folds
  reject unknown fields (`events.md`, project-context items), so each PR
  states its rollback behavior for the new optional fields (`replay`,
  `replayed`, `owner_event_id`, `spawn_key`, `explicit_context_sha256`,
  `retry`).
- **Extension skew.** `ModelToolDescriptor` denies unknown fields, so a newer
  extension that declares `replay` fails registration on an older Euler,
  visibly, as in ADR 0022. An older extension on a newer Euler is `unsafe`.
  `SpawnAgentTask` also denies unknown fields, so a newer extension that
  passes a `spawn_key` fails that spawn on an older Euler.
- **ADR 0020.** `run.continued` is a lifecycle event in the run fold. There is
  no parallel run model, and every run still ends with exactly one
  `run.terminal`.
- **Contract exceptions.** `provenance.md`'s "ordinary late model or tool work
  remains invalid" gains the replay case inside a continued run.
  `multi-agent.md` gains the reattach case in two places: "historical
  incomplete spawn cannot be completed" and "result recording requires a
  current-process `SpawnedAgent` handle that matches an in-memory open spawn
  entry". ADR 0020 gains a note naming the eighth lifecycle kind. Each lands
  with its PR.

## Rollout

Sequenced after ADR 0022 step 3, so both ADR 0022 Euler steps are in place:
step 1 changes `ModelToolDescriptor` (substrate 4, `max_input_bytes`) and
step 3 removes core's special model-tool path. Steps 1 to 3 do not depend on
#239 and ship on their own. One change per PR, each with tests that fail
without it and its owning contract update:

1. **Replay declaration and intent.** Core policy table,
   `ModelToolDescriptor.replay`, `tool.call.replay`, and `owner_event_id`,
   `spawn_key`, and `explicit_context_sha256` on extension-driven
   `agent.spawn`. Recording only; resume is unchanged. Contracts:
   `tools.md`, `extension-sdk.md`, `events.md`, `multi-agent.md`.
2. **Per-call recovery classification.** Resume classifies every unmatched
   call of the interrupted root batch; every call still classifies as a
   closure. This changes plain resume for any multi-call root batch
   interrupted before all its results, because today only the batch's tail
   is examined (`tail_unmatched_tool_call_index`). A crash during an earlier
   call closes the last call, which never ran, with "side effects may have
   occurred", and leaves the earlier calls without a result. A crash after a
   result leaves the later calls without one. A permission prompt on an
   earlier call closes nothing, because the prompt does not belong to the
   tail call. Every unmatched call now gets a closure. Contracts:
   `provenance.md`, `events.md`.
3. **Run continuation with parking.** `run.continued`, its fold,
   `exec --resume --continue` with closures only, the derived restart count,
   the limit, and `--retry`. Parking lands with continuation so no release
   can continue a run without the crash-loop bound. Contracts: `events.md`,
   `provenance.md`; CLI help and `docs/roadmap.md`.
4. **Core tool replay**, after #239. Eligibility, re-dispatch through the
   permission braid, and `replayed: true`, for the core `safe` tools only.
   None of them spawns children. Contracts: `events.md`, `provenance.md`,
   `tools.md`.
5. **Extension tool replay and durable spawn ownership.** Extension-tool
   eligibility, the idempotency key, the optional `SpawnAgentTask.spawn_key`,
   reattach, and the mismatch and duplicate-key errors. These ship together
   because replaying an extension tool that spawns without reattach would
   duplicate its children.
   Contracts: `multi-agent.md`, `extension-sdk.md`, `events.md`.
6. **euler-extensions.** Tools that qualify declare `replay: "safe"`, one PR
   each in that repository.

Required regression tests across the steps: a plain resume of a legacy log
appends the same closures as today, apart from step 2's multi-call batch
cases (a crash during an earlier call, a crash after a result, and a pending
permission prompt on an earlier call); `--continue` refuses a crash-partial
admission and a prompt argument, and never reads piped stdin; `--continue`
refuses an open run with a decided outcome (a cancelled run, and a completed
run whose terminal was not persisted) and appends nothing; a stored-safe
call with a current-unsafe declaration is closed; a removed tool is closed; an
unverified root closes every call; the replayed result is parented to the
original call and carries `replayed: true`; a headless replay that would
prompt is denied; a completed child is returned without a provider call; a
reattached child's earlier rounds count against its budget; a task mismatch
and a duplicate key append nothing; the sixth continuation without progress
is refused and appends nothing; a tool that is unsafe to rerun and crashes
on every continuation parks the run; `--retry` is refused for a run that is
not parked and starts the count at one; a continuation after progress starts
the count at one.

## Resolved questions (2026-10-27)

1. Continuation stays behind an explicit `--continue`, because it re-executes
   work. The TUI gets `/continue` in a later change, after headless use.
2. The restart limit stays fixed at 5. It becomes configurable only if real
   runs show a need.
3. A replay decides permission again. A recorded allow belonged to the crashed
   attempt under older modes and grants; an unattended run that would need a
   prompt denies honestly.
4. Continuation cancels pending steering, as today. That steering addressed
   the interrupted turn, and ADR 0020 keeps it recoverable.
5. A closed owner's incomplete children stay without a result. Synthesizing a
   parent-authored `agent.result` would break `multi-agent.md`'s rule that
   resume never invents one.
6. Replay waits for #239. Root identity is the safety gate; canonical path
   equality misses a moved or replaced workspace. Rollout steps 1 to 3 still
   ship on their own; only the step that re-executes calls needs #239 first.
7. Statically safe `run_shell` commands stay `unsafe`. Static safety is a
   heuristic, and the reads worth replaying already have `safe` tools.
8. `code_swarm_review` declares `safe` once reattach lands, provided the
   extension keys its review artifact on the owning `tool.call` id so a replay
   cannot write a duplicate.

## Consequences

- `exec --resume --continue` continues interrupted work, which gives the
  roadmap's headless-resume item a mechanism.
- Honest closure remains the default and the only path for unsafe tools,
  legacy logs, unverified roots, and parked runs.
- Provenance marks every re-execution (`run.continued`, `replayed: true`), so
  a replayed result is never mistaken for the crashed attempt.
- Extensions gain a replay declaration and a stable call id; whether `safe` is
  true remains the declarer's responsibility.
- Background children stay current-process. Durable background work needs
  serializable tasks and a later ADR.
