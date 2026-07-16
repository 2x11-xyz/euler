# Warm Spine v2.1 — implementation delta

What was built against the spec, what was not, and why. Branch
`feat/warm-spine-v2-1`. Spec: "Euler TUI — Warm Spine design spec" v2.1
(2026-07-10), design project `8e60c297-8b56-40f4-bbcf-f2aa5e1fb2df`.

Gate at time of writing: `cargo clippy --workspace --all-targets` clean;
`cargo nextest run --workspace` 2432/2433 (the one failure is a pre-existing
Python 3.14 environment problem in #130's managed-process test, unrelated to
this work).

---

## 1 · Done

Each item was a real gap, not a rename.

### §4.1 — diff sign + luminance

Main did the exact thing §4.1 supersedes: `diff_row_background()` filled every
span *and* the whole line with `added_tint`/`removed_tint`, and the theme baked
the tints into the diff scopes. Now removed rows dim to faint with syntax
suppressed, added rows read like normal code, the sign column carries the
color, and no row or span carries a background — so the diff survives any
terminal theme with nothing to re-tune.

The tint machinery is gone outright (palette tokens, `*_tint_pct` across three
palettes, the `tint()` blender). After the change it had zero consumers, and
`Palette`'s fields are `pub`, so dead-code analysis would never have flagged it
rotting.

### §4 — Codex vocabulary

`Explored` / `Read` / `Search` / `List` / `Ran` / `Edited` / `Wrote`, bold verb
+ normal-weight target, split at the span level. Step counts and duration are
gone from the group header (`explore · 2 steps · 6s`), as are per-step result
counts (`· 84 lines`). This reverses design review v3 §R3, which specified the
lowercase phrasing — the handoff settled that, and ui.md's lowercase line
(2026-07-09) predates the spec (2026-07-10).

`file_diff` carried a parallel vocabulary that was already half-converted
(`Deleted`/`Changed` capitalized, `write`/`edit` not); it now matches.

### §4.2 — one picker component

Six bespoke renderers sat behind a dispatch in `render_lines`. All seven listed
deviations were live. There is now one `canonical_lines` renderer with per-kind
text in a small `PickerChrome`:

- `›` caret everywhere (`→` on model/resume, bare `>` on generic).
- The select bar on **every** picker — it previously reached only the palette,
  `/code-swarm`, and `/dag`; the rest conveyed selection by caret alone.
- Counter on the title line, not below the rows.
- `Filter:` / `Search:` lines gone; the query echoes inline as a parameter.
- Glyph footers, lowercase — no "Press enter to confirm or esc to go back".
- Rows stop repeating their own value (`> current - xsmall - fastest/least
  reasoning - xsmall`).
- `●`/`○` and `[x]`/`[ ]` markers; groups as faint uppercase headers that
  carry provenance in the description column while filtering.

Two things fell out of having one owner: `line_count` had per-kind arithmetic
mirroring each layout (a second source of truth) and now counts what the
renderer emits; and the detail-under-selected-row was echoing the description
column for compaction/generic.

### §5.1 — postures

The radio had nothing to fill: every posture was built with `current: false`.
The active posture is now derived by matching the session's per-capability
modes against each posture's mapping. A posture is in effect only on an exact
match — hand-tuned modes render `Current: custom` with no filled radio, because
claiming a posture the gate is not enforcing is worse than showing none.
Required a new `Session::configured_mode` (the gate had it; `Session` never
exposed it).

`/permissions` was the wall of toggles the posture model exists to replace: one
flat 40-row list. It is now five rows — three postures, the unavailable sandbox
row, and one nested `Advanced capability settings ›` entry. Advanced rebuilds
from the live session rather than carrying a snapshot, so a revoke there can't
leave a stale list and stepping back re-derives the postures.

`/status` carries the posture *and its envelope* ("Read only · no writes · no
commands · network denied"), never the bare name.

### ui.md

Reconciled with what the code does: the anchor spine, timestamps off, no
hairline-per-event, the Codex vocabulary, and a new diff-rendering section.

---

## 2 · Not done, and why

### §6.1 failure severity taxonomy — deferred to its own PR

The highest-value unbuilt item (today every `ok:false` renders identically red,
so a healthy session with a self-corrected retry reads as broken). Deferred
because it needs two things this branch shouldn't smuggle in:

1. **`/config` does not exist.** §6.1's verbosity lever ("quiet / normal /
   verbose") lives there. That's a new command plus persistence.
2. **It touches the scrollback commit contract.** §6.1 requires an in-flight
   tool error to render as "a transient, viewport-only dim line … that never
   commits to scrollback until it finalizes", so a quiet line is never
   retroactively flipped to loud. Today `queue_finalized_visual_output_for_
   latest_event` finalizes on ingest. CLAUDE.md is explicit: "Do not add code
   paths that write history outside `write_finalized_lines_with_bridge_policy`."
   Getting that wrong corrupts history rendering, and a half-landed
   display-salience policy is worse than none.

Classification itself is easy — `project_events` already sees all events, so
"a later result for the same tool+target succeeded before the turn ended" is a
lookahead. The deferred-finalization path is the real work.

### ctrl+t raw output — spec gap, not implemented

`ctrl+t` appears **only** in two §4 parentheticals ("… +N lines (ctrl+t to view
transcript)"). §8's keybinding table omits it, and no §5 section defines the
surface it opens. Implementing means inventing a viewer from a parenthetical;
changing the marker text alone would advertise a key that does nothing. The
current marker says "ctrl+o expand", which is true and works. **Needs a design
decision.**

### §4 `Edited N files (+A −R)` multi-file group — partial

Single-file edits are done, and §4 explicitly allows them to "drop the group
header and lead with the file row". The multi-file group anchor is a projection
change (grouping consecutive edits, like `Exploration`) plus span-level
green/red diffstat colouring. Coherent as its own unit; not started.

### §4 shell command syntax highlighting — not done

"the command itself syntax-highlighted (green string/flag literals, gold
keywords)". The `Ran` verb and row are done; the command renders unhighlighted.

### Artifact-cell background — investigated, **not a violation**

Flagged in survey as chrome to remove. It isn't: `surfaces.transcript.background`
resolves to `palette.background` — the terminal's own background. The cell is
painted opaquely, which is visually a no-op. Removing it would be churn with
regression risk in transparent-background mode and no visual change.

### Picker per-span theming — grammar done, theming flat

§4.2 asks for a faint description column and bold scope token. The plain-string
render path can't express per-span style, and the canvas variant styles only
the select bar. Alignment and structure are right; dim/bold weighting of the
title, description column, and footer needs `CanvasSpan` roles threaded through
the surface. Not started.

### `◆` posture status line — placement needs a decision

§5.1 wants "◆ Ask every time" as a always-visible line. Two conflicts:
§4 specifies the footer's clusters exhaustively with no posture slot and says
"no second status row; everything else lives in /status"; and §1's glyph table
assigns `◆` to **companions**. Put in `/status` for now.

---

## 3 · Spec defects found

Worth fixing in the source doc.

1. **§5.1 approval keys are wrong.** It says `y / a / n` (+`u`). The code ships
   **`y / a / p / n`** (+`u`), where `p` is project scope, live on single asks
   and correctly suppressed on batches per ADR 0013. §8's table (`y a p n`) is
   closer but omits `u`. §5.1's own decision-record wording already mentions
   "allowed for project", so the options list is just stale.
2. **§4's file pointers are stale.** `ui/activity.rs` — named as the source for
   tool grouping and vocabulary — is a dead 2-line stub. The real code is
   `transcript/render.rs` + `transcript.rs`. Anyone estimating from the spec's
   pointers will mis-plan.
3. **§4.2 vs §5.11 conflict on extension markers.** §4.2: multi-select uses
   `[x]`/`[ ]` with space to toggle. §5.11: extensions use `●`/`○` — and space
   toggles them. Followed §5.11 (more specific; enabled/disabled is a state,
   not a selection), but the rule in §4.2 should say so.
3a. **§2 vs §4 conflict on bold.** §2: "Bold is reserved for user messages,
   markdown headings, and picker/approval titles." §4: "a **bold** capitalized
   verb", three times over. §2's list predates the Codex vocabulary adoption
   and should name the verb set as a fourth use — ui.md now does.
3b. **§4 never names a verb for the git tools.** Children are specified as
   `Read`/`Search`; `git_status`/`git_diff` have no verb, so Euler uses `Git`.
   Worth adopting or replacing in the source doc.
4. **§4's "exactly one `└` result line" and §6's "most informative line" are
   superseded** by ui.md's head+tail amendment (2026-07-11), which postdates the
   spec by a day and says so explicitly. A blanket "spec wins" reconciliation
   would have regressed it. A precedence note now sits in ui.md.
5. **`ctrl+t` missing from §8** (see above).
6. **§9 degradation references a right-aligned palette summary** that doesn't
   exist — palette summaries are plain concatenation, so there is nothing to
   degrade.

---

## 4 · Bugs found along the way

1. **Double anchor with `/timestamps`** (fixed, `f09b8f2`). Every tool cell
   rendered `12:00:06 • • Explored`. `push_cell_parent` hardcoded a 2-cell `"• "`
   placeholder, which `is_ledger_gutter` only recognizes in spine-only mode; with
   the 11-cell gutter a blank gutter was prepended and the splice replaced *that*.
   Invisible because every other anchor test runs gutter-off.
2. **The macOS test gate could not run** (fixed, `db66478`). #129's Linux-only
   sandbox fixtures weren't `cfg`-gated, so `-D warnings` broke `euler-core`'s
   test binary on every macOS checkout; CI on Linux stayed green. Fixing it
   revealed two sandbox tests that could only ever pass on Linux.
3. **§5.3 file mentions are not honest** (not fixed). The `@` palette is wired,
   but `DraftSegment::submit_text` inlines the bare path into the prompt string;
   `mentioned_paths()` has no caller outside `composer.rs`. The spec requires the
   mention be "attached to the turn as a file reference (a context slot), not just
   literal text — the agent receives the path plus a freshness guarantee". Today
   the agent receives text. That's a correctness gap, not a rendering one.
4. **`new_events_pill_text` is unwired scaffolding** (not fixed). `ui/text.rs:121`
   has a formatter kept alive solely by `const _: fn(usize) -> Option<String> =
   new_events_pill_text;` to suppress dead-code, with a comment saying the wiring
   is a follow-up. CLAUDE.md: "a helper lands with its consumer or not at all."

---

## 6 · Review findings (PR #132), all fixed

Every one reproduced. Recorded because the pattern matters more than the
individual bugs: **all five code findings were in paths with no test at all.**
Each fix landed with the test that was missing, and each test was verified to
fail against the unfixed code.

1. **Extension rows repeated every fact** — `› ● ● causal-dag  (bundled)
   causal-dag`. `ExtensionManagerItem::label()` bakes marker + id + kind into
   one string; the unified picker then added its own marker, its own group
   header, and the id again as the description. Contradicted this branch's own
   "rows stop repeating their own value".
2. **Action rows wore posture radios.** The marker came from whether the picker
   held *any* posture, so `Advanced ›` and the unavailable sandbox row got `○`
   — claiming to be unselected postures. The row's action now decides.
3. **`/status` dropped the posture mid-turn.** Guarded by `AppState::Idle`,
   but `/status` answers during `TurnInFlight`, which carries no session —
   which is why the guard existed. Now cached on `StatusSnapshot`.
4. **The Ask envelope overstated the boundary.** "every capability asks" is
   false: statically-safe shell commands (#78) and covered durable grants run
   without a prompt. In the one line whose job is to state the boundary
   exactly, an approximation is the worst possible content.
5. **Bold followed capitalization, not vocabulary** — `File`, `Patch`,
   uppercase filenames. Now the closed `CODEX_VERBS` set.
6. **Docs contradicted themselves**: ui.md required bold verbs while
   Typography allowed bold for only three things (inherited from the spec's own
   §2/§4 conflict); the sign-column contract said `−` while the renderers emit
   ASCII `-` (the renderers are right — a copied row must paste back as a valid
   diff); and ADR 0010 still normatively specified the v1 gutter, hairlines and
   nearest-block `ctrl+o`, now carrying a partial-supersession note.

Also reverted two `expect()` messages a rename regex mangled: the alternation
included bare `a`/`b`, so `"write auth file"` and `"write blob event"` matched.

---

## 5 · Already built before this branch

Verified, so nobody re-does them: the anchor spine, hairline-per-event removal,
timestamps-off default, thinking collapse (`✱ thought for Ns — gist`), the
single-hairline reasoning rail, ADR 0013 operation batching (one panel, N
decision records), guardian decision records with source tag + rationale
(ADR 0011), approval panels as the only bordered element, §5.2 queued input,
§5.4 search, §5.5 `/timestamps`, §5.6 checkpoints + `/rollback`, §5.7 recap +
OSC 9 notifications, §5.8 exit recap, §5.9 companion sub-ledger, §5.10 resume.

Not built: `/dag` (only `/causal-dag`), `/config`, §6's provider-retry line
(`⚠ openrouter 529 · retry 3/5 in 1.6s`), §7's `↓ N new events` pill, and §9's
sub-100-column degradation.
