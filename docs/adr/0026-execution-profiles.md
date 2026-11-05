# ADR 0026: Execution profiles: full, workspace, locked

## Status

Accepted (2026-10-28). The execution boundary and the default approval policy
become a named profile defined in user configuration: three built-in presets
(`full`, `workspace`, `locked`), user profiles that extend a preset, and a
default profile per context (interactive, headless, subagent). Amends ADR 0021
rows B, C, D, and P and its note that Full Access does not disable the
sandbox; rows A, A′, E, and G stand, with A′ restated for profiles in section
5. The questions the approved direction left open were resolved by the
maintainer on 2026-10-28 and are listed at the end; the Decision text cites
them as "resolved question n". No contract changes until the rollout PRs land;
each PR updates its owning contract.

Amended (2026-10-29): the built-in headless default is `full`, the same as
interactive, instead of `workspace` (resolved question 11).

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
| `tmp` | `host`, `private` | host | `host`: writable, persistent | `private`: per command (Linux), no `/tmp` writes and a per-session `TMPDIR` (macOS) |
| `caches` | `writable` (named toolchain subdirectories), `private` | host | `writable` | `private` |
| `credentials` | `private`, `pass` | not applicable: the user's own | `private` | `private` |
| `approvals` | `never`, `escalation-only`, `always` | `never` | `escalation-only` | `always` |
| `escalation` | `allowed`, `not-allowed` | not applicable | `allowed` | `not-allowed` |
| `background_jobs` | `outlive`, `end-with-command` | `outlive` when output is redirected | as `locked` | as today: `end-with-command` (Linux), `outlive` when redirected (macOS) |
| `reviewer` | `user`, `guardian` | the configured reviewer | the configured reviewer | the configured reviewer |

Four rules are not fields:

- The dangerous-command list is always on and cannot be configured off: it
  fires under every profile and never reaches the guardian (ADR 0021 row D).
- A structured write (`write_file`, `edit_file`, `apply_patch`) to a path on
  the interpreter-honored configuration list (anything under `.git`, git,
  npm, and Cargo configuration, shell startup files) asks a human under every
  profile and never reaches the guardian, exactly like its shell form on the
  dangerous list (resolved question 15).
- Structured-write scope follows `sandbox`. With the sandbox on, structured
  writes stay workspace-confined. With it off, they may target any path the
  user can write; Euler records each in provenance, but checkpoints and
  `/rollback` cover only the workspace, so a structured write outside it
  cannot be rolled back (resolved question 22).
- Session-end job cleanup follows `sandbox`. With the sandbox on, Euler ends
  the background jobs it started (including escalated commands) at session
  end and on a profile change, without signalling a reused process group.
  With it off, jobs keep running as in an unsandboxed local agent, and Euler
  lists the ones it started that are still running at exit (resolved
  questions 6 and 12).

`approvals` governs commands that run inside the profile's boundary:

- `never`: no prompt except the dangerous-command list, structured writes to
  interpreter-honored configuration, and two deliberate exceptions:
  privileged secret and config edits (`secrets.md`) and extension install and
  launch consent. Reading a sensitive path does not ask, through a structured
  tool or a shell command (resolved question 15).
- `escalation-only`: also asks for an escalation, for a structured-tool
  request on a sensitive basename, and for a `run_shell` command whose
  operands mention a sensitive path (resolved question 14).
- `always`: every command not statically safe or grant-covered asks, as today.

An escalation always goes to the reviewer. `HOME` and the environment follow
`sandbox` and `credentials`: with the sandbox off they are the user's; with it
on, `HOME` is private and the environment follows section 3. With the sandbox
off, `network`, `writable_paths`, `tmp`, and `caches` are the host's and are
not read. The macOS credential deny-reads (section 3) are fixed per backend,
not a field, except as `credentials: "pass"` lifts them.

**`credentials`** applies only with the sandbox on (resolved question 5).
`private`, the `workspace` and `locked` value, keeps the private `HOME`,
drops secret-shaped variables, and on macOS keeps the credential deny-reads
of section 3. `pass` is for a user profile, usually one extending
`workspace`, whose headless runs use credentialed CLIs and cannot answer an
escalation prompt:

- Environment: secret-shaped variables pass, except Euler's own credentials
  and parent-control variables (the `full` rule, section 2). Row C's hard
  denylist still applies.
- Files: `HOME` stays private. Each credential store in section 3's list
  that exists, other than the keychains, browser profiles, and the Euler
  home, plus `~/.gitconfig` and `~/.config/git`, appears read-only at the
  same path under the private `HOME`: a read-only bind on Linux, and on macOS
  a symlink to the real path with that path's deny-read lifted. A store whose
  real path is also listed in `writable_paths` is writable instead (a
  writable bind on Linux, a write allowance on macOS), for CLIs that refresh
  tokens in place.
- The macOS keychain is not passed. A CLI that keeps its token only there
  (for example `gh` after a default login on macOS) needs the token in an
  environment variable such as `GH_TOKEN`, or escalation.
- Validation: the value is `private` or `pass`. Setting `credentials` on a
  profile whose `sandbox` is `off` is a startup error, because it would
  promise a boundary that does not exist.

A user profile names one preset in `extends` and overrides fields. Paths
listed in `writable_paths` add to the preset's; every other field replaces
the preset's value:

```json
{
  "defaults": { "interactive": "full", "headless": "ci", "subagent": "inherit" },
  "profiles": {
    "ci": { "extends": "locked", "reviewer": "guardian" },
    "data": { "extends": "workspace", "writable_paths": ["~/data"] },
    "agents": { "extends": "workspace", "credentials": "pass" }
  }
}
```

**Tighten** is defined field by field, so a project overlay (section 5) and a
child profile (section 7) can be checked mechanically: `sandbox` off < on
(`bubblewrap` and `seatbelt` order as `on`; a profile with the sandbox off is
looser than any profile with it on, whatever its other fields);
`network` on < off; `writable_paths` tighter when a subset; `tmp` host <
private; `caches` writable < private; `credentials` pass < private;
`approvals` never < escalation-only < always; `escalation` allowed <
not-allowed; `background_jobs` outlive < end-with-command. `reviewer` has no
order: an overlay may not set it.

### 2. The `full` preset

Unrestricted local execution. Commands run on the host with the user's
authority and never ask, except for the dangerous-command list, structured
writes to interpreter-honored configuration (section 1), privileged secret
and config edits (`secrets.md`), and extension install and launch consent
(resolved question 15). Reading a sensitive path does not ask, whether
through `read_file .env`, `cat .env`, or `python3 x.py .env`: under
`approvals: never` the structured-tool sensitive-basename ask is dropped for
reads and for writes outside the interpreter-honored configuration list, and
the interpreter-mention rule moves from the dangerous list to the
sensitive-path flag (section 3), which does not ask here.

Provenance, redaction, rollback for the workspace, and resume stay on.
Structured writes may target paths outside the workspace; provenance records
them, and no checkpoint covers them (section 1, resolved question 22).
`session.start` records the profile (section 9), `command_approvals: "off"`,
and `sandbox_backend: "host"` as separate facts (row D). Background jobs
outlive their command when their output is redirected, and outlive the
session: Euler does not end them, and at exit it lists the ones it started
that are still running (resolved questions 6 and 12). A job that keeps stdout
open holds `run_shell` until its timeout, which ends its group (fact 3).

The environment passes through except Euler's own credentials and
parent-control variables (resolved question 7). Euler's own credentials are
the built-in provider variables, the variables the active `providers.json`
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
  paths identical inside and outside the boundary (resolved question 4).
  Concurrent sessions and host processes share it, as they do without a
  sandbox.
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
  untrusted repositories (resolved question 13).
- **Environment.** Row C, narrowed: inherit the parent environment, apply
  its denylist (implemented with this preset; fact 6), set the pager
  variables, and also drop secret-shaped variables, because network is now on
  (resolved question 5). A user profile extending `workspace` with
  `credentials: "pass"` passes them and named credential stores instead
  (section 1).
- **Secret reads on macOS.** Seatbelt allows broad reads today (Unit 3 owns
  narrowing). Before this preset turns network on, the profile denies reads
  of well-known credential stores, chosen as the default locations of
  credentials that common CLIs read without a prompt: `~/.ssh`, `~/.gnupg`,
  `~/.aws`, `~/.azure`, `~/.config/gcloud`, `~/.config/gh`, `~/.config/op`,
  `~/.kube`, `~/.docker/config.json`, `~/.netrc`, `~/.git-credentials`,
  `~/.npmrc`, `~/.pypirc`, `~/.cargo/credentials.toml`, `~/.terraform.d`,
  the keychains, browser profiles, and the Euler home. This list is a
  minimum, not a boundary: with network on, Unit 3's read narrowing becomes a
  security item rather than parity. `credentials: "pass"` lifts the deny for
  the stores it passes. Linux never mounts the real home; `pass` binds only
  the named stores.
- **Approval (row P, implemented here).** Inside the sandbox, `run_shell`,
  `git_*`, and workspace-confined structured writes run without a prompt.
  Prompts occur only for the dangerous-command list, for escalation, for the
  asks section 1 keeps under every profile, for structured-tool requests on a
  sensitive basename, and for a `run_shell` command whose operands mention a
  sensitive path, because `curl -d @.env` would otherwise leave unprompted
  (resolved question 14). That ask reuses `mentions_sensitive_path` to set
  the request's `sensitive_path` flag, which also carries the
  interpreter-mention rule moved off the dangerous list (resolved question
  15), so it is grant-coverable and guardian-routable. Under
  `approvals: never` the flag does not ask, so it never fires under `full`.
- **Escalation.** With `escalation: allowed`, `run_shell` gains an optional
  `escalate` object with a required `reason`. The model sets it to run one
  command on the host with the `full` environment; the reviewer sees the
  command and the reason and allows once, or for the session for commands
  with that first token (resolved question 8). The danger walk runs first, so
  no grant covers a flagged command. An escalated grant never covers the
  sandboxed form and the reverse (row B's cache-key rule, kept without row
  B's network grants). An unanswered or cancelled escalation follows the
  ordinary `ask` cancellation rule. The tool result records
  `sandbox_backend: "host"`, `escalated: true`, and the reason. Credentialed
  CLIs (`gh`, cloud CLIs) and installs outside the session use this path, or,
  for runs that cannot answer a prompt, a profile with
  `credentials: "pass"`.

### 4. The `locked` preset

Today's behavior, unchanged: the no-network profile, private per-command
`/tmp` on Linux, private `HOME`, and every command not statically safe or
grant-covered asks. Headless `locked` keeps the `--auto-approve` tiers and the
project context rule exactly as today. `locked` has no escalation; leaving it
is a profile change.

### 5. Selection

- **Context defaults.** `profiles.json` holds `defaults.interactive`
  (built-in: `full`), `defaults.headless` (built-in: `full`; `workspace`
  before the 2026-10-29 amendment of resolved question 11), and
  `defaults.subagent` (built-in: `inherit`, see section 7). Headless means
  `euler exec`.
- **First run.** The first interactive launch with no
  `defaults.interactive` shows one picker: `full` (preselected),
  `workspace`, `locked`, each with a one-line boundary. The choice is written
  to `defaults.interactive` and never asked again. This picker replaces row
  D's typed confirmation (resolved question 1). **New prompt**, once per user
  Euler home.
- **Precedence.** `--profile <name>` on `euler` and `euler exec`, then
  `/profile` in the TUI, then the user's `defaults`, then the built-in
  defaults. An unknown name is a startup error listing the defined profiles.
  A user `defaults.headless` naming a profile with `sandbox: off` counts as
  the explicit choice row D requires (resolved question 20).
- **Project overlay.** A repository may hold one tightening overlay in
  `.euler/profile-overlay.json` (a different name from the user file, so a
  workspace at the home directory cannot confuse the two). It applies to
  whichever profile resolves, including one named by `--profile`; each field
  it sets must be at least as tight as the resolved value (section 1), or
  Euler rejects the overlay at startup with the field named. Tightening needs
  no trust acknowledgment, and the startup summary names the overlay.
  Loosening happens only in user configuration or an explicit flag (ADR
  0022's origin reasoning). A project cannot define named profiles (resolved
  question 18).
- **Validation.** Unknown fields and contradictory combinations are startup
  errors naming the file and field, as `providers.json` treats unknown fields
  (resolved question 21). The contradictory combinations are
  `approvals: never` with `escalation: allowed`, which would make the sandbox
  advisory; `sandbox: off` with `escalation: allowed`, which has no boundary
  to leave; and `credentials` on a profile with `sandbox: off` (section 1). A
  user profile may not reuse a preset name and must extend a preset, so there
  are no chains or cycles.
- **`--auto-approve`** is accepted under every profile (resolved question 2).
  Its tiers keep today's meaning for capability asks inside the profile's
  boundary, so under a profile with `approvals: always` they behave as today.
  When the resolved profile already allows at least what the tier asks for
  (any tier under `approvals: never` or `escalation-only`, where commands and
  writes inside the boundary already run and sensitive-path asks already
  escalate past a session allow), the flag is a no-op and Euler prints a
  one-line notice naming the profile. It never errors for asking for less
  than the profile allows, and it never tightens the profile; `--profile`
  does that. The flag never approves an escalation or a dangerous command, so
  no combination with a profile is contradictory and none is an error. A
  preset not shipped yet during the rollout is refused with an error, never
  substituted.
- **TUI.** `/profile` opens the same picker, listing presets and user
  profiles, and can also write `defaults.interactive`. It supersedes
  `--profile` for the rest of the session. A change applies at the next turn
  boundary and is recorded as a profile-change event. It rebuilds the
  subprocess boundary, clears escalation grants, and ends the background jobs
  Euler started under a sandboxed profile. Jobs started under a profile with
  the sandbox off keep running and are listed at exit, so the new profile's
  claim covers commands started after the change (resolved questions 6 and
  12). `/permissions` loses its three posture rows and the unavailable
  sandbox row; Read only becomes an Advanced preset (resolved question 3).
- **Platform.** A profile with the sandbox on keeps row A′: Euler never
  falls back by itself. When this host cannot enforce the resolved profile,
  headless runs fail at startup with a diagnostic naming the missing backend
  and `--profile`; interactive runs show a warning and let the user choose
  among the profiles this host can enforce, or quit (resolved question 19). A
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
  `full` it reviews only the remaining guardian-routable asks, since commands
  no longer ask. Dangerous commands and structured writes to
  interpreter-honored configuration still go only to a human, so they are
  denied unattended.
- Child agents and extension-driven runs use `defaults.subagent`. `inherit`
  means the parent's resolved profile. A named profile is tightened field by
  field to be no looser than the parent's (section 1), so a child never runs
  looser than its parent (resolved question 9). Extension capability
  approval, launch consent, and install consent are unchanged under sandboxed
  profiles; `full` session-allows extension capabilities as today's Full
  access posture does. Request ticks and idle contributors keep "never
  prompt".

### 8. Trust prompts

- Project context acknowledgment stays: one prompt per content digest, then
  silent. The headless rule that `trusted-local` disables `auto` admission
  stays for profiles with `approvals: always`; under other profiles an
  acknowledged digest is admitted headless (resolved question 10).
- Extension install consent ("first contact is never automatic") and launch
  consent stay under every profile. Extensions run outside any sandbox, so
  the profile does not change their trust.

### 9. Provenance, rollback, and resume

`session.start` records `execution_profile`: the resolved name, its effective
fields after any project overlay, and a digest of those fields, beside
`command_approvals` and `sandbox_backend`. Structured writes inside the
workspace checkpoint before the write and `run_shell` observes the workspace
before and after, on every backend, so `/rollback` works the same for the
workspace under every profile. What a profile changes is the observed scope.
Shell writes outside the workspace (`/tmp`, caches, and with the sandbox off
the whole home) and writes made by a background job after its command
returned are neither attributed nor restorable. Structured writes outside the
workspace, possible only with the sandbox off, are recorded in provenance
with their path, but no checkpoint covers them and `/rollback` cannot restore
them (resolved question 22). The recorded fields tell a reader that scope.

Resume resolves the profile for the new invocation as section 5 does. If the
result is looser than the recorded effective fields on any field, Euler
resumes with the recorded fields unless `--profile` is passed, so a session
started under `locked` does not resume under `full` silently (resolved
question 16). A tighter result applies as resolved. Any difference is
recorded and shown in the startup summary. Logs without `execution_profile`
fold as "not recorded" and resolve as a new session: no profile is inferred,
because today's Full access posture ran sandboxed with approvals off, which
matches none of the presets. The recorded `sandbox_backend` and permission
decisions stay the evidence.

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
4. **The `full` preset.** Host execution; approvals off except the dangerous
   list and the asks section 1 keeps; the interpreter-mention rule moved from
   the dangerous list to the `sensitive_path` flag, the sensitive-basename
   ask dropped for reads under `approvals: never`, and structured writes to
   interpreter-honored configuration made human-only under every profile
   (resolved question 15); structured writes outside the workspace, recorded
   in provenance without a checkpoint (resolved question 22); the
   environment rule (resolved question 7) replacing today's secret-shaped
   host scrub; the `--auto-approve` no-op notice (resolved question 2);
   `/profile`; the first-run picker (`full` and `locked` until step 5);
   `full` as the built-in interactive default; and session job tracking that
   lists jobs started under a sandbox-off profile at exit and ends jobs
   started under a sandboxed profile at session end and on a profile change,
   without signalling a reused process group (resolved questions 6 and 12).
5. **The `workspace` preset.** Network, `/tmp`, the per-session `HOME`,
   named writable caches, row C environment and denylist, macOS credential
   deny-reads, the `credentials` field (resolved question 5), row P
   auto-allow, and the `run_shell` sensitive-path flag for every operand
   (resolved question 14). `escalation` is `not-allowed` in the preset until
   step 6.
6. **Escalation.** The `escalate` field, prompt, grants (resolved question
   8), guardian routing, and never-prompt errors.
7. **Headless default.** (Amended 2026-10-29: the built-in
   `defaults.headless` is now `full`, a second **behavior change**; headless
   `exec` runs unrestricted unless a profile says otherwise. See resolved
   question 11.) As first shipped: the built-in `defaults.headless` becomes
   `workspace`, and the project context rule applies (resolved question 10).
   **Behavior change** for scripts relying on read-only exec (resolved
   question 11).
8. **A session-scoped Linux sandbox instance** so sandboxed jobs can outlive
   their command (resolved question 6), only if usage shows the need.

## Consequences

- The interactive default is the `full` preset: no sandbox, no routine
  prompts. Euler keeps provenance, redaction, rollback for the workspace,
  resume, and multi-agent work, and records the profile, approvals-off, and
  sandbox-off.
- Under `full`, background jobs outlive the session as in an unsandboxed
  local agent; Euler lists the ones it started at exit. Under sandboxed
  profiles Euler ends them at session end and on a profile change.
- Under `full`, structured writes can change files outside the workspace.
  Provenance records them; checkpoints and `/rollback` do not cover them.
- Users tune execution in one file: per-context defaults, user profiles that
  extend a preset, and repository overlays that can only tighten.
- Headless runs default to `full`, like interactive ones (resolved question
  11, amended 2026-10-29): no sandbox, no routine prompts, the user's `HOME`
  and credentials. CI and untrusted repositories select `workspace` or
  `locked` through `--profile` or `defaults.headless`. A sandboxed profile
  on a host without user namespaces (some hosted Linux runners) fails
  closed. Sandboxed headless runs that need credentialed CLIs without
  prompts use a profile with `credentials: "pass"`, which hands those
  credentials to sandboxed code that has network.
- `--auto-approve` keeps working under every profile; under profiles that
  already allow its tier it is a no-op with a notice.
- Linux hosts without user namespaces work out of the box under `full`.
- New prompts: the first-run picker (once), escalation (per command, or per
  session and first token), the `workspace` sensitive-path ask for shell
  commands, and the interactive choice when a profile cannot be enforced.
  The first three replace the far more frequent per-command prompts of
  today. Structured writes to interpreter-honored configuration no longer
  reach the guardian, so unattended runs deny them. New blocks: a malformed
  `profiles.json`, a loosening project overlay, or `credentials` on a
  sandbox-off profile is a startup error.
- The `network` capability keeps gating host network APIs for extensions;
  subprocess network becomes a profile field.
- ADR 0021's "Full Access does not disable the sandbox" is reversed.
  ADR 0014's eventual `Auto in sandbox` posture (the picker's unavailable
  "Auto in workspace sandbox" row) is replaced by the `workspace` preset.

## Resolved questions (2026-10-28)

Entries marked **changed** differ from the proposal in the draft.

1. The first-run picker is the explicit choice row D required and replaces
   the typed confirmation. As proposed.
2. **Changed.** `--auto-approve` is accepted under every profile. When the
   resolved profile already allows at least what the tier asks for, it is a
   no-op with a one-line notice; it never errors for asking for less than the
   profile allows. No combination is contradictory, so none is an error
   (section 5).
3. The TUI Read only posture becomes a `/permissions` Advanced preset, not a
   fourth profile. As proposed.
4. `workspace` uses the real host `/tmp`, not a per-session `TMPDIR`. As
   proposed. Costs: `/tmp` writes are shared and outside workspace
   observation, and on macOS a sandboxed command can write into another
   session's Seatbelt scratch tree in the host `TMPDIR` unless the profile
   denies `euler-seatbelt-*` siblings.
5. **Changed.** `workspace` keeps `HOME` private and secret-shaped variables
   out by default. A new `credentials` field (`private` or `pass`) lets a
   user profile extending `workspace` pass secret-shaped variables and named
   credential stores, for headless runs that cannot answer escalation
   prompts (section 1).
6. **Changed.** Linux keeps the per-command PID namespace, so sandboxed jobs
   end with their command there. Euler ends the jobs it started at session
   end and on a profile change only under sandboxed profiles, without
   signalling a reused process group. Under `full` and any sandbox-off
   profile, jobs keep running and Euler lists the ones it started at exit.
7. Under `full` and for escalated commands, the environment passes through
   except Euler's own credentials and parent-control variables, without row
   C's denylist. As proposed.
8. An escalation approval may cover the session for commands with the same
   first token, through the scoped-grant store and its segment-aware
   matching, keyed separately from sandboxed grants. Never offered for
   shells, interpreters, privilege wrappers, or task runners and package
   managers that run arbitrary code (`npm`, `npx`, `uv run`, `cargo run`,
   `make`, `just`, `pip install` of a local path); never covering a flagged
   command. As proposed.
9. Child agents never run looser than their parent, tightened field by
   field. As proposed.
10. Acknowledged project context is admitted headless under profiles other
    than `approvals: always`; today's `trusted-local` rule stays only there.
    As proposed.
11. The built-in headless default flips from read-only to `workspace` in one
    release, without an alias; `--profile locked` restores today's behavior.
    As proposed.

    **Amended 2026-10-29.** The built-in headless default is `full`, the
    same as interactive. An audit of 351 recent unattended long-horizon
    agent runs (14,366 shell commands) found that about 81% needed
    credentials or the real `HOME`: `gh` in 34%, `git push` or `git fetch`
    in 30%, model or API keys in 32%, warehouse and cloud CLIs in 9%, and
    writes to shared state outside the worktree in 52%. Under `workspace`,
    the default `credentials: private`, the private `HOME`, and escalation
    that a headless run cannot answer break those runs. `workspace` and
    `locked` stay available through `--profile` or `defaults.headless`, for
    example for CI or untrusted repositories. Resolved question 20 still
    governs a user `defaults.headless`; the built-in sandbox-off default is
    this maintainer decision.
12. **Changed.** A profile change clears escalation grants and ends the jobs
    Euler started under a sandboxed profile. Jobs started under a
    sandbox-off profile keep running and are listed at exit (see 6).
13. Named toolchain cache subdirectories are writable in `workspace`; cache
    poisoning is an accepted cost. As proposed.
14. In `workspace`, a sandboxed command whose operands mention a sensitive
    path asks. As proposed.
15. **Changed.** Under `approvals: never`, privileged secret and config edits
    and extension install and launch consent remain deliberate exceptions,
    and the structured-tool sensitive-basename ask is dropped for reads. The
    interpreter-mention rule (`python3 x.py .env`) moves from the dangerous
    list to the sensitive-path flag, so `full` does not ask for it and
    `workspace` asks as in 14. Clarification with 22: a structured write to a
    path on the interpreter-honored configuration list (anything under
    `.git`, git, npm, and Cargo configuration, shell startup files) asks a
    human under every profile, matching its shell form on the dangerous list.
16. On resume, Euler keeps the recorded effective fields when the newly
    resolved profile is looser, unless `--profile` is passed. As proposed.
17. The configuration format is JSON, like `providers.json`. As proposed.
18. A project may not define named profiles, only one tightening overlay
    applied to every resolved profile. As proposed.
19. An unenforceable profile shows a warning naming the missing backend and
    a choice among the profiles this host can enforce, or quit; never an
    automatic fallback. As proposed.
20. A user-level `defaults.headless` naming a sandbox-off profile counts as
    the explicit choice row D requires. As proposed.
21. Unknown fields and contradictory combinations are rejected at load, not
    warned about. As proposed; this differs from the direction's suggestion
    of warnings because a silently dropped field can leave a profile looser
    than written.
22. **Changed.** Under `full` and any sandbox-off profile, structured writes
    may target paths outside the workspace; they are recorded in provenance
    and are not covered by checkpoints or `/rollback`. `workspace` and
    `locked` keep structured writes workspace-confined. See 15 for writes to
    interpreter-honored configuration.
