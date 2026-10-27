# ADR 0022: CodeSwarm as an ordinary extension

## Status

Proposed (2026-10-24).

## Context

The boundary contract says core must not know "endpoints, auth names, or
workflow vocabulary of any specific product, including first-party ones."
CodeSwarm breaks that rule. Core hardcodes the `code-swarm` extension id, its
`review` command, the `code_swarm_review` tool, a two-tier reviewer config
store, and redaction of the review material
(`crates/euler-core/src/swarm.rs`, `crates/euler-core/src/session/swarm_tool.rs`,
`Session::set_code_swarm_extension`, `EulerHome::code_swarm_config_path`).
The CLI adds a dedicated `/code-swarm` picker
(`crates/euler-cli/src/ui/app/code_swarm.rs`, `code_swarm_config.rs`,
`PickerKind::CodeSwarmModels`).

ADR 0018 introduced extension model tools and called this path "explicitly
transitional until separately migrated or removed." The `code-swarm`
extension already exists in euler-extensions as a managed-process
`agent-only` command. Several things core does for it have no generic
equivalent:

| Core does today | Generic path today |
|---|---|
| Reads `.euler/code-swarm.json` and `~/.euler/code-swarm.json` | No persistent extension config |
| `/code-swarm` picker of authenticated models writes the tier file | No model-target argument |
| Redacts review context before the extension sees it | Model-tool input reaches the extension unredacted (`session/extension_contributions.rs`), and `spawn_agents` sends task text unredacted (`session/extension_bridge.rs`) |
| Accepts 256 KiB of context plus focus and framing | Whole serialized model-tool input is capped at 64 KiB (`MAX_MODEL_TOOL_INPUT_BYTES`) |
| Writes the review artifact without a prompt (manifest grant) | `artifact-write` is not a root-session default, so a model tool prompts for it |

Provider preflight already has a generic equivalent:
`spawn_reviewers_parallel` resolves every target before launching any
(`session/parallel_spawn.rs`). Only its error text is CodeSwarm-specific.

The round observer and the guardian stay in core. The observer is already a
generic brief, companion, apply chain over any extension's commands. The
guardian is a mode of the permission decider, which core owns.

## Decision

### 1. Delete the special path

Core and the CLI lose every CodeSwarm-specific item: `swarm.rs`,
`session/swarm_tool.rs`, the `code_swarm_review` tool definition and
dispatch, `set_code_swarm_extension`, `SessionConfig::code_swarm_user_config_path`,
`EulerHome::code_swarm_config_path`, the `code_swarm_review` capability
mapping in `tools.rs`, the `/code-swarm` command, `code_swarm_config.rs`,
and CodeSwarm references in comments and error text.

The `code-swarm` extension declares a model tool on its `review` command
(ADR 0018) with the model-visible name `code_swarm_review`. It uses the
ordinary extension braid: `tool.call`, operation-scoped `permission.*`, and
`tool.result` with `extension_id` and `command` attribution. Like every
extension tool it is root-session only.

The extension takes over, unchanged in meaning: reviewer resolution (explicit
models, then project, then user, then an honest unconfigured error that tells
the agent to relay the options and not guess targets), personas, the
five-reviewer cap, partial-failure rendering, and the consolidated artifact.

### 2. Add five product-neutral substrates

Each is useful to any extension and names no product.

1. **Extension config documents.** `HostApi::read_extension_config()` returns
   the calling extension's project and user documents from
   `<workspace>/.euler/extensions/<id>/config.json` and
   `<euler home>/extensions/<id>/config.json`. The host derives `<id>` from the
   command's registration; it is never a parameter. Config is data, not
   authority. `HostApi::write_extension_config(tier, value)` requires the
   existing, unused `config-write` capability. A **project-tier write always
   asks**, and no session allow or grant covers it, because the file is shared
   with everyone who opens the workspace. A user-tier write follows the normal
   modes. Documents are JSON objects of at most 16 KiB, written with a
   same-directory temp file and rename. Last writer wins. A write whose value
   matches a known secret is refused. A malformed document is an error, never
   a silent fall-through.
2. **Model-target arguments.** `ArgValueKind::ModelTargets { max }` lets a
   `user` command take 1 to `max` `provider::model` targets. The TUI renders
   the existing authenticated-model picker for it. Headless takes a
   comma-separated list, validated by the host. This keeps provider and
   authentication knowledge in the host: the extension receives only target
   strings, and no API lists authenticated providers to extensions.
   CodeSwarm's configuration becomes an ordinary `code-swarm.configure` user
   command.
3. **Redaction at both extension boundaries.** The host redacts model-tool
   input before delivering it to the extension; the `tool.call` event stays
   faithful. The host also redacts spawned-agent task text, system prompt,
   explicit context, and any included parent canvas before provider egress.
   Both apply to every extension. Together they preserve what `swarm_tool.rs`
   did for CodeSwarm.
4. **Per-tool input budget.** A model-tool descriptor may declare
   `max_input_bytes` up to a host cap of 320 KiB. The default stays 64 KiB, so
   only tools that ask for it can put large inputs into the canvas. The
   CodeSwarm schema declares the cap and bounds `context` at 256 KiB.
5. **`artifact-write` as a root-session default.** It joins
   `extension-state`, `context-slot`, and `plan-presentation` as
   `session-allow` by default in root sessions. It is host-mediated,
   session-private, and grants no filesystem authority. This keeps today's
   no-prompt review artifact without a per-extension exception.

The generic spawn preflight error keeps its `/login` remediation and drops the
"reviewer-model picker" wording.

### 3. Guard the boundary with a test

An `euler-core` test reads the Rust sources of `euler-core`, `euler-sdk`,
`euler-agents`, and `euler-event`, including doc comments and excluding test
files. It fails on any first-party extension id (`code-swarm`, `causal-dag`,
`autoresearch`, `maxproof`, `session-export`, `plan-todo`,
`diagnostics-report`, and underscore spellings). A self-check plants an id in
a fixture and asserts it is caught. The CLI joins once its remaining
mentions are removed in rollout step 3.

## Compatibility

- **Provenance and resume.** Historical `code_swarm_review` events stay
  readable as ordinary tool events. A resumed session whose tail is an
  unresolved legacy call is treated like any interrupted tool call. New
  results carry extension attribution instead of `grant_source`; renderers
  and session-export are checked for dependence on the old shape.
- **Legacy config.** Core does not read the legacy files, since that would put
  the product name back. `code-swarm.configure` accepts the legacy file
  through the existing `JsonObjectFile` argument
  (`code-swarm.configure --import .euler/code-swarm.json`), so users import
  rather than re-pick. The unconfigured message names this.
- **Version skew.** An older extension on a newer Euler offers no review
  tool. A newer extension on an older Euler fails registration as a whole
  (unknown argument kind, capability, or the tool-name collision), which is
  visible. Updating the extension changes its package fingerprint, so users
  re-consent at launch (ADR 0018 Decision 3).

## Rollout

One change per PR, ordered so the review tool is never missing:

1. **Euler:** the five substrates, neutral preflight text, and one
   transitional rule: core's special tool is not advertised when an enabled
   extension declares a model tool with the same name. Contract updates in
   `extension-sdk.md`, `capabilities.md`, and `secrets.md`.
2. **euler-extensions:** `code-swarm` declares the model tool, reads config
   through the host, and adds `configure` with `--import`. Released after
   step 1.
3. **Euler:** delete the special path and the transitional rule, make the
   picker generic, add the boundary test, and update `multi-agent.md`,
   `tools.md`, and the ADR 0018 consequence.

Each step ships with regression tests: no prompt under session allow for the
review, cancelled-result shape for the extension tool, legacy pending-call
resume, egress redaction of every spawn field, the project-tier write prompt,
and oversize-input rejection at both budgets.

## Consequences

- Core sheds a workflow it was never supposed to own: about 1.1k production
  and 1.1k test lines in `euler-core`, plus the CLI picker and config code.
- Every extension gains persistent config, a model-target argument,
  redaction at both of its boundaries, and an opt-in larger input budget.
- CodeSwarm becomes the proof that a first-party multi-agent workflow needs no
  core privilege.
- Users with a legacy reviewer config import it once.
