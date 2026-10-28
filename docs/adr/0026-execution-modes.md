# ADR 0026: Execution modes: Full access, Workspace, Locked

## Status

Proposed (2026-10-28). Draft for the maintainer's review. Amends ADR 0021
rows B, C, D, and P and its note that Full Access does not disable the
sandbox; rows A, A′, E, and G stand. Choices the approved direction left open
are questions at the end with a proposed answer, marked "(proposed, Qn)" in
the text. No contract changes until the rollout PRs land; each PR updates its
owning contract.

## Context

Euler has become too restrictive for a research agent. Its value should be
pi's ease and flexibility plus what pi lacks: provenance, resume, redaction,
and multi-agent workflows. The sandbox and prompts should be a choice, not
the price of entry.

An audit of 580 local pi sessions that used bash (regex classification,
approximate) measured what the current default would block: 63% of sessions
need network, 63% write outside the project (home, `/tmp`, `/usr/local`), 11%
install something, and 75% hit at least one. Background or long-running jobs
appear in 23%, `gh` in 28%, warehouse and cloud CLIs in 21%. In a live repro
on a Linux VM, a sandboxed `curl` failed with "Could not resolve host" and the
model told the user the machine had no internet; only the sandbox lacked
network. Every command prompted, and one harmless probe was flagged dangerous.

### Current facts (main at 6239829)

1. **Row P is not implemented.** An enforced sandbox does not change approval.
   The interactive gate starts `fs-write`, `shell-exec`, and `agent-spawn` in
   `ask` (`PermissionGate::new`, `permissions.rs`), and nothing in
   `session/tool_dispatch.rs` consults the backend except to record it. Every
   `run_shell` command asks unless it is statically safe or grant-covered;
   `capabilities.md` says so ("Under ADR 0021 row P the sandbox auto-allows
   these later; until then they prompt").
2. **The "Full access (unsandboxed)" posture is not unsandboxed.** It only
   sets every capability to `session-allow` (`PermissionPosture::mode_for`,
   `ui/commands.rs`). The CLI never sets `SessionConfig::subprocess_sandbox`,
   so commands still run in `SubprocessSandbox::default()`, the enforced
   no-network profile on Linux and macOS (`sandbox.rs`). The label and the
   `/status` envelope ("unsandboxed") are wrong. No host-execution preset
   exists yet; the picker shows "Auto in workspace sandbox (not available)".
3. **Background processes.** `run_process` (`tools.rs`) waits until stdout
   and stderr close and kills the process group only on timeout or cancel.
   On Linux, Bubblewrap runs with `--unshare-pid`, so every process in the
   command's PID namespace dies when the command's shell exits. On macOS,
   Seatbelt has no PID namespace: a job that redirects its output survives the
   command, and signals are confined to the same sandbox. On either backend a
   job that keeps the inherited stdout open holds `run_shell` until its
   timeout (120 s default).
4. **`/tmp`.** Bubblewrap mounts a fresh `--tmpfs /tmp` per command and puts
   `HOME`, `XDG_CACHE_HOME`, and `TMPDIR` inside it (`bwrap_command`,
   `sandbox_environment`), so nothing written there survives the command.
   Seatbelt denies writes to the real `/tmp`; `HOME`, the cache, and `TMPDIR`
   point into a private scratch tree created once per session
   (`seatbelt_scratch`, created with the session's `ToolRegistry`) and
   deleted when the session ends.
5. **Network and environment.** The only profile is `WorkspaceNoNetwork`.
   Both launchers clear the environment and set only `HOME`, caches,
   `TMPDIR`, `PATH`, and toolchain variables, so row C (inherit the parent
   environment) is not implemented: proxies and CLI auth variables never
   reach a sandboxed command. Row B's per-command network grant does not
   exist. Host execution (`agent_subprocess`) inherits the environment but
   removes every secret-shaped name (`is_secret_env_name`: `*_API_KEY`,
   `*_ACCESS_KEY`, any `KEY`, `TOKEN`, `SECRET`, `CREDENTIAL(S)`, or
   `PASSWORD` segment), which includes `GITHUB_TOKEN` and
   `AWS_SECRET_ACCESS_KEY`.
6. **The model is not told its boundary.** No system prompt or tool
   description mentions the sandbox, network, or `/tmp`; the `run_shell`
   description covers only timeouts.
7. **Dangerous-command list.** `contains_dangerous_command`
   (`command_safety.rs`, documented in `capabilities.md`) flags commands that
   destroy data with no undo (`rm` with `-r`/`-f`, `find -delete`/`-exec`,
   `git clean -f`, `reset --hard`, forcing `checkout`, `restore <path>`,
   `branch -D`, `stash drop`/`clear`, `shred`, `truncate`, `wipefs`, `dd of=`,
   `mkfs*`), interpreters whose operands mention a sensitive path, and writes
   to paths an interpreter later honors (`.git`, shell startup files,
   `.npmrc`, ...). It unwraps `sudo`, `env`, `nohup`, `xargs`, `command`, and
   other wrappers, and fails closed on parse errors, dynamic command names, an
   unrecognized option on any wrapper, stdin-fed shells (`curl ... | sh`), and
   nesting past depth eight (the function's doc comment; `capabilities.md`
   names only unrecognized `env` options). A flagged command always reaches a
   human, never the guardian; a never-prompt decider denies it. The repro's
   false positive is the fail-closed rule: `command -v uv` is flagged because
   `-v` is not a known option of the `command` wrapper (checked directly
   against `contains_dangerous_command`).
8. **Headless exec** uses the same enforced no-network sandbox, a decider that
   denies every `ask`, and `--auto-approve read-only` by default (`fs-write`
   and `shell-exec` always denied; `trusted-local` allows both)
   (`subagent.rs`). Under `--project-context auto`, `trusted-local` disables
   project context admission.

## Decision

Euler has three execution modes. A mode fixes the execution boundary and the
default approval policy together; per-capability settings and grants remain
under `/permissions` Advanced.

| | Full access | Workspace | Locked |
| --- | --- | --- | --- |
| Default for | interactive TUI | `euler exec` | nobody; opt-in for untrusted repos and CI |
| Sandbox | none (`sandbox_backend: host`) | Bubblewrap or Seatbelt | Bubblewrap or Seatbelt, today's profile |
| Network | host | on | off |
| `/tmp` | host | host `/tmp`, writable, persistent (Q4) | as today: private per command (Linux), no `/tmp` writes and a per-session `TMPDIR` (macOS) |
| `HOME` | real | private, persistent per session (Q5) | private, as today |
| Toolchain caches | real | real cache directories, writable | private, as today |
| Background jobs | outlive the command | outlive the command where the backend allows (Q6) | as today |
| Prompts | dangerous-command list only, plus the always-on asks below | dangerous list, sensitive-path mentions, per-command escalation | as today: every command not statically safe or grant-covered |

### 1. Full access

Pi parity. Commands run on the host with the user's authority and never ask,
except for the always-on dangerous-command list, which keeps ADR 0021 row D's
rule: it fires in every mode and never reaches the guardian. The always-on
asks of every mode stay: the sensitive-basename ask for structured tools,
privileged secret and config edits (`secrets.md`), and extension install and
launch consent. Provenance, redaction, checkpoints,
rollback, and resume stay on. `session.start` records `execution_mode:
"full"`, `command_approvals: "off"`, and `sandbox_backend: "host"` as
separate facts (row D). The environment passes through except Euler's own
provider and auth credentials and row C's hard denylist, which apply
unchanged; other secret-shaped variables reach commands and stay registered
with the redactor (proposed, Q7). Redaction keeps those
values out of tool results and provenance; it does not stop a command from
sending them over the network. The dangerous-command list is a speed bump
against common destructive commands, not a boundary: `python3 -c` deleting
files is not on it, as in pi.

This reverses three ADR 0021 rules on purpose: row D's "no enforcement backend
means never auto-allow", row P's "the prove-safe grammar governs the
unsandboxed preset", and the note that Full Access does not disable the
sandbox.

### 2. Workspace

Sandboxed, with the friction removed:

- **Network on.** Bubblewrap shares the host network namespace; the Seatbelt
  profile gains outbound network and loopback bind. Row B's per-command
  network grants, its loopback restriction, its "retry with network" offer,
  and its veto on escalation under deny-read paths are dropped: network is a
  mode property, sandboxed code reaches every local service as in pi, and an
  escalation is a human-approved host command.
- **`/tmp`.** The host `/tmp` is writable and persistent (plus the host
  `TMPDIR` on macOS, where it lives under `/var/folders`). A per-session
  scratch exposed as `TMPDIR` would not help: Seatbelt cannot remap a path, so
  literal `/tmp/x` writes, which models and scripts use constantly, would
  still fail on macOS, and on Linux a path the model reports to the user
  (`/tmp/plot.png`) would not exist outside the sandbox. A real `/tmp` keeps
  paths identical inside and outside the boundary (proposed, Q4). Concurrent
  sessions and host processes share it, as they do without a sandbox.
- **`HOME` and caches.** `HOME` is a private directory that persists for the
  session on both backends, so a `pip install --user` or a `~/.local/bin`
  install works for the rest of the session. The host's toolchain cache
  directories (`XDG_CACHE_HOME` or the platform cache directory, Cargo
  `registry` and `git`, `GOMODCACHE`, the npm and uv caches) are writable at
  their real paths (a bind on Linux, a write allowance on macOS), so builds
  stop re-downloading. A sandboxed command can therefore poison a cache a
  later host build consumes; Locked keeps private caches for untrusted
  repositories (proposed, Q13).
- **Environment.** Row C, narrowed: inherit the parent environment, apply
  its denylist, set the pager variables, and also drop secret-shaped
  variables, because network is now on (proposed, Q5).
- **Secret reads on macOS.** Seatbelt allows broad reads today (Unit 3 owns
  narrowing). Before Workspace turns network on, the profile denies reads of
  well-known credential stores: `~/.ssh`, `~/.aws`, `~/.config/gcloud`,
  `~/.config/gh`, `~/.kube`, `~/.docker/config.json`, `~/.netrc`, the
  keychains, browser profiles, and the Euler home. This list is a minimum,
  not a boundary: with network on, Unit 3's read narrowing becomes a security
  item rather than parity. Linux never mounts the real home.
- **Approval (row P, implemented here).** Inside the sandbox, `run_shell`,
  `git_*`, and workspace-confined structured writes run without a prompt.
  Prompts occur only for the dangerous-command list, for escalation, for the
  always-on asks of section 1, and for a command whose operands mention a
  sensitive path (the danger walk's
  existing "mention" rule, widened from interpreters to every command),
  because `curl -d @.env` would otherwise leave unprompted (proposed, Q14).
- **Escalation.** In Workspace, `run_shell` gains an optional `escalate`
  object with a required `reason`. The model sets it to run one command on
  the host with the Full access environment; the human sees the command and
  the reason and allows once, or for the session for commands with that
  first token (proposed, Q8). The danger walk runs first, so no grant covers
  a flagged command. An escalated grant never covers the sandboxed form and
  the reverse (row B's cache-key rule, kept without row B's network grants).
  An unanswered or cancelled escalation follows the ordinary `ask`
  cancellation rule. The tool result records `sandbox_backend: "host"`,
  `escalated: true`, and the reason. Credentialed CLIs (`gh`, cloud CLIs) and
  installs outside the session use this path.

### 3. Locked

Today's behavior, unchanged: the no-network profile, private per-command
`/tmp` on Linux, private `HOME`, and every command not statically safe or
grant-covered asks. Headless Locked keeps the `--auto-approve` tiers and the
project context rule exactly as today. Locked has no escalation; leaving it
is a mode change.

### 4. Selection

- **First run.** The first interactive launch with no stored mode shows one
  picker: Full access (preselected), Workspace, Locked, each with a one-line
  boundary. The choice is written to `preferences.json` under the user Euler
  home and never asked again. This picker replaces row D's typed
  confirmation (proposed, Q1). **New prompt**, once per user Euler home.
- **Config.** `preferences.json` holds `execution_mode`. Repository content
  never selects or loosens a mode.
- **CLI.** `--mode full|workspace|locked` on `euler` and `euler exec`.
  Precedence: flag, then stored preference (interactive only), then the
  built-in default. `exec` never reads the stored preference: it defaults to
  Workspace, and Full access requires the explicit flag (row D).
  `--auto-approve` is accepted only with `--mode locked`; elsewhere it is a
  startup error naming the replacement (proposed, Q2). A mode not shipped
  yet during the rollout is refused with an error, never substituted.
- **TUI.** `/mode` opens the same picker and can also persist the choice. A
  change applies at the next turn boundary and is recorded as a mode-change
  event. It rebuilds the subprocess boundary, clears escalation grants, and
  ends the session's background jobs, so no job started under a looser mode
  keeps running under a stricter claim (proposed, Q12). `/permissions`
  loses its three posture rows and the unavailable sandbox row; Read only
  becomes an Advanced preset (proposed, Q3).
- **Platform.** Workspace and Locked keep row A′: no backend means fail
  closed, with a diagnostic that now names `/mode full` or `--mode full` as
  the way out. `exec` never falls back from Workspace to another mode. Full
  access needs no probe.

### 5. The model is told

Every mode adds one always-on line to the system prompt stating the mode and
its boundary, for example: "Execution mode: Workspace. Commands run in a
sandbox with network; writes are allowed in the workspace, /tmp, caches, and a
private home; set escalate with a reason to run one command outside." A mode
change updates that line through the canvas at the next turn. A sandboxed
command that fails on DNS or a denied write gets a one-line hint naming the
boundary, not a guess at the cause. This also applies to Locked, so the
repro's misdiagnosis cannot recur.

### 6. Unattended and extension-driven runs

- A never-prompt decider (headless `exec`, lifecycle hooks) denies any
  escalation, dangerous command, or other ask with a tool error that names
  the mode and the flag that would allow it.
- With the guardian configured, escalation requests are ordinary asks and go
  to the guardian; its allows stay once-scoped and its threshold rules apply,
  so a low-risk escalation runs unattended with the rationale recorded. In
  Workspace the guardian reviews escalations and sensitive-path asks; in Full
  access it reviews only the always-on asks it already handles, since
  commands no longer ask. Dangerous commands still go only to a human, so
  they are denied unattended.
- Child agents run in the parent's mode or a stricter one, never looser
  (proposed, Q9). Extension capability approval, launch consent, and install
  consent are unchanged in Workspace and Locked; Full access session-allows
  extension capabilities as today's Full access posture does. Request ticks
  and idle contributors keep "never prompt".

### 7. Trust prompts

- Project context acknowledgment stays: one prompt per content digest, then
  silent. The headless rule that `trusted-local` disables `auto` admission
  stays in Locked; in Workspace and Full access an acknowledged digest is
  admitted headless (proposed, Q10).
- Extension install consent ("first contact is never automatic") and launch
  consent stay in every mode. Extensions run outside any sandbox, so the mode
  does not change their trust.

### 8. Rollback and resume

Structured writes checkpoint before the write and `run_shell` observes the
workspace before and after, on every backend, so `/rollback` works the same
in all modes. What a mode changes is the observed scope: writes outside the
workspace (`/tmp`, caches, and in Full access the whole home) and writes made
by a background job after its command returned are not attributed or
restorable. `execution_mode` on `session.start` tells a reader that scope.
Resume resolves the mode for the new invocation (flag, then preference, then
default) and records it; if it differs from the recorded one, the startup
summary says so. Logs without `execution_mode` fold as "not recorded": no
mode is inferred, because today's Full access posture ran sandboxed with
approvals off, which matches none of the three modes. The recorded
`sandbox_backend` and permission decisions stay the evidence.

## Alternatives rejected

- **Keep the sandbox default and add per-command network grants (row B).**
  Three in four audited sessions need network, outside writes, or installs,
  and would still meet a prompt or failure.
- **Make Workspace the interactive default.** It still puts credentialed
  CLIs and installs outside the session behind escalation prompts; `gh`
  alone appears in 28% of audited sessions. Users who want it choose it once.
- **Per-session scratch as `TMPDIR` instead of the real `/tmp`.** See section
  2: it cannot cover literal `/tmp` paths on macOS.

## Rollout

Each step is one PR with tests that fail without it and updates its owning
contract (`capabilities.md`, `tools.md`, `events.md`, `ui.md`,
`project-context.md`).

1. **Boundary statement.** Tell the model the current boundary and add the
   failure hint (section 5) for today's profile. Independent; fixes the
   repro's misdiagnosis now.
2. **Danger-walk precision.** Recognize `command -v`/`-V` as a lookup that
   runs nothing, with the repro probe as the test.
3. **Mode plumbing and Locked.** `ExecutionMode` in `SessionConfig`,
   `execution_mode` on `session.start`, `--mode`, the stored preference, and
   the fold rule for older logs. Locked is today's behavior; the posture label
   stops claiming "unsandboxed".
4. **Full access.** Host execution, approvals off except the dangerous list,
   environment rule (Q7), `/mode`, the first-run picker (Full access and
   Locked until step 5), and the interactive default.
5. **Workspace profile.** Network, `/tmp`, persistent `HOME`, writable caches,
   row C environment, macOS credential deny-reads, and row P auto-allow.
6. **Escalation.** The `escalate` field, prompt, grants (Q8), guardian
   routing, and never-prompt errors.
7. **Exec default.** Until this step `euler exec` stays Locked, as today.
   Then it defaults to Workspace; `--auto-approve`
   restricted to Locked; project context rule (Q10). **Behavior change** for
   scripts relying on read-only exec (proposed, Q11).
8. **Linux background jobs** (Q6), only if usage shows the need.

## Consequences

- The interactive default matches pi: no sandbox, no routine prompts. Euler
  keeps provenance, redaction, rollback, resume, and multi-agent work, and
  records that approvals and the sandbox were off.
- Headless runs gain shell, writes, and network inside a sandbox by default,
  so CI on hosts without user namespaces (some hosted Linux runners) fails
  closed until it passes `--mode full`, the only mode that needs no backend.
- Linux hosts without user namespaces work out of the box under Full access.
- New prompts: the first-run picker (once), Workspace escalation (per
  command, or per session and first token), and the Workspace
  sensitive-path ask for shell commands. All three replace the far more
  frequent per-command prompts of today. No new blocks beyond what Locked
  already does.
- The `network` capability keeps gating host network APIs for extensions;
  subprocess network becomes a mode property.
- ADR 0021's "Full Access does not disable the sandbox" is reversed;
  ADR 0014's "Auto in workspace sandbox" row is replaced by Workspace.

## Open questions

1. Is the first-run picker the explicit choice row D required, replacing
   the typed confirmation? **Proposed: yes.** Selecting Full access in a
   picker that states "no sandbox" is explicit; a typed phrase adds friction
   without information.
2. Restrict `--auto-approve` to `--mode locked` and fail elsewhere with a
   message naming `--mode`? **Proposed: yes.** One canonical flag, clear
   error, no alias layer.
3. Fold the TUI Read only posture into `/permissions` Advanced as a preset
   rather than a fourth mode? **Proposed: yes.** It is an approval preset,
   not a boundary.
4. Real host `/tmp` in Workspace rather than a per-session scratch as
   `TMPDIR`? **Proposed: real `/tmp`** (section 2). Cost: `/tmp` writes are
   shared and outside workspace observation, as in pi.
5. In Workspace, keep `HOME` private (persistent per session) and
   secret-shaped variables out, sending credentialed CLIs through
   escalation? **Proposed: yes.** With network on, credentials are what the
   sandbox still protects; users who want `gh` without prompts pick Full
   access or grant `gh` escalation for the session.
6. Background jobs on Linux: keep the per-command PID namespace and tell the
   model jobs end with their command, deferring a session-scoped sandbox
   instance? **Proposed: yes.** Dropping `--unshare-pid` would let sandboxed
   code signal or trace host processes of the same user; macOS already lets
   jobs outlive the command. Euler kills the process groups it started when
   the session ends, on both backends.
7. Under Full access and for escalated commands, pass secret-shaped
   variables through except Euler's own provider and auth credentials?
   **Proposed: yes.** Today's host scrub breaks `gh` and cloud CLIs that read
   `GITHUB_TOKEN` or AWS variables. The redactor already seeds these values
   at session start, which keeps them out of the record, not off the
   network.
8. Let an escalation approval cover the session for commands with the same
   first token? **Proposed: yes**, through the existing scoped-grant store
   and its segment-aware matching (every segment of a compound must be
   covered), keyed separately from sandboxed grants. Never offered for
   shells, interpreters, or privilege wrappers (`sh`, `python3`, `node`,
   `sudo`, ...), and never covering a flagged command.
9. Child agents never run in a looser mode than their parent? **Proposed:
   yes**, matching capability attenuation.
10. Admit acknowledged project context in headless Workspace and Full
    access, keeping today's `trusted-local` rule only in Locked?
    **Proposed: yes.** The acknowledgment is the user's decision on exact
    content; disabling it would drop `AGENTS.md` from every default exec run.
11. Flip `euler exec` from read-only to Workspace in one release, without
    an alias for the old default? **Proposed: yes.** It is the direction's
    default, and `--mode locked` restores today's behavior in one flag.
12. On a mode change, clear escalation grants and end the session's
    background jobs? **Proposed: yes.** Otherwise a job started on the host
    keeps running after the session claims Locked.
13. Bind real toolchain caches writable in Workspace and accept cache
    poisoning as a cost? **Proposed: yes.** Re-downloading on every command
    is the friction Workspace exists to remove; untrusted repositories
    belong in Locked.
14. In Workspace, ask for a sandboxed command whose operands mention a
    sensitive path (`.env*`, keys, credentials)? **Proposed: yes.** Network
    is on, and this keeps shell parity with the structured-tool
    sensitive-basename ask. Full access does not add it (pi parity).
