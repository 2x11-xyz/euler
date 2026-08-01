# ADR 0014: Linux workspace subprocess sandbox

## Status

Accepted (2026-07-14; amended 2026-07-31).

## Context

Euler's permission gate records and mediates capability decisions. It is not an
OS boundary: an approved shell running directly on the host would still inherit
the Euler process's filesystem authority. The session incident that motivated
this amendment demonstrated the concrete failure mode: a session declared one
workspace while a shell changed another repository, and a single-root snapshot
then produced no canonical mutation provenance.

`cwd`, command-string parsing, path inspection, and post-hoc snapshots are
evidence or UX mechanisms, not confinement. Retaining the host network
namespace is not filesystem-neutral either: an unauthenticated localhost
service can act as a deputy and write elsewhere on the host. Networked build
and publication therefore require a future isolated-egress or authenticated
broker design, not a shared-host-network variant of this boundary.

Native Rust extensions execute in the Euler process. They are trusted
in-process code, not sandboxed plugins; a child-process boundary cannot contain
them.

## Decision

Euler provides a Linux-only core subprocess backend using the host's `bwrap`
executable. Euler owns the profile and policy; the launcher is resolved only
from fixed system locations, never from an agent workspace or inherited
`PATH`. Agent-controlled `run_shell`, `git_status`, and `git_diff` never have an
unsandboxed host fallback. `SubprocessSandbox::Disabled` means those subprocess
tools are blocked.

The built-in Git views require `shell-exec`, not `fs-read`. Repository config,
attributes, and submodules can choose process helpers even for a view command.
Euler disables optional locks, fsmonitor, external diff, and textconv for the
built-in forms, snapshots all writable roots before and after, and fails the
view with observed provenance if another helper still mutates state. Shell
commands beginning with `git` are never static-safe.

Fresh sessions use the single **workspace-no-network** profile. It has these
invariants:

- a private tmpfs root;
- the canonical primary workspace and up to seven explicit canonical,
  non-overlapping attached roots as the only writable host directory mounts;
- the root-level `.worktrees` collection, when present, overlaid read-only so
  sibling checkouts are not implicitly writable or recursively observed;
- a small read-only system runtime allowlist (`/usr/bin`, `/usr/include`,
  `/usr/lib`, `/usr/lib64`, `/usr/libexec`, `/usr/share`, `/bin`, `/lib`, and
  `/lib64`) plus up to eight explicit canonical, non-overlapping
  `--runtime-root` directories; `/usr/local` and other host-installed
  toolchains remain explicit runtime authority;
- private `/tmp`, `/proc`, `/dev`, home, and cache mounts;
- a cleared minimal environment and no implicit host home, Euler home, package
  cache, Git config, credential store, SSH key, or provider secret;
- every inherited descriptor except two freshly created write-only agent-output
  pipes marked close-on-exec before Bubblewrap launches, atomically where
  supported and otherwise through a post-fork procfs descriptor scan; the two
  exact pipe writers are then whitelisted only in that post-fork child;
- a separate network namespace, with no host network, resolver, or localhost
  access.

No canonical writable or explicit runtime root may equal or contain any of
the private mount targets `/tmp`, `/proc`, `/dev`, `/tmp/home`, or
`/tmp/cache`. Otherwise its later bind could replace a private boundary by
mount order. An explicitly selected descendant such as `/tmp/toolchain`
remains valid because it does not replace the private mount itself.

The two output-writer descriptor numbers remain dynamic. Remapping them to
fixed descriptors such as 3 and 4 inside `pre_exec` could overwrite Rust's
private spawn-error pipe. Bubblewrap passes other non-`CLOEXEC` descriptors to
the initial child while closing its monitor/PID-1 copies, so Euler clears
`CLOEXEC` only on the two owned writer descriptors after sanitizing the full
post-fork descriptor table. The profile/readiness probe fails closed if a
launcher version does not preserve that path.

Canonical root paths, directory device/inode identities, and kernel mount IDs
are frozen with the profile. This identity and topology check is bounded and
does not recursively walk directory contents, so constructing the session tool
registry is not proportional to the size of the workspace or host image.
Every later profile probe and launch requires the same real directories and
mount identities; path, directory, or mount replacement makes the boundary
unavailable instead of redefining its authority.

Read-only bind mounts do not neutralize Unix sockets, FIFOs, or device nodes:
those nodes can convey host IPC or device authority without a normal file
write. Procfs magic links and nested bind mounts can likewise reintroduce host
authority beneath an apparently ordinary directory. Euler therefore requires
Linux `openat2(RESOLVE_NO_MAGICLINKS)` resolution for every user-supplied root,
then validates every canonical writable, explicit runtime, and fixed system
runtime source against `/proc/self/mountinfo`. The selected mount must have one
unambiguous kernel mount ID, use the versioned ordinary-filesystem allowlist,
and contain no distinct nested mount ID. The allowlist is `bcachefs`, `btrfs`,
`ecryptfs`, `erofs`, `ext2`, `ext3`, `ext4`, `f2fs`, `jfs`, `nilfs2`, `ntfs`,
`ntfs3`, `overlay`, `ramfs`, `reiserfs`, `rootfs`, `squashfs`, `tmpfs`, `ubifs`,
`vfat`, `xfs`, and `zfs`; unknown and control/IPC/pseudo filesystems fail
closed. This rejects procfs mounted under an ordinary alias, FUSE brokers, and
same-device bind mounts; it does not infer mount identity from path spelling or
`st_dev`.

Euler also walks those sources without following symlinks. It rejects all
special nodes and blocks when the bounded walk is incomplete. The walk occurs
during the cached profile probe and again immediately before each subprocess
launch, after the writable-root pre-snapshot. The aggregate walk is capped at
1,000,000 entries; reaching the cap is an unavailable boundary, never
permission to run. An unavailable `openat2` or mount-table inspection also
fails closed. Construction freezes identities and topology without walking
contents, so ordinary startup remains independent of workspace and runtime
tree size while every subprocess still receives both inspections.

The backend probes the complete requested profile and every writable root,
rather than merely locating `bwrap`. An unsupported platform, missing launcher,
invalid/overlapping root set, failed probe, or later launcher failure produces
an incomplete authority inspection, unsafe mount topology, a special node, or
a concise tool failure. Raw Bubblewrap diagnostics are not model or transcript
content. Bubblewrap stdout and stderr are private launcher channels. The inner
wrapper independently NUL-frames two dedicated output pipes after setup, then
redirects only the agent command's stdout and stderr onto them. Launcher bytes
written before or after readiness therefore have no route into tool output;
Euler fails concisely if either inner frame is absent.

Workspace authority is launch configuration, independent of permission
posture. CLI launches accept repeatable `--writable-root` and `--runtime-root`
flags. `session.start` records the profile and
configured attached/read-only roots; each subprocess `tool.call` records the
host-derived profile probe result and only reports the selected mount set when
that probe succeeded. The final prelaunch inspection can still reject an
individual invocation. Resume requires the exact durable profile and
attachment sets. A legacy session may narrow to the enforced primary root but
cannot gain attachments during resume.

Snapshots are not the boundary, but they provide mutation provenance inside
it. `run_shell` captures every writable root before and after execution,
including regular files, symlinks, directories, modes, special entries, and
hardlink identity. Any hardlink alias outside the writable-root set blocks
launch. Large conventional VCS/build/cache directories fold to bounded opaque
fingerprints; the events and tools contracts define exactly what that coarse
evidence proves. Incomplete pre-observation blocks launch and incomplete
post-observation fails explicitly after warning that the command may have
changed files.
The pre-observation freezes the exact protected-surface paths plus their real
directory and mount identities. That same frozen set is passed into command
construction, revalidated immediately before launch, bound read-only without
rediscovery, and reused for post-observation. Removal or replacement of a
selected `.worktrees` directory fails closed; a `.worktrees` directory created
after pre-observation was never selected, remains writable, and is observed as
writable content. Once a child has spawned, pipe or wait supervision failure
kills the process group but does not bypass post-observation.

Structured file tools do not enter Bubblewrap. They retain their narrow
host-owned file API: canonical root selection, rejection of multiply-linked
regular files before any structured read, final stale-preimage and repeated
hardlink checks before mutation, and on Unix fd-anchored `openat` traversal
with symlink following disabled at every component. A write error after open/create is
followed by an fd-anchored observation, and any net partial mutation receives
canonical file provenance before the failed tool result. The tools contract
states the remaining
trusted concurrent-host-actor invariants; this ADR does not claim to sandbox
the Euler process itself. In particular, after the final authority inspection,
another same-user host process must not introduce or substitute a mount or
special node before or during launch. A host process that deliberately connects
to a socket created later by the sandboxed child is likewise a cooperating
authority outside this boundary.

## Scope and non-goals

This decision does not sandbox:

- provider traffic or the Euler process itself;
- native Rust extension code;
- managed extension processes (a future consumer of a generic launcher);
- arbitrary CLI-owned subprocesses such as terminal/editor/clipboard helpers;
- agent-controlled subprocesses on platforms other than Linux, which fail
  closed instead.

The explicit runtime-root mechanism accepts directories and grants read-only
visibility. It is not a credential broker. Authenticated subprocess Git/`gh`
and private package-registry workflows need a separately designed broker or
deliberately prepared runtime directory; broad home or root mounts are not an
acceptable shortcut.
Writable and runtime roots that equal or contain a private mount target are
also invalid; explicit descendants remain eligible bounded authority.

Networked subprocess workflows are also deferred. A future design must provide
isolated egress or an authenticated broker without restoring access to the
host's network namespace or unauthenticated localhost deputies.

Opaque-directory observation is deliberately coarse. A changed fingerprint is
canonical evidence of a durable net subtree change, but budget-exhausted large
file contents are not byte-complete evidence. The event contract must retain
that limitation rather than presenting an unchanged coarse fingerprint as
proof that every descendant byte was unchanged.

## Consequences

- Permission postures no longer imply or disable sandboxing. **Full capability
  access** allows capabilities; it does not change launch authority. If the
  boundary is unavailable, subprocess tools remain blocked.
- Agent subprocesses have no network access. Networked builds and publication
  await the broker/isolated-egress design above.
- Home-managed toolchains must be exposed deliberately as read-only runtime
  roots. Their binaries work, but their installation directories cannot be
  mutated by the agent.
- A checkout nested beneath a primary root's `.worktrees` collection is not an
  attachable overlapping root. Launch in that checkout; only a non-overlapping
  checkout can be attached separately.
- `/status` reports enforcement, profile, writable roots, and read-only runtime
  roots. `/permissions` identifies workspace authority as launch-controlled.
- Raw launcher diagnostics, inherited secrets, and unresolved requested paths
  never masquerade as actual authority in provenance.

## Verification gate

On supported Linux hosts, automated tests must prove every attached root is
writable; non-attached paths, host home, host network, and inherited file/socket
descriptors are unavailable. Tests must cover explicit read-only toolchains,
read-only `.worktrees`, unavailable-backend fail-closed behavior, final
prelaunch rejection of sockets/FIFOs, runtime device-node rejection, and direct
Git/shell paths. Mount fixtures must prove that a same-device nested bind mount
is rejected by mount ID. A live integration fixture must prove that a requested
procfs root, including an ordinary alias or magic-link route, cannot expose a
held host-descriptor canary. Hostile Git fixtures must cover fsmonitor,
external diff, textconv, and a remaining repository-selected helper whose
mutation is failed and recorded.

Mutation tests must cover symlink create/retarget/delete, empty directories,
mode changes, ordinary and opaque same-size content changes, hardlink aliases,
stale structured edits after approval, and incomplete before/after snapshots.
Profile creation or later launch failure must always fail closed.
