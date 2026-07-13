# Tool Contract

Core tools are the minimal coding substrate.

Tool calls must be permission checked, provenance logged, and represented cleanly in the active canvas.

Extension tools use the same contract as core tools. There should not be a second-class tool path.

## Tool Ergonomics

Tools define the agent's information contract. A core tool must support
token-efficient, targeted retrieval: when output can exceed reasonable
context cost, the tool must offer a narrower handle (line range, filter,
query, or pagination) alongside truncation, and its truncation marker must
say how to get the rest. Truncation without a handle is a defect.

Evidence: an early dogfood projection-task failure,
where an agent exhausted its tool budget re-reading a 523-line file that
`read_file` would only return truncated. Rationale and sources:
the context-engineering principle above.

## Format re-teaching (two rungs)

Formatted tools teach their format adaptively instead of assuming it
(issue #94). The trigger is failure, not context depth — failure catches
both "never knew the format" and "forgot it to context rot".

- **Rung 1 — teaching errors:** every parse error names what the format
  *expects*, not just what was wrong. One line, only on failure.
- **Rung 2 — re-teach escalation:** on the **second consecutive failure of
  the same tool**, the full format specification plus a worked example is
  appended to the tool error the model reads next, and keeps being appended
  until that tool succeeds.

Semantics:

- The failure streak is **per tool** and **process-local** (one streak set
  per model context: the driver session and each companion track their own).
  A tool's success resets only that tool's streak; other tools' outcomes
  never touch it. An `apply_patch` heredoc intercepted from `run_shell`
  counts against (and re-teaches) `apply_patch`.
- The streak is **live-session runtime state, not reconstructed from the
  event log**: resume and `/new` start with an empty tracker, so a session
  resumed mid-streak re-teaches from rung 1. Deliberate — the loop is a
  usability aid, and a resume reset costs at most one extra one-line error.
- Escalation is **deterministic**: the same failure sequence always yields
  the same error strings, so fixtures and resume replays stay stable.
- The re-teach text is part of the ordinary `tool.result` error payload —
  no new event kind.
- Tool-agnostic: a tool opts in by registering a re-teach payload (full
  grammar + example) in the `ToolRegistry`; the escalation machinery never
  special-cases a tool. `apply_patch` is the first registered consumer, and
  its payload examples are tested against the real parser so the taught
  syntax cannot drift from the accepted syntax.

## Default coding tools

| Tool | Capability | Notes |
|---|---|---|
| `read_file` | FsRead | Relative path; optional line offset / max_bytes / max_lines |
| `edit_file` | FsWrite | Single exact replacement |
| `write_file` | FsWrite | Create a new file from plain `{path, content}` — no patch dialect. Create-only: fails if the file exists (use `edit_file`/`apply_patch` to modify) or the parent directory is missing. Emits the same `patch.proposed`/`patch.applied`/`file.change`/`file.diff` provenance as the add path of `apply_patch`. |
| `apply_patch` | FsWrite | Structured single-file patch |
| `run_shell` | ShellExec | Workspace root; timeout bounds |
| `git_status` / `git_diff` | FsRead | Short workspace git views |
| `tool_result_get` | FsRead | Rehydrate a demoted/compacted tool result from the **current session** by `event_id` (required); optional `max_bytes`. Session-local only. |
| `code_swarm_review` | AgentSpawn | Session-level review gate: fans out the persisted CodeSwarm reviewer set in parallel and returns per-reviewer findings for the calling agent to adjudicate. No required args; optional `focus`, `personas`, `models` (non-empty one-off override; an empty model-facing list is omission), `max_tokens`. Advertised only in the root session when the `code-swarm` extension is wired and enabled; companions never see it (depth one). Config store, resolution chain, result shape, and failure honesty: multi-agent contract. |

`code_swarm_review` is not executed by the `ToolRegistry`: it is a
session-level tool intercepted after the ordinary permission gate, because
its execution spawns child agents through the session. It rides the same
`tool.call` / `permission.*` / `tool.result` provenance shape as every
other tool.

When canvas stubs show `event <id>` (and optional `handle event:…` / `blob:…`
metadata), prefer `tool_result_get` with that event id over re-running the
original tool if the original inputs are expensive or non-idempotent. Blob-hash
lookup is not supported: live and resumed sessions keep content inline.
