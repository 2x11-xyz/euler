# Anchor spine implementation checklist (Spec v2 §0/§1/§2)

Working branch: `fix/warm-spine-anchor` off `feat/warm-ledger-tui`.
Spec §0 is normative; the design board frame labeled `canonical` shows the
target; SUPERSEDED frame is v1 — do not implement from it.

## Layout contract (v2)
- Flush-left single column: 2-character anchor spine (glyph + space) then
  content. No timestamp gutter by default.
- Anchors by event type: `•` default (prose, tools), `✱` thinking, `✓`/`✗`
  decision records (green/red — the anchor IS the decision color, text dim,
  no gold), `◆` companion, `↩` revert, `⚠` provider, `■` interrupt. User
  messages: tan `▌` rail instead of an anchor (both lines of wrapped
  messages keep the rail).
- Sub-steps (tool children, output tails, diff previews) indent under
  `├`/`└`; never get their own anchor. Every action event pairs with exactly
  one `└` result line when collapsed.
- Separation: one blank line between events. NO hairlines between events.
  Only rules left in the flow: turn dividers (`── Worked for Ns ──`) and the
  composer rule.
- Timestamps: none inline; `/timestamps` opts IN a 9-char gutter beside the
  spine (whole column shifts right together). Default off. Preference load
  in main.rs flips default.
- Theme id: keep `warm-ledger`, alias `warm-spine` — never break config.

## Implementation order
1. text.rs: spine constants (2-char), gutter default off; keep
   `with_timestamp_gutter` machinery as the opt-in (§5.5) — spine width added
   to gutter width when enabled.
2. glyphs.rs: `spine_anchor(kind)` accessors reusing the F25 GlyphSet
   (ASCII fallbacks: `*`, `ok/x`, `&`, `<-`, `!`, `#`?, per §2 table).
3. render.rs: replace `push_hairline` per-event with blank-line separation +
   `stamp_first_line`-style spine stamping (anchor chosen per
   TranscriptItem); remove hairline pushes (keep markdown h1/h2 underline —
   that one stays per §4). Continuation lines: two spaces.
4. cells.rs: artifact/tool cells — dim `•` anchor + single `└` result line
   when collapsed (fold marker moves under it); decision records re-anchored
   (drop the `({decision})` suffix wording per audit S3).
5. Composer flush-left: sending becomes a purely vertical transition
   (composer line becomes ledger entry in place; check composer/render
   left padding).
6. F27 rides along: shell running state (spinner + elapsed + 2-line tail)
   and done informative-line already exist — restructure to the `└` result
   pairing.
7. Tests: transcript_tests vt100 fixtures re-baselined; PTY tests updated
   (the 9-space indent assertions disappear); hairline test
   `hairline_uses_dedicated_theme_token_not_gutter` becomes a turn-divider /
   markdown-underline test.

## Coordination
- Do NOT touch status.rs/footer (footer-status agent) or markdown.rs table
  internals (table-density agent) until their branches merge.
- Audit items riding on the spine: decision-record color/wording (S3),
  explore tree alignment polish (verify post-spine), recap placement under
  the Worked divider (S3 recap item).


## Progress (window 2)
- DONE steps 1-3 core: text.rs spine layer (SPINE_WIDTH=2, BLANK_SPINE,
  gutter default OFF, tree gutters indent at content column, ts gutter =
  9 cells returned separately), glyphs.rs bullet accessor (with consumer),
  render.rs spine_anchor(item)->glyph+style + stamp_first_line rewritten
  (ts + 2-cell anchor spliced into first-row prefix), per-event hairlines
  replaced by one blank Line, push_hairline/hairline_content deleted.
- NEXT (A2): cells still render their own leading glyphs -> double-glyph
  rows. Strip inline glyph prefixes from: render_permission_decision (✓/✗),
  reasoning collapsed/expanded lines (✱), render_interrupted (■),
  workspace-restore row (↩), companion header (◆), error rows (!/✗),
  resume boundary (✓) — the spine anchor now carries the glyph; text starts
  dim at the content column. Decision records: drop "({decision})" suffix
  (audit S3), text dim not gold.
- THEN: re-baseline the 88 failing euler-cli tests (layout assertions:
  9-space indents gone, hairline expectations -> blank lines, anchors at
  col 0). Check PTY tests. THEN steps 4-6 (artifact • + └ result line,
  timestamps opt-in already default-off but /timestamps flip + prefs load
  in main.rs needs check, composer flush-left + F27).


## Progress (window 3)
- A2 COMPLETE: all inline glyphs relocated to the spine anchor (decisions,
  thinking, interrupt, revert, companion headers, errors, resume boundary).
- Timestamps default OFF at all layers (text.rs thread-local, app.rs,
  main.rs). Emoji discouragement in core SYSTEM_INSTRUCTIONS.
- Adversarial re-baseline round 1 found 4 real bugs (all fixed, ac91041):
  is_ledger_gutter spine widths; app default (already fixed); companion
  header rail+glyph duplication; resume boundary missing anchor.
  92 failures -> 14. Round 2 running (remaining ~13 re-baselines; PTY
  commit-exactly-once failure to be triaged as possible bug).
- SSH key for github dropped from agent; push via
  `git push https://github.com/2x11-xyz/euler.git <branch>` (gh token).
- REMAINING: A3 └ result-line pairing (hold until re-baseline lands);
  approval-panel label cleanup (S4); composer flush-left verify + startup
  hug-bottom (S1); recap placement under Worked divider (S3); §9
  degradation order re-check (ts gutter drop applies only when opted in);
  final PTY + workspace gate; merge to feat/warm-ledger-tui.


## Open bug (the ONE remaining failure): spine-mode one-row commit drop
Test: tui_pty_transcript_lines_commit_exactly_once (headless.rs). Agent
bisect: passes @7f4393e, fails from 6246c3c (timestamps default off) —
latent spine-mode bug, previously masked by the 11-col gutter.
Evidence: banner block + user message bridge-committed (7 rows); Paragraph
1's FIRST physical row is in neither bridge rows nor vt100 scrollback;
its wrap-continuations onward are intact. Exactly one row is overwritten
uncommitted at the first commit boundary after the user message.
Rhythm-row removal (this commit) did NOT fix it.
Debug plan: re-add the EULER_DEBUG_COMMITS file logging to terminal.rs
([commit] rows/items/commit_until/offsets + [emit] first-row text; see
git log for the earlier diagnostic shapes), run the failing test, and
compare committed_active_rows against where Paragraph 1's first row sits
in frame.active_frame_lines. Suspects: live_committed prefix rows vs
finalized re-render shifting by one (stamp/anchor divergence between the
LiveTranscript block path at app/visual.rs:86-100 and the History path),
or active_row_count trimming trailing blanks while commit_until counts
them (visual_canvas.rs active_row_count vs Line::default separator).
