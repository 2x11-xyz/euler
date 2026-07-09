# P1 duplicate-line repaint — diagnosis state (WIP)

## Root cause (CONFIRMED)
`UiAction::Resize` → `CoreEffect::ReplayHistoryWithScrollbackPurge` →
`replay_history(true)` (app.rs:437-440). Every terminal resize purges
scrollback (ESC[3J) and re-commits ALL history from row 0.
- Terminals that ignore/partially honor 3J → every committed line duplicated
  (Eli's audit screenshots).
- Terminals honoring 3J (incl. the vt100 test emulator) → rows that are
  neither re-committed nor on the final screen at quit are LOST
  (reproduced: `tui_pty_resize_does_not_duplicate_committed_lines` fails with
  Paragraph 1/2 + orientation line 0×).

## Fix direction (partially built on this branch)
Stop replay-on-resize. Instead: re-render at the new width and REMAP the
committed-scrollback boundary by finalized-item identity:
- transcript/render.rs: `render_projected_entries_with_expansion_and_offsets`
  returns per-item cumulative end-row offsets. DONE.
- visual_canvas.rs: offsets cached + exposed as
  `VisualCanvasFrame::history_item_offsets`; `committed_items` boundary with
  merge/removal guards in `push_finalized` (fixes the second duplication
  mechanism: Exploration/Companion merges mutating committed rows). DONE.
- terminal.rs: commits snap to item boundaries; width-change branch remaps
  `committed_active_rows` from `committed_history_items` via offsets; boundary
  tracked in `set_committed_active_rows`; exposed via
  `committed_history_items()`. DONE but NOT yet effective (see gaps).
- app.rs: boundary fed back to canvas after draw; reset on replay. DONE.

## Gaps (next session)
1. `UiAction::Resize` still maps to ReplayHistoryWithScrollbackPurge — switch
   to a non-purging re-render path (invalidate canvas cache + render_frame),
   keeping replay only for /theme and explicit replays. THE main fix.
2. Diagnostics (`EULER_DEBUG_COMMITS=<file>` in terminal.rs, REMOVE before
   merge) show `offsets=[]` even with history rows present: during live turns
   the History-role rows come from the live-committed block
   (app/visual.rs:86), not `visual_canvas.finalized` — offsets must cover
   that block too, or commits during live turns bypass item accounting
   entirely. Investigate which snapshot blocks carry History role and align
   offsets with the FULL history region (or restrict snapping to the
   finalized sub-range only — offsets are block-relative row 0 which matches
   only if the finalized block is first).
3. `pty_final_state_text` has a dead first-pass loop (headless.rs) — simplify
   to the row-walk fallback which is the one used.
4. Quit path: rows on the final screen above the exit recap never commit —
   verify the exit sequence leaves them in scrollback (relates to the exit
   recap overwriting the footer line, seen in reconstruction).

## Repro assets
- `tui_pty_transcript_lines_commit_exactly_once` (headless.rs) — passes.
- `tui_pty_resize_does_not_duplicate_committed_lines` — FAILS (the P1 case);
  PtyHarness gained `resize()` + resize-aware `screen_text()`;
  `pty_final_state_with_resizes` reconstructs scrollback+screen across
  resizes.

## Iteration state (end of first window)
- Offsets plumbing FIXED (the per-entry push had landed inside the
  turn_footer branch; now at loop tail, footer bumps last offset). Verified
  live: `offsets=[9, 11, 29, 31]` etc.
- Resize no longer replays (UiAction::Resize arm in app.rs now invalidates
  the canvas cache + render_frame; commit accounting survives the resize —
  verified: `width=100 prev_width=80 rows=12 items=2` in the debug log).
- Test STILL fails, new signature: banner/orientation + P1/P2 absent from
  the vt100 reconstruction even though the debug log shows rows=12 (banner +
  user) as committed *accounting*. Open question: were those 12 rows ever
  PHYSICALLY emitted as linefeed history inserts at width 80, or did
  committed_active_rows advance without emission? Suspects:
  (a) rows 0->12 jump happens with no logged emission between the last
      width=80 line (commit_until=0) and the width=100 line — find where;
  (b) `linefeed_history_insert_suspended_after_resize` or
      `linefeed_history_insert_enabled` gating write_finalized_lines...;
  (c) the emission may write into a scroll region (CSI r), which vt100 (and
      real terminals) do NOT push to scrollback — check
      write_finalized_lines_with_bridge_policy and whether commits reset the
      scroll region first (the no-resize test passes, so plain commits do
      reach scrollback — compare the two paths).
- NEXT: instrument write_finalized_lines_with_bridge_policy with the same
  EULER_DEBUG_COMMITS file logging (emitted row ranges + text of first row)
  to answer (a)-(c); then fix, run tui_pty_* + full gate, remove
  diagnostics, clippy, push.
