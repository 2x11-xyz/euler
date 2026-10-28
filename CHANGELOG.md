# Changelog

All notable changes to Euler are documented here. Entries reference the
pull requests that landed them; deeper design rationale lives in
`docs/contracts/` and the GitHub issue ledger.

## Unreleased

### Custom providers speak Anthropic Messages (#263)

- **New `api_family: "anthropic_messages"`** for `providers.json`. It reuses
  the native Anthropic adapter, so signed thinking blocks are replayed on
  later turns (keeping thinking continuity and prompt caching) and reasoning
  effort maps to adaptive thinking when the model entry declares
  `supports_reasoning: true`.
- **Per-model `api_family`.** A model entry may override its provider's
  family, so one gateway can serve some models over Messages and the rest
  over Chat Completions. Both families share the provider's `base_url`: a
  `/v1` root serves `/v1/chat/completions` and `/v1/messages`, and a bare
  host root also works for Messages.
- New load warnings: a model with an unknown `api_family`, a provider or
  model family that a full-endpoint `base_url` cannot serve, and `compat` on
  a Messages model (Chat Completions only, ignored).
- A Messages model's declared `max_output_tokens` caps the default
  `max_tokens` (32000), and stream errors name the custom provider.
- Reasoning recorded without a signature, or with a Chat Completions
  `reasoning_details` array, is no longer replayed over Messages, including
  on the built-in `anthropic` provider. The API rejects such thinking blocks,
  so such a request previously failed.

### Custom provider config location (#258)

- **`providers.json` now honors `EULER_HOME`.** Custom providers are read from
  `$EULER_HOME/providers.json` when `EULER_HOME` is set, matching auth,
  sessions, extensions, and skills. Previously the file was always read from
  `~/.euler/providers.json`; if you set `EULER_HOME`, move the file there. An
  unresolvable Euler home now prints a `warning: ignored provider config:`
  line instead of silently reading `~/.euler`, and the `USERPROFILE` fallback
  is gone (the Euler home requires `HOME` or `EULER_HOME`).

### Replay declaration (ADR 0023 step 1, #259)

- Root `tool.call` events record `replay: "safe"` when the tool's replay
  policy is `safe`: the core read-only tools (`read_file`, `git_status`,
  `git_diff`, `tool_result_get`, `skill_read`) and extension model tools whose
  `ModelToolDescriptor` declares `replay: "safe"`. Everything else, including
  every `run_shell` command, is `unsafe`.
- An `agent.spawn` an extension requests while running as a model tool
  records `owner_event_id` and an ordinal `spawn_key`, and every spawn with
  explicit context records `explicit_context_sha256`.
- Recording only: resume behavior is unchanged.

### Custom providers without a models list (#261)

- **`models` is optional when `default_model` is set.** A custom provider in
  `providers.json` may now name just a `default_model`; Euler treats it as one
  model with no declared metadata, like an unlisted `--model`. Previously such
  a provider, including the `local` example in the headless guide, was
  ignored with a warning. An empty or malformed `models` value, or a provider
  with neither field, is still ignored, and the warning now says how to fix
  it.

### Per-call recovery closures (ADR 0023 step 2, #262)

- Resume now closes every unmatched call of an interrupted multi-call root
  batch, not only the tail call, when its results show the batch ran in
  order (the normal case). The first unmatched call keeps today's
  closure (permission undecided, or side effects may have occurred); each
  later call's closure says it never started because an earlier call in the
  same batch had not finished. Previously a crash during an earlier call
  closed only the last call (which never ran) as possibly executed, and a
  crash after a result or during an earlier call's permission prompt closed
  nothing. Nothing is re-run; single-call batches resume as before.

## 0.1.4 (2026-10-27)

### Subprocess sandboxing

- **Linux and macOS now have default enforced execution boundaries for
  `run_shell` and the `git_*` tools: Bubblewrap and Seatbelt, respectively.**
  On Linux the child gets a private root, its
  workspace bound read-write, private `/tmp` `/proc` `/dev`, a tmpfs `HOME`,
  no host network namespace, and a cleared environment. On macOS Euler invokes
  the pinned `/usr/bin/sandbox-exec` with a static deny-default profile. The
  workspace and a private scratch tree are writable, `.git` is carved out by
  exact and subpath rules with rename defenses, and network access is denied.
- Toolchains installed under the real home stay reachable. The toolchain homes
  the host environment implies (`CARGO_HOME`, `RUSTUP_HOME`, `NVM_DIR`,
  `PYENV_ROOT`, `ASDF_DATA_DIR`, `GOPATH`, `PNPM_HOME`, plus `/nix/store`) are
  detected and bound read-only at their real paths, with the sandbox `PATH`
  built from the host `PATH` entries inside them. The real `$HOME` is never
  mounted read-write, and a write under it fails instead of landing in a
  discarded private copy.
- Availability is probed at session start by running a trivial sandboxed
  command, because an installed `bwrap` is not evidence that it works. The
  outcome is recorded on `session.start` as `sandbox_backend`
  (`bwrap` | `seatbelt` | `host` | `unavailable`) with
  `sandbox_unavailable_reason`. Every completed shell and Git result also
  records the backend that executed it. A
  failed probe fails sandbox-requiring tools closed — never a silent fallback
  to host execution — and emits a diagnostic naming the likely cause
  (userns disabled by sysctl or AppArmor, a container, WSL1, `bwrap` missing)
  and the fix. New `euler --check-sandbox` runs the same probes and exits
  nonzero with that diagnostic. Bubblewrap must be installed on the host;
  bundling a digest-verified `bwrap` is tracked as a follow-up.
- A sandbox probe that times out under host load no longer reads as a host
  that refused the profile, and no longer sticks for the rest of the process.
  It has its own reason, `probe_timed_out`, on `session.start` and
  `session.resumed`, and the tool error and `euler --check-sandbox` say the
  check timed out and to start a new session. That session still fails
  closed; the backend probe caches only definite results, so the next session
  probes again.
- Euler's own Git invocations are neutralized: `core.hooksPath=/dev/null`,
  `safe.bareRepository=explicit`, `attr.tree=`, `core.attributesFile=`,
  `GIT_LFS_SKIP_SMUDGE=1 GIT_TERMINAL_PROMPT=0 GIT_OPTIONAL_LOCKS=0`, every
  configured `filter.*.clean`/`.process` driver blanked through
  `GIT_CONFIG_KEY_n`, the `GIT_DIR`/`GIT_WORK_TREE`/`GIT_CONFIG*`/
  `GIT_INDEX_FILE`/`GIT_ALTERNATE_OBJECT_DIRECTORIES` family stripped from the
  environment, and `--no-ext-diff --no-textconv` on `git_diff`.
  `core.fsmonitor` is probed and preserved only for Git's built-in daemon.
- Euler's own git is neutralized everywhere it runs, not only in the tools:
  the TUI `@`-mention picker's `git ls-files` goes through the same overrides,
  and the build script applies them to its own `git status`.
- **Breaking (library):** `capture_workspace_snapshot` now takes the entry
  bound as a second argument; pass `MAX_WORKSPACE_SNAPSHOT_FILES` for the
  previous behavior. `probe_workspace_sandbox` returns `SandboxStatus` rather
  than `SandboxAvailability`, so the cause its own probe observed travels with
  the result. `SandboxStatus::from_availability` is removed: it could only
  re-derive a cause from the reason, which is strictly worse than what the
  probe already knows.
- Reaching the workspace-observation bound no longer hides changes as "no
  changes". The command runs; the tool text leads with `file observation
  incomplete: <reason>; changes may be unreported`, and the result carries an
  `observation` object (`{status, reason, bound}`). The bound is configurable.
- The macOS local test gate no longer fails on two Linux assumptions: the
  invalid-workspace sandbox test expects the workspace check that Seatbelt
  now performs, and the non-UTF-8 user-skill path test runs only on Linux,
  where the filesystem can create its fixture.
- Resume-equivalence tests pin the subprocess sandbox to `Host`, so their
  transcript comparison no longer depends on probe timing. Each compared
  session probed its workspace sandbox profile under the probe's 2s deadline,
  so on a loaded machine one run could record Seatbelt and the other an
  unavailable sandbox. Production probing is unchanged.

### Resume root identity (#239)

- Sessions record the workspace root's directory identity (`device`, `inode`,
  and birth time where reported) in an optional `root_object_identity` field
  on `session.start` and `session.resumed`. Resume reports `Verified`,
  `Relocated`, or `Unverified { reason }` on `FoldedSession` and
  `ResumeOutcome`, so a directory replaced at the same path is no longer
  indistinguishable from the original. The verdict never blocks, prompts, or
  warns; it is the gate ADR 0023 replay will consume. Older Euler builds
  resume these sessions unchanged.

### Shell command safety

- Static shell approval now parses with `tree-sitter-bash` and uses the
  two-parser design of OpenAI Codex instead of a name-keyed allowlist over
  a hand-rolled tokenizer. A conservative parser may prove a command safe —
  an allowed node-kind tree, literal words only, and an allowlist of exact
  option spellings per binary — and a separate permissive walk may only
  find danger. The walk visits every command node, including inside control
  flow, substitutions, redirection arguments, and the scripts carried by
  `sh -c`, `eval`, `env -S`, and `trap`, and unwraps `sudo`, `su`, `env`,
  `timeout`, `nice`, `xargs`, `exec`, `command`, and the rest.
- A command that walk flags is **never auto-approved, in any mode**: no
  grant covers it, the guardian reviewer never adjudicates it, the approval
  panel offers no grant for it, and every capability mode short of
  `always-deny` is escalated to a prompt for that request, so a forced `rm`
  asks even under a blanket session allow. The danger table is extensible and covers more
  than forced `rm`: `run_shell` closes stdin, so `rm -r` never gets its
  confirmation, and `find -delete`, `git clean -f`, `shred`, `truncate`,
  `dd of=`, `wipefs`, and `mkfs*` are in it too. A command truncated at the
  retention bound is walked in full before it is bounded, so danger hidden
  past the bound is still seen while an ordinary multi-kilobyte command is
  not flagged for being long. The walk also covers the write side of audit
  F34: a redirect target or `cp`/`mv`/`tee`/`install`/`ln` destination on
  the sensitive list asks exactly as `write_file` on that path does.
- Auto-approvals that could act outside the workspace are gone (audit F01,
  F02, F34). `uniq in out` writes `out`, so `uniq` allows at most one
  operand. Shell-rewritable words (globs, `~`, `${...}`, `$'...'`) are
  never provable for any binary. `cd` and `git` left the read-only set —
  `cd` moves the directory confinement is checked against, and read-only
  `git` subcommands still run repository-controlled `diff.external`,
  `core.fsmonitor`, and filter programs. Traversal flags that dereference
  symlinks, attached option values, and GNU long abbreviations are all
  rejected by the allowlist rather than chased with a denylist, as are
  recursive readers — `grep -r`, and `rg` entirely, since it recurses by
  default — whose tree walk reads files the per-operand sensitive check
  never saw. A backslash anywhere in the
  line makes it unprovable, because a backslash-newline is whitespace to
  the grammar and a line continuation to `sh`.
- The sensitive-path denylist (which blocks both static shell approval and
  blanket fs-tool grants) now covers anything under a `.git` component,
  `.gitmodules`, `.gitattributes`, `.gitconfig`, `.cargo/config.toml`,
  `.npmrc`, `.netrc`, and shell startup files, and applies to a path's
  canonicalized form as well as its spelling, so an in-workspace symlink
  cannot read through it.
- Pressing Enter on a flagged command's "Allow once" session or project row
  in the approval panel now allows once, as its key already did. Before, it
  installed an unscoped session `shell-exec` grant that then covered later
  unflagged shell commands.

### Structured file writes and rollback

- Structured file tools (`read_file`, `edit_file`, `write_file`,
  `apply_patch`) now resolve their target by walking down from the workspace
  root to the parent directory and holding that descriptor — `openat2` with
  `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS` on Linux
  (falling back to an `O_NOFOLLOW` hop-by-hop walk where that syscall is
  unavailable), a hop-by-hop walk on other Unix hosts — so a directory or root
  swapped in after the path was resolved fails the open instead of escaping
  the workspace. Creates use `O_EXCL` and never clobber a file that appeared
  after the call was prepared; a modify compares the target's current bytes to
  the exact prepared pre-image and refuses a stale write rather than
  overwriting a concurrent edit; non-regular targets are refused.
- Structured writes are now atomic: the new bytes are written to a sibling
  temporary file, given the target's permissions, made durable, and renamed
  over the target. A file is only ever its complete old content or its
  complete new content, so an interrupted write can no longer truncate it.
  Creates publish the same way through a no-replace rename, so they stay
  no-clobber. A write to a file or directory the user made read-only is
  refused rather than silently published.
- `/rollback` checkpoints are now stored and recorded *before* the destructive
  write, as a `checkpoint.stored` event. A write whose checkpoint cannot be
  stored durably does not happen at all. A checkpoint whose write was never
  observed to complete is never restorable, and restoring an applied
  checkpoint first verifies that the file still holds what the newest
  recorded write left there — so a rollback can no longer silently discard a
  later edit. A checkpointed file the user deleted is recreated, and a restore
  is now recorded as a change of its own, so rollback is repeatable on the
  same file and a restore can itself be undone.

### Project context and skills

- Skill authoring is now more interoperable: a missing or YAML null name
  derives from a valid parent directory, a valid explicit name may differ from
  that directory with a non-fatal compatibility diagnostic, and the
  1,024-character description limit counts Unicode scalar values instead of
  UTF-8 bytes.
- Users can activate any catalogued session skill with
  `/skill:<name> [request]`. Euler resolves only the immutable frozen snapshot,
  preserves the compact command in the transcript, records the exact expanded
  model input in provenance, and uses the same admission path for idle turns,
  steering, and queued follow-ups.
- Euler now discovers user-global skills from `${EULER_HOME}/skills/` and
  project skills from `.euler/skills/`, freezes them in schema-v2 session
  snapshots, renders a compact catalog, and exposes snapshot-backed
  `skill_read` with ordinary tool provenance. Project skills use repository
  acknowledgment; user-global skills remain available when repository context
  is disabled. The catalog and `skill_read` are root-driver-only until
  `inherit` child wiring lands. The acknowledgment digest now covers only the
  project-scoped view of the manifest, so every previously acknowledged
  repository re-prompts once after upgrading (fail closed — nothing loads
  until re-accepted); afterwards user-skill changes never re-prompt.
  Catalog bounds now omit whole skills with explicit diagnostics instead of
  silently hiding accepted entries, frozen bodies use spoof-resistant
  core framing, user-only manifests receive the same context-budget checks,
  and child `none | inherit` policy is enforced for prior skill-result canvas
  rounds and `tool_result_get` rehydration.
- Tests that assert project-context output no longer depend on the host temp
  directory. Their fixture workspaces are their own repository boundaries and
  user-skill fixtures pass a canonical root, so a crowded or symlinked
  `$TMPDIR` (the macOS default) no longer fails them. Production discovery is
  unchanged.

### Steering reliability

- Mid-turn steering and queued follow-ups now preserve FIFO order, hydrate at
  the next model-round boundary, and remain editable while persistence runs.
  A failed `user.message` append stays queued without entering the live bus, so
  repairing provenance and retrying the same event identity admits that input
  exactly once, even when a failed sync left its complete bytes in the log.
  If that unresolved admission blocks terminalizing a detached shadow call,
  the live session fails closed and resume records its unknown outcome before
  accepting more activity.

### TUI interruption

- Escape now dismisses the active slash menu or picker before a later Escape
  interrupts the running turn. Root-turn cancellation releases the session
  from blocked provider reads and rejects late provider events; a synchronous
  adapter thread can remain until its OS or network call returns. Euler kills
  owned shell or Git process groups before reaping their leaders, without
  waiting for the command timeout. Explicit companion runs, permission asks,
  guardian reviews, and managed-process extension commands share the same
  cancellation source. An open model call closes with one parented session
  cancellation error, and recorded tool calls without an earlier result close
  with exactly one cancelled result, retaining collected partial output and
  boundedly observed file-change evidence. Interrupt also clears queued
  activities visibly while preserving queued user steering.

### TUI input across a resize

- A key pressed while the terminal is being resized is no longer held until
  the next keystroke. crossterm 0.29 dropped input that arrived in the same
  wakeup as a resize (crossterm-rs/crossterm#1126); euler now detects input
  left in the tty queue and requests a cursor report so the reader picks it
  up. This reproduced on macOS as the intermittent fold-toggle-after-resize
  test failure; the same test's failures on loaded Linux runners (#177) did
  not reproduce with the fix disabled and remain open.

### Compaction resilience

- Automatic and manual compaction now use a fixed shadow canvas when tool-result
  stubs cannot recover enough space. The active TUI turn remains usable while
  the bounded summary runs; only a validated candidate can atomically replace
  the canvas, and a failed candidate leaves the active history unchanged.
  Assembled byte pressure is checked independently at provider admission:
  Euler waits for recovery and never dispatches a driver canvas over budget.
- Layer-1 compaction advances instead of repeatedly swapping the same tool
  results, stacks correctly over a prior full projection, and resets stale
  context usage only after an accepted swap. At the hard context margin, Euler
  waits for an in-flight candidate and otherwise stops with an honest limit
  event.
- Shadow jobs now close at session lifecycle boundaries, preserve completed
  usage and cost records, and cannot publish late output into a replaced or
  scrubbed session. Candidate validation is host-bounded and proves the exact
  post-swap request both shrinks and fits before accounting is reset.
- Escape at the base composer now interrupts an idle shadow without applying
  its candidate, preserves already-ready usage provenance, reports failure
  separately from cancellation, and preserves the draft. Interrupting a
  driver with a concurrent shadow terminalizes and fences both calls, so
  neither provider's late return can append events or swap the canvas.

### Extension boundary redaction

- Extensions now receive model-tool input redacted, and children spawned on
  an extension's behalf send their task text, system prompt, explicit
  context, and inherited parent canvas redacted. Only CodeSwarm's dedicated
  tool did this before; every extension now gets it (ADR 0022). The session's
  own provenance stays faithful, and `agent.spawn` marks these children with
  `egress_redacted: true`.

### Extension user messages (#163)

- A failing extension command can now tell the user and the model why, through
  `ExtensionError::UserMessage` or a managed-process
  `error.data.user_message`. The host redacts it and keeps it only when it is
  at most 1 KiB, free of control characters other than newline and tab, and
  format-safe. It then reads `extension command failed: <message>` in the
  failed tool result and user command output, and is recorded as
  `user_message` on the extension `error` event. Anything else keeps the
  generic failure (ADR 0018 amendment).

### Session lock release (#243)

- Dropping a provenance writer now releases its session lock even while a
  child process spawned on another thread still shares the lock descriptor
  between fork and exec. Reopening a session right after closing it, for
  example to scrub it, no longer fails intermittently with a session-lock
  error while a tool or sandbox probe is starting.

### Research-neutral core prompts

- The fixed root instructions now introduce Euler as a research and coding
  agent rather than a coding agent, matching `docs/vision.md`.
  `system_instructions_version` is now `2`, so provenance distinguishes calls
  made under the new text. The shadow compactor and permission guardian
  prompts no longer assume a coding task.

### Extension workflow scaffolding

- Extension commands can declare bounded model tools and one terminal-idle
  contributor. Implicit idle work never opens a permission prompt: it runs only
  with standing authority and records an expected rejected stop otherwise.
  Accepted continuations run under the ordinary `RoundLoop` limit with no
  hidden second cap, and become one-shot only when an admitted `model.call`
  binds their exact prepared canvas snapshot; a snapshot-only crash preserves
  them for recovery. Request links fail closed on duplicate event ids,
  malformed snapshot accounting, stale snapshots, or crossed session/agent
  identity, and resume rejects duplicate ids before appending recovery.
- Session-private extension state now has its own `extension-state` capability.
  Root sessions allow it and bounded, namespaced `context-slot` updates by
  default so removable workflows can probe absent or resumed state.
- Root sessions now allow `artifact-write` by default, alongside
  `extension-state`, `context-slot`, and `plan-presentation`, so an extension
  model tool writes its host-mediated artifact without a prompt (ADR 0022).
  Headless `euler exec` tiers keep this default. Child and companion gates are
  unchanged.
- The spawn preflight error for an unconfigured provider no longer names a
  reviewer-model picker; it still points at `/login`.
- A model-tool descriptor may declare `max_input_bytes`, from 1 byte up to a
  host cap of 320 KiB, as the budget for its whole serialized input. Without
  it the budget stays 64 KiB. Any other value fails manifest parsing and
  registration (ADR 0022).
- A `user` command may declare a model-targets argument in its manifest
  `args`: 1 to `max` (at most 16) `provider::model` targets. The TUI opens the
  authenticated-model picker for a required one, and `euler extension run` and
  the headless `extension_run` line take
  `--<flag> a::b,c::d`. The host checks count, shape, and duplicates on every
  route, JSON input included, and the extension receives a JSON array of target
  strings (ADR 0022).
- Extensions can keep persistent config: `read_extension_config` returns the
  calling extension's project (`.euler/extensions/<id>/config.json`) and user
  (`~/.euler/extensions/<id>/config.json`) documents, and
  `write_extension_config` replaces one under `config-write`. Documents are
  JSON objects of at most 16 KiB, written by temp file and rename; a malformed
  document is an error and a value matching a known secret is refused. A
  project-tier write from a model tool always asks a human (no grant or
  session allowance covers it, the approval panel offers only allow-once, the
  guardian never reviews it, headless runs deny it), and lifecycle hooks
  cannot write the project tier. Both calls are managed-process host methods
  (ADR 0022).
- Extensions can publish bounded typed plan presentation through
  `plan-presentation`; core emits canonical structured `plan.update` events
  and renders one checklist cell without taking ownership of workflow policy.
- Disabled extensions no longer leak durable context slots into live model
  snapshots; re-enabling restores their retained state. Extension-authored
  model text consistently rejects Unicode format controls at every admission
  boundary.
- The maintained Python managed-process SDK and examples live in
  `euler-extensions`; Euler retains only an explicitly test-owned protocol
  fixture.

### CodeSwarm leaves core

- Core and the CLI no longer carry a CodeSwarm path: the built-in
  `code_swarm_review` tool, its `.euler/code-swarm.json` and
  `~/.euler/code-swarm.json` reviewer config, and the `/code-swarm` command
  are gone. Reviews come from the `code-swarm` extension's own model tool,
  configured through the extension's commands, which can import a legacy
  reviewer file (ADR 0022).
- Review results now arrive as an extension tool result, which is redacted
  like every extension tool result; the reviewers' own events stay faithful.
- A resumed session that ended mid-review is closed like any other
  interrupted tool call, and older review events stay readable.
- A test now fails if `euler-core`, `euler-sdk`, `euler-agents`, or
  `euler-event` names a first-party extension outside test files.

### ChatGPT subscription compatibility

- GPT-5.6 Luna WebSocket requests again disable parallel tool calls, as
  required by the subscription route's Responses Lite protocol. Top-level
  WebSocket error frames are now terminal provider errors instead of being
  ignored while the session waits indefinitely for another frame. Subscription
  entitlement failures are terminal rejections rather than retryable rate
  limits.

## 0.1.3 — 2026-07-24

The first tagged release since v0.1.1; the 0.1.2 version number was used
during development (and is the floor for price-bearing provider catalogs)
but was never published, so everything below ships together in 0.1.3.

### Agent session resilience

- Shell and Git results now retain complete redacted output in their canonical
  events while exposing bounded head/tail previews to the active canvas and
  transcript. Every preview names its result event, and `tool_result_get`
  supports bounded byte windows so omitted output remains recoverable without
  repeating a command.
- Provider calls now retry bounded, pre-output rate-limit failures through the
  same cancellation-aware policy as transient transport failures. Rejections,
  authentication failures, stream truncation, and partial responses still
  fail without replay; retry diagnostics identify the failure category.
- Under ordinary host execution, agent-launched shell and Git processes use a
  private temporary Euler home and no longer inherit Euler's ambient route,
  TTY, or metrics controls. Credential scrubbing and project-environment
  inheritance, including `RUST_LOG`, remain intact; enforced sandboxes keep
  their private minimal environment.
- Root-agent instructions now require completing or honestly blocking the
  requested work and verifying before claiming success. Each fixed instruction
  identity is persisted with its complete text when first used, with its
  version, digest, and byte length repeated on model-call provenance.

### Core-only extensions (ADR 0015 end state)

- Euler no longer ships bundled extensions. The six formerly bundled crates
  (`session-export`, `causal-dag`, `code-swarm`, `diagnostics-report`,
  `autoresearch`, `maxproof`) are removed from the workspace along with the
  entire bundled machinery (`bundled_extensions.rs`, `StaticExtension`,
  bundled-id reservations, the bundled/linked registry distinction). The
  linked/installed registry is the only extension surface; first-party
  extensions live in the euler-extensions repository, and their ids are now
  free for linked packages.
- Cutovers: the code-swarm review tool and session export resolve their
  extensions from the linked registry, revalidating fingerprint and
  launch consent at execution time; with nothing linked, the tool is absent
  and the CLI explains how to add it. `--observe` resolves only linked
  observers. Stale enablement entries for formerly bundled ids are skipped
  instead of failing session start; `--extensions` and project overlays still
  reject unknown ids.
- The top-level `euler session-export` command is removed. Session export now
  runs through the extension lane only:
  `euler extension run session-export.session-export <session> [--input-file
  <json-object-file>]`.
- The causal-dag TUI surface (`/causal-dag`, `/catch-up`, pickers, footer DAG
  stats) is removed pending the extension's behavior redesign; its schemas
  and golden fixtures are preserved as a spec-only package in
  euler-extensions.

### Footer cost

- The TUI footer now shows cumulative model cost in USD beside context usage,
  using persisted per-result quotes from verified provider-catalog schedules.
  Quotes cover input, output, cache-read, cache-write, and long-context pricing;
  totals include companion calls, survive model switches and resume without
  repricing history, and `/usage` explicitly reports any unpriced calls. The
  footer cost chip follows absence over punctuation: it appears only when the
  priced subtotal exceeds zero, as the plain `$N.NNN`. A zero subtotal (free,
  no calls yet, or only unpriced calls) shows no chip at all, with no `$0` or
  `$?` placeholder; the mixed case (nonzero priced plus some unpriced calls)
  shows the plain subtotal unmarked. The full completeness detail
  (`$? (N unpriced calls)` and `$N.NNN+ (N unpriced calls)`) stays in `/usage`.
  Ambiguous cache-write TTLs fail closed instead of being assigned a guessed
  rate, and price-bearing catalogs require Euler 0.1.2 or newer.

### Mid-turn steering (#147)

- Input typed while a turn runs now steers that turn: entries are absorbed
  at the next round boundary as canonical `user.message` events the model
  sees in-turn, instead of waiting for turn completion. Order-preserving
  (steering never overtakes queued input), ack-after-persist (failures keep
  entries queued), and interrupt-safe (Esc preserves pending input). The
  running footer hint reads `⏎ steer`; pending messages use a one-line,
  word-bounded preview while their full text remains queued unchanged.

### Session locking (#151)

- Provenance writer ownership moved from PID files to OS advisory locks
  held for the writer lifetime: crashes release the lock automatically on
  every platform, and the persistent `events.jsonl.lock` file carries
  diagnostic-only owner metadata. Legacy bare-PID lock files are refused
  with recovery guidance rather than auto-claimed (a pre-lock Euler may
  still be live). The TUI no longer hard-exits during in-flight quits.

### TUI performance (#137–#141)

- Enter-to-first-paint no longer scans the session store: `/resume` picker
  items list lazily, turn-end recency updates touch one sidecar, and the
  extension-registry listing is cached (#137). Session listings cache
  their projection in the sidecar keyed by the event log's identity (#138).
- Scrollback commits derive the cursor from tracked state — zero blocking
  `ESC[6n` round-trips in steady state (#139). The finalized-history
  render cache is incremental (#140) and the streaming answer's committed
  prefix is memoized (#141), so long sessions and spinner ticks stop
  re-rendering the world.

### Warm Spine v2.1 (#132, #134–#136)

- The v2.1 design pass over the Warm Spine sections below: refreshed diff
  rendering with symbol labels and a markdown code hairline, Codex
  vocabulary, a single unified picker, permission postures, quit-time
  chrome cleanup, and turn recaps that report only what a turn changed
  (suppressed when no files moved).

### Managed-process extensions (#130, #131, #133)

- Out-of-process extension runtime: extensions run as separate processes
  over stdio JSON-RPC, with a Python SDK (now maintained in
  `euler-extensions`). Linked managed-process extensions run in live sessions,
  and round observers can be managed processes.

### Permissions, sandbox, compaction (#122, #125–#129)

- Operation-scoped permission approval (#127, ADR 0013) and the
  fail-closed Linux workspace sandbox (#126/#129 series, ADR 0014).
- Configurable compaction policy (#122) and causal-DAG durable research
  records with observer reconciliation hardening (#125, #128).

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
