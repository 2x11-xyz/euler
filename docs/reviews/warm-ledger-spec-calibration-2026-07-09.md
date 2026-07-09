# Warm Ledger TUI — Spec Calibration Review

**Date:** 2026-07-09
**Branch:** `feat/warm-ledger-tui` @ `8f00304`
**Rubric:** *Euler TUI Spec* v1 (Warm Ledger, option 3a — normative; spec text wins over Concepts mockups) + `docs/contracts/ui.md`
**Method:** claim-by-claim verification of every normative statement in spec §2–§10 against the code, with file:line evidence. Findings are calibration (spec conformance), not general code review — a separate correctness review is running in parallel.

Findings are numbered F1–F21, ordered by severity. Each carries an acceptance
check. Do not "fix" anything in the **Verified conformant** section at the
bottom — it is listed to prevent regression churn.

**Round 2 (same day, branch @ `bc52b92`):** F1 re-verified fixed — the
draft-empty hotkey guard plus the key-by-key acceptance test
(`typed_permission_instruction_does_not_fire_hotkeys`) meet the acceptance
criterion exactly. Round-2 findings F22–F23 below; F2–F14 fix verification in
progress separately.

---

## ROUND 2 FINDINGS (branch @ `bc52b92`)

> **Ownership split (agreed 2026-07-09):** the review side takes F22/F23/F24 —
> F22/F23 only reproduce on the macOS/CEST dogfooding host, and F24 is
> mechanical wiring with no product-behavior decisions. The implementing team
> keeps **F25/F26/F27**: F27 shares the live-render seam with the F2 scroll-pill
> wiring, and F25/F26 involve scoping decisions (wire vs re-document) that
> belong to the owning team's debt list. This keeps reviewer independence for
> everything that changes product behavior.
>
> **F22/F23/F24 status: FIXED** in the commit carrying this note; details
> appended to each finding below. Full gate after fixes: 1879/1879 passed on
> stock macOS/CEST environment (no TZ/TMPDIR pinning), clippy clean.

### F22 · New vt100 timestamp tests are timezone-dependent — gate fails outside UTC

- **Where:** `crates/euler-cli/src/ui/transcript_tests.rs:2831` and siblings —
  `vt100_renders_absolute_time_duration_and_turn_footer`,
  `vt100_clamps_out_of_order_timestamp_duration_to_zero`,
  `vt100_skips_invalid_timestamps_without_breaking_transcript`
- **Current:** the tests feed RFC3339 UTC timestamps (`2026-06-20T14:32:07.000Z`)
  and assert the rendered wall-clock literal (`start timing · 14:32:07`). The
  renderer converts to local time — `parse_event_time` maps to
  `DateTime<Local>` (`transcript.rs:1414-1418`), which is correct product
  behavior for a ledger. The tests therefore pass only when the host TZ is UTC.
  On a macOS host in CEST all three fail; `cargo nextest run --workspace` is
  red on the dogfooding machine.
- **Do not "fix" by changing the renderer to UTC** — local wall-clock in the
  gutter is the intended behavior. Fix the tests: compute the expected string
  through the same `Local` conversion the code uses, or inject a fixed offset
  into the projection for tests. Avoid `std::env::set_var("TZ", …)` inside
  tests — it is process-global and racy under `cargo test`'s in-process
  threading (nextest's process-per-test would mask the race).
- **Accept when:** the three tests pass under `TZ=UTC`, `TZ=Europe/Amsterdam`,
  and `TZ=America/New_York` without changing renderer behavior.
- **FIXED (review side):** added a `local_hms` test helper that derives the
  expected stamp through the same `DateTime<Local>` conversion the renderer
  uses; all literal `HH:MM:SS` assertions (including the overflow-omission
  test's negative assertion) now go through it. Renderer untouched. Verified
  under all three TZ values.

### F23 · Headless extension-link test breaks on macOS tempdir symlink

- **Where:** `crates/euler-cli/tests/headless.rs:5416`
  (`extension_cli_links_reloads_unlinks_and_blocks_local_runtime`)
- **Current:** asserts `info_json["source_path"]` starts with
  `extension_dir.path()`. The binary canonicalizes paths (see
  `euler-core/src/home.rs:134`), so on macOS the stored path is
  `/private/var/folders/…` while `TempDir::path()` reports the
  `/var/folders/…` symlink form — the prefix check fails. Passes with
  `TMPDIR` pre-canonicalized, confirming no product bug.
- **Fix:** canonicalize the expectation side:
  `extension_dir.path().canonicalize()` before the `starts_with` comparison
  (grep the headless suite for other `starts_with(…tempdir…)` assertions and
  fix the pattern once).
- **Accept when:** the test passes on stock macOS (default `TMPDIR`) and Linux.
- **FIXED (review side):** canonicalized the expectation side before the
  prefix comparison. The full-suite run (fail-fast disabled) surfaced one more
  test with the same defect —
  `extension_resolution_rejects_unknown_ids_and_malformed_project_file`
  (two assertions building expected error strings from uncanonicalized
  tempdir paths) — fixed the same way. Both pass on stock macOS.

**Round-2 gate status:** with `TZ=UTC` and canonicalized `TMPDIR`, the full
workspace gate is green (1878 passed, 2 skipped) and clippy is clean — no
logic regressions found; F22/F23 are test-hermeticity defects only. The
"1879 tests green" completion claim holds on Linux/UTC but not on the
dogfooding host.

## ROUND 2 · F1–F21 fix verification (branch @ `bc52b92`)

**Verified fixed, close them:** F1 (keystroke acceptance test),
F5 (real elapsed from event timestamps + working ctrl+o expand),
F6 (thresholds + 69/70/85 boundary test), F7 (header counts/elapsed, true
├/└ tree, dead `activity.rs` deleted with nothing live lost), F9 (per-seed
tints, warm-ledger 12/12/10 — lands ≈`#343424`/`#38291d`, slightly above the
spec estimates but genuinely subtle; accepted), F10 (file_diff path now uses
`patch_diff::hunk_symbol`), F12 (│ separators + bold header), F13 (comments
faint italic), F14 (5-row cap effective + test), F17, F19 (deliberate-decision
comments), F20 (char-safe + multibyte test), F21 (pre-existing conformance
confirmed). F2 is honest as scoped: formatter + tests landed, un-wired status
documented at `text.rs:120`. F15/F16 debt comments/plan-line landed. F18
de-emphasized (nit: uses `muted`, not the faint token — optional polish).

**Residual findings — new numbers, fix or document:**

### F24 · (residual of F8) `hairline` token is dead — hairlines still render gutter

- Palette/seeds/tests for `hairline`/`composer_rule`/`user_rail`/`queued_rail`
  all landed (`theme.rs:180-183,331-334,983-986`) and composer/queued rails
  genuinely consume their tokens. But `theme.transcript.hairline`
  (`theme.rs:553`) has **zero consumers**: `push_hairline` still styles with
  `transcript.gutter` (`transcript/render.rs:661`), and markdown
  heading-underlines/rules also use `gutter` (`markdown.rs:243,277,594,676`).
  The visual flattening F8 flagged (hairlines same color as timestamps) is
  unchanged.
- **Accept when:** `push_hairline` and the markdown h1/h2 underline use
  `transcript.hairline`; a render test asserts hairline color ≠ gutter color
  under warm-ledger.
- **FIXED (review side):** `push_hairline` and the h1/h2 underline now consume
  `theme.transcript.hairline`; added
  `hairline_uses_dedicated_theme_token_not_gutter` render test. Table borders
  and code-language tags deliberately stay on `gutter` — the spec labels those
  "faint", which is the gutter tone; only event separators and heading
  underlines are "hairline".

### F25 · (residual of F3) glyph fallback system unconsumed; `■` missing; plan doc over-claims

- The `GlyphSet` system + env detection are solid and tested
  (`glyphs.rs:30-60,156-189,216-299`). But: (a) `■` interrupt is absent from
  the table and used raw (`app.rs:3017,3020`, `cells.rs:471`); (b) the ~9 new
  accessors (`thinking/spinner/check/cross/tree_mid/tree_last/prompt/warning/revert`)
  have **zero consumers** — render sites still hardcode `✱` (`render.rs:171,780`),
  `⠧` (`app.rs:3028-3033`, `cells.rs:634,637`), `↩` (`render.rs:420`),
  `├└` (`text.rs:9-14`), `✓✗` (`cells.rs:245,247,832-851,939-942`); (c) the
  plan doc asserts "Glyph fallbacks wired" (`warm-ledger-tui-plan:334`), which
  is false — ASCII mode currently degrades only the user rail and companion
  glyph.
- **Accept when:** either render sites route through the accessors (add `■`
  to the table), or the plan-doc line is corrected to name the actual state
  and the consumer wiring is an explicit debt entry.

### F26 · (residual of F11) `ran-before` still hardcoded; arrow-select debt silent

- Titles ("Run command?"/"Edit file?"), deny naming, and panel hierarchy all
  landed. But `consequences_row` still emits literal
  `duration unknown · ran-before unknown` (`cells.rs:449-464`) — no session
  history lookup, though grant/decision state exists to derive it. And there
  is still no ↑↓ selection ("Allow once" carries a static `selected:true`,
  `patch_approval.rs:289-290`); neither gap is recorded as debt anywhere
  post-fix.
- **Accept when:** `ran-before N×` is derived from this session's decision
  history (other fields may stay `unknown`), and arrow-select is either
  implemented or added to the debt list.

### F27 · (residual of F4) shell running state remains unbuilt and unlisted

- Done-state correctly reuses `most_informative_line` + duration. The spec §4
  running state (gold spinner + elapsed + 2-line replacing tail +
  esc/ctrl+o hints) still does not exist — `ToolRun` has no running variant
  (`render.rs:246-252`). The round-1 completion table marked F4 "Done".
- **Accept when:** running state ships, or it appears on the acknowledged-debt
  list with the F2 scroll-pill wiring (they will share the live-render seam).

---

## BLOCKER

### F1 · Approval hotkeys fire while typing a denial instruction — accidental grants

- **Where:** `crates/euler-cli/src/ui/app.rs:1363-1384` (`handle_approval_modal_key`)
- **Spec (§5.1):** "Crucially, the composer stays live during the ask. If the user types while the panel is open and then denies, the typed text is sent to the model as the denial instruction in one step."
- **Current:** `KeyCode::Char('y'|'a'|'p'|'n')` match arms have no guard and sit
  before the composer fall-through (`_ => self.handle_modal_composer_key(key)`).
  Any typed instruction containing `y`, `a`, `p`, or `n` fires the corresponding
  permission action instead of inserting the character. Typing "wait" fires
  `AllowSessionScope` at the `a` — a session-wide grant. Typing "yes but…"
  fires `AllowOnce` at the `y`.
- **Why the tests don't catch it:** `app/tests/permission_tests.rs:236` seeds
  the draft via `core.bottom.composer_mut().insert_text("draft")` —
  programmatic insertion, never keystrokes through `handle_input`.
- **Fix direction:** hotkeys fire only while the composer draft is empty; once
  the draft is non-empty, y/a/p/n insert text and only Esc (deny) / a modifier
  chord decide. Update the panel hint line to reflect the mode.
- **Accept when:** a PTY/vt100 test types `wait — use cargo clean instead`
  key-by-key while an approval panel is open, then presses Esc, and asserts
  (1) no `PermissionReply::Allow*` was sent, (2) reply is
  `DenyWithInstruction("wait — use cargo clean instead")`.

---

## CLAIMED SHIPPED, NOT IMPLEMENTED

The branch completion report claims "§7 No-reflow, scroll pill, spinner" and
"§8–9 Keys, degradation" shipped. F2–F7 contradict that. Either implement or
move to the acknowledged-debt list — per ADR 0010, claiming unshipped behavior
is the failure mode this project treats as worst-in-class.

### F2 · Scroll pill missing (§7)

- **Spec:** "If the user scrolls up, streaming must not yank the viewport. Show a faint `↓ N new events` pill above the composer; any bottom-returning action (end key, ⏎ in composer) dismisses it."
- **Current:** no implementation. `grep -rn "new events" crates/euler-cli/src --include="*.rs"` is empty (excluding tests).
- **Accept when:** scrolled-up viewport + arriving events renders the pill with a live count; End key and composer submit dismiss it; streaming does not move the scrolled viewport.

### F3 · Degradation not implemented (§9)

- **Spec:** "Under 100 columns, drop the timestamp gutter first, then right-aligned palette summaries; the approval panel goes full-width with the consequences row wrapping to two lines. Without unicode support, use the ASCII fallbacks from §2." Required fallbacks (§2): `▌`→`|`, `✱`→`*`, `⠧`→rotating `-\|/`, `✓ ✗`→`ok/x`, `◆`→`&`, `↩`→`<-`, `⚠`→`!`, `❯`→`>`, `├ └`→`+- \-`.
- **Current:** gutter hiding exists (`text.rs:28-59`) but is driven only by the
  `/timestamps` user preference (`app.rs:587,1593`) — no width trigger.
  `glyphs.rs` (16 lines) defines only the user rail, `◆`, and companion rail,
  all raw Unicode; no ASCII table, no unicode-capability detection.
- **Accept when:** render at 99 columns drops the gutter with the pref still
  "on"; a no-unicode mode (env or detection) renders every §2 glyph via its
  ASCII fallback; no-color output remains legible via glyphs/weight (this half
  already holds — glyphs are present everywhere color is used).

### F4 · Shell running state missing (§4 Shell runs)

- **Spec:** "Running: gold spinner + elapsed + a live tail of the last 2 output lines that replace in place. Done: keep the most informative result line (e.g. the test summary) plus duration."
- **Current:** `ToolRun` has no running variant (`transcript/render.rs:224-244`,
  `transcript/cells.rs:61-96`). The only spinner text lives in
  `activity.rs:238`, which is `#[allow(dead_code)]`. Done-state keeps a
  head2/tail2 preview + line count (`cells.rs:840-858`) with duration appended
  separately (`render.rs:611-621`) rather than informative-line + duration.
- **Note:** the *failure* informative-line promotion IS implemented and correct
  (`cells.rs:760-774,878-901`) — reuse `most_informative_line` for the done
  state rather than duplicating.
- **Accept when:** a running shell tool renders `bash $ cmd ⠧ Ns` with a
  ≤2-line replacing tail and `esc to interrupt · ctrl+o show full output`
  hints; on completion the row keeps the most informative line + duration.

### F5 · Thinking states: elapsed hard-coded, expand is a no-op (§4 Thinking)

- **Spec:** live `✱ thinking · Ns · esc interrupt` streaming dim+italic; collapsed `✱ thought for Ns — <gist> · ctrl+o expand`; "Expanded state indents the full text behind a hairline."
- **Current:** collapsed line exists with correct gist logic (first sentence,
  ~60 chars — `render.rs:698-729`) but `Ns` is the literal `"0s"`; the live
  thinking line exists only in dead `activity.rs:238` (also `0s`);
  `ModelReasoning` rendering ignores `expanded_artifact_keys` entirely
  (`render.rs:163-172`) so ctrl+o on a thought block does nothing.
- **Accept when:** elapsed is measured (event timestamps suffice; wall-clock
  during stream); ctrl+o toggles between gist line and indented full text.

### F6 · Footer ctx% thresholds missing (§4 Footer)

- **Spec:** "The ctx percentage turns gold at ≥70% and red at ≥85%." (Also normative in `docs/contracts/ui.md`.)
- **Current:** the whole right segment renders as one uniform `status.model`
  span (`status.rs:291-294`).
- **Accept when:** ctx sub-span switches to attention style at ≥70% and
  failure style at ≥85%, others unchanged; covered by a render test at 69/70/85.

### F7 · Tool group headers and tree glyphs (§4 Tool grouping)

- **Spec:** "Header becomes lowercase verb style: `explore · 3 steps · 6s` (teal verb), children as a `├`/`└` tree with aligned sub-verbs (read / grep / ls) and per-step result data ('212 lines', '0 matches')."
- **Current:** live path (`render.rs:245-254`) renders bare `explore` with no
  `· N steps · Ts`; `push_child_rows` (`cells.rs:952-977`) puts `└` on the
  first child and blank gutter on the rest — not a `├…└` tree. The correct
  grouping exists in `activity.rs:161-204` but is `#[allow(dead_code)]`.
- **Accept when:** header carries step count + elapsed; children render
  `├` for all but the last, `└` for the last, with aligned verbs and per-step
  result data. Dead `activity.rs` scaffolding is either wired or deleted
  (ADR 0010: no permanent dual chrome).

---

## THEME & RENDERING DEVIATIONS

### F8 · Missing structural tokens: `hairline`, `user-rail` (§2)

- **Spec token table:** `hairline #38341f` ("event separators; `#453e26` for the composer rule"); `user-rail #b3a67e` ("▌ rail beside user messages and the live composer; `#6b6349` for queued input"). `docs/contracts/ui.md` lists both as required structural tokens.
- **Current:** `Palette` (`theme.rs:174-199`) has neither. Hairlines render with
  `transcript.gutter` = faint `#5f584a` (`transcript/render.rs:580-587`) —
  brighter than spec and identical to timestamp color, flattening the
  hierarchy. The composer rule/rail uses `palette.user` (green `#9db877`)
  (`theme.rs:404`), not `#b3a67e`; queued rail has no dedicated `#6b6349` token.
- **Accept when:** `Palette` gains `hairline`, `composer_rule`, `user_rail`,
  `queued_rail` (names per house style); warm-ledger seeds them with the spec
  hex; gruvbox themes seed sensible equivalents; renderers reference tokens.

### F9 · Diff tints ~2× stronger than spec (§4.1)

- **Spec:** "Added rows: full-width background tint (blend ~12% green over bg → ≈`#2a2f1d`)… Removed rows: full-width red tint (≈`#332119`)… Tints must stay subtle."
- **Current:** `resolve()` blends at 28% (24% changed) (`theme.rs:337-339`),
  producing ≈`#474C33` added / ≈`#513123` removed on the warm-ledger bg —
  roughly twice the spec intensity.
- **Note:** 28% may be a deliberate carry-over from gruvbox where it reads
  fine. If so, make the percentage a per-seed value: keep 28 for gruvbox,
  use ~12 for warm-ledger.
- **Accept when:** warm-ledger added tint lands within a few units of
  `#2a2f1d` and removed within a few of `#332119` (exact match not required;
  "subtle" is).

### F10 · Hunk symbol headers absent on the primary diff path (§4.1)

- **Spec:** "Hunk header: faint italic… `@@ <enclosing symbol>() · line N @@`. Resolve the enclosing function/impl via the syntax layer; fall back to the raw `@@ -a,b +c,d @@` if unavailable."
- **Current:** implemented in `patch_diff.rs:225-242` (+`syntax.rs:283-289`),
  but the artifact FileDiff path (`file_diff.rs:370-380`) only ever emits the
  raw range form — transcript diff cells never get symbol headers.
- **Accept when:** transcript diff cells resolve symbols through the same
  helper as `patch_diff.rs`.

### F11 · Approval panel content & hierarchy (§5.1) — beyond F1

- **Spec:** title "`Run command?` / `Edit file?`" with capability and cwd faint in the corner; consequences row derived "from the sandbox profile and session history; if a value is unknown, print unknown"; options as list with "Allow once (default selection)", arrow keys + ⏎ or hotkeys; hint line ending "every decision is logged"; selected row on select-bg.
- **Current** (`transcript/cells.rs:290-375`, `patch_approval.rs:179-193`):
  - Title is `"Approval required"` — spec copy not used.
  - Consequences row prints all-unknown always
    (`consequences: write scope unknown · network unknown · duration unknown · ran-before unknown`,
    `cells.rs:366-375`). Honest per the letter, but nothing is ever derived —
    `ran-before N×` is cheap from session history and carries the most signal
    for the "decidable in under two seconds" goal. Session grant state already
    exists (`euler-core/src/grants.rs`) for deriving repeat counts.
  - No arrow-key selection, no default highlight; hotkey-only.
  - Every row renders in one gold-bold style (`push_permission_panel_row`,
    `cells.rs:345-358`) — no title/meta/options/hint hierarchy.
  - Options say `n/esc  Deny` — drop of "with instructions" hides the panel's
    best affordance.
- **Accept when:** titles match spec copy by capability class; `ran-before`
  is derived from session history (others may stay `unknown` until the sandbox
  profile exposes them); ↑↓+⏎ selection works with "Allow once" default;
  panel rows use title/meta/body/hint styles; deny option names instructions.

### F12 · Markdown tables (§4 Markdown)

- **Spec:** "Tables: box-drawing with faint borders, bold header row."
- **Current:** horizontal separators only, columns joined by two spaces; header
  row not bold (`markdown.rs:502-521, 657-664, 710-713`).
- **Accept when:** faint box-drawing column borders + bold header row.

### F13 · Syntax comments dim instead of faint (§4.1 syntax palette)

- **Spec:** "comments & doc comments — faint `#5f584a`, italic".
- **Current:** `SyntaxScopes.comment` = `palette.muted` (dim `#8b8570`) italic (`theme.rs:621-623`).
- **Accept when:** comment scope maps to the faint/gutter token.

### F14 · New-file preview cap never applies (§4.1)

- **Spec:** "New files (write) preview their first 4–5 lines as all-added rows."
- **Current:** `NEW_FILE_PREVIEW_ROWS=5` (`patch_diff.rs:14`) is consumed by a
  `max()` the general 6-row preview always wins (`render.rs:32-34`).
- **Accept when:** write-cells cap at 5 rows (or the constant is deleted and
  the 6-row general cap is documented as the intended behavior — either way,
  no dead constant).

---

## MINOR

### F15 · /usage lacks cost (§5.11)

Tokens per provider/model only; explicitly "no catalog prices" (`app.rs:3210,3266`). Spec asks for "token and cost breakdown". Needs a price catalog — acceptable debt if listed.

### F16 · Extension manager actions are UI notices, not canonical decision records (§5.11)

"Every add/remove/toggle lands in the ledger as a decision-record line" — current lines are transcript notices (`app.rs:1700-1738`), not `decision.record` events; provenance does not capture them. Same class as the acknowledged `/timestamps` debt — add to the debt list or emit real events.

### F17 · Companion running header omits elapsed (§5.9)

Spec: `◆ <name> ⠧ · <task> · elapsed`. Done-state has elapsed; running does not (`cells.rs:544` vs `604`).

### F18 · Checkpoint suffix not faint (§5.6)

`· ckpt eNNNN` renders in the `patch` (gold) style with the title (`file_diff.rs:73`, `cells.rs:180`); spec says faint suffix.

### F19 · Deletion rows numbered from old file (§4.1)

Spec says new-file numbering; implementation uses Codex convention (old-file numbers for deletions — `file_diff.rs:152-158`, `patch_diff.rs:316-323`). Arguably more useful; flag for a deliberate call, document whichever wins.

### F20 · Thinking gist trades under multibyte (§4 Thinking) — verify

Gist truncation at ~60 chars (`render.rs:708-729`): confirm it is char-boundary-safe for non-ASCII reasoning text (not byte-sliced). Not verified either way during calibration.

### F21 · Banner tagline layout (§4 Startup banner)

Version renders right-aligned to the wordmark edge rather than `e^(iπ) + 1 = 0 · vN` dot-joined (`banner.rs:90-100`). `ui.md` says keep tagline "exactly" — if the pre-branch banner already right-aligned, this is conformant; confirm against main and drop if so.

---

## Verified conformant — do not churn

- **§5.2 queued input** — queue-on-⏎ (`app.rs:1000,1463`), dim rail + index (`composer/render.rs:643-655`), ↑ recall (`app.rs:1018,2467`), ctrl+u unqueue (`app.rs:987,2482`), FIFO as normal turns (`app.rs:2504,2782`), interrupt preserves queue (`app.rs:820`), footer copy (`status.rs:202`).
- **§5.3 @-mentions** — gitignore-respecting palette (`workspace_files.rs:19-45,99`), atomic token insert (`composer.rs:84,345`); context-slot deferral honestly documented in code (`app.rs:1442-1446`).
- **§5.4 search** — footer swap + k/N (`search.rs:57-64`), `!a`/`!f` (`search.rs:142-147,201-221`), read-only over copied lines, full history (`app.rs:1175-1192`).
- **§5.5 /timestamps** — persisted pref + single faint confirmation (`app.rs:1592-1616`); visual-only status honestly stated.
- **§5.6 checkpoints** — content-addressed pre-images with secret/binary/oversize skip (`checkpoints.rs:39-52`), wired on patch apply (`session.rs:1840,2400`), revert event with exact spec copy (`transcript/render.rs:401-403`), transcript never rewritten (`session.rs:1325`). Known v0 limit: modify-only (new-file adds store no pre-image).
- **§5.7 recap + notify** — recap line + faint file list (`turn_recap.rs:32-58`), test-summary detection incl. nextest/pytest (`turn_recap.rs:183-235`); exactly 4 notify events (`notify.rs:9-15`) at exactly 4 call sites (`app.rs:2656,2936,2937,901`), unfocused-only via real crossterm focus tracking (`terminal.rs:235`, `app.rs:876`), OSC 9 + BEL fallback (`notify.rs:30-34`).
- **§5.8 exit recap** — 3 lines ≤5, copy-ready resume command (`turn_recap.rs:297-314`, `app.rs:483-485,543-552`).
- **§5.9 companions** — teal rail + ◆ header (`cells.rs:541-546`), collapsed done line (`cells.rs:642-646`), report re-enters as normal message (`app.rs:2844`), asks tagged with companion name (`cells.rs:301`); **no fabricated tool rows** — everything projects from real `agent.*` events (`transcript.rs:923-988`).
- **§5.10 resume** — picker fields/grouping/filter (`support.rs:217-260`, `bottom_surface.rs:1201-1231,1793-1810`), ctrl+o read-only ledger preview (`app.rs:2153-2180`), mid-turn refusal copy (`app.rs:2134-2138`), boundary line + folded-stubs divider with exact spec copy (`cells.rs:452-465,716`, `line.rs:152`).
- **§5.11 slash set** — /diff, /rollback, /timestamps, /dag→`causal-dag.export` (`app.rs:1618-1683`), extension manager full flow (`app.rs:3427-3469`, `bottom_surface.rs:233-260`), ⋄ annotation + EXTENSIONS grouping + core-wins collisions (`bottom_surface.rs:804,1596-1601`, `commands.rs:617-644`), disabled-teach line (`commands.rs:774-794`).
- **§6 failure states** — `✗ exit N` red (`render.rs:198-202`), informative-line promotion via error[…]/FAILED/panicked/fatal (`cells.rs:760-774,878-901,1068-1099`), edit-failure inline cause verbatim (`cells.rs:745-758`), two-step quit with exact copy (`app.rs:73,2418-2428`).
- **§8 keys** — ctrl+o nearest-viewport-center fold with tie→later (`app.rs:2549-2568`), ctrl+f, ctrl+c×2, ctrl+d empty-composer quit, ctrl+x (blocked mid-turn with notice).
- **Approval/diff single source** — approval preview calls `patch_diff::render_patch` directly (`patch_approval.rs:195-211`); spec's "must never look different" holds structurally.
- **Grant honesty** — `ScopePattern` validation; invalid/oversize patterns fall back to allow-once, never broaden (`tui_decider.rs:56-64`, test at `:162`).
- **Flat cells** — boxed artifact chrome deleted (no `┌` outside tests); borders only on approval panels (`cells.rs:322-357` rounded `╭╮`).
- **§4 diff mechanics** — 4-char line column (`patch_diff.rs:12`), 1-char sign column, added-only syntax with removed uniformly dim (`syntax.rs:69-96`, `theme.rs:584-587`), ≤2 context rows (`patch_diff.rs:10`), 6-row fold with exact marker copy (`patch_diff.rs:13,183-197`).
- **§4 markdown** — h1/h2 gold bold + hairline underline (`markdown.rs:255-277`), inline code teal-on-inset, code blocks with faint lang tag, links teal underlined.
- **Banner** — pixel wordmark kept; exactly one added faint help line with exact spec copy (`banner.rs:121-124,200-203`).

## Debt list corrections

The branch's acknowledged-debt list is accurate but incomplete. As shipped-vs-claimed, add: F2 (scroll pill), F3 (degradation), F4 (shell running states), F5 (thinking elapsed/expand), F6 (ctx thresholds), F7 (group headers) — or implement them. Existing acknowledged items (/timestamps logging, @ context slots, live companion tools, provider retry line) were all verified as honestly handled in code.
