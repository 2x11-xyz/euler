# Warm Ledger TUI — gap map & implementation plan

**Date:** 2026-07-09  
**Sources:** `~/euler-agent-design-concept-spec.zip`  
- *Euler TUI Spec* (normative, option 3a) — **wins over mockups**  
- *Euler TUI Concepts* (visual board; non-normative where it conflicts)  
- screenshots: current boxed gruvbox TUI vs design board  

**Audience:** engineers and coding agents.

**Revision:** Incorporates aggressive GPT-5.5 plan audit (2026-07-09): normative
items promoted out of “parallel/if cheap,” foundation/diff/slash/resume
rescheduled, contract prerequisites and Spec-vs-Concepts traps locked.

---

## 0 · Intent

Ship **Warm Ledger**: a Codex-class chronological ledger transcript with Euler
activity affordances, quiet color *roles*, one fold key, always-live
composer, and scoped approvals — without sidebars, second chat panes, or
workflow logic in core.

This is a **multi-slice program on one long-lived branch** (`feat/warm-ledger-tui`).
Each slice must be shippable alone for dogfood. Several slices are pure
rendering; scoped grants, checkpoints, companions, and slash/extension surfaces
require core/contracts first.

### Process rules (session standing orders)

1. **LOC budget is PR-stage only.** Do not shape design or intermediate
   implementation around the core LOC ceiling or per-crate allocations. Measure
   and discuss budget only when the user is ready to open a PR — not while
   planning or mid-slice. Prefer good code (AGENTS.md Prime Directive) over
   premature shrinking.
1b. **No intermediate PRs.** All Warm Ledger work lands on a long-lived local
    branch. The user dogfoods Euler on that branch. Open a PR only when they
    say the branch looks good; merge only after that review.
2. **Mandatory GPT-5.5 review at each slice/milestone.** Before treating a
   plan, design delta, or implementation slice as done, run an opencode
   **worker** subagent (ChatGPT GPT-5.5, high reasoning) as an *aggressive
   reviewer* — not as an implementer. Brief it with: the slice goal, diff or
   plan text, relevant contracts/AGENTS.md constraints, and ask it to hunt
   direction drift, AGENTS.md violations, fake-to-pass structure, core/extension
   boundary breaks, and honesty hazards (permissions, checkpoints, canvas,
   reasoning taint). Incorporate blocking findings before moving on.
3. **Long interactive dogfood per visual/behavior slice.** Not a 30-second
   smoke. Drive real TUI sessions (fixture and, when needed, live provider):
   multi-turn work, tools, folds, approvals, queue, resume, narrow width,
   theme switches. Capture scrollback / screenshots for review. Prefer
   sessions long enough to hit streaming, scroll-stickiness, and failure paths.
4. **GPT-5.5 visual co-review per slice.** Same worker class reviews captured
   TUI output (text dumps and images) against Spec § and the Spec-wins table —
   not only the code diff. Blocking visual findings must be fixed before
   slice-complete.

### Theme vs layout (important)

**Warm Ledger is the layout and interaction system**, not a single locked
palette. Renderers bind to **semantic tokens** (`user` / `ok`, `fail`,
`attention`, `read` / `companion`, `fg` / `dim` / `faint` / `hairline` /
`bg` / `bg-inset` / `select`). A **theme profile** only supplies concrete
colors for those roles.

| Layer | Owns | Examples |
|-------|------|----------|
| Ledger system | Structure, glyphs, fold, chrome rules | Flat rows, nearest-block `ctrl+o`, no boxes except approval |
| Semantic tokens | Meaning of color | user/success; fail only; attention; read/companion |
| Theme profile | Hex/RGB for each token | `warm-ledger` (design-board reference), `gruvbox-dark/light` (shipped), later Solarized family, etc. |

The design board’s warm-ochre palette is the **reference mapping** for the
token set and contrast checks. It is not “Euler only ever looks like this.”
`/theme` selects catalog profiles; new families map the same roles without
forking renderers.

Today: `theme_catalog.rs` has only `gruvbox-dark` / `gruvbox-light`. Early
slices add `warm-ledger` as a profile and keep the catalog extensible.
**Light** variants: derive by inverting neutral lightness while keeping the
four role hues; validate against gruvbox-light before shipping (Spec §9). Defer
light if validation is not ready — do not ship an unvalidated light profile.

---

## 1 · Design principles (normative reminders)

| # | Principle | Implication |
|---|-----------|-------------|
| 1 | Transcript is a ledger | Fixed 9-char timestamp gutter + hairline per *meaningful* event; tool children nest without own stamps/hairlines |
| 2 | Quiet default, loud when it matters | Dim almost everything; color only via semantic roles |
| 3 | Text first, chrome last | **No boxes** in the flow; **only** approval panels may border |
| 4 | One fold key | Universal nearest-block `ctrl+o` only; never a second expand affordance |
| 5 | Nothing is lost | Interrupts, denials, reverts = ledger events, not erasures |
| 6 | Composer always alive | Type during stream/tools/approval; queue if busy |
| 7 | Themes are data | Layout never hardcodes palette hex; only semantic tokens |
| 8 | Typography discipline | One mono family; hierarchy from color/weight **never size**. Bold only for user messages, markdown headings, picker/approval titles. **No bold inside code.** Italic only where specified (reasoning, hunk headers, comments) |

**Semantic color roles** (stable across themes): user/success · failure/denial ·
attention/activity · read/reference/companion · neutrals. No fifth *role* for
decoration. Themes may remap hues for those roles; no-color/ASCII must still
carry meaning via glyphs and weight.

### Glyph vocabulary + ASCII fallbacks (Spec §2)

| Glyph | Meaning | ASCII fallback |
|-------|---------|----------------|
| `▌` | user rail | `\|` |
| `✱` | thinking | `*` |
| `⠧` (braille cycle) | spinner | rotating `-\|/` |
| `✓` / `✗` | outcome | `ok` / `x` |
| `◆` | companion | `&` |
| `↩` | revert | `<-` |
| `⚠` | provider warning | `!` |
| `■` | interrupt | (keep or `!`) |
| `❯` | prompt | `>` |
| `├` / `└` | tree | `+-` / `\-` |

### Spec wins over Concepts (known traps)

| Topic | Concepts (non-normative) | Spec (wins) |
|-------|--------------------------|-------------|
| Startup | Simplified `euler — ~/path` frame | Keep **existing** pixel wordmark, stripe mark, `e^(iπ) + 1 = 0 · vN` tagline **exactly**; add one faint help line |
| Composer ghost | Shows `message euler · / commands` | Same — empty ghost is **`message euler · / commands`** (Spec: “faint ghost message euler · / commands”) |
| Edit approval options | Often `y` / `a` / esc only | Include **`p` project grant** for edits under top-level dir |
| Exit resume line | `resume euler --resume …` | Copy-ready `euler --resume eNNNN` |
| Markdown | Plain frames | h1/h2 attention bold + hairline underline; code blocks `bg-inset` + faint language tag |
| Fold copy | Varied (`full command`, `review full diff`) | Universal fold language; markers `… N more lines · ctrl+o expand` unless approval hint still targets same fold |

---

## 2 · Gap map: design → current code → status

File roots: `crates/euler-cli/src/ui/` unless noted.

### 2.1 Foundations & restyle (spec §2–§4)

| Design surface | Spec | Current reality | Status | Primary files |
|----------------|------|-----------------|--------|---------------|
| Theme tokens | Semantic roles; profiles supply colors | Gruvbox only; hex partly scattered | **Partial** | `theme.rs`, `theme_catalog.rs` |
| Color semantics | Four roles + neutrals | Scopes exist; not hard roles | **Partial** | all renderers |
| Typography | Weight/bold rules above | Unenforced | **Missing** | renderers |
| Timestamp gutter | **Mandatory** 9-char `HH:MM:SS` faint; toggle later | `GUTTER_WIDTH = 4` quote rail; timestamps ignored | **Missing** | `text.rs`, `transcript/*` |
| Hairlines | Under each meaningful event; not under children | Box chrome instead | **Missing** | transcript cells |
| Flat ledger cells | No box borders; footer data moves **inline** | `┌─┐` artifacts; footer fields | **Conflict** | `transcript/cells/artifact.rs` |
| Tool vocabulary | `explore · N steps · Ts` + `├/└` aligned sub-verbs + per-step data | `Explored` / `Edited` / `Ran` boxed | **Restyle** | `activity.rs` |
| Shell live/done | `bash $ cmd`; spinner+elapsed; **≤2** live tail lines replace in place; done = most informative result line | Partial | **Partial** | `cells/shell.rs`, activity |
| Diffs (§4.1) | Line #, sign, tints, syntax on added only, hunk headers, shared with approval | Partial Codex-ish | **Partial** | `patch_diff.rs`, `file_diff.rs`, `syntax.rs` |
| Thinking | Live → collapse gist · `ctrl+o`; dim+italic | Present; not as specified | **Partial** | activity, markdown_stream |
| Markdown | Full §4 checklist | Exists; not Warm Ledger | **Restyle** | `markdown.rs`, `syntax.rs` |
| Composer | Ghost `message euler · / commands`; dim block cursor; working rail dims + `⠧ working · Ns · esc to interrupt`; shift+⏎ rail continues | Rail exists | **Polish + fixtures** | `composer*` |
| Footer | **One** line: hints left; `eNNNN · model · ctx N% · branch` right; ctx attention@≥70 fail@≥85; **no second status row** | Dual/richer status | **Restyle** | `status.rs`, `bottom_surface.rs` |
| Banner | Keep pixel art exactly; add exact faint `new session eNNNN · resumable with /resume · / for commands` | Banner exists | **Small add** | `banner.rs` |
| Slash palette | Gold match, Tab ghost, ⏎ arg dispatch vs picker, errors red + preserve input | Working | **Restyle + fixtures** | `commands.rs`, `bottom_surface.rs` |
| Fold affordance | **Nearest-block** `ctrl+o` (invariant) | Global `tool_artifacts_expanded` bool | **Wrong model** | `app.rs` |
| Glyphs + fallbacks | Table above | Partial | **Extend** | `glyphs.rs` |

**Meaningful events (hairline + timestamp)** — define in `ui.md` Slice 0:

- user message, assistant prose block, tool group, decision record, companion
  block, resume boundary, interrupt/failure records as top-level ledger rows.
- **Not** meaningful (no own stamp/hairline): tool children, output tails,
  live shell tail lines, thinking body when nested under thinking header,
  queued-message rows (dim rail only).

### 2.2 New features (spec §5) — all normative unless marked deferred by product

| Feature | Spec essentials | Current | Status | Blockers |
|---------|-----------------|---------|--------|----------|
| **5.1 Approval** | Only bordered UI; fixed content order; consequences row (write scope, network, est. duration, ran-before count; `unknown` never omit); `y/a/p/n`; default allow-once; hotkeys immediate; composer live; empty deny → ghost `denied — tell euler what to do instead`; hint ends `every decision is logged` | Cap-wide Allow/AllowSession/Deny; dual patch modal | **Major gap** | Scoped grant core; project config write recorded; shared diff renderer |
| **5.2 Queue** | Position index; ↑ recall last; ctrl+u unqueue **selected**; esc interrupt keeps queue; FIFO after turn; footer `⏎ queue · esc interrupt now` | Missing | **Missing** | Selection model (product); survival across quit (product) |
| **5.3 @ mentions** | Fuzzy, **gitignore-respected**; green in composer; context slot + **freshness guarantee** | Missing | **Missing** | Freshness definition (contract) |
| **5.4 Search** | Footer swap `find: · k/N`; select-bg matches; ⏎/shift+⏎; esc bottom; `!a`/`!f`; **read-only — never mutates or folds** | Missing | **Missing** | Scope of folded content (product) |
| **5.5 `/timestamps`** | Toggle gutter; user pref; faint confirm; **logged** | Missing | **Missing** | Pref + event kind |
| **5.6 Checkpoints** | Every **edit/write** pre-image; `· ckpt eNNNN`; picker: event id, action, path, time; restore ledger event; history intact | Hashes only | **Core missing** | Blob store; **deletes not in scope** unless product decides |
| **5.7 Recap + notify** | After Worked-for: file count + diffstat + test status if test-like + ctx%; faint file list; notify **only unfocused**, exactly 4 events, OSC 9 + bell fallback | Partial dividers | **Missing** | Focus detect; privacy policy |
| **5.8 Exit recap** | **≤5 lines**: saved + id, event count + files changed, `euler --resume eNNNN`, export cmd faint | Partial | **Partial** | Exit path |
| **5.9 Companion** | Teal rail; header; own ledger/perms note; ≤2 live tool rows; findings gold; collapse line; report re-enters main; asks bubble tagged; concurrent stack; true chrono interleave | Spawn exists; UI not nested | **UI + event model gap** | Multi-agent projection contract first |
| **5.10 Resume** | Three entry points; picker fields/groups/cap 20; `ctrl+o` **read-only ledger-tail preview**; mid-turn refusal copy; replay + honest fold boundary + warnings; post-fold ctx% | Picker shipped | **Normative polish** | Preview + restyle — **scheduled, not opportunistic** |
| **5.11 Slash set** | New: `/diff` `/rollback` `/timestamps` `/extension` manager `/dag` `/usage`; ext `⋄`, EXTENSIONS group, core wins collisions, disabled teaches | 18 cmds; `/extension run` only | **Normative** | SDK fields; manager boundary — **scheduled, not opportunistic** |

### 2.3 Failure, motion, keys, degradation (spec §6–§9)

| Area | Spec | Status |
|------|------|--------|
| Tool failure | `✗ exit N`; first line = informative pattern (`error[…]` / `FAILED`), not last line | **Partial** |
| Edit failure | Inline cause, never bare “failed” | **Partial** |
| Provider trouble | Gold in-place retry line; disappears on success (events remain); red on give-up | **Partial** |
| Interrupt | `■ interrupted — tell euler what to do differently`; partial output stays | **Partial** |
| Quit | Two-step ctrl+c with resume reassurance | **Check** |
| Streaming | Progressive md; painted lines never reflow except fold; spinner ≤10 fps; elapsed 1 Hz; tails ≤2 replace in place | **Cross-cutting** |
| Scroll | Scrolled-up must not yank; `↓ N new events` pill; End or ⏎ in composer dismisses | **Missing / partial** |
| Reduced motion | Static `·` spinner | **Missing** |
| Keys | Full §8 table including **ctrl+x** `$EDITOR`, **ctrl+d** quit when empty, Esc precedence (palette → interrupt → deny) | **Incomplete** |
| Narrow &lt;100 cols | Drop gutter first, then right palette summaries; approval full-width, consequences wrap 2 lines | **Missing** |
| No color / ASCII | Glyphs + weight only | **Partial** |

---

## 3 · Contract & ADR updates required

Update contracts **before** or **with** the first behavior that needs them.

### 3.1 Must update (Slice 0 or before dependent slice)

| Doc | Direction |
|-----|-----------|
| `docs/contracts/ui.md` | Warm Ledger block grammar: Message / Artifact / Interactive / Status; flat cells; **mandatory** timestamp gutter + hairline rules; meaningful vs child rows; fold state + nearest-block `ctrl+o`; no boxes except approval; theme-agnostic tokens; typography; streaming/scroll acceptance; canvas separation reminder (ledger ≠ canvas) |
| ADR **0010** (Warm Ledger) | Layout/interaction system; themes as profiles; supersedes boxed Zot chrome in practice |
| ADR **reasoning display** (amend 0007 or sub-ADR) | Resolve Spec §4 “reasoning in ledger” vs ADR 0007 / AGENTS taint: what may render (provider summary vs raw allowed stream vs never opaque/encrypted); taint-tested projection required before thinking fixtures |
| `docs/contracts/capabilities.md` | Scoped grants: once / session-prefix / project-prefix (cmd first token or edit top-level dir); project grant = explicit config write recorded; revocation via `/permissions` |
| `docs/contracts/events.md` | Grant-scope fields on permission decisions; workspace checkpoint/restore events (or tool results); UI action logs (`/timestamps`, extension toggle, rollback) as canonical events — no parallel UI log; decision records |
| `docs/contracts/tools.md` | Restore/rollback host surface |
| `docs/contracts/multi-agent.md` | **Prerequisite for companion UI:** what nested live tool rows can honestly project in v0; `agent.message` exclusion; incomplete spawn after resume; UI may label extension compositions “companion” without core `Companion` lifecycle types |
| `docs/contracts/extension-sdk.md` | Slash token + source annotation; disabled-teaches; collision `/ext.cmd`; manager validate/link/install/audit as host-mediated steps not core package manager |
| `docs/contracts/persistence.md` | Project grants; user prefs (timestamps, theme, reduced-motion) |
| `docs/contracts/provenance.md` | Grants, restore, UI toggles durable/queryable |
| `docs/contracts/canvas.md` | @-mention slots stay canvas-clean; deny/queue/recap do not auto-enter canvas |
| `docs/roadmap.md` | Point near-term TUI at this build order |

### 3.2 Explicit non-goals / stop conditions

- No sidebars, dashboards, or second chat pane in core CLI.
- No workflow logic in core for causal-DAG beyond `/dag` → extension dispatch.
- No rewriting transcript history on rollback.
- No fifth semantic color *role*; no hardcoded palette hex in cell renderers.
- No second expand key; no global-only fold pretending to be nearest-block.
- No core `Companion` lifecycle types — presentation of existing agent events only.
- Checkpoints = **workspace file pre-images** (`WorkspaceCheckpoint`), not
  extension event-feed cursors (`EventFeedCheckpoint`).
- No secret-like content in pre-image blobs; skip/omit with policy before code.
- Project grant `p` must record config write; never silent project config mutate.
- OS notifications: privacy-limited bodies; only unfocused; only four events.
- Provider-opaque reasoning never rendered outside owning adapter (AGENTS stop).

---

## 4 · Permissions (critical path for approval slice)

**Today:** `ApprovalMode::{Ask,SessionAllow,AlwaysDeny}`;
`DeciderVerdict::{Allow,AllowSession,Deny}` — capability-wide; separate patch modal.

**Design (Spec §5.1):**

```text
y  Allow once                         (default selection)
a  Allow <prefix>* this session       (cmd first token; edits: top-level dir)
p  Allow <prefix>* this project       (persist; config write recorded)
n / esc  Deny with instructions       (composer text → model in one step)
```

**Core work before honest UI:**

1. Structured request (argv / edit path) for scope derivation.
2. Verdicts: Once / SessionScope / ProjectScope / Deny (not fake labels on AllowSession).
3. Session + project grant stores; list/revoke under `/permissions`.
4. Single panel component for shell + edit (capability gate remains authority).
5. Deny: non-empty composer → user turn + denial record; empty → focus + ghost
   `denied — tell euler what to do instead`.
6. Panel content order (fixture-locked): title → capability+cwd corner → command
   or diffstat+2–3 line preview → consequences (`unknown` never omit) → options
   → hint ending `every decision is logged`.
7. Diff preview **reuses** transcript §4.1 renderer verbatim.

**Open product decisions:** command “ran before” exact vs normalized vs prefix;
estimated duration source; network/write-scope derivation for shell.

---

## 5 · Checkpoint substrate (critical path for rollback slice)

**Today:** `file.change` hashes/lengths only — not restorable.

**Need (Spec §5.6 — edit/write only; deletes out of scope until product decides):**

1. Content-addressed pre-image blob when safe (size/binary/redaction aligned with
   `file.diff`; **never** secret-like raw content).
2. Index event id → blob; row suffix `· ckpt eNNNN` only when store succeeded.
3. `/rollback` picker: event id, action, path, time.
4. Restore appends new ledger event
   `↩ reverted <path> → ckpt eNNNN · files restored, history intact`.
5. Multi-file shell observations / external disk drift: define restore semantics
   in contract before code.

---

## 6 · Build order (shippable slices) — revised

Each slice: green fmt/clippy/relevant tests; transcript-render fixtures where
visual; **long interactive dogfood**; **GPT-5.5 code + visual review** before
slice-complete (Process rules); LOC only at PR.

**Cross-cutting acceptance (from Slice 1 onward, harden over time):**

- Spinner ≤10 fps; elapsed 1 Hz; live tails ≤2 replace in place.
- Painted lines never reflow except fold/unfold.
- Scroll-up does not yank; `↓ N new events` pill; dismiss on End / composer ⏎.
- Reduced-motion → static `·`.
- &lt;100 cols: drop timestamp gutter first, then right palette summaries.
- ASCII / no-color: glyph fallbacks + weight.
- Esc precedence: close palette/picker → interrupt turn → deny approval.

### Slice 0 — Docs lock-in

- This note (already).
- ADR 0010 Warm Ledger layout/interaction + themes as profiles.
- ADR/amend: **reasoning display** policy (summary / allowed raw / never opaque).
- Rewrite `docs/contracts/ui.md` (block grammar, gutter, hairline, fold, no boxes,
  typography, streaming/scroll, canvas separation).
- Roadmap → this sequence.
- List open product decisions (Appendix A) without blocking foundation code.

### Slice 1 — Semantic themes + ledger foundation

**Pure rendering + fold model.**

1. Tokenize all render paths (no literal RGB in cells/activity/composer/status).
2. Catalog: `warm-ledger` dark profile; keep gruvbox; data-driven profiles;
   light only if §9 validation ready.
3. **Mandatory** 9-char timestamp gutter + hairlines on meaningful events
   (children nested, no own stamps). `/timestamps` toggle can wait for Slice 7.
4. **Nearest-block fold targets** replace global expand bool; markers
   `… N more lines · ctrl+o expand`. **Default target rule (locked):** the
   foldable block whose vertical span is closest to the **viewport center**
   (tie → lower/later block). No separate selection mode required for v1.
5. Flatten artifact cells; **migrate footer data inline** (exit, duration, line
   count, fold state, result summary).
6. Tool grammar: `explore · N steps · Ts` + `├/└` aligned sub-verbs + per-step data.
7. Shell: `bash $` normalized; running spinner+elapsed+≤2 live tail; done
   informative result line.
8. Footer: single line, ctx thresholds, no second status row.
9. Banner: keep pixel art exactly; add faint Spec copy:
   `new session eNNNN · resumable with /resume · / for commands`.
10. Composer states: empty ghost `message euler · / commands`; multiline rail;
    working dim rail + interrupt copy (queue UI may still be later).
11. Core ledger glyphs (user-rail, thinking, check/cross, revert,
    interrupt, companion) route through GlyphSet accessors; remaining
    tree-glyph/spinner/warning consumers still hardcode Unicode — consumer
    wiring is debt.
12. Update tests that assert `┌`.

**Exit:** Startup/active structure matches Spec (banner exact); `/theme`
switches profiles without layout churn; fold is nearest-block.

### Slice 2 — Diff renderer (§4.1) — before approval

Dedicated so approval and transcript share one path.

- Hunk header: enclosing symbol via syntax layer; fallback raw `@@`.
- Columns: 4-char line #, 1-char sign, code.
- Added: ~12% ok-role tint + full syntax; removed: fail-role tint + dim no
  syntax; context ≤2; no-color survives on signs.
- Collapsed: first hunk ≤6 rows + fold marker; write: first 4–5 all-added lines.
- “Representative lines” for compact edit preview: product default =
  first 2–3 changed lines of first hunk (document in fixtures).
- Syntax roles: keywords/ops→attention; calls→read; strings/nums→ok;
  idents→fg; comments→faint italic; **never fail-role for syntax; no bold in code**.

### Slice 3 — Thinking collapse + markdown restyle

- Only after reasoning ADR from Slice 0.
- Live `✱ thinking · Ns · esc interrupt` → collapse
  `✱ thought for Ns — <gist≤~60> · ctrl+o expand`.
- Markdown: h1/h2 attention bold + hairline underline; h3+ fg bold; inline code
  chips; code blocks bg-inset + language tag; tables faint box-drawing; display
  math centered + unicode fallback else faint `$$`; links read-role underline.

### Slice 4 — Live composer substrate + queued input

**Before or with approval** so approval does not invent a parallel live-input path.

- Composer always accepts input while working/approval pending.
- Queue while turn active: dim rail, position index; ↑ recall last; ctrl+u
  unqueue selected; default selection = last queued unless arrows move.
- **Normal turn completion:** queued messages flush FIFO automatically as the
  next user turns (Spec §5.2).
- **Interrupt path:** esc keeps queue; show queue above composer; user may
  edit/unqueue before anything resumes — **no auto-flush on interrupt**; first
  new send or explicit continue after interrupt starts FIFO flush of remaining
  queue.
- Keys: ctrl+u; document interaction with non-empty draft.

### Slice 5 — Approval panel + scoped grants

**Contracts first** (capabilities, events, project config write).

1. Core grant store + verdicts.
2. Single bordered panel; content order + consequences + hotkeys + default y.
3. Decision records in ledger.
4. Deny-with-instruction via Slice 4 composer path.
5. Diff preview = Slice 2 renderer.
6. Merge/remove legacy patch modal.

### Slice 6 — Failure, provider retry, interrupt, quit copy

- Informative first failure line heuristics.
- Edit cause inline.
- Provider gold in-place retry; vanish on success; red on give-up.
- Interrupt + two-step quit copy with resume reassurance.
- Wire reduced-motion / scroll pill if not complete.

**Residual (Slice 6 implementer, 2026-07-09):** Provider in-flight retry is
transport-only in `euler-core` (`diagnostics::transport_retry` / round-loop
backoff). No session event or UI signal is published for attempt/backoff, so
the gold in-place `⚠ provider · retry n/m` line cannot be surfaced honestly
without inventing telemetry. Leave until core emits a UI-safe retry progress
event (or the CLI otherwise receives attempt state).

### Slice 7 — Workspace checkpoints + `/rollback`

- Core pre-image store (edit/write); secret/binary policy.
- `· ckpt eNNNN` on rows; `/rollback` picker fields; restore event.
- Append-only tests.

### Slice 8 — Search, @mentions, `/timestamps`

- `ctrl+f` full Spec behavior; read-only/no-fold; `!a`/`!f` kind map (product).
- `@` fuzzy gitignore-respected; green mention; context slot + freshness
  (contract).
- `/timestamps` toggle + pref + logged confirmation (gutter already exists).

### Slice 9 — Slash command set + extension surfaces

Normative Spec §5.11 (not opportunistic):

- `/diff` (session aggregate via Slice 2 renderer; product: session-attributed vs WT).
- `/rollback` (if not already from Slice 7).
- `/timestamps`, `/usage` (or `/status` section), `/dag` → extension dispatch
  (disabled teaches).
- `/extension` manager: toggle/add/remove; validate→link→install→audit→enable;
  ledger decision lines; bundled toggle-only; host owns mechanics, core owns
  registry/config/prompts.
- Extension slash: `⋄`, EXTENSIONS group, collisions, provenance source.

### Slice 10 — Resume polish (Spec §5.10)

Scheduled normative work on top of PR #15:

- Restyle picker (existing fields); type-to-filter label/id/root.
- `ctrl+o` read-only ledger-tail preview before commit.
- Mid-turn refusal: faint line, input preserved.
- Replay boundary: `✓ resumed …` + warnings/recovery; centered
  `──── N events replayed · model context folded to stubs ────`.
- Status shows **post-fold** ctx%.

### Slice 11 — Recaps + notifications + exit recap

- Turn-end recap after Worked-for; test-like summary parse; ctx%; faint files.
- Notify only unfocused: turn done, approval, failure, stall &gt;30s; OSC 9 +
  bell; privacy-limited body; no dup spam.
- Exit ≤5 lines including `euler --resume eNNNN` and faint export.

### Slice 12 — Companion sub-ledger

**Blocked on multi-agent projection contract.**

- Nested `◆` teal rail; header; ≤2 live tool rows; findings; collapse line;
  report re-enters main; permission bubble tagged; concurrent stack; chrono
  interleave.
- Honest degradation if v0 events cannot stream child tools: show spawn/result
  summary only — **no fake live child ledger**.

### Hardening (can ship with last functional slice or as final PR)

- Full keybinding matrix (incl. ctrl+x `$EDITOR`, ctrl+d).
- Degradation fixtures (&lt;100 cols, ASCII, no-color, reduced-motion).
- Light theme validation if not earlier.

---

## 7 · File ownership map

| Concern | Own here |
|---------|----------|
| Theme profiles / tokens | `theme.rs`, `theme_catalog.rs` |
| Gutter / hairline / fold targets | `text.rs`, `transcript/*`, `app.rs` |
| Transcript projection | `transcript.rs`, `cells*`, `render.rs` |
| Activity / tool groups | `activity.rs` |
| Diffs | `patch_diff.rs`, `file_diff.rs`, `syntax.rs` |
| Composer / queue | `composer*`, `app.rs`, `event_loop.rs` |
| Footer / pickers / search bar | `status.rs`, `bottom_surface.rs` |
| Slash | `commands.rs` |
| Approval panel | new `approval_panel.rs` + `tui_decider.rs`; retire dual `patch_approval` path |
| Banner | `banner.rs` |
| Core grants | `euler-core` `permissions.rs`, session/project config |
| Checkpoints | `euler-core` edit path + blob store; CLI `/rollback` |
| Contracts / ADRs | `docs/contracts/*`, `docs/adr/*` |
| Tests | `transcript_tests.rs`, `app/tests*`, headless PTY |

Prefer deleting box chrome and dual approval paths over parallel renderers
(design quality). Extension manager UI-thin; host owns install/audit mechanics.

---

## 8 · Test strategy

| Layer | What |
|-------|------|
| Unit | Tokens; fold targets; grant scope parse; queue FIFO; glyph fallbacks |
| Transcript render | Ledger rows, hairlines, gutter, tool tree, shell tail, §4.1 diffs, thinking, markdown, approval panel order, decision records |
| App | Hotkeys y/a/p/n; deny-with-instruction; queue; search read-only; Esc precedence |
| Core | Scoped grants + project persist; checkpoint restore; append-only; redaction |
| Headless PTY | Banner exactness; footer one-line; resume boundary; approval |
| Cross-cut | Scroll pill, reduced-motion, &lt;100 cols, no-color glyphs |

**Spec “done when”:**

- Approve repeat command in one keypress.
- Denial with guidance = type + one keypress.
- Undo bad edit &lt;5s with complete record.
- Resume named session &lt;10s with honest folded ctx%.
- Deep file mention ≤5 keystrokes.

---

## 9 · Risk register

| Risk | Mitigation |
|------|------------|
| LOC ceiling | PR-stage only |
| Direction drift | GPT-5.5 review every slice |
| Fake scoped grants | Contracts + real scope store before UI |
| Checkpoint secrets / size | Redaction + bounds; no secret blobs |
| Companion overclaim | Contract first; honest incomplete spawn |
| Reasoning taint | ADR before Slice 3; never render opaque |
| Canvas pollution | Ledger ≠ canvas; slots only via policy |
| Project grant silent write | Config write is explicit recorded approval |
| Notify leakage | Privacy body limits; unfocused only |
| Parallel live-input paths | Slice 4 before/with approval |
| Concepts drift | Spec-wins table §1 |

---

## 10 · Recommended immediate next work

1. **Slice 0** — ADR 0010, reasoning ADR, `ui.md` rewrite, roadmap pointer;
   GPT-5.5 review of the docs PR.
2. **Slice 1** — tokens + warm-ledger profile + gutter/hairline + nearest fold +
   flat cells + tool grammar + footer/composer/banner.
3. Do **not** start scoped approval or checkpoints until contracts land.
4. Do **not** treat resume/slash/fold as optional.

---

## 11 · Traceability

| Spec | Plan |
|------|------|
| §1 Principles | §1 |
| §2–3 Foundations / color / glyphs | §1–2.1, Slice 1 |
| §4 Mapping existing TUI | §2.1, Slices 1–3 |
| §4.1 Diffs | Slice 2 |
| §5.1 Approval | §4, Slice 5 |
| §5.2 Queue | Slice 4 |
| §5.3–5.5 Mentions/search/timestamps | Slice 8 |
| §5.6 Checkpoints | §5, Slice 7 |
| §5.7–5.8 Recap/exit/notify | Slice 11 |
| §5.9 Companions | Slice 12 |
| §5.10 Resume | Slice 10 |
| §5.11 Slash / extension | Slice 9 |
| §6 Failure | Slice 6 |
| §7 Streaming/scroll | Cross-cutting from Slice 1 |
| §8 Keys | Cross-cutting + hardening |
| §9 Degradation | Cross-cutting + hardening |
| §10 Build order | §6 (revised) |

---

## Appendix A · Open product decisions

Locked defaults (may override later with user input):

| # | Decision | Default | First blocks |
|---|----------|---------|--------------|
| L1 | Nearest fold target | Viewport-center closest foldable (tie → later) | Slice 1 |
| L2 | Composer ghost | `message euler · / commands` | Slice 1 |
| L3 | Banner help line | `new session eNNNN · resumable with /resume · / for commands` | Slice 1 |
| L4 | Queue on normal turn end | Auto FIFO flush | Slice 4 |
| L5 | Queue on interrupt | Keep; no auto-flush until user continues | Slice 4 |
| L6 | Edit representative lines | First 2–3 changed lines of first hunk | Slice 2 |

Still open (do not invent beyond Spec; tag first blocking slice):

1. Default theme after warm-ledger lands: gruvbox vs warm-ledger — **Slice 1** (ship both; leave default gruvbox until user switches or product picks).
2. Queue selection arrows vs last-only; queue survives quit/crash? — **Slice 4**.
3. Search: folded-hidden text or visible only; `!a`/`!f` kind map — **Slice 8**.
4. Approval “ran before” exact/normalized/prefix; duration & network sources — **Slice 5**.
5. `/diff` scope: session-attributed vs full working tree — **Slice 9**.
6. `/usage` cost when catalog has no prices — **Slice 9**.
7. File-mention freshness guarantee — **Slice 8**.
8. Checkpoint deletes / multi-file shell / external drift — **Slice 7**.
9. Ctrl+x `$EDITOR`: always local trusted vs permission-gated — **hardening**.
10. Reduced-motion / no-color: detect vs user setting — **Slice 1 / hardening**.
11. Notification body privacy limits — **Slice 11**.
12. Extension manager add/remove/toggle records: current transcript notices must become canonical decision-record ledger events — **Slice 9 debt**.
13. Full glyph consumer wiring: route remaining tree-glyph, spinner, warning,
    prompt, and non-ledger notice glyphs through `GlyphSet` accessors before
    claiming complete ASCII/no-color fallback coverage — **Slice 1 debt / hardening**.

## Appendix B · GPT-5.5 audit incorporation checklist

Addressed in this revision:

- [x] Timestamp gutter mandatory early; only toggle deferred  
- [x] Nearest-block fold out of “parallel”  
- [x] Resume + slash set scheduled slices  
- [x] Diff slice before approval  
- [x] Queue/live-composer before or with approval  
- [x] Reasoning ADR required before thinking UI  
- [x] Companion contract prerequisite  
- [x] Approval content order + consequences + deny ghost  
- [x] Typography, glyphs, shell live-tail, markdown checklist  
- [x] Spec-vs-Concepts traps table  
- [x] Keybindings/degradation cross-cutting  
- [x] Checkpoint deletes not casually in scope  
- [x] Notification privacy + exit ≤5 lines  
- [x] Architecture hazards (canvas, secrets, project config, `/dag`, extension manager boundary)

Design package extract: `/tmp/opencode/euler-design-spec/` from
`~/euler-agent-design-concept-spec.zip`.
