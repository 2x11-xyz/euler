# Tool Contract

## Skill snapshot reads

`skill_read(name)` reads one accepted skill body from the current session's
immutable project-context snapshot. The tool is exposed to the model only when
that snapshot contains at least one accepted skill. The `name` argument must
exactly match a normalized skill name from the compact catalog; Euler performs
no trimming or other argument normalization.

The tool performs no filesystem access, executes no helper, grants no
permission, and requires no capability. Its result carries stable scope,
source path, and body-digest metadata in a core-owned header, followed by the
frozen body with a core-owned indent on every line. Removing that indent
reproduces the body bytes; body text can never occupy a core marker position.
Every read is recorded through ordinary `tool.call` and `tool.result`
provenance. Its result carries `project_context_snapshot_digest`, the candidate
digest of the immutable snapshot that supplied the bytes. User-global skills
remain available when repository context is disabled; project skills follow
the repository context admission decision recorded in the snapshot.

`skill_read` and the skill catalog are root-driver-only: companion and
spawned agents are advertised the coding substrate without `skill_read`, and
a companion call to it is refused — children default to project-context
`none` and receive no skill surface until `inherit` wiring lands
(docs/contracts/project-context.md).

The classification survives canvas projection, compaction, and
`tool_result_get`. Child retrieval is policy-aware even though retrieval reads
the canonical session event stream: `none` rejects every classified result,
while `inherit` accepts only an exact candidate-snapshot-digest match and
propagates the same classification to the new result.

User-explicit `/skill:<name> [request]` activation is a session command, not a
synthetic tool call. It resolves against the same frozen registry and uses the
same attributed framing, but remains a canonical `user.message`; Euler never
forges `tool.call` or `tool.result` provenance for it.

Core tools are the minimal coding substrate.

Tool calls must be permission checked, provenance logged, and represented cleanly in the active canvas.

Extension tools use the same contract as core tools. There should not be a second-class tool path.

An extension model tool is an explicitly advertised `agent-only` extension
command (ADR 0018). It uses the same `tool.call` / operation-scoped
`permission.*` / `tool.result` braid as a core tool, with additive
`extension_id` and `command` attribution. Core validates its closed, bounded
input schema before approval, and a successful result is a bounded JSON object.
Extension host events may occur between call and result; the call id remains
the canonical pair key. Extension tools are root-session only.

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
| `read_file` | FsRead | Relative path; optional line offset / max_bytes / max_lines. A sensitive path (capabilities contract, “Sensitive-basename ask”: anything under a `.git` component, git/npm/cargo/shell configuration, `.env*`, `*secret*`, `*credential*`, `id_rsa`, `id_ed25519`, `*.pem`, `*.key` — literal or symlink-resolved) escalates the request from blanket `session-allow` to an explicit ask. |
| `edit_file` | FsWrite | Single exact replacement. The prepared pre-image must still match at apply time (see “Structured file confinement”). |
| `write_file` | FsWrite | Create a new file from plain `{path, content}` — no patch dialect. Create-only: fails if the file exists (use `edit_file`/`apply_patch` to modify) or the parent directory is missing. Emits the same `patch.proposed`/`patch.applied`/`file.change`/`file.diff` provenance as the add path of `apply_patch`. |
| `apply_patch` | FsWrite | Structured single-file patch |
| `run_shell` | ShellExec | Workspace root; timeout bounds. Canonical output is complete; the active canvas receives a bounded, recoverable head/tail preview when needed. |
| `git_status` / `git_diff` | FsRead | Workspace git views. Canonical output is complete; the active canvas receives a bounded, recoverable head/tail preview when needed. |
| `tool_result_get` | FsRead | Rehydrate a demoted, compacted, or previewed tool result from the **current session** by `event_id` (required); optional `offset_bytes` (default `0`) and `max_bytes` (default 64 KiB) select a byte window. Session-local and project-context-policy-aware for children. |
| `code_swarm_review` | AgentSpawn | Session-level review gate over required explicit `focus` (≤7 KiB) and `context` (≤256 KiB). The calling agent gathers material first through ordinary tools, so this gate has no hidden file, git, GitHub, or network authority. It forwards only that supplied context and a small reviewer brief — never ambient session canvas — fans out the persisted reviewer set, and returns every finding for caller adjudication. Optional: `personas`, `models` (non-empty one-off override; an empty model-facing list is omission), `max_tokens`. Advertised only in the root session when the `code-swarm` extension is wired and enabled; companions never see it (depth one). Config, result shape, and failure honesty: multi-agent contract. |

## Structured file confinement

`read_file`, `edit_file`, `write_file`, and `apply_patch` never hand a joined
path to the kernel. Each target is resolved by walking down from the workspace
root to the target's *parent directory* and holding that descriptor: on Linux
one `openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS |
RESOLVE_NO_MAGICLINKS`, falling back to a hop-by-hop `O_DIRECTORY | O_NOFOLLOW
| O_CLOEXEC` walk when that syscall is unavailable (kernels before 5.6, or a
seccomp profile that denies it); the fallback is latched process-wide, which
is safe because that capability does not change while Euler runs and the
walker is itself confined. Other Unix hosts always walk hop by hop. A
component replaced between path resolution and the open — a directory swapped
for a symlink, the root itself substituted — fails the walk rather than
escaping the root. Every subsequent check runs on a descriptor obtained from
that directory, so it describes the file that is actually read or written.

Mount crossings inside the workspace are allowed. Refusing them would break
ordinary setups (a devcontainer volume at `node_modules/`, a tmpfs at
`target/`), while planting a hostile mount inside the workspace needs
privileges the same-user threat model already excludes.

Euler ships on Linux and macOS. There is no confined open for other targets,
so the structured tools fail closed there rather than opening a joined path.

- **Regular files only.** A target that is not a regular file when the
  descriptor is opened is refused for reads and writes alike.
- **Creates are exclusive.** `write_file` and the add path of `apply_patch`
  open with `O_CREAT | O_EXCL`, so a file that appeared after the tool call
  was prepared is reported as already existing and is never clobbered.
- **The prepared pre-image must still be there.** A modifying write reads the
  target's current bytes and compares them to the exact content the tool call
  was prepared against. Anything else — a user edit, a `git checkout`, another
  agent — is a refusal, not an overwrite, and the file is left alone. The
  comparison and the rename are two steps: a same-user edit landing between
  them is replaced. Closing that would need file locks Euler does not take,
  and the writer is a same-user process the workspace already trusts. A
  symlink planted at the target name in that window is replaced as a
  directory entry, never written through, so it cannot redirect the write.
- **Writes are atomic.** The new bytes go to a temporary file created in the
  same confined directory, are given the target's permissions, are made
  durable, and are then renamed over the target name; the directory is synced
  afterwards. The target is only ever its complete old content or its complete
  new content, never a truncated intermediate, so a crash or an I/O failure
  mid-write cannot leave a partial file. A create works the same way and adds
  a no-replace rename (`renameat2 RENAME_NOREPLACE` on Linux, `renameatx_np
  RENAME_EXCL` on macOS), so it publishes atomically and still refuses a name
  that appeared after the call was prepared.
  The replacement inherits the old file's permission bits masked to `0o777`
  (setuid, setgid, and the sticky bit are never carried onto agent-written
  content) and, where the process has the privilege, its ownership. Extended
  attributes and ACLs are not copied; that loss is inherent to replace-by-
  rename and is shared with `git` and most editors.
  Because the replacement is a new inode, any other hard link to the old file
  keeps the old content: editing a multiply-linked file (a pnpm store,
  `cargo vendor`, `cp -al`) is allowed, stays confined, and silently breaks
  the link — the alias keeps the pre-edit bytes.
  A crash between creating the temporary file and renaming it can leave a
  `.euler-write-<id>.tmp` sibling. Nothing sweeps them: enumerating the
  directory by path would break the fd-anchored rule, and unlinking by name
  could delete a concurrent Euler's in-flight temporary. Workspace
  observation ignores the name shape instead, so a leftover is never reported
  as a change; deleting one is safe and left to the user.
- **A read-only target is refused, not replaced.** Publishing by rename makes
  the kernel check the directory rather than the file, so a write to a file
  the user made read-only (`chmod a-w`), or into a read-only directory, is
  refused before any checkpoint is recorded — matching what an in-place write
  would have done.
- **A published write is applied.** Once the rename succeeds the change is
  present. If the directory entry cannot then be made durable, the tool still
  succeeds and carries a durability warning; it is never reported as a failed
  write.

## Rollback checkpoints

A modifying structured write stores its rollback pre-image *before* the
destructive write and records it as a `checkpoint.stored` event. If the
pre-image cannot be stored durably the write does not happen at all, and the
tool fails saying the file was not changed. After the write completes, the
`file.change` event carries the same `pre_image_blob` plus
`checkpoint_event_id`; that `file.change` existing is what makes the
checkpoint applied and restorable.

`/rollback` lists applied checkpoints only. A `checkpoint.stored` record with
no `file.change` referencing it describes a write that was never observed to
complete, so restoring it would be a destructive edit of its own. Before
restoring, Euler verifies that the file still holds what the *newest*
`file.change` for that path recorded — so an A→B→C chain can be rolled back to
A, while a change made outside the ledger is refused rather than discarded.
The verification and the replacement run against one confined target, so an
edit landing between them is refused too. A checkpointed file the user deleted
has nothing to discard and is recreated.

A restore is itself a destructive write, and is recorded as one: it
checkpoints the content it replaces and appends its own `file.change` with
origin `workspace.restore`. That advances the baseline the next rollback
verifies against — so rollback is not one-shot per file — and makes the
restore undoable like any other write — except recreating a deleted file,
which replaces nothing and reports that it cannot be undone. The per-path
baseline includes changes a shell command was observed to make, so an edit
made by `run_shell` after a checkpoint blocks a naive restore just as a user
edit does. Event shapes: `docs/contracts/events.md`.

Process launch/executor completion and process success are separate facts.
`run_shell` and direct Git tools retain collected output and the observed exit
code whenever execution reaches a process result, but only exit code zero is a
successful `tool.result`. A nonzero exit is canonical failure (`ok: false`, an
`error`, plus any collected `output` and `exit_code`) and is supplied to the
next model as failed tool output. Legacy event compatibility is owned by the
effective-outcome rule in `docs/contracts/events.md`, not by individual tool or
UI special cases.

## Execution boundary

On Linux, Bubblewrap is the default and enforced backend for `run_shell` and
the `git_*` tools (ADR 0021 row A′). The child gets a private root tmpfs, its
workspace bound read-write at `/workspace`, a private `/tmp`, `/proc` and
`/dev`, a tmpfs `HOME`, no host network namespace, and a cleared environment.
Every other platform has no backend yet, so those tools run directly on the
host under the ordinary permission decision; the Seatbelt backend replaces
that.

Because `HOME` inside the sandbox is a private tmpfs, toolchains installed
under the real home would otherwise be unreachable. Euler detects the
toolchain homes the host environment implies — `CARGO_HOME`, `RUSTUP_HOME`,
`NVM_DIR`, `PYENV_ROOT`, `ASDF_DATA_DIR`, `GOPATH`, `PNPM_HOME`, each falling
back to its conventional location under `$HOME`, plus `/nix/store` — and binds
them read-only at their real paths, with the sandbox `PATH` built from the
host `PATH` entries those roots contain and caches redirected to the sandbox
cache tmpfs. The real `$HOME` is never mounted read-write: the directory
holding those roots is a read-only mount, so a write under the real home
fails rather than landing in a discarded private copy.

Cargo's credential files (`credentials.toml`, `credentials`) are masked with
an empty file wherever the sandbox can reach the Cargo home, not only where
Euler mounts it itself — the official Rust images put `CARGO_HOME` under
`/usr/local`, which the system runtime bind already carries. Cargo also
accepts a registry token in its config, which is **not** masked: that file
carries the registry sources and build settings a build needs. Euler reports
it once at session start instead, naming the file and suggesting the token
move to `credentials.toml`. A Cargo home *inside* the workspace is neither
masked nor reported — the workspace is readable by design — but its variable
and `PATH` are re-pointed at the bound workspace path so the toolchain still
works.

Availability is probed at session start by running a trivial sandboxed
command, because an installed `bwrap` is not evidence that it works. The
outcome is recorded on `session.start` as `sandbox_backend`
(`bwrap` | `host` | `unavailable`) with `sandbox_unavailable_reason`. When the
probe fails, sandbox-requiring tools fail closed with a concise reason; there
is no automatic fallback to host execution. `euler --check-sandbox` runs the
same probes and prints the diagnostic, which names the likely cause
(user namespaces disabled by sysctl or AppArmor, a container, WSL1, `bwrap`
missing) and the host change that fixes it.

Euler's own Git invocations are neutralized before they run (ADR 0021 row G).
`git_status` and `git_diff` set `core.hooksPath=/dev/null`,
`safe.bareRepository=explicit`, `attr.tree=`, `core.attributesFile=`, and
`GIT_LFS_SKIP_SMUDGE=1 GIT_TERMINAL_PROMPT=0 GIT_OPTIONAL_LOCKS=0`; they strip
the `GIT_DIR` / `GIT_WORK_TREE` / `GIT_CONFIG*` / `GIT_INDEX_FILE` /
`GIT_ALTERNATE_OBJECT_DIRECTORIES` family from the environment; they blank
every configured `filter.*.clean` and `filter.*.process` driver through
`GIT_CONFIG_KEY_n`; and `git_diff` keeps `--no-ext-diff --no-textconv`.
`core.fsmonitor` is probed and preserved only for Git's built-in daemon rather
than blanket-disabled, and `diff.ignoreSubmodules=dirty` stops the recursive
submodule spawn, which would otherwise run a driver configured in a
submodule's own config with only the superproject's blanking applied. The same
overrides cover every git invocation Euler makes, including the `@`-mention
picker's `git ls-files`. A command the agent runs itself through `run_shell`
is confined by the sandbox instead and keeps the repository's configuration.

`diff.ignoreSubmodules=dirty` has a visible cost: worktree edits inside a
submodule do not appear in `git_status` or `git_diff`. Hiding that would be
the silent loss ADR 0021 row E forbids, so when the repository declares
initialized submodules the tool output says the changes are not shown and
where to see them, on a run that completed. The flag stays because the
alternative is running a driver configured in a submodule's own config.

A probe Euler cannot complete fails the tool closed: only `git config`'s
"nothing configured" exit is an answer. Residual risk: the probe and the real
command are separate processes, so a writer that adds a driver between them is
not covered; under the Linux sandbox that driver runs inside the sandbox, and
on a host backend it needs an agent racing its own tool call.

Under ordinary host execution, agent-controlled shell and Git subprocesses
inherit project environment variables, including `HOME` and `RUST_LOG`, but
not credential-shaped values or the owning Euler process's routing, TTY, and
metrics controls. They receive one private temporary `EULER_HOME` per live
tool registry, preventing a nested Euler from falling through to the user's
`$HOME/.euler`; the directory is removed with the registry. Commands may set
an explicit command-local value when the task requires one. Enforced sandbox
profiles retain their documented private, minimal environment.

`code_swarm_review` is not executed by the `ToolRegistry`: it is a
session-level tool intercepted after the ordinary permission gate, because
its execution spawns child agents through the session. It rides the same
`tool.call` / `permission.*` / `tool.result` provenance shape as every
other tool.

`code_swarm_review` is deliberately not a source-acquisition tool. Its
`context` is material the calling agent already selected using ordinary core
tools, whose permissions and provenance remain visible at the retrieval step.
This keeps the review gate's authority honest and its model-facing canvas
small: reviewers receive only explicit context, not the parent canvas.

Euler-owned shell and Git subprocesses run in a host-owned process group.
Cancellation or timeout signals that group before reaping its leader, covering
ordinary descendants that remain in the group; a descendant that deliberately
escapes it is outside this guarantee. After cancellation, Euler drains only
immediately available pipe data within a fixed byte budget. Ordinary
`run_shell` cancellation may then spend bounded time observing file changes
for evidence: at most 4,096 files (configurable), 256 KiB per file, and 64 MiB
total. That finite evidence pass can delay terminal publication after the
process has already stopped.

Reaching a bound does not block the command (ADR 0021 row E). The command
runs; when either the before or the after capture is incomplete, the tool
result text leads with `file observation incomplete: <reason>; changes may be
unreported`, and the result carries an `observation` object
(`{status: "incomplete", reason, bound}`) distinct from process success. An
incomplete capture reports no file changes, and that is "not observed", never
"no changes" — the two are textually distinct in the agent-visible output.
`.git`, `node_modules`, `target`, `dist`, `build`, `vendor`, the Python and
JS cache directories, and structured-write temporary files are excluded from
the walk.

When canvas previews or stubs show `event <id>` (and optional
`handle event:…` / `blob:…` metadata), prefer `tool_result_get` with that event
id over re-running the original tool if the original inputs are expensive or
non-idempotent. Continue a bounded result at the returned `offset_bytes`.
Offsets and result ranges address the redacted, canonically stored output.
They are UTF-8 byte positions and returned ranges are half-open. An offset
inside a code point advances to its next boundary; a byte budget smaller than
the next code point expands just enough to return that code point and guarantee
progress. Blob-hash lookup is not supported: live and resumed sessions keep
content inline.
