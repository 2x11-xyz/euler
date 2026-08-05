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
| `read_file` | FsRead | Relative primary-root path or canonical absolute path inside an attached writable root; optional line offset / max_bytes / max_lines. A sensitive basename (capabilities contract, “Sensitive-basename ask”: `.env*`, `*secret*`, `*credential*`, `id_rsa`, `id_ed25519`, `*.pem`, `*.key` — literal or symlink-resolved) escalates the request from blanket `session-allow` to an explicit ask. |
| `edit_file` | FsWrite | Single exact replacement |
| `write_file` | FsWrite | Create a new file from plain `{path, content}` — no patch dialect. Create-only: fails if the file exists (use `edit_file`/`apply_patch` to modify) or the parent directory is missing. Emits the same `patch.proposed`/`patch.applied`/`file.change`/`file.diff` provenance as the add path of `apply_patch`. |
| `apply_patch` | FsWrite | Structured single-file patch |
| `run_shell` | ShellExec | Primary workspace cwd inside the enforced writable-root boundary; timeout bounds. Canonical output is complete; the active canvas receives a bounded, recoverable head/tail preview when needed. |
| `git_status` / `git_diff` | ShellExec | Workspace Git views inside the same enforced subprocess boundary as `run_shell`. Repository config, attributes, and submodules can select process helpers, so these tools are never represented as pure `fs-read`. Euler disables optional locks, fsmonitor, external diff, and textconv on these built-in views; it still snapshots every writable root and fails the view if any remaining helper mutates one. Canonical output is complete; the active canvas receives a bounded, recoverable head/tail preview when needed. |
| `tool_result_get` | FsRead | Rehydrate a demoted, compacted, or previewed tool result from the **current session** by `event_id` (required); optional `offset_bytes` (default `0`) and `max_bytes` (default 64 KiB) select a byte window. Session-local and project-context-policy-aware for children. |
| `code_swarm_review` | AgentSpawn | Session-level review gate over required explicit `focus` (≤7 KiB) and `context` (≤256 KiB). The calling agent gathers material first through ordinary tools, so this gate has no hidden file, git, GitHub, or network authority. It forwards only that supplied context and a small reviewer brief — never ambient session canvas — fans out the persisted reviewer set, and returns every finding for caller adjudication. Optional: `personas`, `models` (non-empty one-off override; an empty model-facing list is omission), `max_tokens`. Advertised only in the root session when the `code-swarm` extension is wired and enabled; companions never see it (depth one). Config, result shape, and failure honesty: multi-agent contract. |

Process launch/executor completion and process success are separate facts.
`run_shell` and direct Git tools retain collected output and a process exit
code when one exists. Only a normal exit code zero is a successful
`tool.result`. A nonzero exit is canonical failure (`ok: false`, an `error`,
plus any collected `output` and `exit_code`) and is supplied to the next model
as failed tool output. Timeout, cancellation, supervision loss, and a Git view
invalidated by observed mutation are also canonical failures, but their error
must name that terminal condition rather than falsely claiming that the
process exited normally. A compatibility status may accompany such a result;
the durable error and collected output retain the truthful reason, and a
timeout/cancellation/signal/abnormal/supervision header never renders that
sentinel as an `exit`. Legacy
event compatibility is owned by the effective-outcome rule in
`docs/contracts/events.md`, not by individual tool or UI special cases.

## Workspace subprocess authority

Model-controlled shell and direct Git processes never execute directly with
the Euler process's host filesystem authority. `SubprocessSandbox::Disabled`
means those tools fail closed. On Linux the active Bubblewrap profile mounts a
private root, the canonical primary workspace plus at most seven explicit
non-overlapping `--writable-root` directories, and no other writable host path.
The single `workspace-no-network` profile also creates a separate network
namespace: host networking and localhost services are unavailable. Unsupported
platforms, a missing backend, an invalid root set, or a failed profile probe
produce a tool failure and never fall back to host execution.
Tests that require a successful agent subprocess are Linux-only; non-Linux
coverage asserts the typed `UnsupportedPlatform` failure and absence of side
effects rather than pretending the command can succeed.

Bubblewrap stdout and stderr are private launcher channels, never tool output.
Before launch, Euler atomically creates two write-only child-output pipes,
closes every other inherited non-stdio descriptor at exec, and whitelists only
those exact pipe writers in the post-fork launcher child. The inner wrapper
writes independent NUL-framed readiness markers to the dedicated pipes after
Bubblewrap setup, then redirects only the agent command's stdout and stderr to
them. Launcher diagnostics written before or after readiness therefore cannot
enter tool output, transcript, model context, or provenance. If either marker
is absent, a normal completion becomes the concise typed enforcement failure;
cancellation before readiness returns no raw launcher output.

Structured file tools use a relative path for the primary workspace. An
attached root is selected by a canonical absolute path under that exact root;
other absolute paths and traversal/symlink escapes fail. Tool descriptions list
the accepted attached roots. V0 path-scoped grants have no durable multi-root
identity, so attached-root writes do not invent index aliases or reuse
primary-root directory grants: they remain explicit asks unless the whole
`fs-write` capability is session-allowed.

Structured path authority is independent of Bubblewrap availability. A missing
or unsupported subprocess backend blocks shell/Git tools without disabling
ordinary structured file tools. On Linux, however, structured reads and writes
use the same kernel mount-identity boundary as subprocess roots: the mount table
is validated at registry construction and immediately before access, and a
distinct nested mount ID fails closed even when it has the same device number.
The final file open uses `openat2` relative to the selected root with
`RESOLVE_BENEATH`, `RESOLVE_NO_XDEV`, `RESOLVE_NO_MAGICLINKS`, and
`RESOLVE_NO_SYMLINKS`, closing the mount/symlink race at the file descriptor.
If Linux cannot inspect that topology or perform the constrained open,
structured access is unavailable rather than falling back to a path-prefix
claim. macOS reads the mount table into bounded Euler-owned `getfsstat` buffers
to freeze/revalidate the root mount topology and uses one descriptor-anchored
`openat(O_NOFOLLOW)` component walk for reads and writes. The opened root must
match the frozen device, inode, filesystem ID, and mount point; `fstatfs` must
then report that same frozen filesystem/mount identity at every hop and for the
final descriptor. Other platforms fail structured access closed until
they have an equivalent mount-identity boundary. Subprocesses remain
fail-closed unless their own OS boundary exists.

Language runtimes outside the system allowlist are explicit read authority,
not writable authority. `--runtime-root` mounts a canonical non-overlapping
UTF-8 directory read-only and adds that directory (and its `bin/`, when
present) to the sandbox PATH. Runtime-root paths containing the PATH separator
`:` are invalid rather than ambiguously expanding execution search authority.
The configured host home and every ancestor that would expose it are rejected;
an intentionally prepared subdirectory beneath that home remains eligible as
the exact, explicit read-only authority.
After canonicalization, neither a writable root nor a runtime root may equal
or contain `/tmp`, `/proc`, `/dev`, `/tmp/home`, or `/tmp/cache`: mounting such
a root later in the profile would replace a private sandbox mount. Exact
descendants such as `/tmp/toolchain` remain eligible because the private
target does not lie beneath them.
Euler never mounts the user's home, package caches, global
Git config, credential stores, SSH keys, or provider secrets implicitly.
Repository-local Git config remains available in a writable root. The
profile does not mount resolver configuration and has no network. Authenticated
subprocess Git/`gh`, package fetches, and publication need a future
authenticated broker or isolated-egress design. A deliberately prepared
runtime directory may expose read-only toolchain/configuration data, but
`--runtime-root` accepts directories, not individual config files. These
workflows must not be made to work by exposing host home or the shared host
network namespace.

Every host-backed bind source has its canonical directory and mount identity
frozen at profile construction. That bounded topology check does not walk the
directory contents, so constructing the session tool registry does not
recursively scan the workspace or host image. Each user-selected root
must resolve through Linux
`openat2(RESOLVE_NO_MAGICLINKS)`. Euler freezes its canonical directory
device/inode identity and reads kernel mount identities from
`/proc/self/mountinfo`: the root must remain that same directory and select
exactly one mount from
the versioned ordinary-filesystem allowlist and contain no distinct nested
mount ID. That allowlist is `bcachefs`, `btrfs`, `ecryptfs`, `erofs`, `ext2`,
`ext3`, `ext4`, `f2fs`, `jfs`, `nilfs2`, `ntfs`, `ntfs3`, `overlay`, `ramfs`,
`reiserfs`, `rootfs`, `squashfs`, `tmpfs`, `ubifs`, `vfat`, `xfs`, and `zfs`;
unknown and control/IPC/pseudo filesystems fail closed. This rejects procfs
magic-link routes, procfs mounted under an ordinary alias, FUSE brokers, and
nested bind mounts even when `st_dev` is unchanged. Path spelling and device
number alone are not mount authority.

Euler walks the resulting sources without following symlinks and rejects Unix
sockets, FIFOs, block devices, character devices, and unknown special node
types even in a read-only source. A bounded or unreadable walk, unavailable
`openat2`, or incomplete mount table makes the boundary unavailable. Every
writable root, explicit runtime root, and fixed system runtime source is
inspected during the cached profile probe and again immediately before each
launch. The aggregate 1,000,000-entry limit is a launch limit, not a
partial-success mode. A successful `tool.call` profile probe therefore
identifies selected bind sources, while the terminal result remains
authoritative about whether the final inspection and launch succeeded.

Before `run_shell`, `git_status`, or `git_diff`, Euler captures every writable
root. Hitting a granular file/byte/read bound or an opaque namespace bound
blocks execution. It captures
every root again after execution; failure there returns an explicit incomplete-
observation error that says the command ran and may have changed files. The
pre-capture freezes each selected protected subtree's path, directory
device/inode identity, and kernel mount identity. Command construction receives
that exact set, revalidates every selected surface immediately before launch,
and binds exactly those paths read-only without rediscovery. Post-capture
reuses and revalidates the same set. Removal or replacement fails closed; a
new `.worktrees` tree that was absent before launch remains writable and is
observed rather than skipped. Process-supervision failures after a successful
spawn kill the owned group and still take this post-capture. The
granular snapshot includes regular files, symlinks, empty/ordinary directories,
modes, and special entries. Regular files larger than the diff-content limit
are still fully hashed so same-length changes remain detectable. Files with a
hardlink alias outside the full writable-root set block launch; aliases wholly
inside the set are all observed.

The built-in Git views minimize repository-selected side effects with Git's
no-optional-locks mode, a `core.fsmonitor=false` command override, and
`--no-ext-diff --no-textconv` for `git_diff`. This does not prove that arbitrary
repository configuration is process-free (filters and submodules remain
examples), which is why the capability stays `shell-exec`. If a view changes a
writable root despite those controls, the result is a canonical failure and
the complete bounded observed change set is emitted with `origin` equal to the
Git tool name.

Common build, dependency, VCS, cache, and Euler-local state directories are
folded into a single `opaque-directory` entry, not silently ignored. Euler walks
their complete bounded namespace, fingerprints descendant metadata, and hashes
small-file content under a separate bounded budget. A changed fingerprint is
canonical evidence that some durable net state changed, but it does not name
the descendant and an equal fingerprint does not claim byte-complete equality
for large or budget-exhausted files. The top-level `.worktrees` collection is
not an opaque writable exception: Bubblewrap overlays it read-only and
structured writes reject it. Launch in the intended checkout (or attach a
separate non-nested root) to mutate that checkout.

Every structured file tool rejects a multiply-linked regular file immediately
after the constrained open and before reading any content. This deliberately
conservative boundary prevents an in-root hardlink to an inode with an
out-of-authority alias from becoming a host-side read channel; `run_shell` is
the only path for files whose aliases are all observed inside the attached
writable-root set. Structured writes re-resolve the path after permission
review, reject a stale whole-file pre-image, and recheck the link count after
pre-image observation immediately before mutation. Linux performs the final
read/create/update open through the constrained `openat2` boundary above;
macOS opens every parent directory and the final file through the shared
`openat`/mount-identity walk above. Other platforms stop before file access.
The canonical root set is
frozen when the registry is constructed; a root later replaced by a symlink,
another canonical path, inode, or Linux mount identity becomes unavailable
instead of changing authority. This closes pathname and mount substitution
between preparation and mutation. Structured read/write/edit paths accept only
regular files (or create a new regular file); they never open a FIFO, socket,
device, or another special node as a host-side I/O channel.
If an I/O failure occurs after the fd is opened or the target is created, Euler
observes that fd before returning the failed tool result. Any net truncation or
partial write is emitted as `file.change` / `file.diff` parented to the tool
call; no `patch.applied` is emitted. If that observation itself is incomplete,
the tool fails with an explicit incomplete-observation error rather than
claiming that no mutation occurred.
The remaining concurrency invariant is narrow: another trusted host actor must
not rename an already-open directory out of the authority set or modify the
same inode through an existing descriptor during the final precondition/write
sequence. It also must not substitute directory entries while Euler performs a
bounded path-based snapshot or authority walk. Detected instability makes the
observation incomplete, but Euler does not claim to contain a same-user process
racing individual host syscalls. For subprocess launch, a same-user host actor
must not introduce or substitute a mount or special node after the final
inspection, or deliberately connect to a socket the sandboxed child creates
later. Such cooperating or hostile concurrent host mutation is outside the
agent-process boundary; Euler never claims the permission gate can contain the
owning host process or other same-user processes. Structured Linux file opens
remain protected against such a mount substitution by `RESOLVE_NO_XDEV` even
after their preceding topology inspection.
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
for evidence: at most 4,096 files, 256 KiB per file, and 64 MiB total. That
finite evidence pass can delay terminal publication after the process has
already stopped.

Structured writes observe an already-open regular file before and after a
failed write. `/rollback` uses that observed path too: it records a terminal
failed `workspace.restore` and emits every captured partial mutation before
returning the write error. It never discards a post-truncate change or claims
that a failed restore completed.

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
