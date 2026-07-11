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

## Default coding tools

| Tool | Capability | Notes |
|---|---|---|
| `read_file` | FsRead | Relative path; optional line offset / max_bytes / max_lines |
| `edit_file` | FsWrite | Single exact replacement |
| `apply_patch` | FsWrite | Structured single-file patch |
| `run_shell` | ShellExec | Workspace root; timeout bounds |
| `git_status` / `git_diff` | FsRead | Short workspace git views |
| `tool_result_get` | FsRead | Rehydrate a demoted/compacted tool result from the **current session** by `event_id` (required); optional `max_bytes`. Session-local only. |
| `code_swarm_review` | AgentSpawn | Session-level review gate: fans out the persisted CodeSwarm reviewer set in parallel and returns per-reviewer findings for the calling agent to adjudicate. No required args; optional `focus`, `personas`, `models` (one-off override), `max_tokens`. Advertised only in the root session when the `code-swarm` extension is wired and enabled; companions never see it (depth one). Config store, resolution chain, result shape, and failure honesty: multi-agent contract. |

`code_swarm_review` is not executed by the `ToolRegistry`: it is a
session-level tool intercepted after the ordinary permission gate, because
its execution spawns child agents through the session. It rides the same
`tool.call` / `permission.*` / `tool.result` provenance shape as every
other tool.

When canvas stubs show `event <id>` (and optional `handle event:…` / `blob:…`
metadata), prefer `tool_result_get` with that event id over re-running the
original tool if the original inputs are expensive or non-idempotent. Blob-hash
lookup is not supported: live and resumed sessions keep content inline.
