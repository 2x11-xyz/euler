# How Codex answers the #217 product questions

Date: 2026-09-06. Source: OpenAI Codex CLI checkout at `~/code/codex`, head `1fb5158b34` (2026-09-07). Five parallel read-only reviews (Opus), one per Euler decision A–E, each citing `file:line`. Companion to [pr-217-decision-summary.md](pr-217-decision-summary.md) and [pr-217-split-plan.md](pr-217-split-plan.md).

Codex is the closest shipping analogue to Euler: a Rust CLI agent, Linux and macOS, sandboxed shell execution, an approval model, and a git-aware workspace. Where Codex made a different call than the plan, the reason is recorded here, with a verdict on whether Euler should follow.

## Summary of deltas

| Decision | Codex does | Euler plan said | Verdict |
| --- | --- | --- | --- |
| A macOS | Seatbelt (`sandbox-exec -p`) deny-default profile; macOS is its **most** sandboxed target. Never runs unsandboxed as an availability fallback. | Host execution behind consent until a backend exists | **Change.** Ship a minimal Seatbelt backend in Unit 2, not Unit 3. Consent-gated host execution only as an explicit user-chosen profile. |
| A Linux | Bubblewrap via a re-exec'd helper binary; **bundled, SHA256-verified bwrap**; startup probe warns; fails closed at runtime if userns is unavailable. | bwrap default; fall back to consent-gated host execution when userns unavailable | **Change.** Bundle bwrap; probe and warn at startup; fail closed for sandbox-requiring commands. Unsandboxed only via the explicit profile (D). |
| B network | Off by default; per-command prompt; session tier (in-memory, dies on resume); **persistent per-host tier** written to a rules file; no loopback unless a managed proxy runs; escalation vetoed when policy has deny-read paths; sandbox permission is part of the approval cache key. | Off; per-command + session; loopback tier; expire on resume; retry affordance | **Extend.** Add the persistent per-host tier. Scope loopback to ports the agent itself bound. Port the two invariants. Keep the retry affordance (Codex's own code says failure attribution is unreliable). |
| C environment | Inherit **everything** by default, secret filter off; five-variable internal denylist; relies on the sandbox for `LD_PRELOAD`/`NODE_OPTIONS`/`GIT_CONFIG_*`. Sets `PAGER=cat`, `TERM=dumb`, `NO_COLOR=1`. | clearenv + allowlist + hard denylist | **Keep Euler's, stricter is right** since Euler has an unsandboxed mode. Refine: set `PAGER=cat` rather than deny; make `GIT_SSH_COMMAND` Euler-settable but not user-settable; re-apply the denylist at the spawn boundary. |
| D host mode | Full Access **does** fuse sandbox-off with approvals-off, mitigated by a typed confirmation, admin blocks, a hard startup error on policy conflict, and a dangerous-command denylist that fires even under full access. Invariant: no backend ⇒ never auto-allow. | Separate named flag; never a side effect of Full Access | **Keep Euler's.** Port the two invariants and the always-on dangerous-command denylist (`rm -f`, wrapper unwrapping through `sudo`/`env`/`trap`). |
| E observation | **No snapshotting and no fatal bound anywhere.** Turn diff is derived only from its own patch tool; shell-caused changes are invisible. The one bounded walk truncates and discloses. Noisy-dir exclusion list matches Euler's. | Run and report incomplete; exclude noisy dirs; configurable bound | **Confirmed.** Euler's snapshot is stricter than Codex; "report incomplete" is the right failure mode. |

## The two findings that most change the plan

### 1. Codex removed its static safe-command allowlist

Codex no longer has `is_known_safe_command`. Its replacement has three layers (`codex-rs/shell-command/src/bash.rs`, `codex-rs/core/src/exec_policy.rs`, `codex-rs/execpolicy/`):

- **A conservative prove-safe parser** (`bash.rs:209-284`): tree-sitter-bash; only node kinds `program|list|pipeline|command|command_name|word|string|raw_string|number|concatenation` and only operators `&& || ; |`. Any word containing `* ? [ ] { } ~ $ \` \\ ^ #` is rejected (`:257-272`), so globs are never proven safe. Redirections and substitutions are rejected by construction, which kills `uniq in > out` at the parse layer. Wrapper form accepted only as `[sh|bash|zsh, -c|-lc, script]` (`:291-305`).
- **A separate permissive find-danger parser** (`bash.rs:136-160`) that walks every command node including control flow, with a comment (`:133-135`) that it must never be used to prove safety. The two-parser split is the key insight: one parser cannot be both conservative and complete.
- **User-authored Starlark prefix rules** (`prefix_rule(pattern, decision, justification)`) in `$CODEX_HOME/rules/default.rules`; nothing ships built-in. Interpreter prefixes (`bash -lc`, `node -e`, `env`, `git`, …) are banned from becoming persisted allow rules (`exec_policy.rs:58-140`).

Implication for the **prerequisite**: name-keyed allowlisting is the wrong shape and the `uniq` bug is a symptom. Replace the allowlist grammar with the two-parser design. This is larger than "list edits" (the swarm review already said so) but it is a known-good target rather than a bespoke parser.

### 2. `.git` is protected inside writable roots on every backend

`PROTECTED_METADATA_PATH_NAMES = [".git", ".agents", ".codex"]` (`protocol/src/permissions.rs:27-36`). `default_read_only_subpaths_for_writable_root` (`:2230-2267`) protects `.git` as a directory **or** as a worktree/submodule pointer file (resolving `gitdir:`), blocks *creating* it when absent (`protocol.rs:1157-1170`), and each backend enforces it: Seatbelt carves it out with both `literal` and `subpath` denies plus `file-write-unlink` on protected ancestors so `mv repo repo2` cannot move `.git` out of its carveout (`sandboxing/src/seatbelt.rs:571-616`, `898-920`); bwrap mounts an empty read-only directory over it and pre-creates protection for a missing `.git` (`linux-sandbox/src/bwrap.rs:405-436`, `585-612`).

Implication for **F34**: this is the structural fix. Denying `.git/` as a static-safe write target (the plan's prerequisite) is defense-in-depth; the boundary should be the sandbox's protected subpath, on every backend including macOS.

## Per-decision detail

### A — sandbox backends (`sandboxing/`, `linux-sandbox/`)

- macOS: `/usr/bin/sandbox-exec` hard-pinned (`seatbelt.rs:63`); static SBPL fragments compiled in; `(deny default)` base (`seatbelt_base_policy.sbpl:8`); writable roots as `-D` params referenced by `(subpath (param …))` (`seatbelt.rs:923-959`); process fork/exec allowed, children inherit. Cost: one wrapper process, no privileges.
- Linux: helper binary is the Codex binary re-exec'd with an overridden `argv[0]` (`manager.rs:435-465`); bwrap `--unshare-user --unshare-pid --die-with-parent --new-session`, `--unshare-net` when restricted, `--ro-bind / /` then `--bind` per writable root (`bwrap.rs:270-292`, `461`, `581`). Bundled bwrap under `codex-resources/`, SHA256-verified (`bundled_bwrap.rs:118-145`). Landlock exists but is legacy/opt-in.
- Availability: `get_platform_sandbox()` is static per OS, not a probe (`manager.rs:67-81`). Startup probe (`bwrap --unshare-user … /bin/true`, 500 ms, four known stderr strings) yields a **warning** (`bwrap.rs:58-136`); at runtime a missing userns is a hard failure (`linux_run_main.rs:239`). WSL1 is a hard error (`manager.rs:737-751`).
- Recording: `ExecCommandBeginEvent` has **no** sandbox field (`protocol.rs:3459-3479`); only OTel tags record backend and escalation. Euler's per-command provenance marking is stricter and should stay.

### B — network (`sandboxing/seatbelt.rs`, `linux-sandbox/landlock.rs`, `core/src/tools/`)

- Default `network_access = false` (`config/src/types.rs:982`). Denial: Seatbelt by absence of `(allow network-outbound)`; Linux by `--unshare-net` plus seccomp `EPERM` on socket syscalls other than `AF_UNIX` (`landlock.rs:187-217`).
- Loopback only when the managed proxy runs with `allow_local_binding` (`seatbelt.rs:308-341`); a test asserts bare workspace-write cannot reach `127.0.0.1` (`linux-sandbox/tests/suite/landlock.rs:584`). `NO_PROXY=""` is set deliberately so local targets still hit policy (`network-proxy/src/proxy.rs:758-765`): loopback-open is an SSRF/metadata-endpoint surface.
- Escalation is model-driven (`sandbox_permissions: require_escalated` in the tool schema, `shell_spec.rs:240-244`), not failure-attributed; `OnRequest` refuses to auto-retry (`orchestrator.rs:366-369`). The stderr heuristic (`denial.rs:13-58`) has no network keywords and its doc comment concedes unreliability. Network prompts exist only because the proxy reports a structural `BlockedRequest`.
- Tiers: once / this session (in-memory `HashSet`, dies on resume, `network_approval.rs:271-272`) / **persist as `network_rule(host=…)` in `default.rules`** (`execpolicy/src/amend.rs:85-124`).
- Invariants: escalation vetoed when the policy has any deny-read path (`sandboxing.rs:269-279`); `sandbox_permissions` is part of the approval cache key so approving the sandboxed command never authorizes its escalated twin (`runtimes/unified_exec.rs:90-99`).

### C — environment (`protocol/src/shell_environment.rs`, `core/src/spawn.rs`)

- `populate_env` (`shell_environment.rs:90-160`): inherit → default excludes → user exclude → user set → include_only → inject → strip non-inheritables; applied with `env_clear()` + `envs()` (`spawn.rs:83-84`).
- Default `inherit = All` (`shell_environment_policy.rs:135`). Secret filter (`*KEY*`, `*SECRET*`, `*TOKEN*`) exists but `ignore_default_excludes` defaults to `true` = off (`:136`); a test asserts `API_KEY` reaches the child (`exec_env_tests.rs:88-110`). Opt-in `Core` set: `PATH SHELL TMPDIR TEMP TMP HOME LANG LC_ALL LC_CTYPE LOGNAME USER`.
- Hard denylist: five Codex-internal auth variables only (`shell_environment.rs:14-20`), re-applied after user `set` and again at spawn (`:155-157`, `spawn.rs:64`). Nothing stops `GIT_CONFIG_COUNT`, `NODE_OPTIONS`, `LD_PRELOAD`, `DYLD_INSERT_LIBRARIES`, `BASH_ENV`. `GIT_SSH_COMMAND` is *set* by Codex to route git through its proxy (`network-proxy/src/proxy.rs:669-707`).
- Sets `CODEX_SANDBOX=seatbelt`, `CODEX_SANDBOX_NETWORK_DISABLED=1` (advisory), and a hygiene block `NO_COLOR=1 TERM=dumb LANG=C.UTF-8 PAGER=cat GIT_PAGER=cat GH_PAGER=cat CODEX_CI=1` (`process_manager.rs:90-101`).
- No user documentation for `[shell_environment_policy]` exists.

### D — approvals and Full Access (`protocol/src/protocol.rs`, `core/src/exec_policy.rs`)

- `AskForApproval`: `UnlessTrusted`, `OnRequest` (default; `on-failure` is now only a serde alias), `Granular`, `Never` (`protocol.rs:984-1007`). `SandboxPolicy`: `DangerFullAccess`, `ReadOnly`, `ExternalSandbox`, `WorkspaceWrite` (`:1070-1119`).
- Decision matrix is code, not docs (`exec_policy.rs:770-855`): dangerous command, or no enforcement backend ⇒ `Never`→Forbidden, others→Prompt (`:799-807`). `docs/sandbox.md` is a three-line stub.
- Full Access preset = `Never` + `Disabled` profile (`approval-presets/src/lib.rs:51-58`); `--dangerously-bypass-approvals-and-sandbox` / `--yolo` sets both. Red typed confirmation (`permission_popups.rs:431-455`); admin requirements can forbid it; `never` + `danger-full-access` when forbidden is a **startup error**, not a silent downgrade (`config/mod.rs:4026-4035`). Dangerous-command denylist (`rm -f` with wrapper unwrapping through `sudo`/`env`/`trap`, depth cap 8; `is_dangerous_command.rs:123-205`) still prompts under full access.
- Headless (`codex exec`) forces `Never` (`exec/src/lib.rs:565-567`): prompts become rejections returned to the model; escalation requests get "you cannot ask for escalated permissions" (`:348-358`).

### E — observation and git (`core/src/turn_diff_tracker.rs`, `git-utils/`)

- `TurnDiffTracker` tracks only committed `apply_patch` deltas "without rereading the workspace filesystem" (`turn_diff_tracker.rs:47-48`); shell commands never touch it and do not even invalidate it. No file-count or byte bound; only a 100 ms per-file diff timeout that degrades rather than blocks (`:17`). The startup workspace map caps depth 2 / 20 entries, prunes `.git node_modules target dist build __pycache__`, and discloses truncation (`realtime_context.rs:43-56`, `436-441`).
- Codex's own git: `safe.bareRepository=explicit` everywhere (`git-utils/src/lib.rs:16`); `core.hooksPath=/dev/null`; `core.fsmonitor` **probed** and preserved only for the built-in daemon (`fsmonitor.rs:41-124`); clean/process filters blanked via `GIT_CONFIG_KEY_n` (`get_git_diff.rs:157-197`); `--no-textconv --no-ext-diff --submodule=short --ignore-submodules=dirty`; `GIT_OPTIONAL_LOCKS=0`; worktree creation strips 17 `GIT_*` redirect vars and sets `attr.tree= core.attributesFile= GIT_LFS_SKIP_SMUDGE=1 GIT_TERMINAL_PROMPT=0` (`worktree/src/git.rs:100-134`). `GIT_CONFIG_NOSYSTEM`/`GIT_CONFIG_GLOBAL` appear only in tests: the threat model is repo-local config.
- Agent-run git through the shell gets no neutralization; the sandbox (with `.git` read-only) is the protection.

## Where the plan changes as a result

1. **Unit 2 gains a macOS Seatbelt backend** (was Unit 3): a static deny-default SBPL profile with writable roots as `-D` params and `.git` carve-outs, invoked via `/usr/bin/sandbox-exec -p`. Unit 3 becomes "Seatbelt hardening and conformance parity", still dated. Consent-gated host execution on macOS is no longer the interim; it exists only as the explicit profile in D.
2. **Linux fails closed** when userns is unavailable, after a startup probe with an actionable warning and a bundled, digest-verified bwrap. The "fall back to consent-gated host execution" clause is removed.
3. **Prerequisite becomes the two-parser design** (conservative prove-safe grammar plus permissive find-danger walk) instead of patching the name-keyed allowlist.
4. **`.git` (and pointer files) becomes a protected subpath on every backend**, including creation and ancestor-rename defenses. The static-safe denylist stays as defense-in-depth.
5. **B gains a persistent per-host tier**, loopback scoped to agent-bound ports, and the two invariants (deny-read veto; sandbox permission in the approval cache key).
6. **C refinements**: set `PAGER=cat`/`GIT_PAGER=cat`/`TERM=dumb`/`NO_COLOR=1` rather than denying `PAGER`; `GIT_SSH_COMMAND` Euler-settable, not user-settable; enforce the denylist at the spawn boundary as well as at policy merge.
7. **D gains** the "no backend ⇒ never auto-allow" invariant, "policy conflict ⇒ startup error", and an always-on dangerous-command denylist.
8. **Git neutralization** adopts Codex's set, with fsmonitor probed rather than blanket-disabled.

## Where Euler stays stricter, deliberately

- Per-command sandbox state recorded in provenance (Codex has none in its transcript).
- clearenv + allowlist + hard denylist (Codex inherits everything by default).
- Full Access does not disable the sandbox (Codex fuses them).
- Workspace snapshotting around shell commands (Codex does not observe shell-caused changes at all).
