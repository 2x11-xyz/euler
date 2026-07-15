# ADR 0014: Linux workspace subprocess sandbox

## Status

Accepted.

## Context

Euler's permission gate records and mediates capability decisions, but an
agent-controlled shell command otherwise receives the full OS authority of the
Euler process. A low-friction autonomous posture is not honestly
workspace-contained without an execution boundary.

The intended boundary is narrower: ordinary agent-controlled subprocess work
may modify the selected workspace, but must not read the user's home, other
repositories, Euler credentials, or reach the network unless a separately
designed profile permits it.

Native Rust extensions execute in the Euler process. They are trusted
in-process code, not sandboxed plugins; a child-process boundary cannot
contain them.

## Decision

Euler provides a Linux-only core subprocess-sandbox backend using the host's
`bwrap` executable. Euler owns the profile and policy; a wrapper crate or
generic agent framework is not the authority.
The launcher is resolved only from fixed system locations, never from an
agent workspace or inherited `PATH`.

The first profile, **sandboxed workspace**, has these invariants:

- the selected workspace is the sole writable host bind mount;
- the host home, Euler home, arbitrary host paths, and `/etc` are not mounted;
- only a small read-only runtime allowlist (`/usr`, `/bin`, `/lib`, `/lib64`)
  is mounted;
- `/tmp`, `/proc`, and `/dev` are private sandbox mounts;
- the child has a cleared, minimal environment and is killed with its Euler
  parent;
- a trusted in-sandbox wrapper closes every inherited descriptor except
  stdin/stdout/stderr before it executes the agent-controlled program, so open
  host files or sockets cannot bypass the mount and network boundary through
  `/proc/self/fd`;
- the default profile creates a separate network namespace with no network.

The backend probes the complete requested profile, rather than merely checking
whether `bwrap` exists. If it cannot enforce the profile, the selected command
fails with a concise public reason; Euler never silently runs that command on
the host.

`SessionConfig` selects either disabled subprocess sandboxing or enforced use
of a profile. The initial integration covers agent-controlled `run_shell` and
direct Git tool subprocesses. It is deliberately not user-selectable yet: the
permission UI must only advertise sandboxed autonomy after the product-level
workflow and toolchain support are ready.

## Scope and non-goals

This first slice does not sandbox:

- provider traffic or the Euler process itself;
- native Rust extension code;
- arbitrary CLI-owned subprocesses such as terminal/editor/clipboard helpers;
- platforms other than Linux.

Out-of-process extensions are a future consumer of the same generic launcher.
An explicitly network-enabled sibling profile requires its own approval and
enforcement contract; it is never a fallback for this profile.

The strict mount allowlist intentionally does not bind a user's Rust toolchain
or home-managed package caches. Supporting common build tools safely requires
an explicit, reviewed runtime/toolchain mount policy, not a broad home or root
bind mount.

## Consequences

- The eventual `Auto in sandbox` permission posture has a real OS boundary
  behind it. Until the UI can activate it honestly, it remains unavailable.
- `Full access` remains a separately labelled, intentionally unsandboxed
  choice.
- Existing secret-environment scrubbing remains in place as defense in depth.
- Raw Bubblewrap diagnostics and mount paths do not enter model-facing output
  or provenance.
- Sandbox profile choice and enforcement outcome require compact, secret-free
  status/provenance representation before user activation.

## Verification gate

On supported Linux hosts, automated tests must prove that a sandboxed child
can write inside a temporary workspace, cannot see a planted secret outside
the allowed mounts, cannot connect to a host listener, and can run both shell
and direct Git tool paths. They must also prove that intentionally inherited
non-`CLOEXEC` host file and socket descriptors cannot be used inside the
sandbox. Profile creation failure must fail closed.
