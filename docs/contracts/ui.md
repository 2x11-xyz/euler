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

- Default **two-column** transcript: fixed **9-character** timestamp gutter
  (`HH:MM:SS`, faint) + content column. Gutter is toggleable via user pref
  (`/timestamps`); hairlines and layout rules otherwise unchanged.
- **Hairline** under each **meaningful** event. Meaningful events include:
  user message, assistant prose block, tool group, decision record, companion
  block, resume boundary, interrupt/failure top-level records.
- **Not** meaningful (no own timestamp/hairline): tool children, output tails,
  live shell tail lines, nested thinking body, queued-input rows.
- **No box-drawing borders** in the flow. **Exception:** approval panels
  (single 1px attention-role border).
- Tool groups use lowercase verb headers (e.g. `explore · N steps · Ts`) and
  `├` / `└` children with per-step result data.
- Fold marker language: `… N more lines · ctrl+o expand` (and matching collapse).

### Collapsed tool output preview (v4 spec amendment)

Collapsed tool-run blocks use the Codex head+tail preview model. This
supersedes the earlier "exactly one `└ ` result line" rule (review v2 §14.2)
and its most-informative-line scoring: the collapsed preview never selects,
promotes, or reorders lines.

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
- **Bold** only for: user messages, markdown headings, picker/approval titles.
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
- Add one faint help line with exact copy:
  `new session eNNNN · resumable with /resume · / for commands`.
- Do not replace the pixel banner with a Concepts-board simplified header.

### Composer and footer

- Composer: left rail + user-role text, no box. Empty ghost:
  `message euler · / commands`. While the agent works, rail dims and shows
  working/interrupt copy; typing remains accepted (queue when appropriate).
- Footer: **one** line below the composer — key hints left; session identity
  right (`eNNNN · model · ctx N% · branch`). Ctx% uses attention at ≥70% and
  failure at ≥85%. No second status row; detail lives under `/status`.

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

## Activity and thinking

Short activity/status lines may show intent while the agent works (not boxed
chrome). Full provider-exposed reasoning is a separate collapsible ledger
element driven by `model.reasoning`, subject to the reasoning policy below.

## Canvas separation

Visible terminal activity is **not** automatically part of the next model
canvas. The canvas assembler decides what matters for the next model action
(ADR 0002). Queued input, denials, recaps, and UI toggles must not leak into
canvas except through canonical events and canvas policy.

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
