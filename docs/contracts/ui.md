# Terminal UI Contract

Euler adopts **Warm Ledger** as the core CLI layout and interaction system
(ADR 0010), with Codex-class product density as the quality bar. Themes are
swappable profiles over semantic tokens. The CLI representation is an ordered
event transcript, not a sidebar or dashboard.

Normative design detail lives in the Warm Spine Spec (v2.1, option 3a
lineage). Where a mockup and this
contract disagree, this contract and the Spec text win over Concepts frames.

## Baseline

- Ratatui-based terminal interface unless a simpler renderer is needed for tests.
- Calm, compact, scannable transcript:
  - user messages use a left rail (not a box),
  - assistant prose is quiet primary text,
  - tools are flat ledger rows / groups with foldable tails,
  - long outputs collapse by default with a single expand affordance,
  - permission prompts are explicit (approval panel is the only bordered flow element),
  - slash commands and pickers are discoverable,
  - code changes are clear edit/write/diff events, not sidebar widgets.

## Warm Ledger grammar

### Block families

Organize the renderer around composable blocks, not feature-specific branches:

- **MessageBlock** — user and assistant text, including markdown projection.
- **ArtifactBlock** — tool output, diffs, file reads, plan/progress, extension
  artifacts, future image previews (with text fallback).
- **InteractiveSurface** — composer, slash menu, approval panel, pickers,
  search bar, file-mention palette.
- **StatusSurface** — session identity, model, ctx%, branch, key hints,
  transient notices.

Shared properties: stable id, optional title, body/render payload, fold state
and height policy, focusability and action hints, width-bounded layout,
deterministic resize, safe text fallback.

Acceptance: a new nontrivial artifact should touch the block definition,
fixtures, and a narrow registration seam — not composer + scroll + terminal
emission + unrelated renderers.

### Ledger layout

- Default **single-column** transcript on a **2-character anchor spine**: one
  glyph anchor at column 0 per event (`•` default, `✱` thinking, `✓`/`✗`
  decision records, `◆` companion, `↩` revert, `■` interrupt), then content.
  User messages are the exception — the `▌` rail replaces the anchor.
- Timestamps are **off by default**. `/timestamps` opts into a fixed
  **9-character** left gutter (`HH:MM:SS`, faint) beside the spine; the whole
  column shifts right together so composer and transcript keep one left edge.
  Timestamps always exist per-event in provenance regardless of the setting.
- Separation is the spine **plus one blank line** between events. There is
  **no hairline per event**. The only horizontal rules in the flow are turn
  dividers (`── Worked for Ns ──`) and the composer rule.
- Sub-steps (tool children, output tails, live shell tail lines, nested
  thinking body, queued-input rows) indent under `├`/`└` and never get their
  own anchor.
- **No box-drawing borders** in the flow. **Exception:** approval panels
  (single 1px attention-role border).
- Tool groups use the **Codex vocabulary**: a **bold capitalized** verb, and
  `├`/`└` children each opening with a bold capitalized sub-verb (`Read`,
  `Search`, `List`) + target. The group header is the **verb alone**
  (`Explored`) — no step count, no duration — and children carry **no
  per-step result data** (no `· 84 lines`, no `· 0 matches`). Repeated reads
  fold onto one row: `Read a.rs, b.rs, c.rs`. This supersedes the earlier
  lowercase `explore · N steps · Ts` phrasing (design review v3 §R3).
- Fold marker language: `… N more lines · ctrl+o expand` (and matching collapse).

### Plan updates

A structured `plan.update` renders as one `Updated Plan` ledger cell: optional
explanation first, then ordered `├`/`└` checklist rows. Completed rows use a
check marker and cross out only the step text (never the tree gutter);
`in_progress` uses the activity marker and attention color; pending uses
`[ ]`. Legacy summary-only events retain the compact `Updated Plan: …` row.

When an extension model tool emits a causally descended, identically
attributed plan update, its successful generic JSON result row is omitted only
when the originating call and result carry the same nonempty provider call
`id`, so the checklist is the one coherent UI action. The tool call, plan
update, and tool result all remain in provenance. A failed, malformed, or
mismatched result is never hidden.

### Diff rendering

Diffs use a **sign + luminance** model with **no background fills**, so they
survive no-color terminals and any user theme unchanged, with nothing to
re-tune per theme. (This supersedes the earlier `added_tint`/`removed_tint`
row-fill approach: a tint has to be re-blended per terminal theme to stay
legible and fights user palettes.)

- Columns: a **4-char right-aligned faint line-number** column (added/context
  use new-file numbering, removed use old-file numbering), a **1-char sign**
  column (`+` green / `-` dim red / blank for context), then code with
  indentation preserved verbatim. The sign column is **ASCII** (`+` / `-`):
  it is diff syntax, not typography — a row copied out of the transcript has
  to paste back as a valid diff, which a Unicode minus (`−`, U+2212) breaks.
  The diffstat below is prose and does use `−`.
- **Added** rows: normal luminance with full syntax highlighting — they read
  like a normal code block. Added code is the star.
- **Removed** rows: the whole row dims to **faint**, with syntax accents
  suppressed. Removed code is evidence it's gone, not reading material.
- **Context** rows: `fg` with syntax highlighting, no sign.
- Semantics ride on the sign column + luminance **only**. No row-fill
  background anywhere, on any row or any span within a row.

### Collapsed tool output preview (v4 spec amendment)

Collapsed tool-run blocks use the Codex head+tail preview model. This
supersedes the earlier "exactly one `└ ` result line" rule (review v2 §14.2)
and its most-informative-line scoring: the collapsed preview never selects,
promotes, or reorders lines.

> **Precedence.** This amendment (2026-07-11) **postdates** the Warm Spine
> design spec v2.1 (2026-07-10), so it wins over that spec's §1/§4 "exactly
> one `└ ` result line when collapsed" and its §6 "first surfaced output line
> must be the most informative one". The spec is normative for everything it
> is current on; it is not current here. Do not "restore" one-line/most-
> informative selection while reconciling the rest of the spec.

- **Head** = the literal first **2** buffer lines; **tail** = the literal
  last **3** buffer lines; both strictly in buffer order. The tail is where
  test summaries and errors live, so it gets the larger share.
- The fold marker sits **between** head and tail and carries the hidden
  count: `… K more lines · ctrl+o expand`, with `K = total − head − tail`.
- `└` elbow on the first preview line; sibling preview lines (rest of head,
  marker, tail) are indented two extra spaces to align under it.
- Outputs short enough to fit (≤ the collapsed row budget, or ≤ head+tail
  lines) render whole with no marker — head and tail can never overlap.
- Head 2 / tail 3 keeps the whole collapsed cell (header + 6 preview rows)
  inside the default 10-row collapsed budget (`TOOL_CALL_MAX_LINES`).
- The buffer both views render is normalized once at ingest: the leading
  `exit N` status row run_shell emits is stripped there (the header owns
  exit status) and trailing whitespace padding is never stored, so the
  collapsed and expanded views agree on line count and order by
  construction. The expanded view is the full buffer, in buffer order.
- The header status uses the canonical effective tool outcome. A nonzero
  `exit_code` is always failure, including for legacy events that also carry
  `ok: true`; raw legacy metadata cannot turn `exit 101` into a successful
  `Ran` block, suppress a failed extension result, or produce a passing recap.

### Fold

- **One** fold key: `ctrl+o`. **Global toggle** (issue #49), not a per-cell
  gesture: one press expands every foldable cell in the transcript at once
  (tool output, reasoning, diffs); the next press collapses them all
  together. No per-cell targeting and no invisible "nearest to viewport
  center" heuristic — this is deliberate: mouse capture is off (native
  selection and native scrollback stay intact outside of resize
  reconciliation, see the Mouse section), so there is no honest per-cell
  input method, and a predictable global state beats an invisible one.
  Native scrollback and `ctrl+f` search remain the navigation tools; `ctrl+o`
  only decides how much of each cell is showing.
- Search and other read-only modes must not mutate fold state.

### Typography

- One mono family (the terminal’s). Hierarchy from color and **weight**, never size.
- **Bold** only for: user messages, markdown headings, picker/approval titles,
  and the Codex tool verb that opens a ledger row (`Explored`, `Read`,
  `Search`, `List`, `Git`, `Ran`, `Edited`, `Wrote`, `Deleted`, `Changed`).
  The verb is a **closed set** (`CODEX_VERBS`) — capitalization alone does not
  earn bold, or titles like `File added …` and uppercase filenames would take
  it. Only the verb is bold; the target keeps the row’s own weight.
- **Plan exception:** the active `in_progress` checklist step is bold so the
  next action is scannable; completed/pending steps are dim, and completed
  step text is crossed out.
- **No bold inside code.**
- Italic only where specified (e.g. reasoning, hunk headers, comments).

### Color roles and themes

Semantic roles (stable across themes):

- **user / success**
- **failure / denial** (never decoration)
- **attention / activity** (spinners, pending, interrupts, cursor)
- **read / reference / companion** (non-destructive verbs, links, companion rail)

Structural / neutral tokens: `fg`, `dim`, `faint`, `hairline`, `bg`, `bg-inset`,
`select`, `user-rail` (and dimmed rail for queued input). These are not a fifth
semantic “meaning” role; they are chrome/structure.

Theme profiles supply concrete colors for roles and structural tokens only.
Renderers must not hardcode palette hex. No-color and ASCII modes must remain
legible via glyphs and weight (see glyph fallbacks in the Warm Ledger plan).

### Startup banner

- Keep the existing pixel wordmark, stripe mark, and
  `e^(iπ) + 1 = 0 · vN` tagline **exactly**.
- No orientation/help line under the banner: the wordmark and caption stand
  alone (startup declutter, #21; `banner_has_no_orientation_line` enforces
  the absence). Session ids live in `/status` and resume copy, not here.
- Do not replace the pixel banner with a Concepts-board simplified header.

### Composer and footer

- Composer: left rail + user-role text, no box. An empty composer shows the
  rail and a dim cursor, nothing else — no ghost copy (startup declutter,
  #21; the deny-with-instruction ghost is separate and stays). While the
  agent works, the rail dims and shows
  working/interrupt copy; typing remains accepted. After an active model run
  is durably admitted, submitting composer input opens an explicit choice:
  **Steer** delivers it within that run at the next safe round boundary, while
  **Follow up** allocates a distinct queued run. Follow-up is the safe default;
  the UI never infers one mode from timing. Selected steering rows form one
  ordered group and the running model turn absorbs them exactly once as
  canonical `user.message` events (the model sees them in-turn;
  docs/contracts/events.md). The surface
  installs the worker as in-flight immediately, without waiting on provenance
  I/O. Until the worker reports that the initial `run.started + user.message`
  admission is durable and Core has atomically opened that exact run's queue
  identity and group, the surface is explicitly not steering-capable and a
  submit is refused with a clear notice while its text remains in the
  composer. Processing that worker report opens the steering affordance
  asynchronously, after which the user can make that explicit choice for the
  retained text. The choice freezes the run observed at submit time for both
  modes. If that run terminalizes before the choice is persisted, Core returns
  a typed stale-run failure and the UI restores the draft; it never strips the
  source identity and reinterprets the input as an idle follow-up. Render,
  Escape, and composer editing must remain available while
  admission waits on compaction or fsync; no queue mode is guessed in the
  startup window, and an explicitly selected steering enqueue is never
  reinterpreted by Core.
  Deny-with-instruction is the one specialized input path: it is guidance for
  the exact run currently blocked by the displayed permission request, not a
  new composer message. The UI captures that run when the ask opens, queues
  the instruction at the front as same-run steering, and sends the denial only
  after the enqueue is durable. A missing, changed, or terminal run retains the
  instruction and prompt; timing never converts it to a follow-up.
  A completed
  no-tool response is a boundary too: if steering arrived while its final
  text streamed, the worker stays active and dispatches the hydrated stack in
  the next model request. Escape pauses absorption while cancellation wins;
  terminal admission then cancels every undelivered steering row for that
  exact run before recording `run.terminal`. Cancelled steering is never
  silently rebound as a follow-up or replacement run. The UI exposes each
  terminal-cancelled steering row through an explicit private recovery choice:
  requeue the original as a follow-up, edit then requeue it, or dismiss it.
  Escape keeps the recovery pending, and an empty submit reopens its decision.
  The same recovery projection and FIFO order are restored on resume. A live
  persistence failure leaves the recovery worker on the exact retained Core
  batch, including its original target and replacement identity; the UI does
  not restore or resubmit draft state while that outcome remains ambiguous.
  An explicit empty-submit
  continue dispatches only a pending follow-up head. Dispatch reserves the
  head without removing it; only the durable
  initial `user.message` acknowledges the reservation. An append failure
  leaves that exact reserved follow-up row intact. The dispatch request carries
  the snapshotted head `queue_id`; if the head moved, Core returns a typed
  head-changed error and dispatches neither row. A matching head that is only
  behind an active queue write is reported as temporarily unavailable, not as
  a different head. A latched context stop does
  not auto-flush pending follow-ups; terminal admission still cancels the
  ended run's steering rows as described above. After provenance is repaired,
  retry reuses the retained event identity and accepts the head exactly once
  on both the live bus and durable log, including when
  the failed sync left a complete physical line behind. A globally unique
  queue-row ULID is the complete identity, so another queue's same-shaped row
  has a different id and cannot claim or acknowledge it. The pending owner is
  installed before an older accepted backlog is flushed; a backlog failure
  therefore protects the same row as a candidate failure. While that admission
  remains unresolved, `/new`, `/resume`, and queue clear refuse to detach or
  discard it. More generally, `/new` and `/resume` reconcile the accepted feed
  and refuse while admission, enqueue, replace/cancel, terminal, or scrub
  persistence is active, waiting for writer order, or retained for retry.
  Invalid accepted lifecycle state is reopen-only and also blocks replacement.
  An ambiguous child-authored companion/reviewer append is likewise
  restart-only: auto-flush and deferred extension/companion dispatch leave
  their queues untouched, and the UI must tell the user to stop and restart
  Euler before reopening the session. In-process `/new` or `/resume` cannot
  bypass that authoritative fence.
  During an accepted replacement, cloned submitters remain fenced from the
  old-session clear through the state swap and durable bind of the new session;
  only then may input reopen. One live Session cannot install a second queue
  object, and ordinary bind cannot move one queue to a different
  writer/session/agent owner; only the lifecycle-transition guard authorizes
  that owner swap. Bind and queued-dispatch canonicalization are transactional:
  an authority mismatch or missing reserved row leaves the queue's entries,
  reservation, and current authority unchanged. If bind fails before the app
  swaps session state, dropping the transition reopens the still-current,
  durably cleared old owner. If authority or app-state replacement may already
  have occurred, failure stays closed rather than falling back to a detached
  writer.
  Mid-turn absorption uses the same admission transaction without
  holding the queue lock during persistence. The queue
  hydrates each accepted steering row at the next model-round boundary; it
  never waits for a later tool round when a boundary is already available.
  Completion auto-flush does not start another turn while the context latch is
  active. Ordinary input queued while non-model work is active remains a
  separate follow-up and is never inferred to be steering. Entries arriving
  after the worker's terminal boundary flush into the next turn. The final
  steering check and group close are one named transaction: same-turn idle
  work runs before it, and only a final Stop closes the group. Pending rows
  are absorbed there only if another model request is available. When an
  explicit round ceiling has consumed its final request, the same atomic
  transaction closes the group without persisting steering the turn cannot
  observe: input linearized before the close stays deferred, and input after
  the close is a follow-up. Cancellation has precedence over a coincident
  round-limit completion. A follow-up whose source run completed normally may
  auto-dispatch. A follow-up whose source run failed, was cancelled, or was
  interrupted remains queued behind an explicit confirmation; the same gate
  applies to auto-dispatch and empty-submit continue. Queued rows show one
  visual line with stable FIFO position, mode, and a bounded source/planned-run
  identity before the message preview. The message body is capped at
  64 terminal display cells, truncates at a word boundary with an ASCII
  ` ...` suffix, and never alters the full queued input. Two fallbacks apply at
  tight widths: when the first word alone exceeds the budget there is no word
  boundary to keep, so the preview hard-cuts mid-word before appending the
  suffix; and when the budget is four cells or fewer (at or below the suffix
  width) the ` ...` suffix is dropped and the body is a bare hard cut. The
  currently targeted queue row carries an explicit `›` marker; Left/Right
  moves that marker so recall, replace, and unqueue never rely on color alone.
  The running footer hint reads `⏎ submit`, which describes the key action
  without promising admission or a queue mode: submission may open the
  steer-versus-follow-up chooser, default to a follow-up for non-model work,
  or remain in the composer when pre-admission refuses it.
- Skill commands: each accepted frozen skill contributes one dynamic
  `/skill:<name>` palette row using its catalog description. Selecting it with
  optional request text submits the canonical literal command through the same
  idle, steering, or queued-follow-up path as ordinary input. The TUI keeps a
  cache of the immutable session catalog while the session runs on a worker;
  core admission remains authoritative and rejects stale or unavailable names
  without starting a turn. Transcript and composer history show the compact
  literal command. The exact expanded model input remains inspectable in the
  event's `model_content` and provenance blob when externalized.
- Footer: **one** line below the composer — two hard-edged clusters:
  contextual hints then `cwd (branch)` flush-left; `model · ctx N%` with an
  optional `· $N.NNN` (plus the session name once named) flush-right. The cost
  chip follows absence over punctuation: it renders only when the priced cost
  subtotal is greater than zero, as the plain `$N.NNN`. A zero subtotal (a
  genuinely free session, no calls yet, or only unpriced calls) shows no chip at
  all: no `$0`, no `$?`, no placeholder, and the ` · ` separator never orphans.
  The mixed case (a nonzero priced subtotal alongside some unpriced calls) shows
  that plain subtotal, unmarked. The footer carries no unknown or partial
  markers; quote completeness and unpriced call counts stay explicit in
  `/usage`, which keeps `$? (N unpriced calls)` for the all-unpriced history and
  `$N.NNN+ (N unpriced calls)` for the partial one. The subtotal is cumulative USD over
  valid persisted `model.result.cost` snapshots in the session, including
  companion calls. The model-result emission boundary computes each snapshot
  once from disjoint usage buckets and the exact resolved catalog schedule;
  live display, resume, and replay validate the saved component arithmetic
  against the saved usage and selected rates, then sum the persisted integer
  components. Only the primary session actor updates the active `ctx` counters;
  companion and reviewer calls contribute cost without replacing that reading.
  Catalog refresh never reprices history. Subscription-backed ChatGPT shows an
  equivalent API-price estimate when its catalog entry has a quote; this is
  neither an invoice nor a claim about incremental spend. No session id in the
  footer — ids live in `/status` and resume copy (#21). Ctx% uses attention at
  ≥70% and failure at ≥85%. Canvas compaction data, including demotion counts
  and tier, remains canonical `canvas.snapshot` provenance (events contract),
  not footer state. No second status row; detail lives under `/status`.

### Escape and interruption

- Keyboard input is dispatched from the topmost interactive layer inward.
  When a slash palette, picker, search surface, prompt, or modal owns input,
  `Esc` dismisses exactly that layer and cannot publish turn cancellation.
  One keypress performs one layer transition. Only `Esc` received with the
  composer owning input may interrupt an active turn or an idle shadow
  compaction.
- Publishing root-turn cancellation atomically pauses the steering queue
  before setting the shared signal. Neither operation waits for provenance
  I/O: absorption reserves an id under a short queue lock, persists its owned
  copy without the lock, then commits by id. A steer either reserved before
  the pause and may finish as durable evidence, or remains queued after Esc.
  The loop rechecks cancellation after that commit, so a late append cannot
  start another provider round or consume later input.
  Explicitly queued companion/extension activities are cleared with a visible
  notice; queued user/steering text is preserved.
- The provider request driver and tool supervisor observe that same signal.
  The session/UI stops waiting even when a synchronous provider adapter is
  blocked, and provider events observed after cancellation are rejected at the
  request boundary. Built-in HTTP and WebSocket adapters additionally use
  bounded socket I/O and cancellation-aware polling, so a connected detached
  worker leaves the transport within the adapter bound; the WebSocket path
  actively shuts down its control socket. OS name resolution or an arbitrary
  compatibility provider that does not implement the provider liveness
  contract can only be detached, with no route back into the session.
  Transport heartbeats and attempt-control signals never become
  transcript/model content or meaningful progress.
  A `model.call` that had no `model.result` receives exactly one parented,
  cancellation-attributed session error. That error is canonical provenance,
  while the TUI renders its ordinary interruption row instead of treating it
  as a driver failure. If the root turn and a shadow compaction are both
  running, root cancellation also terminalizes and fences the shadow before
  the session returns; late output from either provider call cannot append
  events or swap the canvas. A shadow whose physical attempt has already
  published its content-free terminal boundary settles its result and usage
  before interruption closes it, but interruption still discards the candidate
  rather than swapping the canvas. Before that boundary, the ordinary finite
  grace and detach behavior preserves prompt cancellation for a genuinely
  blocked compatibility provider.
- A permission ask observes the same signal. Cancellation closes the active
  prompt without converting it into denial, installs no grant, and cannot let a
  stale modal reply satisfy a later ask. Write tools recheck cancellation at
  their final filesystem-mutation boundary.
- Explicit companion runs and managed-process extension commands observe the
  same signal. The host makes a best-effort protocol cancellation notification
  to a managed peer before killing its process group; a cancelled companion
  still records its terminal `agent.result`.
- Agent subprocesses run in their own process group. Cancellation kills the
  still-owned group before reaping its leader, covering the leader and ordinary
  descendants that remain in that group, without waiting for the tool timeout.
  A descendant that deliberately moves to another process group is outside
  this ownership guarantee. Every already-recorded call without a terminal
  result receives exactly one cancelled `tool.result`; partial subprocess
  output and workspace changes completed before termination remain canonical
  evidence and are never reported as successful completion. After process
  termination, ordinary `run_shell` performs its existing bounded evidence
  scan (4,096 files, 256 KiB per file, 64 MiB total) before closing the result;
  that finite scan may make transcript closure follow the process stop.

### Streaming, scroll, motion

- Prose streams with progressive markdown styling; once a line has painted it
  does not reflow except on explicit fold/unfold.
- Spinner ≤10 fps; elapsed counters update once per second; live output tails
  are at most **two** lines and replace in place.
- Reduced-motion: static `·` instead of spinner.
- If the user scrolls up, streaming must not yank the viewport. Show a faint
  `↓ N new events` pill above the composer; End key or send in composer
  dismisses it.

### Degradation

- Under 100 columns: drop timestamp gutter first, then right-aligned palette
  summaries; approval panel goes full-width with consequences wrapping.
- Without unicode: ASCII glyph fallbacks.
- Without color: semantics via glyphs and weight only.
- Light themes: invert neutral lightness, keep role hues; validate before ship.

### Mouse

Mouse capture is deliberately off (terminal enter-session modes never emit
`\x1b[?1000h`/`\x1b[?1006h`) so the terminal's native text selection and
native scrollback stay usable — copying transcript text and scrolling back
through history work as they would in any other CLI output, **with the one
exception below for resize reconciliation**. A practical consequence:
crossterm never delivers mouse events in a real terminal, so click/drag is
not a supported input path — there is no click-to-expand affordance.
`ctrl+o` (global fold toggle) and `ctrl+f` (search) are the supported
disclosure and navigation controls.

**Resize exception — settled-resize scrollback purge (issue #38).** Per-tick
incremental append during a resize corrupted output in all three major
terminals tested (Ghostty, iTerm2, Terminal.app): stale-viewport re-renders
scrolled prior rows into native scrollback, accumulating one fossil
transcript copy per width tick. There is no terminal escape/control sequence
that scopes a scrollback purge to "only euler's rows" — `ESC[3J` (and the
native scrollback buffer generally) is all-or-nothing per terminal session.
Given that constraint, the mechanism is: intermediate resize ticks re-render
the live viewport only (no scrollback writes); once the resize settles (a
450ms trailing debounce with no further resize events), euler runs exactly
ONE purge+replay — it clears the entire native scrollback buffer (`ESC[2J`
+ `ESC[3J`), **including any content the user had in their terminal before
euler started**, and re-emits euler's own transcript from its internal
event-log model at the settled width. This is a deliberate, disclosed
trade-off, not an oversight: it is strictly better than the fossil-copy
corruption it replaces, but it does mean a user who resizes their terminal
loses pre-euler scrollback history.

Full-repaint invariants (settled-resize replay, `ctrl+o` fold toggle, theme
switch, resume — anything that clears the surface and rebuilds it):

- **Live geometry.** A repaint reads the terminal's live dimensions at the
  moment it runs; a resize event only updates the cached size and schedules
  the repaint. No repaint may consume dimensions older than the most recent
  resize event already drained.
- **Fresh anchor.** The anchor is recomputed from scratch: content shorter
  than the screen with nothing committed above is top-anchored (the
  session-start layout); otherwise the bottom chrome pins to the screen
  bottom.
- **Every row painted.** Rows the repaint does not cover are painted with
  the theme background — never left as terminal-default voids.
- **Re-emission prints through the screen.** After the clear, the committed
  history prefix is re-emitted by printing rows through the screen so they
  physically flow into native scrollback. The scroll-region linefeed bridge
  is only valid incrementally, when the region above the bottom band still
  holds the previously committed rows; used right after a clear it scrolls
  blank rows into scrollback while the viewport draw overpaints the rows it
  wrote, destroying the history head.
- **Theme switch history policy.** A theme switch is a full-repaint
  consumer: the whole region repaints and history is re-emitted in the new
  theme. History above the fold must remain reachable in scrollback after
  the switch — old-theme cells are acceptable, a purged-to-void history is
  not.

> **Owner-acceptance pending (real-terminal dogfood).** This mechanism is
> PTY-tested (see the drag-resize test in `tests/headless.rs`) but has not
> yet been hands-on validated by the owner in real terminal emulators
> (Ghostty, iTerm2, Terminal.app). Treat the purge-on-settled-resize
> behavior above as a disclosed, not-yet-fully-settled trade-off until that
> dogfood pass confirms it reads correctly outside of PTY harness
> reconstruction.

## Transcript event model

The terminal UI renders the canonical session event stream from
`docs/contracts/events.md` as an ordered ledger. Avoid permanent sidebars,
dashboards, and boilerplate panels in the core CLI.

A failed, cancelled, or resume-interrupted root response with durable text
checkpoints renders that exact partial text as one incomplete assistant cell,
alongside its canonical status and error source/message. It offers only real
recovery affordances: `Ctrl+Shift+C` or `/copy` copies the retained text, and
the user may submit a new instruction to continue or retry. Euler never
silently retries after visible output and never injects a recovered draft into
the model canvas. A completed response follows the ordinary
`model.result`/`assistant.message` path with no partial duplicate. Malformed,
cross-actor, and child checkpoints render no assistant prose; transcript and
resume share the same core-owned protocol fold.

Pending queue state is a private projection of canonical `queue.*` events,
not transcript history. `queue.enqueued` and `queue.replaced` content may be
shown only in the dedicated queued-composer surface; `run.*` and `queue.*`
events do not produce ledger rows or assistant/user prose. The content enters
the ledger only through the `user.message` accepted in its delivery batch.
Interactive enqueue and cancel persistence runs on one serialized background
boundary, never on the terminal event loop. Rendering, composer editing, and
Escape remain live while it saves. Distinct rapid submits stage in request
order, leave the composer free for the next draft, and appear only as labelled
`saving` rows until each append is reconciled. They are not acknowledged as
durable queue rows. The visible queue applies staged positions to the last
reconciled canonical snapshot, so a worker commit cannot duplicate a row or
move a front insertion only after acknowledgment. If append or sync has an
ambiguous outcome, the same worker retries Core's retained exact enqueue or
change batch with bounded backoff. The row stays labelled `saving`; no new id,
timestamp, parent, payload, or fresh fenced mutation is generated, and the UI
never tells the user to resubmit it. Later staged commands remain behind that
retry. A definitive non-ambiguous rejection or stopped worker returns an
unaccepted draft to its owning composer in request order. Cancellation of an
approval preserves an unaccepted denial draft, and a repeated cancellation of
the same stable row is refused. Orderly shutdown waits within its cleanup bound
for staged or exact-retry mutations and stays open after a timeout. It never
discards process-private accepted input merely because quit was requested
again.
Session replacement waits for staged mutations as well as core queue writes,
so an accepted worker command cannot cross into the replacement session.
Unqueue, clear, and replace must persist their lifecycle events before changing
the displayed queue. A persistence failure leaves the row and selection intact
and surfaces the typed operation error; it must not create a hidden durable
item or an optimistic edit that resume would reverse.

Up-arrow recall selects the visible row by stable `queue_id` without removing
it. Edited submit uses canonical `queue.replaced`, preserving FIFO position,
mode, planned run, and source run while allocating the replacement id. Escape
or explicitly clearing the recalled draft abandons edit mode and leaves the
original row pending. Up within a recalled multiline or wrapped draft moves
only the visual composer cursor and retains the edit identity; it never recalls
history or abandons the row. Unqueue likewise selects the stable id; simultaneous replace/unqueue
requests for the same id are refused instead of retargeting a successor. The
dedicated queue surface rehydrates canonical pending FIFO rows and private
recoverable rows on resume; it is a projection over Core state, not a second
queue machine.

Queued-turn dispatch canonicalizes the reserved row against the newly bound
durable projection before using its text anywhere. Composer history, ledger
projection, and the model prompt are populated only from that returned
canonical row. Once that dispatch is installed, the Session core ignores any
detached caller-supplied prompt and refreshes the reservation from queue
authority again at admission. A scrub between binding and admission therefore
cannot let a pre-scrub queue clone briefly reveal or submit stale content.
The UI passes only the expected head id into this path; prompt bytes remain
owned by the canonical Core row.
After every live scrub outcome, including a fail-closed persistence error, the
UI discards or refreshes all process-private content clones from the scrubbed
Core projection: recoverable rows, an open recovery modal/edit, queue-mode
content, stashed and failed drafts, composer history, clipboard cache, activity,
transcript, and canvas. Escape from a recovery modal followed by `/scrub` and
an empty submit must therefore reopen only the scrubbed canonical recovery;
requeue can never restore the pre-scrub clone. Closed-session scrub has no live
UI cache and continues to rebuild solely from rewritten durable surfaces.

## Activity and thinking

The pinned Activity block has one deterministic, replayable projection of the
canonical session event stream, not an independent activity log. While a root
turn is live, the process-local provider observer may refine that same
projection with content-free attempt stages (waiting for headers, first byte,
or semantic output; retrying; timed out; cancelled). Those controls are an
ephemeral overlay: they are never inserted into the transcript, visual-canvas
history, provenance, or model canvas, and replay ignores them. Companion,
reviewer, and compaction scopes never alter the foreground block. The block is
one or two lines: its first line carries the spinner (or stall marker),
high-level phase, **phase age**, and the sole esc-to-interrupt affordance; an
optional second line carries changed-file aggregation, the latest completed
milestone, and **time since meaningful progress**. The esc affordance appears
there exactly once — nowhere else, including the transcript's live thinking
header.

Phase age and progress age are separate event-timestamp clocks. Accepted user
input, response text/reasoning delta kinds, completed tools/checks, file
changes, and terminal outcomes establish meaningful progress. Context
assembly, a new model call, and root provider attempt/control liveness may
advance the phase or last-observed-event clock but do not reset meaningful
progress or replace the latest completed milestone. A retry therefore remains
visibly stalled when no substantive work preceded it within the threshold,
even though its phase age restarts. A stall becomes visible after 30 seconds
without meaningful progress in a phase that can advance; waiting for user
approval is exempt. Replayed events use their provenance timestamps, while
live rendering injects only the current clock used to calculate ages.
Durable response checkpoints update observed-event liveness only; because they
mirror already-observed text, they do not independently reset meaningful
progress or expose their content in the Activity block.

Ordinary extension, guardian, and nonterminal session errors are failed
operation milestones, not authority to terminalize the Activity projection.
The worker's run outcome owns completed/failed/cancelled terminal state. A
provider-terminal error may retain the existing short `turn failed — waiting
for cleanup` gap while the worker returns session ownership.

Concurrent tool calls are grouped into one stable high-level phase and retain
their batch peak until the batch settles (`Inspecting 5 files`, `Editing 3
files`, `Running 2 checks`). The projection may classify commands into broad
observable families such as inspection, editing, checks/tests, and Git
publication, but it never renders command output or model content. Legacy
shell/check results are successful only when `ok` is true and any present
`exit_code` is zero; a nonzero process exit remains a visible failure even if
an older event recorded `ok: true`.

The Activity block never carries reasoning body text. A text or reasoning
`model.delta` kind can establish the body-free `Receiving model response`
phase and meaningful progress, but the projection does not inspect or render
the delta. A provider-opaque reasoning artifact may update the last-observed
event clock, but it never establishes meaningful progress and remains
unavailable to core UI.

Reasoning TEXT is owned solely by the transcript: a separate collapsible
ledger element driven by `model.reasoning`, subject to the reasoning policy
below.

Reasoning has three states, all riding the same single continuous
**hairline** rail (`▏`, faint gutter color) — never a per-line `|` pipe
(one vertical rule regardless of wrap, distinct by weight from the bold
user `▌` rail, and never a box-drawing border; `│` and friends stay
reserved for the approval panel):

- **Streaming (live)**: while reasoning deltas arrive, the header
  `✱ thinking · Ns` shows the event-derived elapsed time (no esc hint —
  that affordance belongs to the HUD) and the text streamed so far types
  out behind the hairline. This live body is **bounded**: a trailing tail
  window (a fixed char cap, ~16 wrapped lines), never the unbounded full
  stream — the finalized `model.reasoning` event carries the full text, so
  the bound never truncates the committed/expanded thought. The body is
  also **viewport-only** (the transient/mutable inline region): it must
  never commit to native scrollback row-by-row.
- **Collapsed gist**: on finalize the live body is replaced by a single
  dim committed line (`✱ thought for Ns — gist · ctrl+o expand`), no
  rail. This one line is the only reasoning content that enters
  scrollback.
- **Expanded body**: `ctrl+o` reveals the full finalized body indented
  behind the hairline, wrapped at the rail-relative width.

## Canvas separation

Visible terminal activity is **not** automatically part of the next model
canvas. The canvas assembler decides what matters for the next model action
(ADR 0002). Queued input, denials, recaps, and UI toggles must not leak into
canvas except through canonical events and canvas policy. In particular,
pending `queue.enqueued`/`queue.replaced` content is excluded; only its
delivered canonical `user.message` is eligible.

## Provider reasoning

- Render only adapter-classified **user-displayable, taint-safe** reasoning
  (summary and/or allowed raw). Collapsible and bounded.
- **Never** render provider-opaque/encrypted/signature-only artifacts in core UI.
- Do not require providers to expose reasoning; degrade to no reasoning UI.
- Provenance may still store maximum fidelity for the owning adapter; storage ≠ display ≠ canvas.

## Permissions UI

Approval is the only bordered flow element. Decision records stay in the
ledger. Scoped grants and project persistence are defined in
`docs/contracts/capabilities.md` (when extended); the UI must not claim a
scope the gate did not grant.

An extension command with several uncovered static capabilities renders one
operation panel that names the command and lists every requested capability.
Its choices are allow once, allow every listed capability for this session, or
deny. The corresponding ledger entries remain separate per-capability
decisions. Project and durable-user choices are absent from this panel because
the grouped operation has no single narrow subject for those scopes.

The extension install consent card (capabilities contract, "Install consent")
uses this same bordered approval treatment, in the TUI and in the CLI verbs
alike: source and pin, declared build argvs, required toolchains, and provided
extension ids with capability envelopes, shown before anything fetches or
builds. Extension listing surfaces must show distribution provenance —
source and pin for installed extensions, an explicit linked-path marker for
linked ones — so a released extension and a dev tree are never visually
interchangeable.

The project-context acknowledgment card and the resume relocation-consent card
(project-context contract, "Acknowledgment record" and "Resume relocation and
consent") use this same bordered approval treatment. Both are pre-session cards:
they are presented before the session is constructed, because the decision
determines the immutable bootstrap the session records at `session.start`, so
they render on a self-contained inline surface rather than as an in-transcript
modal. Both are single-keypress with a safe-bias default highlight (Skip for the
acknowledgment card, Cancel for the relocation card). Because this system has no
horizontal button row, the choices are a stacked single-key list, exactly like
the permission panels.

The acknowledgment card distinguishes candidates that were omitted from
skills that were admitted with a compatibility advisory. It never describes
an admitted skill as skipped. `skill_name_directory_mismatch` warnings are
counted only after final catalog admission and render separately from the
skipped count.

The relocation card's content is facts only, never a guessed reason for the
change: the recorded workspace path, the current workspace path, and when the
session was last active, followed by a plain-language statement that resuming
here adopts the current folder for this session, that approvals from the old
folder (project grants and project-context acknowledgments) do not carry over,
and that the session keeps the project guidance it started with. Declining
changes nothing. Example copy:

```
╭─ This session last ran in a different folder ─────────────────────────╮
│ This session last ran in a different folder                           │
│                                                                       │
│   Last ran in:   /home/ada/projects/euler                             │
│   Now opening:   /home/ada/projects/euler-fork                        │
│   Last active:   2026-07-19 14:32                                     │
│                                                                       │
│ Resuming here makes this folder the session's home from now on.       │
│ Approvals from the old folder don't carry over: this folder keeps its │
│ own permissions and its own answer about loading project guidance.    │
│ The session keeps the guidance it already loaded. The new folder's    │
│ EULER.md isn't read until you start a new session here.               │
│                                                                       │
│   r  Resume here                                                      │
│ › n  Cancel (leave the session where it was)                          │
╰───────────────────────────────────────────────────────────────────────╯
```

Headless resume never shows this card. It fails closed with plain-language
remediation, for example:

```
Cannot resume: this session was recorded in /home/ada/projects/euler, but the
current folder is /home/ada/projects/euler-fork. Re-run from the recorded
folder, start a new session here, or pass --accept-relocation to move this
session to the current folder.
```

`/permissions` offers session-local postures before its advanced
per-capability controls: **Read only** permits `fs-read`, `provenance-read`,
and `diagnostics-read`; **Ask every time** puts every capability in `ask`; and
**Full access (unsandboxed)** permits every capability for the current
session. **Auto in workspace sandbox** stays visibly unavailable until an
enforced Linux workspace-sandbox backend exists. A permission posture is not
a sandbox claim and does not override secret/config guardrails. Applying a
posture clears transient session grants, so a prior session approval cannot
silently survive a switch to **Ask every time**; explicit project/user rules
remain separately visible in the advanced controls.

Runs that did not prompt render their provenance as a dim tag on the tool
header instead of a standalone decision record: `· session grant` /
`· project grant` for covered grants, `· safe` for static-safety
auto-approvals (whose `mode: "static-safe"` decision events are suppressed
from the transcript, like extension `static-grant` records).

## Non-goals

Do not build:

- sidebars or a dashboard-first core CLI,
- a second chat pane for companions (nested sub-ledger only),
- persistent chat UI state independent of provenance,
- provider-specific reasoning UX that breaks when reasoning is absent,
- parallel expand keys or permanent dual chrome (boxes + flat ledger).

## Web UI and rich visualization

Sidebars, dashboards, timelines, graphs, and rich visualization belong outside
the core CLI — extensions or companion processes reading bounded
provenance/projection APIs.
