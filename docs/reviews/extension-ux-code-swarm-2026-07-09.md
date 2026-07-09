# Extension UX in the TUI — code-swarm inspection

**Date:** 2026-07-09
**Branch:** `explore/extension-ux-code-swarm` (off `feat/warm-ledger-tui` @ `9cf87c6`)
**Scope:** why using extensions from the TUI "is not as flawless as it should be",
starting with code-swarm. Findings E1–E8 with evidence and acceptance criteria;
method was code-trace plus driving the real binary (`euler exec` with the
fixture provider, then `euler extension run …` against the created session).

## The intended code-swarm journey, as shipped

1. `/review-brief` (auto-registered slash token) → emits companion `AgentTask`
   briefs for up to three reviewer personas (correctness / safety / tests).
2. The user spawns each brief as a companion: `/companion run <json-agent-task>`.
3. `/review-report` → pairs `agent.spawn`/`agent.result` events from provenance
   and writes a consolidated review artifact.

Every step of that journey has a defect or a cliff. In practice the flow is
unusable end-to-end in the TUI today.

---

## E1 · `review-report` is broken from the CLI — contract mismatch on `session_id` (bug, high)

- **Repro:** `euler extension run code-swarm.review-report <session-id>` →
  `CommandFailed("review-report", Message("unknown input field `session_id`"))`.
  Reproduced against a fresh fixture-provider session.
- **Cause:** the offline runner injects `session_id` into the input object
  because the command declares `accepts_session_id: true`
  (`crates/euler-cli/src/offline_extension_runner.rs:49-58`), but the
  extension's `reject_unknown_fields` allowlist is
  `["limit", "scan_limit", "after_event_id"]`
  (`crates/euler-extension-code-swarm/src/lib.rs:228`) — the injected field is
  rejected. The command that produces code-swarm's actual deliverable cannot
  run from the CLI at all.
- **Why tests miss it:** the extension's unit tests call `execute()` directly
  and never go through the injection path; no headless test runs
  `extension run code-swarm.*` (the causal-dag one exists and passes — this
  gap is code-swarm-specific).
- **Asymmetry:** the TUI path (`session/extension_bridge.rs:81`) passes input
  verbatim — no injection — so `/review-report` works in-session. Same
  command, two behaviors.
- **Accept when:** `review-report` accepts (and ideally validates) the
  injected `session_id`; a headless test runs the full
  `enable → exec → extension run code-swarm.review-report` CLI path; a
  contract note states that `accepts_session_id: true` obliges the input
  parser to accept the field.

## E2 · TUI slash tokens silently drop arguments (bug, high)

- **Repro (code trace):** `dispatch_parsed` routes unmatched tokens as
  `token => extension_slash_or_unknown(token, context)`
  (`crates/euler-cli/src/ui/commands.rs:770`), discarding `parsed.arg`.
  `extension_slash_or_unknown` always dispatches
  `input: Value::Object(Map::new())` (`commands.rs:774-789`).
- **Effect:** `/review-brief {"reviewers":["tests"]}` (or any argument) runs
  the command with **default input, silently**. The user gets three briefs
  when they asked for one and nothing tells them their filter was ignored.
  The extension's own `ArgSpec` declarations (`--reviewer`, `--max-tokens`,
  `--limit`, `--after-event-id`) are honored by the CLI but unreachable from
  the TUI.
- **Spec conflict:** §5.11 — extension commands "follow the existing dispatch
  conventions", and invalid usage "prints a red `usage:` line preserving the
  input". Silent swallowing violates both.
- **Accept when:** an argument after an extension slash token is either parsed
  (JSON object, or ArgSpec-mapped flags for parity with the CLI) or rejected
  with a red usage line; never silently dropped.

## E3 · Linked extensions advertise commands the TUI cannot run (bug, high)

- **Trace:** the extension manager lists linked extensions with their
  commands (`app.rs:3294`, `append_linked_manager_items` pushes
  `commands` from the link), and `build_extension_slash_commands`
  (`commands.rs:588+`) turns those into palette entries — ⋄-annotated,
  enabled-aware. But `resolve_extension_run` resolves **bundled only**
  (`bundled_descriptor_by_id`, `app.rs:1958`), so invoking a linked
  extension's slash command or `/extension run <linked-ext>.<cmd>` fails at
  dispatch with `unknown extension id`.
- **Effect:** the manager's add flow (validate → link → audit → "enable now?")
  ends in an extension whose commands appear in the palette and then refuse
  to run. The design's add-flow promise breaks at the last step.
- **Accept when:** either linked extensions are runnable in-session (resolve
  through the registry, honoring audit/enablement state), or their commands
  are excluded from the palette with a teach line ("linked — run via
  `euler extension run` CLI"), whichever the runtime-trust posture allows.
  Advertised-but-unrunnable is the one wrong state.

## E4 · Extension output renders as one minified-JSON wall (UX, high)

- **Trace:** `handle_extension_outcome` renders
  `extension {id}.{cmd} result: {serde_json::to_string(&output)}` as a single
  `SessionSummary` transcript item (`app.rs:1937-1944` region).
- **Effect (measured):** `review-brief`'s real output is a ~1.9 KB single line
  containing three ~600-char system prompts with `\n` escapes. In the ledger
  this wraps into dozens of rows of unreadable JSON. It does not use the
  artifact-cell / fold machinery the Warm Ledger design built for exactly
  this ("output as indented dim tail lines … `… N more lines · ctrl+o
  expand`"), and `/copy` only covers the last assistant response, while mouse
  capture owns the terminal's native selection — so the output can't even be
  copied out cleanly.
- **Accept when:** extension results render as a foldable artifact block
  (pretty-printed or summarized headline + fold), and there is some way to
  get the payload out (e.g. `/copy` variant or an artifact file path in the
  result line — `review-report` already returns `relative_path`; print it).

## E5 · No bridge from briefs to companions (workflow gap, medium)

- The brief output *is* the `/companion run` input shape, but the only way to
  connect them is to visually read minified JSON out of the transcript and
  retype/paste it — three times. With E4 unfixed this is effectively
  impossible; even with E4 fixed it is hostile.
- Options, in increasing ambition: (a) `review-brief` result line offers the
  ready-to-paste `/companion run {…}` commands; (b) a code-swarm command that
  spawns the companions itself (needs an agent-spawn host capability in the
  SDK — a contracts change); (c) a `/companion` picker fed from the latest
  briefs artifact. (a) is cheap and honest; (b) is the real product answer
  but touches `docs/contracts/multi-agent.md`.
- **Accept when:** a user can go from `/review-brief` to three running
  companions without hand-assembling JSON.

## E6 · The model cannot orchestrate extensions or companions (design question, not a defect)

- Model-facing tools are exactly: `read_file`, `edit_file`, `apply_patch`,
  `run_shell`, `git_status`, `git_diff`, `tool_result_get`
  (`euler-core/src/tools.rs`). There is no spawn-companion or run-extension
  tool, so "swarm" workflows are human-orchestrated by construction.
- This may be deliberate (ADR 0010 non-goal: "workflow logic in core"), but it
  should be a *recorded* decision: today nothing documents whether the agent
  is ever supposed to drive code-swarm itself. If it is, that's a tools/
  multi-agent contract extension; if not, E5's UX becomes the whole story.
- **Accept when:** an ADR or contract note records the intended orchestrator
  (human vs model) for companion swarms.

## E7 · CLI requires a session for session-free commands (minor)

- `euler extension run code-swarm.review-brief` without a session argument
  fails with "requires a session id, name, or events path" even though the
  command declares `accepts_session_id: false` and is a pure function of its
  input. Minor, but it forces ceremony exactly where the command needs none.
- **Accept when:** commands with `accepts_session_id: false` run without a
  session target (or the requirement is documented).

## E8 · Disabled-extension error is raw on the `/extension run` path (minor)

- The slash-token path teaches (`/dag — provided by causal-dag (disabled) ·
  /extension to enable`), but `/extension run code-swarm.review-brief` on a
  disabled extension dispatches anyway and surfaces the core error
  (`extension disabled: code-swarm`) as a failure item. Inconsistent voice
  for the same situation; the teach line should cover both entrances.

---

## What already works (verified — don't churn)

- Slash auto-registration with collision handling and ⋄ annotation; the
  disabled-teach line on the token path (`commands.rs:774-794`).
- Enablement is enforced at execution (`extension_bridge.rs:89-91`), and
  extension-emitted events publish into the session ledger.
- Mid-turn extension/companion runs queue instead of colliding with the
  active turn (`app.rs:1936-1943`).
- The extension itself is well-built: strict unknown-field rejection (E1 is
  a missing allowlist entry, not sloppiness), honest bounded-window pairing
  contract with an actionable zero-results error, review-only persona prompts.
- CLI flag mapping via `ArgSpec` (`--reviewer tests` works headlessly).

## Suggested order

1. **E1** — one allowlist entry + a headless CLI test (smallest, unblocks the
   deliverable command).
2. **E2** — parse-or-reject slash args (small, kills the silent-drop class).
3. **E4** — route extension output through the artifact-cell fold machinery
   (pure §4 rendering work, same seam as the warm-ledger cells).
4. **E3** — decide the linked-extension posture, then implement or teach.
5. **E5/E6** — one design conversation (who orchestrates swarms?), then either
   the cheap bridge (a) or the SDK capability (b).
