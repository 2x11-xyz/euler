# PR #217 landing plan — swarm-reviewed and adjudicated

Date: 2026-09-06. Baseline: `main` at `9fd5f12`; #217 head `c3fcdbf`.
Raw swarm output: [pr-217-swarm-review.md](pr-217-swarm-review.md) (three decorrelated reviewers: Kimi K3, GLM 5.3 Flash, Qwen 3.8; run via swarm-factory `plan` mode). Every swarm claim below was checked against #217's source where checkable; verdicts are marked **confirmed**, **refuted**, or **design call**.

## Decision summary

Euler supports Linux and macOS. #217's sandbox is Bubblewrap, which needs Linux user namespaces, so the PR as written removes `run_shell` and every `git_*` tool on macOS and on any Linux host where unprivileged userns is restricted. The decision is therefore not "which platforms" but five policy questions the PR currently answers implicitly:

| | Question | Decision |
| --- | --- | --- |
| A | macOS execution | Host execution behind explicit consent, with per-command provenance marking and a persistent indicator; fail-closed revisited when a macOS backend exists (Unit 3, dated) |
| B | Network inside the sandbox | Off by default; **per-command** grant with optional "remember for this session"; a **loopback-only** tier for dev servers; grants expire on resume |
| C | Environment | `--clearenv` plus an explicit passthrough allowlist **and** a hard denylist user config cannot override; `PATH` constructed, not inherited |
| D | Host mode | Never a side effect of Full Access. Only via an explicit `--no-sandbox`-style flag, and only where no backend is available |
| E | Snapshot bound | Run and report incomplete observation, with the incompleteness stated in the agent-visible text, not only a metadata field |

## Sequencing (hard gates, not suggestions)

```
Prerequisite: static allowlist fixes  ──gate──▶  Unit 1  ──▶  Unit 2  ──▶  Unit 3 (dated)
                                                         └──▶  Unit 4: multi-root / resume identity
```

All three reviewers independently flagged "before or with Unit 2" as too weak. The allowlist fixes are a merge-queue ancestor requirement for Unit 2, because on macOS the allowlist is the entire confinement story until Unit 3.

## Prerequisite — static allowlist fixes (audit F01, F02, F34)

Scope was understated in the first draft. Reviewer verdict **confirmed**: F02 is a parser change (glob expansion before confinement, `cd` tracking across compound lists, traversal-flag detection), not a list edit. Budget it as such.

- Remove `uniq` from unconditional approval (F01).
- Deny interpreter-honored paths as write targets and operands for static-safe commands, as an **enumerated denylist with one regression test per entry**: `.git/` (whole directory), `.gitmodules`, `.gitattributes`, `.gitconfig`, `.cargo/config.toml`, `.npmrc`, `package.json`, `.bashrc`/`.zshrc`/`.profile`, `Makefile`. (F34; the swarm's enumeration replaces the draft's "and similar".)
- Unquoted globs, `cd`-compound lists, and `-L`/`--follow`/`-R` traversal require a normal permission decision (F02).
- Convert the audit's probe suite into regression tests.
- **Design call (Kimi, GLM):** once a sandbox backend is enforced, a *read-only git subcommand* allowlist under the same neutralizing flags is defensible; make the static-safe set a function of the active backend to avoid prompt fatigue pushing users toward Full Access. Follow-up, not a gate.

## Unit 1 — structured stale-write and path hardening (land first)

Extract from #217: `O_EXCL` creates, descriptor-based confined open, regular-file and link checks, exact preimage comparison at apply, failed-write observation, existing negative tests.

Verified against `c3fcdbf`:
- All descriptors are opened `O_CLOEXEC | O_NOFOLLOW`; directories are walked hop-by-hop with `O_DIRECTORY` on macOS and via `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV | RESOLVE_NO_MAGICLINKS)` on Linux. **Refutes** the fd-leak (X3) and intermediate-symlink (1d) concerns; keep the tests anyway.
- The hardlink check (`hardlink_count`) runs in `validate_open_structured_file` on the opened descriptor, so it is fd-based at apply time. **Refutes** the prepare-time TOCTOU concern; require the between-prepare-and-apply hardlink test to pin it.
- `attached_writable_roots: Vec<PathBuf>` is threaded through `ToolRegistry`. **Confirmed** coupling risk (Q1): extraction must keep a single primary root and must not carry the plural-root API. Add a test asserting Unit 1's public surface has no `attached_writable_roots`/`writable_roots`/`resume_root_identity` symbols.
- The "nested bind mount" test soft-skips with an `eprintln!` when the fixture cannot be created. It does **not** need bwrap (**refutes** B4) but it is already silent coverage loss; make it hard-fail on Linux CI where the fixture is available.

Changes to the draft, all **confirmed** by two or three reviewers:
1. **Hardlink policy:** reject `nlink > 1` for writes only; allow reads **when a sandbox backend is active**; prompt for hardlinked reads on unsandboxed backends (an agent can `ln ~/.aws/credentials ./x` on macOS host mode). Error text must say "file has multiple links; copy, then edit".
2. **F36 checkpoint ordering:** storing the checkpoint before the write opens the inverse window (checkpoint exists, write never happened). Record checkpoint status `prepared | applied`; rollback of a `prepared`-only entry is a no-op; rollback verifies current bytes match the recorded post-write state before restoring and prompts otherwise; if the checkpoint write fails, the destructive write does not proceed. Tests: crash between checkpoint and write; rollback with an intervening user edit; checkpoint-write failure aborts.
3. **Provenance forward-compatibility:** the failed-write observation event gets an optional `root` field defaulting to the primary root now, so Unit 4 does not need a second event variant. Readers ignore unknown fields; add a round-trip test.

## Unit 2 — sandboxed subprocess execution

### Linux default: Bubblewrap enforced

Verified: `$HOME` is remapped to a private `/tmp/home` tmpfs and the environment is `--clearenv`'d with `PATH`, `HOME`, and cache variables set explicitly. #217 already supports an explicit read-only runtime/toolchain root set (there is a test that runs `cargo` from one). Consequence, **confirmed** by all three reviewers as the biggest first-week breakage: with default configuration, `cargo`, `rustup`, nvm/pyenv/asdf toolchains under the real home are unreachable and `run_shell("cargo build")` fails with command-not-found.

- **Toolchain mounts:** detect and read-only-bind the toolchain homes implied by the host environment (`CARGO_HOME`/`~/.cargo`, `RUSTUP_HOME`/`~/.rustup`, `~/.nvm`, `~/.pyenv`, `~/.asdf`, `GOPATH`) as default runtime roots, with caches redirected to the sandbox cache tmpfs. Conformance test: `cargo --version` and `node --version` succeed inside the sandbox. Real `$HOME` is never mounted; a sandboxed write under the real home must fail and can never succeed unobserved.
- **Availability probe:** the bwrap binary existing is not sufficient (Ubuntu 23.10+/24.04 AppArmor userns restrictions, hardened kernels, most containers, WSL1). Probe at session start with a trivial sandboxed command; record the outcome in `session.start`; on failure emit an actionable diagnostic naming the likely cause. Add `--check-sandbox`. CI job on a userns-restricted image asserting the actionable error. The ADR must not claim "fail-closed on Linux" as an absolute.
- **Network (B):** per-command prompt is the default; "remember for this session" is an explicit opt-in; the grant is a permission decision in provenance and **expires on resume**. Add a **loopback-only** profile tier (bwrap can express it) so agent-started dev servers are reachable from the user's browser without egress. **Refuted (Kimi):** the draft's "prompt when a command is denied for network reasons" is unimplementable; bwrap yields `Connection refused`/DNS failures, not a structured denial. Replace with a "retry with network" affordance in the failed tool result. Per-destination filtering is a separate project (proxy sidecar), not Unit 2.
- **Environment (C):** keep `--clearenv`. Passthrough allowlist: `PATH` (constructed for the sandbox view), `HOME` (sandbox home), `LANG`/`LC_*`, `TERM`, `USER`, plus toolchain locations rewritten to the mounted paths. **Hard denylist that overrides user configuration**, applied on every backend: `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_*`/`GIT_CONFIG_VALUE_*`, `GIT_CONFIG_GLOBAL`, `GIT_DIR`, `GIT_SSH_COMMAND`, `GIT_ASKPASS`, `GIT_PAGER`, `NODE_OPTIONS`, `RUSTFLAGS`, `PYTHONSTARTUP`, `BASH_ENV`, `ENV`, `CDPATH`, `PROMPT_COMMAND`, `LD_PRELOAD`, `LD_LIBRARY_PATH`, `LD_AUDIT`, `DYLD_INSERT_LIBRARIES`, `PAGER`, `EDITOR`/`VISUAL`, `IFS`. The draft listed `NODE_OPTIONS` as a passthrough example; all three reviewers **confirmed** that is a code-execution channel, and on macOS host mode `GIT_CONFIG_COUNT` alone voids the F34 fix. Injection tests for `GIT_CONFIG_COUNT`-based hooksPath and `NODE_OPTIONS=--require`.
- **Snapshot bound (E):** run the command; the tool result **text** leads with "file observation incomplete: bound N reached; changes may be unreported", the result carries a distinct non-success status, and provenance records `{reason, bound}`. A baseline-snapshot overflow is "not observed", never "no changes". Exclude `.git/objects`, `node_modules`, `target/` from the walk by default (configurable) and make the bound configurable so "incomplete" stays rare enough to mean something. Rollback semantics under incomplete observation must be specified.
- **Git neutralization (#217 keeps):** `--no-optional-locks`, `core.fsmonitor=false`, `--no-ext-diff`, `--no-textconv`. **Confirmed gap (Qwen 2f):** `git diff` against the worktree can run `filter.*.clean`; add `-c core.hooksPath=/dev/null`, `-c core.pager=cat`, `-c credential.helper=`, neutralize `filter.*.clean/smudge`, and `GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null`. Test: plant `* filter=evil` with `filter.evil.clean=touch marker`; run `git_diff`; assert no marker.

### macOS: honest host execution (A), with conditions

Verified: `c3fcdbf` forces `SubprocessSandbox::Enforce` unconditionally at CLI startup and returns `UnsupportedPlatform` off-Linux; Full Access does not and must not disable the sandbox (**refutes** X2 as a current defect; **adopt** its recommendation: unsandboxed execution only via an explicit, named flag).

Conditions, **confirmed** across reviewers:
- The allowlist prerequisite is a hard ancestor gate.
- "Unsandboxed" is stated **at each permission prompt** and in the Full Access grant dialog, not only in a status bar; headless mode refuses host execution without the explicit flag.
- Every host-executed command carries a per-command `sandbox: host` field in provenance, in addition to the backend recorded in `session.start`.
- Agent-run `git` via `run_shell` on macOS inherits repo config (`core.fsmonitor`, `core.hooksPath`); inject the same neutralizing `-c` overrides for agent-run git in host mode, or record it as a residual risk in the ADR.
- Parity test: any command that would be confined on Linux must at minimum prompt on macOS.

### ADR

Move the ADR 0014 amendment out of Unit 1 into Unit 2 and have it record decisions A–E as actually made (honest degradation, not the fail-closed posture #217 declares). **Confirmed** by two reviewers: landing the current amendment documents a decision nobody made.

## Unit 3 — macOS backend (dated)

Scope as an investigation milestone first: validate that Seatbelt (`sandbox-exec`, deprecated and undocumented since 10.15) can express workspace-only filesystem, no network, and no external spawn on macOS 13/14/15. Commit to the `SandboxProfile` abstraction, not to Seatbelt. Publish the list of conformance tests that cannot pass on macOS and their compensating controls. Set a date by which macOS flips to fail-closed if the backend has not landed; otherwise Unit 3 is a permanent follow-up.

## Unit 4 — multi-root provenance and resume root identity (was orphaned)

**Confirmed** by all three reviewers: the draft scheduled #217's item 5 (multiple writable roots, attachment roots, resume root-identity validation, `.worktrees` restrictions) nowhere. Resume root-identity validation is a real fix (stale provenance replay against a replaced workspace). Schedule it as its own unit after Unit 2, consuming the optional `root` field Unit 1 introduces. State the interim risk in the ADR.

## Tests required (consolidated)

- **Unit 1:** hardlink added between prepare and apply; crash between checkpoint and write; rollback with intervening edit; checkpoint-write failure aborts; unknown-`root`-field round trip; no plural-root symbols in the public surface; bind-mount test hard-fails where the fixture is available.
- **Prerequisite:** one test per denied interpreter-honored path; probe suite converted; `uniq` with output operand not auto-approved.
- **Unit 2:** userns-restricted probe → actionable error and provenance record; env denylist injections stripped even when user-allowlisted; per-command network grant recorded and expired on resume; loopback tier reachable; toolchain availability inside the sandbox; sandboxed write under real `$HOME` fails; incomplete observation textually distinct from "no changes"; symlink-farm workspace runs and reports incomplete (no DoS); macOS per-command `sandbox: host` field; headless refuses host mode without the flag; hardlinked read prompts on host mode; `git_diff` clean-filter neutralization.
- **Unit 3:** shared conformance suite on macOS CI with an explicit skip list.
- **Split boundary:** after Unit 1, the full suite passes with sandbox tests gated; after Unit 2, any test failing on Unit 1's tip but passing on Unit 2's must be one Unit 2 added.

## Swarm claims not adopted

- Per-destination network filtering via seccomp/landlock `connect()` filtering (Qwen 2b): out of scope for Unit 2; loopback tier plus per-command grant covers the stated cases.
- Two-tier env passthrough that *sanitizes* `NODE_OPTIONS` by stripping flags (Qwen 2c): rejected in favor of denying it outright; flag-stripping is an arms race.
- Sandboxing hardlink *reads* on Linux (GLM S1 first half): unnecessary under an enforced backend; adopted only for unsandboxed backends.
