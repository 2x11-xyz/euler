# Plan under review: how to land PR #217 ("Enforce explicit workspace authority boundaries") in Euler

Please review this plan critically. Euler is a Rust CLI/TUI coding agent (7-crate workspace: euler-event, euler-sdk, euler-managed-process, euler-agents, euler-provider, euler-core, euler-cli). It supports Linux and macOS only. Its core invariants: an append-only canonical event log ("provenance"), explicit per-capability permissions (FsRead/FsWrite/ShellExec), and tools that run agent-requested shell commands and file edits inside a workspace root.

## Background: what PR #217 does today (one ~10.9k-line PR, 61 files)

1. Structured file writes become fd-anchored: creates use O_EXCL; edits verify the exact prepare-time preimage bytes at apply; the final path component is opened O_NOFOLLOW; hardlinked targets (nlink > 1) are rejected for both reads and writes; failed writes are observed and recorded. Good negative tests exist (stale preimage, symlink substitution, root substitution, FIFO, hardlink, nested bind mount).
2. Agent subprocesses (run_shell, git_* tools) run ONLY inside a Bubblewrap (bwrap) sandbox: workspace-only filesystem, no network, cleared environment. If no usable sandbox exists, shell and git tools fail closed. Bubblewrap needs Linux user namespaces, so on macOS there is no backend and shell/git tools disappear entirely.
3. The "host mode" fallback (unsandboxed execution even under Full Access) is removed.
4. Workspace snapshot bounds become fatal: the shell tool takes a before/after filesystem snapshot to report file changes; the snapshot stops at 4,096 entries per root (directories and symlinks count). Previously an overflowed snapshot silently reported "no changes"; now it blocks the command from starting at all.
5. Provenance records multiple writable roots; resume validates root identity; git is removed from the static "safe command" allowlist (so `git status` now prompts instead of auto-running); git side-effect configs (hooksPath, fsmonitor, external diff, textconv) are disabled for the tool's own git invocations.
6. ADR 0014 is amended to declare the fail-closed sandbox as accepted.

## Relevant facts from a recent repository audit

- F01: the static safe-command allowlist auto-approves `uniq`, which takes an output file operand and therefore writes files without any permission prompt. Unfixed by #217.
- F02: the static path-confinement check runs before glob expansion, checks `cd a && cat x` against the original root rather than the changed directory, and does not reject `rg --follow` / `-L` traversal; all three can read outside the workspace via a planted symlink. Unfixed by #217. On Linux, bwrap would confine these; on macOS nothing would.
- F34: `.git/config` is not a "sensitive basename", so `uniq payload .git/config` followed by `git status` (both auto-approved today) executes attacker-chosen `core.hooksPath` / `core.fsmonitor`. #217 fixes only the second half (git no longer static-safe) and only for the tool's own git invocations.
- F04: prepared writes overwrote intervening user edits. #217 fixes this (item 1 above).
- F36: the rollback checkpoint is stored from the prepare-time preimage AFTER the write, so a crash between write and checkpoint leaves no preimage. Partially addressed by #217 (write is verified, but checkpoint ordering unchanged).
- O3: the audit recommended reporting incomplete snapshot observation explicitly rather than blocking or silently reporting nothing.

## Decisions the PR currently makes implicitly that need an explicit answer

A. macOS behavior: (i) fail closed, shell disappears on Mac; (ii) fall back to host execution with explicit permission prompts and a visible "unsandboxed" indicator; (iii) build a macOS backend first (Apple sandbox-exec / Seatbelt is deprecated but functional and used by other agent CLIs).
B. Network inside sandboxed shell: always off (breaks git fetch/push, cargo/npm/pip downloads, localhost dev servers inside the tool) vs. a per-session or per-command grant.
C. Environment: clearenv (loses PATH additions, toolchain variables, credentials) vs. an allowlist of passthrough variables.
D. Host mode: remove entirely, or keep behind Full Access with explicit consent.
E. Snapshot bound: block execution at 4,096 entries vs. run and report incomplete observation (O3).

## The proposed plan

Split #217 into three units and decide A–E explicitly rather than letting one PR set them.

### Unit 1 (land now): structured stale-write and path hardening
Extract item 1: O_EXCL creates, descriptor-based confined open, regular-file and link checks, exact preimage comparison, failed-write observation, and the existing negative tests. Keep the current single primary-root model. Also fix F36 ordering here: store the checkpoint before the destructive write. Do NOT import shell policy, attachment roots, `.worktrees` restrictions, or the resume/root-identity changes into this unit. Expected size: roughly a third of the PR.

Open question for reviewers: should the hardlink rejection (nlink > 1) apply to reads? It makes pnpm `node_modules`, Nix-store-backed trees, and `cargo vendor` hardlink layouts unreadable. Proposal: reject hardlinks for writes only; allow reads.

### Unit 2 (land second): sandboxed subprocess execution as the Linux default, honest degradation on macOS
- Linux: Bubblewrap enforced by default for run_shell and git_*.
- Network: default off inside the sandbox, with a per-session grant ("allow network for this session") recorded as a permission decision in provenance, and a per-command prompt when a command is denied for network reasons. Do not hard-code always-off.
- Environment: pass through an allowlist (PATH, HOME, LANG/LC_*, TERM, toolchain variables such as CARGO_HOME/RUSTUP_HOME/GOPATH/NODE_OPTIONS as configured), not clearenv. Secrets are never passed through unless explicitly granted.
- macOS: fall back to host execution gated by the existing permission decider, with a persistent "unsandboxed" status indicator in the TUI and a line in provenance's session.start recording the sandbox backend in use. This is the current behavior plus honesty; it is not a regression.
- Host mode: keep available under Full Access only, and only where no sandbox backend exists; never silently.
- Snapshot bound: do not block. Run the command and record `observation: incomplete { reason, bound }` on the shell result so the UI and provenance can distinguish "no changes" from "not observed" (audit O3).
- Keep: git removed from static-safe; git side-effect configs disabled.

### Unit 3 (follow-up): macOS sandbox backend
Implement a Seatbelt (sandbox-exec) profile backend behind the same SandboxProfile abstraction, with the same conformance tests as bwrap where the platform allows. Once it exists, revisit whether macOS should fail closed by default.

### Prerequisite that becomes more urgent under this plan
If macOS stays unsandboxed (Unit 2), the static allowlist is macOS's only protection. So land the audit's allowlist fixes before or with Unit 2: remove `uniq` from unconditional approval; treat `.git/` (the directory, and other interpreter-honored paths) as a denied write target and operand for static-safe commands; require a normal permission decision for unquoted globs, `cd`-compound lists, and `--follow`/`-L`/`-R` traversal. These are small list/grammar edits with an existing probe suite to convert into regression tests.

## Questions for reviewers

1. Is splitting this way sound, or does Unit 1 secretly depend on the multi-root/resume changes in a way that makes extraction unsafe?
2. Is "host execution with an honest indicator" on macOS an acceptable interim, or is fail-closed the only defensible security posture given F01/F02/F34 are still open?
3. Is a per-session network grant the right granularity, or should it be per-command (or per-destination)?
4. Are there failure modes in the environment allowlist approach (e.g., credentials leaking through innocuously named variables, `LD_PRELOAD`-class variables) that argue for clearenv plus an explicit passthrough list rather than a default list?
5. Does "run and report incomplete observation" for the snapshot bound create a worse safety problem than blocking (e.g., the agent believing a destructive command changed nothing)?
6. Anything the plan misses that would bite a Linux user or a macOS user in the first week?

Be specific: point at the exact step, say what breaks, and propose the smallest change that fixes it.
