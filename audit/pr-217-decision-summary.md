# PR #217 — decision summary

Status: **awaiting approval of decisions A–E.** Nothing is merged or opened from this plan yet.
Written 2026-09-06 against `main` `9fd5f12` and #217 head `c3fcdbf`.

Full plan with rationale, verification against source, and the test matrix: [pr-217-split-plan.md](pr-217-split-plan.md).
Raw swarm review (three decorrelated reviewers): [pr-217-swarm-review.md](pr-217-swarm-review.md).

## Why a decision is needed

Euler supports Linux and macOS. #217's sandbox is Bubblewrap, which requires Linux user namespaces. As written, the PR removes `run_shell` and every `git_*` tool on macOS, and on any Linux host where unprivileged userns is restricted (Ubuntu 24.04 AppArmor defaults, hardened kernels, most containers). It also hard-codes network-off, a cleared environment, and a fatal 4,096-entry snapshot bound. Those are policy choices, and the PR makes them implicitly.

## Decisions A–E

Revised 2026-09-06 after comparing against Codex's implementation ([pr-217-codex-comparison.md](pr-217-codex-comparison.md)). Changes from the swarm-reviewed version are marked **Δ**.

**Guiding principle (owner's input, 2026-09-06):** follow Codex's path where it is both correct and easy to use. Euler may be stricter than Codex only where the user cannot feel it (provenance, internal invariants, sandbox internals). Where strictness would show up as extra prompts, broken toolchains, or extra flags, follow Codex. Rows marked **Δ²** were changed under this rule.

| | Question | Proposed decision | Approve? |
| --- | --- | --- | --- |
| A | macOS execution | **Δ** Ship a minimal Seatbelt backend (`/usr/bin/sandbox-exec -p`, deny-default profile, writable roots as `-D` params, `.git` carved out) in Unit 2. macOS is sandboxed from day one, as in Codex. Consent-gated host execution is **not** an interim; it exists only as the explicit profile in D. Per-command provenance marking of the backend stays. | ☐ |
| A′ | Linux without userns | **Δ** Bundle a digest-verified `bwrap`; probe at startup and warn with an actionable diagnostic; **fail closed** for sandbox-requiring commands (as Codex does). No automatic fallback to host execution. | ☐ |
| B | Network inside the sandbox | Off by default; per-command grant; "remember for this session" (expires on resume); **Δ** a persistent **per-host** tier written to a rules file; loopback **scoped to ports the agent itself bound**, not all of `127.0.0.0/8`; escalation vetoed when the policy has deny-read paths; sandbox permission is part of the approval cache key; "retry with network" affordance rather than failure attribution | ☐ |
| C | Environment | **Δ²** Follow Codex: **inherit the parent environment by default** so toolchains, `PATH`, proxies, and CLI auth work unchanged; opt-in `core` mode. Correctness via a **tiered** hard denylist user config cannot override: *always* deny loader/injection variables with no legitimate user use (`LD_PRELOAD`, `LD_AUDIT`, `LD_LIBRARY_PATH`, `DYLD_INSERT_LIBRARIES`, `BASH_ENV`, `ENV`, `GIT_CONFIG_COUNT/KEY_n/VALUE_n`, `GIT_CONFIG_GLOBAL`); deny dual-use variables (`NODE_OPTIONS`, `RUSTFLAGS`, `PYTHONSTARTUP`, `GIT_DIR`) **only when running unsandboxed**, since users set them legitimately and the sandbox already contains them. Set `PAGER=cat GIT_PAGER=cat TERM=dumb NO_COLOR=1`; `GIT_SSH_COMMAND` Euler-settable, not user-settable; denylist re-applied at the spawn boundary. | ☐ |
| D | Host mode | **Δ²** Follow Codex's **preset UX**, keep Euler's separate knobs underneath: one "Full access (unsandboxed)" preset in the same picker as the other modes, behind a typed confirmation; provenance records approvals-off and sandbox-off as two facts. Headless requires the explicit preset flag. Add Codex's invariants: no enforcement backend ⇒ never auto-allow (prompt, or forbid under a never-prompt policy); a policy conflict is a startup error, not a silent downgrade; an always-on dangerous-command denylist (`rm -f` through `sudo`/`env`/`trap` wrappers) that fires even under full access. | ☐ |
| E | Snapshot bound | Run and report incomplete observation; the incompleteness leads the agent-visible text; `.git/objects`, `node_modules`, `target/` excluded by default; bound configurable. **Confirmed** by comparison: Codex has no fatal observation bound anywhere and does not observe shell-caused changes at all. | ☐ |
| P | Approval shape | **Δ²** Follow Codex: once a sandbox backend is enforced, the **sandbox is the primary boundary** and non-dangerous commands run inside it **without prompting**; prompts occur only when the model requests escalation, the command matches the dangerous-command denylist, or no backend is available. This removes the prompt fatigue the swarm flagged (`git status` prompting every session). The prove-safe grammar then governs only read-only mode and the unsandboxed preset: replace the name-keyed allowlist with Codex's two-parser design (conservative tree-sitter-bash prove-safe grammar: plain words only, operators `&& \|\| ; \|` only, no glob characters, no redirections or substitutions, wrapper form only `[sh\|bash\|zsh, -c\|-lc, script]`; plus a separate permissive find-danger walk). Make `.git` (directory **and** worktree pointer file, including creation and ancestor renames) a protected subpath on every backend; keep the static denylist as defense-in-depth. | ☐ |

## Sequencing

```
Prerequisite: static allowlist fixes ──gate──▶ Unit 1 ──▶ Unit 2 ──▶ Unit 3 (dated)
                                                       └──▶ Unit 4: multi-root / resume identity
```

| Unit | Content | Order |
| --- | --- | --- |
| Prerequisite | Remove `uniq` from auto-approval; enumerated denylist of interpreter-honored write targets (`.git/`, `.gitmodules`, `.gitattributes`, `.gitconfig`, `.cargo/config.toml`, `.npmrc`, `package.json`, shell rc files, `Makefile`), one regression test each; globs, `cd`-compound lists, and `-L`/`--follow`/`-R` traversal require a normal permission decision. Budget as a parser change. | First. Hard merge-queue ancestor gate for Unit 2. |
| Unit 1 | Structured write hardening extracted from #217: `O_EXCL` creates, fd-anchored confined open, exact preimage check at apply, hardlink rejection for **writes only** (reads prompt only on unsandboxed backends), checkpoint `prepared | applied` status with rollback verifying post-write state, optional `root` field on the observation event for forward compatibility. Single primary root; no plural-root API. | After prerequisite. |
| Unit 2 | Bubblewrap enforced by default on Linux with auto-detected read-only toolchain mounts, startup availability probe and `--check-sandbox`, per-command network grant plus loopback tier, env allowlist plus hard denylist, run-and-report snapshot, extended git neutralization (hooks, pager, credential helper, clean/smudge filters). macOS: host execution per decision A. ADR 0014 rewritten here to record A–E as actually decided. | After Unit 1; gated on prerequisite. |
| Unit 3 | macOS backend. Scoped as an investigation first (Seatbelt viability on macOS 13/14/15; commit to the `SandboxProfile` abstraction, not to Seatbelt). Publish the macOS conformance skip list. Set the date by which macOS flips to fail-closed if no backend has landed. | Follow-up, dated. |
| Unit 4 | Multi-root provenance, attachment roots, resume root-identity validation, `.worktrees` restrictions (was orphaned by the first draft). Consumes Unit 1's optional `root` field. | After Unit 2. |

## What changed from the first draft after swarm review

- Toolchain mounts added to Unit 2: `$HOME` is a private tmpfs inside the sandbox, so `cargo`/`rustup`/nvm/pyenv are unreachable by default. Biggest first-week breakage; all three reviewers found it.
- `NODE_OPTIONS` moved from passthrough to hard denylist; `GIT_CONFIG_*` denied so the F34 fix holds on macOS.
- "Before or with Unit 2" hardened into a gate.
- F36 fix gained checkpoint status and rollback verification to avoid reintroducing F04.
- Unit 4 created for the orphaned multi-root/resume work.
- Unimplementable "prompt on network denial" replaced with a retry-with-grant affordance.
- Refuted by reading `c3fcdbf`: fd leak (all fds `O_CLOEXEC`), intermediate-symlink traversal (`openat2` with `RESOLVE_NO_SYMLINKS`), prepare-time nlink TOCTOU (check is fd-based at apply), bind-mount test needing bwrap (it soft-skips), Full Access disabling the sandbox (it does not).

## Next action once approved

Open the prerequisite PR (static allowlist fixes). It gates everything else and is independent of every other decision here.
