# ADR 0026: Execution profiles: full, workspace, locked

## Status

Proposed (2026-10-28). Draft for the maintainer's review. The execution
boundary and the default approval policy become a named profile defined in
user configuration: three built-in presets (`full`, `workspace`, `locked`),
user profiles that extend a preset, and a default profile per context
(interactive, headless, subagent). Amends ADR 0021 rows B, C, D, and P and its
note that Full Access does not disable the sandbox; rows A, A′, E, and G
stand, with A′ restated for profiles in section 5. Choices the approved
direction left open are questions at the end with a proposed answer, marked
"(proposed, Qn)" in the text. No contract changes until the rollout PRs land;
each PR updates its owning contract.

## Context

Euler has become too restrictive for a research agent. Its value should be
the ease and flexibility of unrestricted local execution, like an unsandboxed
local agent, plus what such agents lack: provenance, resume, redaction, and
multi-agent workflows. The sandbox and prompts should be a choice, not
the price of entry, and a user should be able to make that choice per context,
for example unrestricted at the keyboard and locked down for headless agents.

An audit of 580 local coding-agent session logs that used bash (regex
classification, approximate) measured what the current default would block:
63% of sessions need network, 63% write outside the project (home, `/tmp`,
`/usr/local`), 11% install something, and 75% hit at least one. Background or
long-running jobs appear in 23%, `gh` in 28%, warehouse and cloud CLIs in 21%.
In a live repro on a Linux VM, a sandboxed `curl` failed with "Could not
resolve host" and the model told the user the machine had no internet; only
the sandbox lacked network. Every command prompted, and one harmless probe was
flagged dangerous.

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
   and stderr close and kills the process group only on timeout, cancel, or a
   drain or wait error. It keeps the leader unreaped while a pipe is open, so a
   cancellation cannot signal a reused process group, and reaps it once both
   pipes close. On Linux, Bubblewrap runs with `--unshare-pid`, so every
   process in the command's PID namespace dies when the command's shell
   exits. On macOS, Seatbelt has no PID namespace: a job that redirects its
   output survives the command, and signals are confined to the same sandbox.
   On either backend a job that keeps the inherited stdout open holds
   `run_shell` until its timeout (120 s default). Nothing ends surviving jobs
   when the session ends.
4. **`/tmp`.** Bubblewrap mounts a fresh `--tmpfs /tmp` per command and puts
   `HOME` (`/tmp/home`), `XDG_CACHE_HOME` (`/tmp/cache`), and `TMPDIR` inside
   it (`bwrap_command`, `sandbox_environment`), so nothing written there
   survives the command. Seatbelt denies writes to the real `/tmp`; `HOME`,
   the cache, and `TMPDIR` point into a private scratch tree created once per
   session in the host temp directory (`seatbelt_scratch`, created with the
   session's `ToolRegistry`) and deleted when the session ends.
5. **Network and environment.** The only profile is `WorkspaceNoNetwork`.
   Both launchers clear the environment and set only `HOME`, caches,
   `TMPDIR`, `PATH`, and toolchain variables, so row C (inherit the parent
   environment) is not implemented: proxies and CLI auth variables never
   reach a sandboxed command. Row B's per-command network grant does not
   exist. Host execution (`agent_subprocess`) inherits the environment but
   removes every secret-shaped name (`is_secret_env_name`: `*_API_KEY`,
   `*_ACCESS_KEY`, any `KEY`, `TOKEN`, `SECRET`, `CREDENTIAL(S)`, or
   `PASSWORD` segment), which includes `GITHUB_TOKEN` and
   `AWS_SECRET_ACCESS_KEY`, and Euler's parent-control names
   (`is_parent_control_env_name`: `EULER_HOME`, `EULER_AUTH_FILE`,
   `EULER_PROVIDER`, ...).
6. **Row C's hard denylist is not implemented.** No code or contract names
   `LD_PRELOAD`, `BASH_ENV`, or `DYLD_INSERT_LIBRARIES`. Host execution
   removes only the two name classes in fact 5.
7. **The model is not told its boundary.** No system prompt or tool
   description mentions the sandbox, network, or `/tmp`; the `run_shell`
   description covers only timeouts.
8. **Dangerous-command list.** `contains_dangerous_command`
   (`command_safety.rs`, documented in `capabilities.md`) flags commands that
   destroy data with no undo (`rm` with `-r`/`-f`, `find -delete`/`-exec`,
   `git clean -f`, `reset --hard`, forcing `checkout`, `restore <path>`,
   `branch -D`, `stash drop`/`clear`, `shred`, `truncate`, `wipefs`, `dd of=`,
   `mkfs*`), interpreters whose operands mention a sensitive path
   (`mentions_sensitive_path`), and writes to paths an interpreter later
   honors (`.git`, shell startup files, `.npmrc`, ...). It unwraps `sudo`,
   `env`, `nohup`, `xargs`, `command`, and other wrappers, and fails closed on
   parse errors, dynamic command names, an unrecognized option on any
   wrapper, stdin-fed shells (`curl ... | sh`), and nesting past depth eight
   (the function's doc comment; `capabilities.md` names only unrecognized
   `env` options). A flagged command always reaches a human, never the
   guardian; a never-prompt decider denies it. The repro's false positive is
   the fail-closed rule: `command -v uv` is flagged because `-v` is not a
   known option of the `command` wrapper (checked directly against
   `contains_dangerous_command`).
9. **Sensitive paths outside the dangerous list.** A structured-tool request
   on a sensitive basename (`.env*`, keys, credentials) escalates a session
   grant back to `ask` in every posture (`permissions.rs`). Privileged secret
   and config edits ask "regardless of session permission mode"
   (`secrets.md`). So today `read_file .env` asks under the Full access
   posture while `cat .env` does not.
10. **Headless exec** uses the same enforced no-network sandbox, a decider
    that denies every `ask`, and `--auto-approve read-only` by default
    (`fs-write` and `shell-exec` denied, unless the guardian is configured,
    which returns both to `ask` for guardian review; `trusted-local` allows
    both) (`subagent.rs`). Under `--project-context auto`, `trusted-local`
    disables project context admission.
11. **Configuration files.** `providers.json` under the user Euler home is
    parsed with `deny_unknown_fields`: an unknown field is an error, not a
    warning (`provider_catalog.rs`). No repository file can change permissions
    or the sandbox today.

## Decision

The execution boundary and the default approval policy are a named
**profile**. Euler ships three built-in presets and reads user profiles from
`<euler home>/profiles.json`. Each context has a default profile. A user who
never opens the file gets the presets and the built-in defaults. Per-capability
settings and grants remain under `/permissions` Advanced.

### 1. Profile fields and the built-in presets

A profile is a set of fields, each composing machinery Euler has or this ADR
adds:

| Field | Values | `full` | `workspace` | `locked` |
| --- | --- | --- | --- | --- |
| `sandbox` | `off`, `on` (host backend), `bubblewrap`, `seatbelt` | `off` (`sandbox_backend: host`) | `on` | `on`, today's profile |
| `network` | `on`, `off` (a host allowlist later) | host | `on` | `off` |
| `writable_paths` | the workspace always, plus listed paths | the host, with the user's authority | workspace, host `/tmp`, the private home, toolchain caches | workspace and private scratch, as today |
| `tmp` | `host`, `private` | host | `host`: writable, persistent (Q4) | `private`: per command (Linux), no `/tmp` writes and a per-session `TMPDIR` (macOS) |
| `caches` | `writable` (named toolchain subdirectories), `private` | host | `writable` (Q13) | `private` |
| `approvals` | `never`, `escalation-only`, `always` | `never` | `escalation-only` | `always` |
| `escalation` | `allowed`, `not-allowed` | not applicable | `allowed` | `not-allowed` |
| `background_jobs` | `outlive`, `end-with-command` | `outlive` when output is redirected | `outlive` when output is redirected and the backend allows (Q6) | as today: `end-with-command` (Linux), `outlive` when redirected (macOS) |
| `reviewer` | `user`, `guardian` | the configured reviewer | the configured reviewer | the configured reviewer |

Two rules are not fields. The dangerous-command list is always on and cannot
be configured off: it fires under every profile and never reaches the
guardian (ADR 0021 row D). Structured writes (`write_file`, `edit_file`,
`apply_patch`) stay workspace-confined under every profile, because the
checkpoint and rollback guarantees of section 8 cover only the workspace
(proposed, Q22).

`approvals` governs commands that run inside the profile's boundary:

- `never`: no prompt except the dangerous-command list and the always-on asks
  (Q15).
- `escalation-only`: also asks for an escalation and for a sandboxed command
  whose operands mention a sensitive path (Q14).
- `always`: every command not statically safe or grant-covered asks, as today.

An escalation always goes to the reviewer. `HOME` and the environment follow
`sandbox`: with the sandbox off they are the user's; with it on, `HOME` is
private and the environment follows section 3. With the sandbox off,
`network`, `writable_paths`, `tmp`, and `caches` are the host's and are not
read. The macOS credential deny-reads (section 3) are fixed per backend, not a
field.

A user profile names one preset in `extends` and overrides fields. Paths
listed in `writable_paths` add to the preset's; every other field replaces
the preset's value:

```json
{
  "defaults": { "interactive": "full", "headless": "ci", "subagent": "inherit" },
  "profiles": {
    "ci": { "extends": "locked", "reviewer": "guardian" },
    "data": { "extends": "workspace", "writable_paths": ["~/data"] }
  }
}
```

**Tighten** is defined field by field, so a project overlay (section 5) and a
child profile (section 7) can be checked mechanically: `sandbox` off < on
(`bubblewrap` and `seatbelt` order as `on`; a profile with the sandbox off is
looser than any profile with it on, whatever its other fields);
`network` on < off; `writable_paths` tighter when a subset; `tmp` host <
private; `caches` writable < private; `approvals` never < escalation-only <
always; `escalation` allowed < not-allowed; `background_jobs` outlive <
end-with-command. `reviewer` has no order: an overlay may not set it.

### 2. The `full` preset

Unrestricted local execution. Commands run on the host with the user's
authority and never ask, except for the dangerous-command list and the
always-on asks. Which always-on asks remain is Q15: the proposed answer keeps
privileged secret and config edits (`secrets.md`) and extension install and
launch consent as deliberate exceptions, and drops the structured-tool
sensitive-basename ask under `approvals: never`. Provenance, redaction, checkpoints, rollback, and resume stay on.
`session.start` records the profile (section 8), `command_approvals: "off"`,
and `sandbox_backend: "host"` as separate facts (row D). Background jobs
outlive their command when their output is redirected; one that keeps stdout
open holds `run_shell` until its timeout, which ends its group (fact 3).

The environment passes through except Euler's own credentials and
parent-control variables (proposed, Q7). Euler's own credentials are the
built-in provider variables, the variables the active `providers.json`
references, and the values in the auth file. Row C's denylist is not applied
here: it would strip `LD_LIBRARY_PATH`, `NODE_OPTIONS`, and `RUSTFLAGS` from
the user's own toolchain setup, which an unsandboxed local agent leaves alone.
Other secret-shaped variables reach commands and stay registered with the
redactor. Redaction keeps those values out of tool results and provenance; it
does not stop a command from sending them over the network. The
dangerous-command list is a speed bump against common destructive commands,
not a boundary: `python3 -c` deleting files is not on it, as in any
unsandboxed local agent.

This reverses three ADR 0021 rules on purpose: row D's "no enforcement backend
means never auto-allow", row P's "the prove-safe grammar governs the
unsandboxed preset", and the note that Full Access does not disable the
sandbox.

### 3. The `workspace` preset

Sandboxed, with the friction removed:

- **Network on.** Bubblewrap shares the host network namespace; the Seatbelt
  profile gains outbound network and loopback bind. Row B's per-command
  network grants, its loopback restriction, its "retry with network" offer,
  and its veto on escalation under deny-read paths are dropped: network is a
  profile field, sandboxed code reaches every local service, and an
  escalation is a reviewed host command.
- **`/tmp`.** The host `/tmp` is writable and persistent (plus the host
  `TMPDIR` on macOS, where it lives under `/var/folders`). A per-session
  scratch exposed as `TMPDIR` would not help: Seatbelt cannot remap a path, so
  literal `/tmp/x` writes, which models and scripts use constantly, would
  still fail on macOS, and on Linux a path the model reports to the user
  (`/tmp/plot.png`) would not exist outside the sandbox. A real `/tmp` keeps
  paths identical inside and outside the boundary (proposed, Q4). Concurrent
  sessions and host processes share it, as they do without a sandbox.
- **`HOME` and caches.** `HOME` is a private per-session directory under the
  session's state directory, outside `/tmp` and `TMPDIR`, on both backends,
  so a `pip install --user` or a `~/.local/bin` install works for the rest of
  the session and no other session can write into it. Named toolchain cache
  subdirectories are writable at their real paths (a bind on Linux, a write
  allowance on macOS): the pip, uv, and npm caches, Cargo `registry` and
  `git`, `GOMODCACHE`, and the Hugging Face hub cache. The cache root is not
  bound, so credential files beside those caches (for example
  `~/.cache/huggingface/token`) stay out. A sandboxed command can poison a
  cache a later host build consumes; `locked` keeps private caches for
  untrusted repositories (proposed, Q13).
- **Environment.** Row C, narrowed: inherit the parent environment, apply
  its denylist (implemented with this preset; fact 6), set the pager
  variables, and also drop secret-shaped variables, because network is now on
  (proposed, Q5).
- **Secret reads on macOS.** Seatbelt allows broad reads today (Unit 3 owns
  narrowing). Before this preset turns network on, the profile denies reads
  of well-known credential stores, chosen as the default locations of
  credentials that common CLIs read without a prompt: `~/.ssh`, `~/.gnupg`,
  `~/.aws`, `~/.azure`, `~/.config/gcloud`, `~/.config/gh`, `~/.config/op`,
  `~/.kube`, `~/.docker/config.json`, `~/.netrc`, `~/.git-credentials`,
  `~/.npmrc`, `~/.pypirc`, `~/.cargo/credentials.toml`, `~/.terraform.d`,
  the keychains, browser profiles, and the Euler home. This list is a
  minimum, not a boundary: with network on, Unit 3's read narrowing becomes a
  security item rather than parity. Linux never mounts the real home.
- **Approval (row P, implemented here).** Inside the sandbox, `run_shell`,
  `git_*`, and workspace-confined structured writes run without a prompt.
  Prompts occur only for the dangerous-command list, for escalation, for the
  always-on asks, and for a `run_shell` command whose operands mention a
  sensitive path, because `curl -d @.env` would otherwise leave unprompted
  (proposed, Q14). That ask reuses `mentions_sensitive_path` to set the
  request's `sensitive_path` flag, so it is grant-coverable and
  guardian-routable; it does not change the danger walk, so it never fires
  under `full`.
- **Escalation.** With `escalation: allowed`, `run_shell` gains an optional
  `escalate` object with a required `reason`. The model sets it to run one
  command on the host with the `full` environment; the reviewer sees the
  command and the reason and allows once, or for the session for commands
  with that first token (proposed, Q8). The danger walk runs first, so no
  grant covers a flagged command. An escalated grant never covers the
  sandboxed form and the reverse (row B's cache-key rule, kept without row
  B's network grants). An unanswered or cancelled escalation follows the
  ordinary `ask` cancellation rule. The tool result records
  `sandbox_backend: "host"`, `escalated: true`, and the reason. Credentialed
  CLIs (`gh`, cloud CLIs) and installs outside the session use this path.

### 4. The `locked` preset

Today's behavior, unchanged: the no-network profile, private per-command
`/tmp` on Linux, private `HOME`, and every command not statically safe or
grant-covered asks. Headless `locked` keeps the `--auto-approve` tiers and the
project context rule exactly as today. `locked` has no escalation; leaving it
is a profile change.

### 5. Selection

- **Context defaults.** `profiles.json` holds `defaults.interactive`
  (built-in: `full`), `defaults.headless` (built-in: `workspace`), and
  `defaults.subagent` (built-in: `inherit`, see section 7). Headless means
  `euler exec`.
- **First run.** The first interactive launch with no
  `defaults.interactive` shows one picker: `full` (preselected),
  `workspace`, `locked`, each with a one-line boundary. The choice is written
  to `defaults.interactive` and never asked again. This picker replaces row
  D's typed confirmation (proposed, Q1). **New prompt**, once per user Euler
  home.
- **Precedence.** `--profile <name>` on `euler` and `euler exec`, then
  `/profile` in the TUI, then the user's `defaults`, then the built-in
  defaults. An unknown name is a startup error listing the defined profiles.
  A user `defaults.headless` naming a profile with `sandbox: off` counts as
  the explicit choice row D requires (proposed, Q20).
- **Project overlay.** A repository may hold one tightening overlay in
  `.euler/profile-overlay.json` (a different name from the user file, so a
  workspace at the home directory cannot confuse the two). It applies to whichever profile resolves, including
  one named by `--profile`; each field it sets must be at least as tight as
  the resolved value (section 1), or Euler rejects the overlay at startup
  with the field named. Tightening needs no trust acknowledgment, and the
  startup summary names the overlay. Loosening happens only in user
  configuration or an explicit flag (ADR 0022's origin reasoning). A project
  cannot define named profiles (proposed, Q18).
- **Validation.** Unknown fields and contradictory combinations
  (`approvals: never` with `escalation: allowed`, which would make the
  sandbox advisory; `sandbox: off` with `escalation: allowed`, which has no
  boundary to leave) are startup errors naming the file and field, as
  `providers.json` treats unknown fields (proposed, Q21). A user profile may
  not reuse a preset name and must extend a preset, so there are no chains or
  cycles.
- **`--auto-approve`** is accepted only when the resolved profile has
  `approvals: always`; elsewhere it is a startup error naming `--profile`
  (proposed, Q2). A preset not shipped yet during the rollout is refused with
  an error, never substituted.
- **TUI.** `/profile` opens the same picker, listing presets and user
  profiles, and can also write `defaults.interactive`. It supersedes
  `--profile` for the rest of the session. A change applies at the next turn
  boundary and is recorded as a profile-change event. It
  rebuilds the subprocess boundary, clears escalation grants, and ends the
  session's background jobs, so no job started under a looser profile keeps
  running under a stricter claim (proposed, Q12). `/permissions` loses its
  three posture rows and the unavailable sandbox row; Read only becomes an
  Advanced preset (proposed, Q3).
- **Platform.** A profile with the sandbox on keeps row A′: Euler never
  falls back by itself. When this host cannot enforce the resolved profile,
  headless runs fail at startup with a diagnostic naming the missing backend
  and `--profile`; interactive runs show a warning and let the user choose
  among the profiles this host can enforce, or quit (proposed, Q19). A
  profile with `sandbox: off` needs no probe.

### 6. The model is told

Every profile adds one always-on line to the system prompt stating the
resolved profile's boundary, for example: "Execution profile: workspace.
Commands run in a sandbox with network; writes are allowed in the workspace,
/tmp, caches, and a private home; set escalate with a reason to run one
command outside. Background jobs must redirect their output." A profile
change updates that line through the canvas at the next turn. A sandboxed
command that fails on DNS or a denied write gets a one-line hint naming the
boundary, not a guess at the cause. This also applies to `locked`, so the
repro's misdiagnosis cannot recur.

### 7. Unattended and extension-driven runs

- A never-prompt decider (headless `exec`, lifecycle hooks) denies any
  escalation, dangerous command, or other ask with a tool error that names
  the profile and the flag that would allow it.
- With `reviewer: guardian`, escalation requests are ordinary asks and go to
  the guardian; its allows stay once-scoped and its threshold rules apply, so
  a low-risk escalation runs unattended with the rationale recorded. Under
  `workspace` the guardian reviews escalations and sensitive-path asks; under
  `full` it reviews only the always-on asks it already handles, since
  commands no longer ask. Dangerous commands still go only to a human, so
  they are denied unattended.
- Child agents and extension-driven runs use `defaults.subagent`. `inherit`
  means the parent's resolved profile. A named profile is tightened field by
  field to be no looser than the parent's (section 1), so a child never runs
  looser than its parent (proposed, Q9). Extension capability approval,
  launch consent, and install consent are unchanged under sandboxed profiles;
  `full` session-allows extension capabilities as today's Full access posture
  does. Request ticks and idle contributors keep "never prompt".

### 8. Trust prompts

- Project context acknowledgment stays: one prompt per content digest, then
  silent. The headless rule that `trusted-local` disables `auto` admission
  stays for profiles with `approvals: always`; under other profiles an
  acknowledged digest is admitted headless (proposed, Q10).
- Extension install consent ("first contact is never automatic") and launch
  consent stay under every profile. Extensions run outside any sandbox, so
  the profile does not change their trust.

### 9. Provenance, rollback, and resume

`session.start` records `execution_profile`: the resolved name, its effective
fields after any project overlay, and a digest of those fields, beside
`command_approvals` and `sandbox_backend`. Structured writes checkpoint before
the write and `run_shell` observes the workspace before and after, on every
backend, so `/rollback` works the same under every profile. What a profile
changes is the observed scope: writes outside the workspace (`/tmp`, caches,
and with the sandbox off the whole home) and writes made by a background job
after its command returned are not attributed or restorable. The recorded
fields tell a reader that scope.

Resume resolves the profile for the new invocation as section 5 does. If the
result is looser than the recorded effective fields on any field, Euler
resumes with the recorded fields unless `--profile` is passed, so a session
started under `locked` does not resume under `full` silently (proposed, Q16).
A tighter result applies as resolved. Any difference is recorded and shown in
the startup summary. Logs without
`execution_profile` fold as "not recorded" and resolve as a new session: no
profile is inferred, because today's Full access posture ran sandboxed with
approvals off, which matches none of the presets. The recorded
`sandbox_backend` and permission decisions stay the evidence.

## Alternatives rejected

- **Three fixed modes.** The first draft of this ADR hard-coded Full access,
  Workspace, and Locked with one stored preference. It cannot lock down
  headless agents while the keyboard stays unrestricted, cannot add one data
  directory or turn network off without new code, and gives a repository no
  way to tighten. Profiles reuse the same machinery, and the presets keep the
  simple path for users who never open the file.
- **Keep the sandbox default and add per-command network grants (row B).**
  Three in four audited sessions need network, outside writes, or installs,
  and would still meet a prompt or failure.
- **Make `workspace` the interactive default.** It still puts credentialed
  CLIs and installs outside the session behind escalation prompts; `gh`
  alone appears in 28% of audited sessions. Users who want it set it once.
- **Per-session scratch as `TMPDIR` instead of the real `/tmp`.** See section
  3: it cannot cover literal `/tmp` paths on macOS.

## Rollout

Each step is one PR with tests that fail without it and updates its owning
contract (`capabilities.md`, `tools.md`, `events.md`, `ui.md`,
`project-context.md`).

1. **Boundary statement.** Tell the model the current boundary and add the
   failure hint (section 6) for today's profile. Independent; fixes the
   repro's misdiagnosis now.
2. **Danger-walk precision.** Recognize `command -v`/`-V` as a lookup that
   runs nothing, with the repro probe as the test.
3. **Profile plumbing and the `locked` preset.** `profiles.json` parsing and
   validation, the built-in presets, `--profile`, context defaults, the
   tighten-only project overlay, `execution_profile` on `session.start`, the
   resume rule, and the fold rule for older logs. Only `locked` is enforced;
   naming another preset is refused until its step, and both built-in
   defaults stay `locked` (today's behavior). The posture label stops
   claiming "unsandboxed".
4. **The `full` preset.** Host execution, approvals off except the dangerous
   list, the environment rule (Q7) replacing today's secret-shaped host scrub, `/profile`, the first-run picker (`full`
   and `locked` until step 5), `full` as the built-in interactive default,
   and session job tracking that ends surviving jobs at session end and on a
   profile change without signalling a reused process group (Q6, Q12).
5. **The `workspace` preset.** Network, `/tmp`, the per-session `HOME`,
   named writable caches, row C environment and denylist, macOS credential
   deny-reads, row P auto-allow, and the `run_shell` sensitive-path flag.
   `escalation` is `not-allowed` in the preset until step 6.
6. **Escalation.** The `escalate` field, prompt, grants (Q8), guardian
   routing, and never-prompt errors.
7. **Headless default.** The built-in `defaults.headless` becomes
   `workspace`; `--auto-approve` restricted to `approvals: always`; project
   context rule (Q10). **Behavior change** for scripts relying on read-only
   exec (proposed, Q11).
8. **Linux background jobs** (Q6), only if usage shows the need.

## Consequences

- The interactive default is the `full` preset: no sandbox, no routine
  prompts. Euler keeps provenance, redaction, rollback, resume, and
  multi-agent work, and records the profile, approvals-off, and sandbox-off.
- Users tune execution in one file: per-context defaults, user profiles that
  extend a preset, and repository overlays that can only tighten.
- Headless runs gain shell, writes, and network inside a sandbox by default,
  so CI on hosts without user namespaces (some hosted Linux runners) fails
  closed until it selects a profile with `sandbox: off`, the only kind that
  needs no backend.
- Linux hosts without user namespaces work out of the box under `full`.
- New prompts: the first-run picker (once), escalation (per command, or per
  session and first token), the `workspace` sensitive-path ask for shell
  commands, and the interactive choice when a profile cannot be enforced.
  The first three replace the far more frequent per-command prompts of
  today. New blocks: a malformed `profiles.json` or a loosening project
  overlay is a startup error.
- The `network` capability keeps gating host network APIs for extensions;
  subprocess network becomes a profile field.
- ADR 0021's "Full Access does not disable the sandbox" is reversed.
  ADR 0014's eventual `Auto in sandbox` posture (the picker's unavailable
  "Auto in workspace sandbox" row) is replaced by the `workspace` preset.

## Open questions

1. Is the first-run picker the explicit choice row D required, replacing
   the typed confirmation? **Proposed: yes.** Selecting `full` in a picker
   that states "no sandbox" is explicit; a typed phrase adds friction
   without information.
2. Accept `--auto-approve` only when the resolved profile has
   `approvals: always` (`locked` or a profile keeping that value), and fail
   elsewhere with a message naming `--profile`? **Proposed: yes.** One
   canonical flag, clear error, no alias layer.
3. Fold the TUI Read only posture into `/permissions` Advanced as a preset
   rather than a fourth built-in profile? **Proposed: yes.** It is an
   approval preset, not a boundary.
4. Real host `/tmp` in `workspace` rather than a per-session scratch as
   `TMPDIR`? **Proposed: real `/tmp`** (section 3). Cost: `/tmp` writes are
   shared and outside workspace observation, as without a sandbox. On macOS
   the host `TMPDIR` also holds other sessions' Seatbelt scratch trees; the
   private `HOME` moves out of `TMPDIR`, but a sandboxed command can still
   write into a `locked` session's scratch unless the profile denies
   `euler-seatbelt-*` siblings.
5. In `workspace`, keep `HOME` private and secret-shaped variables out,
   sending credentialed CLIs through escalation? **Proposed: yes.** With
   network on, credentials are what the sandbox still protects; users who
   want `gh` without prompts pick `full` or grant `gh` escalation for the
   session.
6. Background jobs on Linux: keep the per-command PID namespace and tell the
   model jobs end with their command, deferring a session-scoped sandbox
   instance? **Proposed: yes.** Dropping `--unshare-pid` would let sandboxed
   code signal or trace host processes of the same user; macOS already lets
   jobs outlive the command. Separately, Euler will end the jobs it started
   when the session ends, on both backends (fact 3: it does not today). The
   mechanism must not signal a reused process group: keep each leader
   unreaped until cleanup, use a pidfd where available, or run one sandbox
   instance or cgroup per session.
7. Under `full` and for escalated commands, pass the environment through
   except Euler's own credentials and parent-control variables, without row
   C's denylist? **Proposed: yes.** Today's host scrub breaks `gh` and cloud
   CLIs that read `GITHUB_TOKEN` or AWS variables, and row C's dual-use tier
   breaks CUDA, conda, and toolchain setups. Costs: a user script that needs
   the same key Euler uses (`OPENAI_API_KEY` while Euler runs on OpenAI)
   does not get it; loader variables such as `LD_PRELOAD` pass through, as in
   the user's own shell. The redactor already seeds secret values at session
   start, which keeps them out of the record, not off the network.
8. Let an escalation approval cover the session for commands with the same
   first token? **Proposed: yes**, through the existing scoped-grant store
   and its segment-aware matching (every segment of a compound must be
   covered), keyed separately from sandboxed grants. Never offered for
   shells, interpreters, privilege wrappers (`sh`, `python3`, `node`,
   `sudo`, ...), or task runners and package managers that run arbitrary code
   (`npm`, `npx`, `uv run`, `cargo run`, `make`, `just`, `pip install` of a
   local path), since a session grant for `npm` is a host shell through
   `npm exec`. Never covering a flagged command.
9. Child agents never run looser than their parent, tightened field by
   field? **Proposed: yes**, matching capability attenuation.
10. Admit acknowledged project context headless under profiles other than
    `approvals: always`, keeping today's `trusted-local` rule only there?
    **Proposed: yes.** The acknowledgment is the user's decision on exact
    content; disabling it would drop `AGENTS.md` from every default exec run.
11. Flip the built-in headless default from read-only to `workspace` in one
    release, without an alias for the old default? **Proposed: yes.** It is
    the direction's default, and `--profile locked` restores today's
    behavior in one flag.
12. On a profile change, clear escalation grants and end the session's
    background jobs? **Proposed: yes.** Otherwise a job started on the host
    keeps running after the session claims `locked`. Depends on the job
    tracking of Q6 (rollout step 4).
13. Make named toolchain cache subdirectories writable in `workspace` and
    accept cache poisoning as a cost? **Proposed: yes.** Re-downloading on
    every command is the friction this preset exists to remove; untrusted
    repositories belong in `locked`. Binding named subdirectories rather
    than the cache root keeps credential files stored in the cache tree out.
14. In `workspace`, ask for a sandboxed command whose operands mention a
    sensitive path (`.env*`, keys, credentials)? **Proposed: yes.** Network
    is on, and this keeps shell parity with the structured-tool
    sensitive-basename ask. `full` does not add it.
15. Which asks remain under `approvals: never`? The approved direction
    allows "no permission prompts except the always-on dangerous-command
    list". **Proposed:** keep privileged secret and config edits
    (`secrets.md`) and extension install and launch consent as deliberate
    exceptions, since they guard Euler's own credentials and code that runs
    outside any sandbox. For the structured-tool sensitive-basename ask, the
    options are: keep it (cost: `read_file .env` asks while `cat .env` runs
    silently, the asymmetry fact 9 describes), or drop it under
    `approvals: never` for parity (cost: a model can read `.env` with no
    prompt, as an unsandboxed local agent can). **Proposed: drop it** under
    `approvals: never`. Separately, the interpreter mention rule
    (`python3 x.py .env`) stays on the dangerous list under every profile,
    so it can never be granted; the alternative is to move it to the
    sensitive-path flag so `full` stops asking for it.
16. On resume, keep the recorded effective fields when the newly resolved
    profile is looser, unless `--profile` is passed? **Proposed: yes.** The
    alternative, accepting silent loosening with only a startup-summary
    line, undercuts `locked` for untrusted repositories now that
    `euler --continue` is a routine path (ADR 0023).
17. Configuration format: JSON, TOML, or YAML? **Proposed: JSON**, like
    `providers.json`; Euler has no TOML or YAML dependency for user
    configuration.
18. May a project define new named profiles? **Proposed: no**, only one
    tightening overlay, applied to every resolved profile including one
    named by `--profile`. A user who disagrees edits or removes the
    repository file.
19. How is an unenforceable profile handled interactively? **Proposed:** a
    warning naming the missing backend and a choice among the profiles this
    host can enforce, or quit. Never an automatic fallback (row A′).
20. ADR 0021 row D requires an explicit flag for headless host mode. Does a
    user-level `defaults.headless` naming a profile with `sandbox: off`
    count as explicit? **Proposed: yes**, because only the user's own
    configuration can set it. The alternative keeps `--profile full` (or
    another sandbox-off profile) required on every headless run.
21. Reject unknown fields and contradictory combinations at load, rather
    than warn? **Proposed: reject**, as `providers.json` does: a misspelled
    field silently dropped can leave a profile looser than written. The
    direction suggested warnings; this is the one point where the proposal
    differs.
22. Keep structured writes workspace-confined under every profile?
    **Proposed: yes.** Checkpoints and rollback cover only the workspace.
    Cost: under `full`, the model writes outside the workspace through shell
    commands, which skip checkpoints.
