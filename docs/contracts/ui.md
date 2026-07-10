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

### Fold

- **One** fold key: `ctrl+o`.
- Target: the foldable block whose vertical span is closest to the **viewport
  center** (tie → later block). No second expand affordance.
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

The live terminal may enable mouse capture so wheel events drive in-app
transcript scroll while streaming. Wheel = scroll intent only; clicks/drags are
not semantic input unless a focused surface owns them.

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
