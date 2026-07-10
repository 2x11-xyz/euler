# Review round 2 — triage & execution plan

Source: design project "Euler TUI Implementation Review v2" (14 sections,
normative together with spec v2.1; review §13 lists the spec deltas).
Priority order per Eli: P1 resize, P1 functional, P2 conformance, P3 polish.

## Bucket 1 — overhaul-PR blockers (defects in feat/warm-ledger-tui; fix BEFORE the PR split)

- **B1 (P1) Resize corruption (§11/§12).** One full transcript re-render is
  appended to scrollback per resize tick; copy widths track the drag
  (85→100→68). Root cause: the item-boundary remap re-emits everything after
  the last committed item boundary on every width change — unbounded when the
  tail item is large; plus stored physical rows re-emitted verbatim
  (fossilized wraps, mid-line splices, doubled spacing). Fix direction
  (normative): coalesce resize events (repaint once per quiescent size);
  re-wrap logical lines from the event log; repaint in place; NEVER append
  to scrollback during a resize. Scrollback commits happen only in normal
  flow when content scrolls away.
- **B2 (P1) /code-swarm dispatch error (§4).** Palette-selected /code-swarm
  routes ExtensionRun{command:"swarm"} to the host → "unknown command".
  The token-typed arm works; the bottom_surface palette-confirm path
  bypasses it. Per 5c: selecting /code-swarm opens the config multi-select
  (not the old picker-by-token special case); bare /code-swarm never errors.
- **B3 (P1) Wrong grant recorded (§8).** After "Allow for session", the next
  command prints "allowed once". Determine: (a) session grant not persisting
  (would also explain repeated approval panels) or (b) record renderer
  label bug. Additionally: commands running under an existing session grant
  emit NO standalone decision record — trace goes in the tool header as dim
  "· session grant".
- **B4 (P1) /model mid-session switch rejected (§14.5).** "provider is not
  configured: openrouter" mid-session while fresh sessions work — provider
  config resolution differs between session start and mid-session switch.
- **B5 (P1) Duplicate lines/notices without resize (§2/§3).** Third
  shell-exec block renders with no header (bypasses the block renderer),
  double blank lines, duplicated output rows; /code-swarm teach notice
  prints twice. Same repaint family as B1; reproduce on feature branch.

## Bucket 2 — spine PR (fix/warm-spine-anchor; after blockers, before/with the spine seam work)

- **S1 (P2) Thinking events never render (§14.1).** Reasoning silently
  dropped across 47s–5m turns. Investigate adapter classification vs
  renderer. Target: live `✱ thinking · Ns` dim italic stream; collapse to
  gist; gold via theme token.
- **S2 (P2) └ result line (§14.2, was A3).** Exactly one └ per collapsed
  action carrying the most informative output; kills the exit-0-as-output
  leak (§2 carry-over).
- **S3 (P2) Recap touched-files line (§14.3)** — faint second line, only
  when files changed.
- **S4 (P2) Teach-don't-fail for disabled extensions (§14.4)** — one faint
  line, printed once, no ✗/ui: prefix (ties into B2 routing + S6).
- **S5 (P3) Startup removals (§1):** orientation line GONE, ghost text GONE,
  footer hints reduced to `/ commands` (+contextual ctrl+o only when a fold
  exists), no session id in footer (name only, once named; ids in /status).
  NOTE: reverses parts of round-1; spec v2.1 wins.
- **S6 (P3) Notices (§3/§6):** muted dim one-liners, no ✗/✓ on neutral
  notices, no "ui:" prefix, indented to content column, consecutive toggles
  stack without blank lines; drop stray `*` before Summary.
- **S7 (P3) Timestamps (§6):** opt-in gutter must actually render times for
  EVERY event (restamp whole transcript from provenance times; no blank
  column, no partial re-render). Confirmed bug: visual-canvas path renders
  finalized items with timing: None.
- **S8 (P3) Approval panel (§7/7b):** blank line before options; delete
  "hint: every decision is logged"; consequences row OMITTED entirely while
  all-unknown (render only known fields when data exists); no label
  prefixes (Approval required ·, command:, (default selection)); select-bg
  #38311c + gold selection; panel stays in content column.
- **S9 (P3) Working line (§9/9b):** HUD line directly above composer (no
  blank line): ANIMATED braille spinner (⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏, 80–100ms, gold,
  never frozen) + stateful verb (thinking/exploring/reading X/writing X/
  running bash/running tests; "working" only as fallback) + dim
  `· elapsed · esc to interrupt`; verb swaps in place, never stacks.
- **S10 (P3) Tables round 2 (§10/10b):** header separator is the ONLY rule
  (nothing above header, nothing after last row); one blank line BETWEEN
  rows (wrapped cells stay glued to their row); header cream bold, first
  column dim, divider+rule #38341f; keep vertical divider.
- **S11 (P3) Slash palette (§5/5b):** contained inside the composer rail
  (one region; nothing below the footer); 8 visible rows + counter; ⌫ over
  the / exits to normal input; full-width select-bg + gold selected row;
  typed / stays green.
- **S12 (P3) /code-swarm config (5c):** palette-select opens "code-swarm
  config" multi-select in the composer container (space toggle, n/5
  counter, ⏎ apply → dim provenance line, ⌫ back to palette, esc close).
- **S13 (P3) Composer max height 6 → 12 lines (§13.4).**
- **S14 (P3) Tappable folds (§13.8):** fold marker reworded
  `… N more lines · tap to expand`; per-cell mouse click target; ctrl+o
  unchanged.

## Bucket 3 — post-merge PRs
Nothing hard-new; 5c/S12 and tappable folds could slip to a follow-up PR if
the spine PR grows too large.

## Status
- B2 FIXED+pushed: palette-selected extension entries route through
  dispatch_command (code-swarm opens its config; teach-line consistent).
- B3 FIXED+pushed: covered grants emit no fresh decision record; tool result
  carries grant_source -> dim `· session grant` on the bash header. Core
  regression test (1 prompt / 1 decision / tagged second result).
- B4 FIXED+pushed: resume_provider_set now fills the full builtin+custom
  provider set (shared fill_provider_set with startup); mid-session /model
  switches work after resume.
- B5 LIKELY-FIXED-BY-B3 (unconfirmed): the 3-shell-calls + session-grant PTY
  scenario passes on HEAD — well-formed headers x3, no duplicates, exactly
  one decision record, session-grant tags on the two covered runs (invariant
  locked by tui_pty_session_grant_keeps_tool_blocks_well_formed). The old
  mid-flow decision event was the prime suspect for the malformed pairing.
  NEEDS Eli dogfood confirmation with a real provider before closing.
- B1 OPEN (the deep one): replace resize commit accounting with
  coalesce + re-wrap-from-logical-lines + repaint-in-place, never append to
  scrollback on resize. Supersedes the item-remap approach. Verify with the
  PTY drag test (multi-step resize) + AppleScript real-terminal harness.

## Execution order
1. B1–B5 on feat/warm-ledger-tui (B1 is the deep one; B2–B4 are contained).
2. PR split: euler-core PR (grants/checkpoints/contracts — B3 fix included),
   then the UI overhaul PR (links review docs).
3. Rebase fix/warm-spine-anchor; finish the spine seam + S1–S14 in priority
   order; spine PR.
4. Spec v2.1 supersedes conflicting round-1 guidance (consequences row,
   orientation line, hint line, ghost text, session id in footer).
