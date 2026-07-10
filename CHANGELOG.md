# Changelog

All notable changes to Euler are documented here. Entries reference the
pull requests that landed them; deeper design rationale lives in
`docs/contracts/` and the GitHub issue ledger.

## Unreleased

### Terminal UI — Warm Spine redesign (#40, #41, #44, #45/#51, #50)

- Anchor-spine transcript: two-cell glyph anchors (`•` prose/tools, `✱`
  thinking, `✓`/`✗` decisions, `◆` companion, `▌` user rail), one blank
  line of rhythm per event, no hairlines. The renderer is the single
  owner of vertical rhythm.
- Live thinking: `✱ thinking · Ns` streams while the model reasons, with
  the reasoning text beneath it (bounded, readable-fidelity only),
  collapsing to `✱ thought for Ns — gist` on completion (#43, #50).
- Collapsed actions carry a single `└` result line with the most
  informative output; exit codes live in the header, never the output.
- `ctrl+o` is a global expand/collapse toggle for all foldable cells
  (tool output, diffs, thoughts). Mouse capture is deliberately never
  enabled, preserving native text selection and native scrollback.
- Startup declutter: no orientation line, no composer ghost text; the
  footer is two hard-edged clusters — contextual hints plus
  `~/dir (branch)` on the left, `model · ctx N%` and the session name
  (once named) on the right.
- Slash palette contained in the composer rail: 8 rows, position
  counter, select bar, backspace-over-`/` exits. The code-swarm config
  picker uses the same container; `⌫` steps back to the palette.
- Working HUD directly above the composer: animated braille spinner
  with stateful phase verbs (thinking / exploring / reading X /
  running tests…), dim elapsed, esc hint.
- Opt-in timestamp gutter stamps the whole transcript with real event
  times, including across resume and rollback rebuilds.
- Approval panel v2.1: blank line before options, no label prefixes or
  disclaimer hints, consequences shown only when known, gold select bar.
- Neutral notices are muted, indented, and stack without blank lines;
  red is reserved for real failures. Disabled-extension invocations
  teach instead of erroring.
- Markdown tables: single header rule, a blank line between rows, dim
  first column.
- Terminal resize no longer corrupts or duplicates scrollback: resizes
  coalesce and repaint the transcript in place from the event log
  (real-terminal validation harness tracked in #38).
- `/export` writes only persistable events — runtime-only `model.delta`
  events are filtered through the same classifier the ledger uses.

### Permissions & security (#39)

- Scoped permission grants: session- and project-scoped grants sit
  above `ask`; covered requests run under the original decision with a
  dim `· session grant` tag instead of a fresh (and previously
  misleading) decision record.
- Project grants require consent outside the repository: the workspace
  `.euler/grants.json` activates only when matched by a per-root
  consent entry in the user's euler home. A cloned repo can never
  preseed authority; sessions without a consent dir fail closed.
- Scoped shell grants cover only simple invocations — any control
  operator, substitution, or redirection re-asks (execution is
  `sh -c` on the whole line). Scoped file grants match the
  canonicalized workspace-relative path, so `..` and symlink escapes
  re-ask.
- Revoking an unscoped session grant restores the ask gate.
- Workspace checkpoints (`/rollback`) store content-addressed
  pre-images with hardened write discipline (random `create_new` 0600
  temp files, symlink-rejecting dedup) and a broader secret-content
  detector.
- Workspace file checkpoints, `/rollback`, and turn-end recaps with the
  touched-file list.

### Multi-agent & extensions (#42)

- `HostApi::spawn_agent` (capability `agent-spawn`, multi-agent
  contract v0.1): extensions run child agents synchronously, depth one,
  with exact-flat capability attenuation against the invoking command's
  grant and the same `agent.spawn`/`agent.result` provenance as the
  session companion path. A host-side quota bounds fan-out per command.
- Extension capabilities are user decisions, not declarations: the TUI
  prompts per declared capability (recorded in provenance; session
  approvals cover later runs). Piped headless runs announce granted
  capabilities on stderr.
- code-swarm self-orchestrates through `spawn_agent` with a single
  `review` command (1–5 reviewers, `provider::model` multi-select
  picker persisted to preferences); the CLI-side orchestration state
  machine is gone.

### Reliability

- PTY-based headless test harness with scroll-region bridge
  reconstruction; hermetic test suite (isolated HOME for all spawned
  euler processes, no wall-clock or unbounded-path assertions).
