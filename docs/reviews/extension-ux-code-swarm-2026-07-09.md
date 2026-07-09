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

---

## What it should do — the codex-swarm prototype (added after Eli provided it)

Eli supplied the pre-Rust prototype this extension descends from; it is
archived verbatim at `docs/reference/codex-swarm-prototype.py`. Reading it
reframes E5/E6: the shipped extension is not an unfinished version of the
prototype — it is a **different concept** that lost the prototype's core value.

**Scope note (per Eli, 2026-07-09):** the prototype is a *semantics*
reference, not an architecture to replicate. The capability must live
natively **within Euler** — the Rust extension using `euler-provider` for
model calls, host `write_artifact` for reports, session/provenance for
records, and Euler's own config — NOT an external Python process, direct
OpenRouter HTTP, `/tmp` report files, or a `.codex-swarm.json` sidecar. When
a G-item below says "port", read it as "port the behavior", never the
mechanism.

Gaps, in order of importance:

### G1 · The swarm was multi-MODEL, not multi-persona

The prototype fans the **same review context out to N different models in
parallel** (`DEFAULT_MODELS = claude-fable-5, glm-5.2, gpt-5.4` via
OpenRouter; hard cap of 5; `run_swarm` with a thread pool) and presents their
independent takes side by side. Cross-model disagreement is the product. The
shipped extension inverts this: three persona prompts (correctness / safety /
tests) on **one inherited provider/model** (`"provider": "", "model": ""` in
every brief). Personas are cheap to vary; model diversity is what caught
different bug classes. Note the brief shape already carries `provider`/`model`
fields — the companion mechanism could express the real swarm today if
`review-brief` accepted model targets (e.g. `--models a,b,c`, personas ×
models) and the runtime honored non-empty targets.

### G2 · Context assembly was built in, with honest budgets

The prototype assembles the review context itself: `plan` (prompt only),
`review-code --files …`, `review-diff [--staged | --base REF]` (drives
`git diff`), `review-pr [--pr N | --current]` (drives `gh pr view/diff`,
optional full files and comments) — all under explicit byte budgets
(`max_file_bytes`, `max_total_bytes`, per-source truncation) with every
omission **reported in a `skipped` list** that survives into the final report.
The shipped extension assembles nothing: companions "see the session canvas",
so anything not already in the session (a diff vs a base branch, a PR, files
never opened) is unreviewable. The skipped-list discipline is very Euler
(degradation is visible, never silent) and should carry over regardless of
mechanism.

### G3 · One-shot UX

`codex-swarm.py review-diff --base main --prompt "check the retry logic"` →
one markdown report, stdout or `--out`, `--dry-run` to preview the exact
context without spending tokens, `--json` for structured output, config file
(`.codex-swarm.json`) for defaults, exit code reflects whether any reviewer
succeeded. Compare the shipped journey at the top of this document. The
target UX for the Rust extension should be one command:
`/code-swarm review-diff --base main` (or `/extension run code-swarm.review …`)
→ spawn → collect → report, no manual JSON at any step.

### G4 · The prototype answers E6: the swarm was a model-facing TOOL

The prototype's `helper` mode speaks a host ABI (JSONL over stdio,
`HOST_ABI_VERSION 1`) and registers an SDK snapshot exposing `swarm_review`
as a **tool** — params (`prompt`, `mode`, `files`, `base`, `staged`, `pr`,
`current`, bounded `context`), limits (`timeout_ms`, `max_output_bytes`,
`max_artifact_bytes`, `max_artifacts: 1`, `max_host_calls: 0`), and a declared
markdown `report` artifact — plus a `/code-swarm` slash command surface. So
the original design intent was: **the agent (or user) invokes the swarm as a
bounded tool call**; orchestration lives inside the extension, not in the
human. E6's open question has a documented prior answer. (The snapshot also
declares a sidebar pane — that part is explicitly a non-goal under ADR 0010
and `ui.md`; drop it, keep the tool.)

### G5 · The report was a readable artifact with per-reviewer provenance

`format_markdown` produces a per-model report with latency, token count, and
`finish_reason` — flagging truncated reviewers ("output may be incomplete"),
listing failed models with their errors, and leading with the skipped-context
list. Written to a file with tight path discipline (must be directly under
/tmp, no symlinks, `O_EXCL`, mode 0600) and returned as a declared artifact
path. This reinforces E4: the Rust version's minified-JSON transcript dump is
a regression from a prototype that already knew the answer (markdown artifact
+ path + bounded inline content).

### G6 · Guardrails worth porting verbatim

Review-only enforced in every system prompt; ≤5 models per swarm; per-model
timeout; artifact byte caps; deterministic `fake_results` gated behind
`test_mode`; a no-network self-test that exercises the full protocol including
the unsafe-path cases. The Rust extension kept the review-only spirit but
lost the resource caps that matter once real fan-out exists.

### Where the shipped version is genuinely ahead

Provenance-first report pairing (`generated_from` event ids, artifact written
through the host with source events), capability declarations checked by the
host, strict unknown-field input rejection, and session-ledger integration.
The synthesis target is: prototype's *workflow and fan-out semantics* on top
of the shipped version's *provenance and capability honesty*.

### Decided direction (Eli, 2026-07-09)

E1–E4 remain immediate fixes. For the swarm itself, the design decision is
made — no open conversation remains on the mechanism:

**The swarm is powered by Euler's multi-agent (companion) framework.** Not by
extension-side HTTP fan-out. Reviewers are companions: each spawn/result pair
is provenance, permission scopes apply, results interleave in the ledger as
companion blocks, and the existing `review-report` pairing logic stays the
consolidation step.

**The slash command exposes agent-count and model/provider optionality,
per the "Code Swarm TUI Submenu" design, option 1b** (design project
`16e99352-4da5-4d4b-9d62-9d70947db690`, `Code Swarm TUI Submenu.dc.html`):

- `/code-swarm` (no args) opens a checklist picker where **selection IS the
  count**: bordered block titled `/code-swarm · reviewer models`, corner
  badge `N selected · 1–5`, rows as `[x] provider/model` with a faint
  context-size column, type-to-filter with match highlight and
  `k/n shown · esc clears filter`, select-bg cursor row.
- Constraints live in the picker: **min 1, max 5**; at 5/5 further checks
  are refused (row stays visible, dimmed). Default selection: 3 models.
- Keys: `space` toggle · `↑↓` move · `⏎` save · `esc` cancel.
- Saving emits a decision-record line — `✓ code-swarm → N models · saved as
  euler default` — and persists: applies to this session and becomes the
  user default for future sessions. **Adaptation per the scope note:** the
  mock's `.codex-swarm.json` per-project override becomes Euler-native
  project config (the `.euler/` overlay), not a sidecar file.
- `/code-swarm review [--prompt "focus…"] [--personas correctness,safety,tests]`
  runs the swarm with the saved model set. Personas assigned round-robin
  across agents when counts differ. One model selected three times is legal
  (three reviewers, one model); distinct models give true cross-model
  disagreement (G1).
- **It is an extension slash command, not a core command** (Eli,
  2026-07-09, referencing the Warm Ledger board): registered through the
  extension slash mechanism — ⋄-annotated, grouped under the EXTENSIONS
  palette header, `code-swarm · …` source in the summary column, the
  disabled-teach line when the extension is off, `/code-swarm.<cmd>` on
  collision. Not a `/dag`-style core shortcut.
- **Chrome reconciliation:** the 1b mock draws a ratatui-style border, but
  the Warm Ledger contract (`docs/contracts/ui.md`) reserves borders for
  approval panels only, and spec text wins over mockups. The picker adopts
  1b's content model (checklist-is-the-count, corner badge, filter,
  refuse-at-5, min-1) inside the shared borderless picker pattern used by
  /model and /rollback.
- **v1 wiring note:** pickers are TUI chrome and extension commands are
  JSON-in/JSON-out, so the no-arg → picker behavior needs the TUI to know
  this command opens a surface. v1 keys that on the code-swarm extension id
  (documented debt: an SDK descriptor field for "arg-less form opens a
  picker" generalizes it — same family as extension-provided pickers for
  other extensions).

**Feasibility, verified in-tree:** the companion runtime already honors
explicit targets — `AgentTask` carries `provider`/`model`
(`euler-agents/src/lib.rs:120-155`) and `spawn` inherits only when the field
is empty (`inherit_if_empty`, `euler-core/src/session/companion.rs:126-127`).
No runtime change is needed for multi-model companions; the work is the
command surface and orchestration.

**Open implementation choice (the one remaining decision):** where the
spawn-N-companions orchestration lives —

- *(a) SDK host capability:* extend `HostApi` with a bounded, capability-gated
  `spawn_companion` so the extension command runs the whole
  brief → spawn ×N → collect → report flow itself. Principled (workflow stays
  out of core/CLI), but it is a `docs/contracts/multi-agent.md` +
  `capabilities.md` change — contracts land first per ADR 0010.
- *(b) CLI-layer orchestration:* `/code-swarm` in the TUI calls
  `review-brief` (with model targets), feeds the briefs through the existing
  companion machinery, and triggers `review-report` when the last result
  lands. No SDK change, ships sooner, but encodes workflow in the CLI the way
  ADR 0010 tries to avoid.

Recommendation: (b) as the shippable first slice behind the `/code-swarm`
surface, with (a) as the recorded target so the orchestration migrates into
the extension when the host capability exists. Either way, prerequisites are
**E2** (slash args must reach extension commands — the flags above are
impossible today) and `review-brief` accepting model targets (personas ×
models) so briefs stop hardcoding empty provider/model.

Context assembly (G2) and report rendering (G5/E4) apply unchanged on top of
whichever orchestration lands: bounded diff/PR/file context with a visible
skipped-list, and a markdown report artifact instead of transcript JSON.
