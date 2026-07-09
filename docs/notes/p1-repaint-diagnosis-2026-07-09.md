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
