# ADR 0021: Subprocess Authority, Sandbox Backends, and Approval Shape

## Status

Accepted (2026-09-06). Supersedes the fail-closed posture declared by the
ADR 0014 amendment proposed in PR #217; ADR 0014 remains the record of the
original Linux sandbox. Implementation is staged in the units listed under
Consequences; none had landed when this was accepted.

## Context

PR #217 made agent subprocess execution Bubblewrap-only. Bubblewrap needs
Linux user namespaces, so the PR as written removed `run_shell` and every
`git_*` tool on macOS and on any Linux host where unprivileged namespaces are
restricted. It also hard-coded network-off, a cleared environment, removal of
host execution even under Full Access, and a fatal 4,096-entry workspace
snapshot bound. Each of those is a product decision the PR made implicitly.

The repository audit of 2026-09-05 (`docs/reviews/2026-09-05-repository-audit.md`)
found that the static safe-command allowlist auto-approves `uniq`, which
writes files; that unquoted globs, `cd` compounds, and `--follow` traversal
escape path confinement through symlinks; and that a write to `.git/config`
followed by `git status` reaches arbitrary execution with zero prompts
(findings F01, F02, F34). It also found that prepared file writes overwrite
intervening user edits (F04) and that the rollback checkpoint is stored after
the destructive write (F36).

A three-reviewer swarm review of the split plan and a source-level comparison
against OpenAI Codex (`docs/reviews/2026-09-05-pr-217-split-plan.md`,
`docs/reviews/2026-09-05-pr-217-codex-comparison.md`) informed the decisions
below.

## Guiding principle

Follow Codex's path where it is both correct and easy to use. Euler may be
stricter than Codex only where the user cannot feel it: provenance, internal
invariants, sandbox internals. Where strictness would show up as extra
prompts, broken toolchains, or extra flags, follow Codex.

## Decision

| | Question | Decision |
| --- | --- | --- |
| A | macOS execution | Ship a Seatbelt backend (`/usr/bin/sandbox-exec -p`, static deny-default profile, writable roots as `-D` parameters, `.git` carved out). macOS is sandboxed from day one. Consent-gated host execution is not an interim; it exists only as the explicit profile in D. Each exec event records the backend that ran it. |
| A′ | Linux without user namespaces | Bundle a digest-verified `bwrap`. Probe at session start with a trivial sandboxed command and record the outcome in `session.start`; on failure emit a diagnostic naming the likely cause and the way out. Sandbox-requiring commands fail closed. No automatic fallback to host execution. |
| B | Network inside the sandbox | Off by default. Per-command grant; "remember for this session", expiring on resume; a persistent per-host tier written to a rules file. Loopback is allowed only for ports the agent's own processes bound, not all of `127.0.0.0/8`. Escalation is vetoed when the policy has any deny-read path. The sandbox permission is part of the approval cache key, so approving a sandboxed command never authorizes its escalated twin. A failed sandboxed command offers "retry with network" rather than attributing the failure to network denial from stderr. |
| C | Environment | Inherit the parent environment by default so toolchains, `PATH`, proxies, and CLI auth work unchanged; opt-in `core` mode. A tiered hard denylist that user configuration cannot override: always deny loader and injection variables with no legitimate user use (`LD_PRELOAD`, `LD_AUDIT`, `LD_LIBRARY_PATH`, `DYLD_INSERT_LIBRARIES`, `BASH_ENV`, `ENV`, `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n`, `GIT_CONFIG_GLOBAL`); deny dual-use variables (`NODE_OPTIONS`, `RUSTFLAGS`, `PYTHONSTARTUP`, `GIT_DIR`) only when running unsandboxed. Set `PAGER=cat GIT_PAGER=cat TERM=dumb NO_COLOR=1`. `GIT_SSH_COMMAND` is Euler-settable, not user-settable. The denylist is re-applied at the spawn boundary. |
| D | Host mode | Never a side effect of Full Access. One "Full access (unsandboxed)" preset in the same picker as the other modes, behind a typed confirmation; provenance records approvals-off and sandbox-off as two facts. Headless requires the explicit preset flag. Invariants: no enforcement backend ⇒ never auto-allow (prompt, or forbid under a never-prompt policy); a policy conflict is a startup error, not a silent downgrade; an always-on dangerous-command denylist (`rm -f` through `sudo`/`env`/`trap` wrappers) fires even under full access. |
| E | Snapshot bound | Run the command and report incomplete observation. The incompleteness leads the agent-visible tool text and the result carries a distinct non-success status; provenance records `{reason, bound}`. A baseline-snapshot overflow is "not observed", never "no changes". `.git/objects`, `node_modules`, `target/` are excluded from the walk by default; the bound is configurable. |
| P | Approval shape | Once a sandbox backend is enforced, the sandbox is the primary boundary and non-dangerous commands run inside it without prompting; prompts occur only when the model requests escalation, the command matches the dangerous-command denylist, or no backend is available. The prove-safe grammar governs read-only mode and the unsandboxed preset, and replaces the name-keyed allowlist with a two-parser design: a conservative prove-safe grammar (plain words only; operators `&&`, `\|\|`, `;`, `\|` only; no glob characters, redirections, or substitutions; wrapper form only `[sh\|bash\|zsh, -c\|-lc, script]`) plus a separate permissive find-danger walk that must never be used to prove safety. `.git` (directory and worktree pointer file, including creation and ancestor renames) is a protected subpath on every backend; the static sensitive-path list stays as defense in depth. |
| G | Git neutralization | Euler's own git invocations set `core.hooksPath=/dev/null`, `safe.bareRepository=explicit`, blank clean/process filters via `GIT_CONFIG_KEY_n`, `attr.tree=`, `core.attributesFile=`, `GIT_LFS_SKIP_SMUDGE=1`, `GIT_TERMINAL_PROMPT=0`, `GIT_OPTIONAL_LOCKS=0`, and strip the `GIT_DIR`/`GIT_WORK_TREE`/`GIT_CONFIG*`/`GIT_INDEX_FILE` family. `core.fsmonitor` is probed and preserved only for the built-in daemon rather than blanket-disabled. |

## Consequences

Implementation is split into units, each its own PR, in this order:

1. **Prerequisite**: the two-parser approval grammar and `.git` protection (P). Fixes F01, F02, F34. A merge-queue ancestor gate for Unit 2.
2. **Unit 1**: structured-write hardening extracted from #217 (fd-anchored confined open, `O_EXCL` creates, exact preimage comparison at apply, hardlink rejection for writes) plus checkpoint status `prepared | applied` with rollback verifying post-write state (F04, F36). Single primary root.
3. **Unit 2**: sandbox backends and policy (A, A′, B, C, D, E, G), with this ADR's decisions recorded as the shipped behavior.
4. **Unit 3**: Seatbelt hardening and conformance parity with the Linux backend, dated.
5. **Unit 4**: multi-root provenance, attachment roots, resume root-identity validation, `.worktrees` restrictions.

PR #217 remains open as the extraction source and is not merged as a unit.

Where Euler stays stricter than Codex, deliberately: per-command sandbox state
in provenance; Full Access does not disable the sandbox; workspace
snapshotting around shell commands; the tiered environment denylist.

Accepted costs: a Seatbelt backend on a deprecated Apple API (Codex has run on
it for years; Unit 3 owns parity and a fail-closed review date); Linux hosts
without user namespaces lose shell tools unless the user selects the
unsandboxed preset; the prerequisite is a parser replacement, not a list
edit.

## References

- `docs/reviews/2026-09-05-repository-audit.md` — findings F01–F38.
- `docs/reviews/2026-09-05-pr-217-split-plan.md` — swarm-reviewed plan and test matrix.
- `docs/reviews/2026-09-05-pr-217-codex-comparison.md` — source-level comparison with citations.
- ADR 0014 — the original Linux workspace subprocess sandbox.
- PR #217, PR #224.
