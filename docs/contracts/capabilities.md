# Capability Contract

Capabilities govern tools, extensions, and child agents.

Child agents may only receive capabilities that are a subset of the parent
session's capabilities. In v0, agent capability attenuation uses exact set
semantics over the flat `Capability` strings below. Equality is allowed.
Path-aware, host-aware, provider-aware, model-aware, and secret-name-aware
narrowing are deferred until capabilities gain structured scope fields.

There are no trusted bypass flags for normal workflows. If a first-party extension cannot do its job without a bypass, the capability model is wrong.

## Trust Model Honesty

Native Rust extensions run in-process. Managed-process extensions run as a
child process and the host owns their protocol, lifecycle, capability-gated
host APIs, provenance events, and canvas admission. Both are trusted-code
surfaces: the capability model constrains what the *host APIs* will do and
records every decision in provenance, but neither runtime is an OS sandbox. A
native extension or child process that ignores host APIs is limited only by the
operating system. What capabilities buy is least-privilege discipline, a
reviewable declared-authority surface, and an audit trail. Containment of
actually-untrusted code requires a separate OS-level isolation milestone; a
subprocess alone is not that boundary. Do not describe extension capabilities
as a security boundary against malicious extension code.

## V0 Capability Scopes

Minimum v0 scopes:

- `fs-read`
- `fs-write`
- `extension-state`
- `provenance-read`
- `diagnostics-read`
- `artifact-write`
- `agent-record`
- `agent-spawn`
- `context-slot`
- `plan-presentation`
- `shell-exec`
- `network`
- `config-write`
- `secret-resolve`

## Execution boundary and capability decisions

A capability decision and an execution boundary are separate facts. On Linux
and macOS the enforced Bubblewrap and Seatbelt backends are the boundary for
`run_shell` and the `git_*` tools; the permission decision still happens, and
the sandbox never turns a denial into an allow. On unsupported platforms the
permission decision is the whole confinement story for those tools, which is
why the prove-safe grammar stays conservative there.

The backend is probed at session start, not assumed, and recorded on
`session.start` as `sandbox_backend` (`bwrap` | `seatbelt` | `host` |
`unavailable`). When a required probe fails, sandbox-requiring tools fail
closed: no backend is ever silently downgraded to host execution.
`euler --check-sandbox` reports
the same result with an actionable diagnostic.
The model reads the same boundary in the `run_shell` description, and a
failed sandboxed command that looks like a network denial carries a host note
saying network is disabled (tools contract, "Execution boundary").

A probe that does not finish within its deadline is a timeout, not a refusal:
it is reported with its own reason (`probe_timed_out`), its own message (the
check timed out, most likely under host load; start a new session to check
again) in the tool error, on `session.start`, and in `euler --check-sandbox`,
and the session still fails closed. The host-level backend probe caches a
definite result (enforced, or a real unavailability such as `bwrap` missing,
user namespaces blocked, or Seatbelt refusing the profile) for the process
lifetime, but never caches a timeout, so the next session probes again. Deliberate unsandboxed
execution is the `full` execution profile (ADR 0026, below); it is never a
side effect of an approval mode, and no `/permissions` posture changes the
sandbox.

## Execution profiles

The execution boundary and the default approval policy are a named profile
(ADR 0026). This release enforces the `full`, `workspace`, and `locked`
presets.

`locked` is the boundary described above, unchanged: the no-network
sandbox, a private `/tmp` and `HOME`, and every command not statically safe
or grant-covered asks. Its fields are `sandbox: on`, `network: off`,
`writable_paths: []`, `tmp: private`, `caches: private`,
`credentials: private`, `approvals: always`, `escalation: not-allowed`,
`background_jobs: end-with-command` on Linux and `outlive` elsewhere, and
`reviewer: user`.

`workspace` is sandboxed with the friction removed. Its fields are
`locked`'s except `network: on`, `tmp: host`, `caches: writable`,
`approvals: escalation-only`, and `escalation: allowed`.

- **Network.** Bubblewrap shares the host network namespace; the Seatbelt
  profile allows outbound IP connections, DNS, and binding and accepting on
  loopback. Seatbelt allows Unix-socket connects only under the workspace,
  the private `HOME`, `/private/tmp`, and the host `TMPDIR`, so a local
  service such as PostgreSQL's `/tmp` socket stays reachable while the
  Docker socket and other host agents do not. Under
  `credentials: "private"` the launchd SSH agent socket is denied too; on
  Linux an `ssh-agent` socket under the host `/tmp` (`/tmp/ssh-*`) stays
  reachable by path, though `SSH_AUTH_SOCK` is dropped (Linux does not bind
  `/run`, where the Docker socket and desktop keyring agents live).
- **`/tmp`.** The host `/tmp` is writable and persistent (a bind on Linux;
  `/private/tmp` and the host `TMPDIR` on macOS, where `TMPDIR` is the
  host's), so a path the model reports exists outside the sandbox. On macOS
  writes into another session's `euler-seatbelt-*` scratch tree there,
  including renaming the tree itself, are denied.
- **`HOME`.** A private per-session directory, `<session dir>/home` for a
  session in the store; a session with no store (`--provenance <path>`)
  gets a fresh directory under `<euler home>/state/homes`, removed when the
  session ends. It is bound at its real path on Linux and allowed on
  macOS, persists between commands, and is never under `/tmp` or `TMPDIR`
  by construction. `XDG_CACHE_HOME` is `$HOME/.cache`; inherited
  `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, and `XDG_STATE_HOME` are dropped so
  their defaults follow `HOME`.
- **Caches.** The pip, uv, and npm caches, Cargo's `registry` and `git`,
  `GOMODCACHE`, and the Hugging Face hub cache are writable at their real
  paths when they exist on the host (a variable the user set wins over the
  default location), and their variable (`PIP_CACHE_DIR`, `UV_CACHE_DIR`,
  `npm_config_cache`, `GOMODCACHE`, `HF_HUB_CACHE`) points the toolchain
  there. A cache root is never bound, and a missing cache is created in the
  private `HOME`: an inherited cache variable (those above,
  `NPM_CONFIG_CACHE`, and `HF_HOME`) is dropped unless it names a bound
  cache.
- **Environment (ADR 0021 row C).** The parent environment is inherited,
  minus row C's hard denylist (`LD_PRELOAD`, `LD_AUDIT`, `LD_LIBRARY_PATH`,
  `DYLD_INSERT_LIBRARIES`, `BASH_ENV`, `ENV`, `GIT_CONFIG_COUNT`,
  `GIT_CONFIG_KEY_*`, `GIT_CONFIG_VALUE_*`, `GIT_CONFIG_GLOBAL`, and
  `GIT_SSH_COMMAND`, which only Euler sets), Euler's parent-control
  variables, Euler's own credentials (the built-in provider keys and the
  variables the Euler home's `providers.json` references in `api_key` or a
  header), secret-shaped variables, and the credential-agent variables
  `SSH_AUTH_SOCK` and `GPG_AGENT_INFO`. `HOME`, `TMPDIR`, `PATH`, the
  toolchain and cache variables, and `PAGER=cat GIT_PAGER=cat TERM=dumb
  NO_COLOR=1` are set.
- **Credential reads on macOS.** Seatbelt denies reads of `~/.ssh`,
  `~/.gnupg`, `~/.aws`, `~/.azure`, `~/.config/gcloud`, `~/.config/gh`,
  `~/.config/op`, `~/.kube`, `~/.docker/config.json`, `~/.netrc`,
  `~/.git-credentials`, `~/.npmrc`, `~/.pypirc`,
  `~/.cargo/credentials.toml`, `~/.terraform.d`, the keychains, browser
  profiles, and the Euler home (except the session's private `HOME`).
  Linux never mounts the real home.
- **`writable_paths`.** Each listed path must exist; it is bound writable at
  its real path on Linux and allowed on macOS. A missing path is a startup
  error naming the field.
- **`credentials: "pass"`.** For a user profile extending `workspace`:
  secret-shaped variables, `SSH_AUTH_SOCK`, and `GPG_AGENT_INFO` pass
  except Euler's own credentials (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
  `OPENROUTER_API_KEY`, `XAI_API_KEY`, and the variables `providers.json`
  references) and parent-control variables; row C's denylist still
  applies. On macOS the SSH agent socket `SSH_AUTH_SOCK` names is
  reachable. Each listed credential store that exists, except the keychains,
  browser profiles, and the Euler home, plus `~/.gitconfig` and
  `~/.config/git`, appears at the same path under the private `HOME`: a
  read-only bind on Linux (writable when `writable_paths` covers its real
  path), and on macOS a symlink to the real path with its read deny lifted.
  Cargo's credential mask is lifted too. The macOS keychain is never
  passed.
- **Approvals (row P).** Inside the sandbox, an `ask` for `fs-write` or
  `shell-exec` becomes `session-allow`, so `run_shell`, the `git_*` tools,
  and workspace-confined structured writes run without a prompt. This holds
  only while the session's sandbox backend is enforced; with no backend
  (`sandbox_backend: "host"`) those requests keep their configured mode
  (ADR 0021 row D). Dangerous commands, sensitive-path requests
  (structured tools, and `run_shell` commands whose text names a sensitive
  path), `agent-spawn`, the asks every profile keeps, and a configured
  `always-deny` (the Read only posture) are unchanged. For `run_shell` a
  path-shaped token (one with `/` or `.`) is checked against the full
  sensitive-basename rules, and a bare word only against exact key names
  (`id_rsa`, `id_ed25519`), so `gh secret list` does not ask. Under
  `euler exec` the profile wins over the `--auto-approve` tier for
  `fs-write` and `shell-exec` while the backend is enforced.
- **Background jobs.** As `locked`: on Linux they end with their command;
  on macOS a job that redirects its output outlives its command. Under any
  sandboxed profile Euler ends the jobs its commands left running when the
  session ends and on a profile change, without signalling a reused process
  group.
- **Escalation.** Under `escalation: allowed`, `run_shell` advertises an
  optional `escalate` object with a required `reason` (tools contract). The
  command then runs once on the host, outside the sandbox, with the user's
  own `HOME` and the environment `full` gives host commands (everything
  except Euler's own credentials and parent-control variables; see "The
  `full` preset"), after an ask:
  - An escalation always asks: no profile lifts it, static safety does not
    skip it, and the danger walk still runs first, so a flagged command
    offers no grant. Under a profile with `escalation: not-allowed`,
    `escalate` is a tool error naming the profile, before any prompt; a
    configured `always-deny` for `shell-exec` denies it as any command.
  - The ask offers allow once, or for the session for commands with the
    same first token, derived as a sandboxed command's session scope (the
    one token shared by every segment that is not statically safe). No
    session grant is offered or installed when any segment is a shell, an
    interpreter (`ssh` included; `python*`, `node`, `ruby`, `perl`, `awk`,
    ...), a command, privilege, or tracing wrapper (`sudo`, `env`, `xargs`,
    `timeout`, `strace`, ...), a container `run`, `exec`, `compose`,
    `create`, `start`, or `container` (`docker`, `podman`), a program named
    by path (`./deploy.sh`), a task or test runner or package manager that
    runs project code (`npm`, `npx`, `pnpm`, `yarn`, `make`, `just`,
    `gradle`, `mvn`, `pytest`, `uv run`, `uvx`, `cargo run`/`r`/`test`/`t`/
    `bench`/`rustc`, `go run`/`test`/`generate`/`tool`, `poetry run`, ...),
    a `pip install` of a local path, editable install, or requirements
    file, or `git` told to run a program. `cargo` given `--config` or `-Z`,
    `go` given `-toolexec`, `-exec`, or `-vettool`, and any of `uv`,
    `cargo`, `go`, `gh`, `pip`, `pip3`, `pipx`, `poetry`, `pdm`, `hatch`,
    `conda`, `docker`, or `podman` whose first argument is an option
    (which can hide the subcommand) also count. `git` counts when given
    `-c`, `--config*`, or `--exec-path`, for a subcommand that is not a
    known built-in (an alias, or `submodule`, `bisect`, `config`,
    `difftool`, `mergetool`, `filter-branch`, ...), and for an option
    naming a program
    (`--upload-pack`, `--receive-pack`, `push`/`archive --exec`,
    `clone -u`/`--template`, `init --template`, `grep -O`,
    `rebase -x`/`--exec`). `gh alias`, `gh extension`, and `rsync` given
    `-e`, `--rsh`, or `--rsync-path` count too. The panel then offers allow
    once and deny. An allow at any other scope (unscoped, project, user)
    covers the one request.
  - Escalation grants live in their own session store: an escalated grant
    never covers the sandboxed form and a sandboxed or durable grant never
    covers an escalation. They are not listed in `/permissions`, not
    persisted or resumed, and end when the session's profile is set.
  - The `permission.prompt` carries `escalated: true`; the `tool.result`
    records `sandbox_backend: "host"`, `escalated: true`, and the redacted
    `escalation_reason`. An unanswered or cancelled escalation follows the
    ordinary ask cancellation rule.
  - With `reviewer: guardian` an escalation is an ordinary ask: the
    guardian's brief says the command runs on the host and gives the
    reason, its allows stay once-scoped, and a reason longer than its
    field bound goes to a person.
  - An escalated command's background jobs end with the session and on a
    profile change, as sandboxed ones do.
- **Never-prompt denials.** When a decider that cannot ask anyone
  (headless `euler exec`) denies an ask in the root session, the tool error
  names the execution profile and what would allow the request instead of
  "denied by the user": `--permission-reviewer guardian` (unless the
  guardian is the reviewer, when it says whether the guardian abstained or
  could not review the request), and then a profile with
  `credentials: "pass"` for an escalation, since no profile runs one
  unasked, or `--profile` for another ask. For a dangerous command it says
  that only a person can approve it. Requests a configured `always-deny`
  refuses keep their message.

With the sandbox on, only these two boundaries are enforced: a profile
with `network: on` must keep `workspace`'s `tmp`, `caches`, and
`background_jobs`, and one with `network: off` must keep `locked`'s, with
no `writable_paths` and private credentials. `escalation` and `approvals`
are not part of either boundary: `escalation` may be `allowed` or
`not-allowed` with either, and `approvals` may be `escalation-only` or
`always` with either boundary, so
`{"extends": "locked", "approvals": "escalation-only"}` runs commands
without a prompt inside the no-network sandbox.
`approvals: never` belongs to the `full` preset and is refused with the
sandbox on. Changing `network` alone moves a profile to the other boundary
with the wrong `tmp` and `caches`, so the startup error names `network`.

`full` is unrestricted local execution. Its fields are `sandbox: off`,
`network: on`, `writable_paths: []`, `tmp: host`, `caches: writable` (with
the sandbox off these four are the host's and are not read),
`credentials: private` (not applicable with the sandbox off),
`approvals: never`, `escalation: not-allowed`, `background_jobs: outlive`,
and `reviewer: user`. Under `full`:

- `run_shell` and the `git_*` tools run on the host with the user's
  authority (`sandbox_backend: "host"`), and `session.start` records
  `command_approvals: "off"` as a separate fact.
- Every request inside the boundary is allowed, extension capabilities
  included, as the Full access posture did; the configured capability
  modes are unchanged and `always-deny` still denies. A command still
  asks when the dangerous-command list flags it, and a structured write to
  interpreter-honored configuration or to a privileged Euler file asks a
  human (all rules below that hold under every profile). Extension install and
  launch consent keep their own prompts. The `sensitive_path` flag does
  not ask: `read_file .env`, `cat .env`, and `python3 x.py .env` all run.
- The environment passes through to host commands except Euler's own
  credentials and parent-control variables (ADR 0026 resolved question 7).
  Euler's own credentials are the built-in provider variables
  (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`,
  `XAI_API_KEY`), the variables `providers.json` references in `api_key` or
  a header (`$NAME`, `${NAME}`), and any variable whose value is a credential Euler holds (the
  auth file's values and secrets its providers resolve). Other
  secret-shaped variables (`GITHUB_TOKEN`, cloud keys) reach commands and
  stay registered with the redactor; row C's denylist is not applied. This
  rule applies to every host-backend command, including an unsupported
  platform's.
- `write_file`, `edit_file`, and `apply_patch` accept a path outside the
  workspace (absolute, or relative and leaving it). The write is recorded
  with its canonical absolute path; no checkpoint covers it and `/rollback`
  cannot restore it (resolved question 22). Writes inside the workspace
  checkpoint as under every profile.
- A job that redirects its output outlives its command and the session;
  Euler lists the jobs it started that are still running at exit (resolved
  questions 6 and 12).

A user profile extending `full` may set `approvals: always` (today's
prompts, on the host); `escalation-only` and any tighter host boundary
field (`network: off`, a private `tmp`, `writable_paths`) are refused as
claims the host does not keep.

**Rules under every profile.** The dangerous-command list always applies
(see "Static command safety"). A structured write (`write_file`,
`edit_file`, `apply_patch`, or an `apply_patch` heredoc through
`run_shell`) whose named path, workspace form, or host target is
interpreter-honored configuration (anything under `.git`,
`.gitmodules`, `.gitattributes`, `.gitconfig`, `.config/git/config`,
`.npmrc`, `.netrc`, shell startup files, `.cargo/config.toml`) is marked
like a dangerous command: it asks a human, no grant covers it, and it never
reaches the guardian (ADR 0026 resolved question 15). A structured write
whose target resolves to a privileged Euler file is a privileged secret
and config edit (`secrets.md` "Config-Edit Guardrail") and is marked the
same way. The privileged Euler files are the ones that hold secrets or
decide trust: `providers.json`, the auth file (`<euler home>/auth.json` and
an `--auth-file` or `EULER_AUTH_FILE` override), `<euler home>/profiles.json`,
`<euler home>/user-grants.json`, the project-grant consent store
`<euler home>/project-grants/`, the project-context acknowledgments
`<euler home>/project-context/`, and the extension records
`<euler home>/extensions/state.jsonl`, `links.json`, and
`linked-activation.jsonl`. The rest of the Euler home (skills, sessions,
`preferences.json`, installed extension packages) follows the normal
profile rules; installed extension packages are inert today, and launch
consent is keyed to the reviewed fingerprint (ADR 0015). The match ignores
ASCII case, so on a case-insensitive filesystem a write to
`<euler home>/PROFILES.json` asks too. For the privileged Euler files only
the structured form is checked: a `run_shell` command that writes one
(`echo > ~/.euler/providers.json`) is not flagged and runs under the
profile's shell rules, so under `full` it runs without asking. An
interpreter (`sed`,
`awk`, `perl`, `ruby`, `python`, `node`, `patch`, `ed`) whose operands
mention a sensitive path sets the request's `sensitive_path` flag rather
than the dangerous flag, as a sensitive path named anywhere in a
`run_shell` command does (see "Sensitive-path shell ask"): it asks under
`approvals: escalation-only`, keeps the shell's configured mode under
`approvals: always`, stays grant-coverable and guardian-routable, and does
not ask under `approvals: never`.

**Approval override.** A profile's `approvals` acts per request through one
function, `execution_profile::profile_mode_override`, which the permission
gate applies after `always-deny`, the escalation rule (an escalated
`run_shell` asks where the profile allows escalation and is denied
otherwise), and the danger veto, and before the structured sensitive-path
escalation, for tool calls, companion loops,
extension capability approval, and idle contributors. `never` returns allow
for every request that reaches it, sensitive paths included.
`escalation-only` asks for a `run_shell` sensitive-path request and, while
the sandbox backend is enforced, turns an `ask` for `fs-write` or
`shell-exec` into `session-allow` (see `workspace` above). `always` keeps a
`run_shell` sensitive-path request at the capability's configured mode and
otherwise returns nothing, so the configured modes apply as before.

**Background jobs.** Each `run_shell` command runs in its own process group.
When its shell exits while other members of the group still run, Euler
keeps the leader exited but unreaped, which pins the group id. Jobs a
command started with the sandbox off keep running, are only reaped at
session end (or carried to the next session on `/new`), and are listed on
stderr at exit (`euler`, `euler tui`, `euler exec`). Jobs a sandboxed or
escalated command started are ended when the session ends and on a
profile change (Bubblewrap already ends sandboxed ones with their
command). A descendant that moves itself into another process group
is not tracked.

- **Presets.** `full`, `workspace`, and `locked` are reserved names.
- **`<euler home>/profiles.json`.** Optional. `defaults` holds `interactive`
  (`euler`, `euler tui`), `headless` (`euler exec`), and `subagent`
  (`inherit` or a profile name); each must name a preset or a defined
  profile. `profiles` maps a name (letters, digits, `-`, `_`; not a preset
  name or `inherit`) to an object with a required `extends` naming a preset
  and any profile fields. Listed `writable_paths` (absolute or `~/`) add to
  the preset's; every other field replaces the preset's value. Unknown
  fields, duplicate keys, unknown values, and contradictory combinations
  (`approvals: never` with `escalation: allowed`, `sandbox: off` with
  `escalation: allowed`, and `credentials` set on a sandbox-off profile) are
  startup errors naming the file and field. A missing file means the built-in defaults; an unresolvable
  Euler home reads no file and says so on stderr.
- **Selection.** `--profile <name>` on `euler` and `euler exec`, then
  `/profile` in the TUI, then the context default in `profiles.json`, then
  the built-in default: `full` interactive, `workspace` headless. An unknown
  name is a startup error listing the defined profiles.
  `--permission-reviewer` overrides the profile's `reviewer`. Child agents
  and companions run inside the session's boundary and approvals:
  `defaults.subagent` is validated but not applied yet, so a child never
  runs looser than its parent, and when it names a profile rather than
  `inherit`, startup prints a warning saying so.
- **First run.** An interactive launch without `--profile` whose
  `profiles.json` has no `defaults.interactive` asks once, on a terminal
  (stdin and stderr both terminals), which preset to use: `full`
  (preselected), `workspace`, or `locked`, each with a one-line boundary.
  The choice is written to `defaults.interactive`, keeping the rest of the
  file, and never asked again. Without a terminal nothing is asked or
  written and the built-in default applies. End of input at the prompt
  quits without starting a session.
- **`--auto-approve`** is accepted under every profile. Under
  `approvals: always` its tiers configure the gate as before. Under any
  other profile commands and writes inside the boundary already run, so the
  flag is a no-op; when it was given, stderr says so in one line naming the
  profile. The implicit read-only tier never tightens a profile.
- **Profile change.** `/profile <name>` (or the `/profile` picker) switches
  the session between turns: it rebuilds the subprocess boundary and the
  structured-write scope, applies the new profile's approval override and
  escalation setting, clears escalation grants, and records
  `session.profile_changed`. Jobs started under a sandboxed profile,
  escalated ones included, are ended first; jobs started with the sandbox
  off keep running.
  `/profile default <name>` writes `defaults.interactive` for new sessions.
- **Project overlay.** A repository may hold `.euler/profile-overlay.json`
  with any profile field except `reviewer` and `extends`. It applies to
  whichever profile resolves, including one named by `--profile`, and each
  field it sets must be at least as tight as the resolved value, or startup
  fails naming the file and field; duplicate keys are errors too. Tighter
  is: `sandbox` off < on (`bubblewrap` and `seatbelt` order as on);
  `network` on < off; `writable_paths` a subset; `tmp` host < private;
  `caches` writable < private; `credentials` pass < private; `approvals`
  never < escalation-only < always; `escalation` allowed < not-allowed;
  `background_jobs` outlive < end-with-command. Tightening needs no trust
  acknowledgment; stderr names the overlay at startup. An overlay that sets
  only `network: off` over `workspace` is tighter but is still a startup
  error naming `network`, because no enforced boundary has network off with
  the host `/tmp` and writable caches; set `tmp: private` and
  `caches: private` with it.
- **Enforcement.** A resolved profile this build cannot deliver is a
  startup error naming the profile and field: with the sandbox on, one
  neither sandboxed boundary delivers (see above, or a `sandbox` backend
  name other than this host's); with it off, any field the `full`
  paragraph above refuses. When the project overlay is what makes the
  profile unenforceable (for example `{"network": "off"}` on `full`), the
  error names the overlay and suggests `--profile locked`. On a host whose
  sandbox backend is missing, sandbox-requiring tools fail closed; the
  headless startup failure of ADR 0026 section 5 is not implemented yet.
- **Resume.** Resume resolves the profile as a new launch does, then
  compares it with the latest profile the log recorded (`session.start`, a
  later `session.resumed`, or a `session.profile_changed`). If the resolved
  profile is looser on any field, the recorded profile applies unless
  `--profile` was passed; otherwise the resolved one applies. A kept
  recorded profile also gets the project overlay, and one this host cannot
  enforce (for example a Linux `background_jobs: end-with-command` resumed
  on macOS) is a startup error asking for `--profile`, never a
  substitution. A bad `profiles.json`, overlay, or `--profile` fails
  resume before anything is appended to the log. stderr shows any change,
  naming the fields that changed; the TUI's in-app `/resume`, which has no
  `--profile`, applies the same rule with the interactive default and shows
  the change as its notice. A log without a
  recorded profile resolves as a new session; no profile is inferred. A
  malformed recorded profile fails resume closed.

## Approval Modes

A capability decision is one of:

- `ask` — prompt the user at use time.
- `session-allow` — allow for the current session/scope.
- `always-deny` — deny without prompting.

Permission prompts and decisions are session events and are recorded in provenance. Privileged secret/config edits always require explicit approval even if broader write access was granted.

Cancelling an operation while it awaits an `ask` is neither an allow nor a
deny: Euler records no permission decision or grant, closes the owning
operation through its canonical cancellation path, and invalidates that
prompt's reply route so a late answer cannot decide a later request.

## Permission reviewer (guardian)

The session has one **permission reviewer** for uncovered `ask` decisions:
`user` (default — the configured decider, e.g. the TUI approval panel) or
`guardian` (ADR 0011). With `guardian`, an uncovered ask is reviewed by a
flag-gated companion agent spawned with an **empty capability set** and a
one-round, zero-tool budget, on the same decision channel the human would
use. Rules (normative; enforced in code, not only in the guardian prompt):

- Verdict shape: `{risk_level: low|medium|high|critical, user_authorization:
  unknown|low|medium|high, outcome: allow|deny|abstain, rationale}`.
- Thresholds: low/medium risk → allow; high risk → allow only when
  `user_authorization` ≥ medium; critical → deny, not overridable.
- Fail closed: guardian spawn failure, companion failure, or an unparseable
  verdict is a deny.
- `abstain` falls back to the configured decider (the human). `deny` is
  final for the ask; it never falls back.
- Guardian allows are once-scoped; the guardian never installs session or
  project grants. Requests covered by existing grants run under those grants
  and are not guardian-reviewed.
- The guardian never adjudicates a project-tier extension config write
  started by a model tool (ADR 0022): that ask always goes to the human
  decider. See `config-write` below.
- The guardian adjudicates only requests it can see **verbatim** (ADR 0011
  amendment): if the command was truncated at the retention bound, or the
  task brief's own field bound would alter the command or path, the guardian
  is never consulted — the ask goes directly to the human decider
  (fail-to-human, enforced in code).
- Denials inject guidance into the failed tool result telling the model not
  to work around the block. Three consecutive guardian denials in one turn
  interrupt the turn (circuit breaker).
- Every guardian decision is a `permission.decision` event tagged
  `decision_source: "guardian"` with the verdict fields (events contract);
  automated decisions are always distinguishable from user decisions.
- Headless `exec`: auto-approve tiers leave no capability in `ask` mode, so
  configuring the guardian returns `fs-write` and `shell-exec` to `ask` —
  every use is guardian-reviewed, overriding the tier for those two
  capabilities in both directions (read-only's always-deny and
  trusted-local's session-allow). A guardian abstain then hits the headless
  decider's unconditional deny (fail closed; no prompt exists).

## Extension capability approval

A command descriptor's `required_capabilities` is a *declaration*, never a
grant. On surfaces that can ask the user (the TUI), one extension command
gets one operation-level prompt listing every uncovered static capability:
explicit `session-allow` grants silently, explicit `always-deny` rejects the
whole operation before a prompt, and remaining `ask` or unconfigured
capabilities form the batch. An allow or deny applies to the operation as a
whole, but the ledger records one `permission.decision` per listed capability,
all parented to the shared `permission.prompt` and carrying `extension_id` and
`command`. Session-scoped approvals cover later runs (covered requests run
under the original decision, with no fresh record). Batch approval offers only
once and unscoped session scope; it never turns a multi-capability operation
into a project or durable user rule. Piped headless runs cannot prompt (stdin
is the command protocol): there, explicitly invoking a named command grants
its declared capabilities for that run, announced on stderr — visible, never
silent.

The extension host records each resolved registration grant or denial as a
`static-grant` decision before it reports registration success or the ordinary
capability-denied result. That provenance is mandatory: an append failure is
an infrastructure failure, not an implicit grant or denial, and a failed
allowed-decision append leaves no partially registered extension or command in
the host. A command cannot catch an SDK-level denial/provenance error and turn
that host failure into success; the host checks its own failure latch after
the command returns. Diagnostics are emitted only after the decision append
succeeds.

Terminal-idle contributions are implicit lifecycle work and therefore never
open this operation prompt. Every required capability must be
`session-allow`, or be `ask`/unconfigured and covered by an existing grant.
`always-deny` always rejects. Missing standing authority stops the contribution
before its command starts; the rejected stop is recorded without an `error`
event. Explicit model tools retain the ordinary operation-level prompt above.

Root request ticks (ADR 0019) use the same standing-authority rule and never
open an operation prompt. Missing authority latches only that tick contributor
for the remainder of the live Session; later contributors and the root request
continue. During an authorized tick, the ordinary command descriptor remains
the sole capability source. The request boundary's injected provenance cutoff
narrows an already-authorized `provenance-read` query; it grants no capability
and cannot be changed by the extension. Cancellation and mandatory permission
or command provenance failures retain their ordinary session-wide semantics.

## Install consent (extension distribution)

Installing an extension involves three distinct consents that must never be
merged into one record:

1. **Install consent** (this section): agreeing to fetch a pinned source and
   run its declared build steps. An install-time build is arbitrary code
   execution and is treated as such (ADR 0015).
2. **Launch consent**: the existing per-extension enable step that echoes the
   exact argv (managed-process runtime contract).
3. **Capability approval**: the per-command operation prompts above, at use
   time.

Install consent is recorded against the pinned content fingerprint of the
source. A changed fingerprint — any content movement, not only a manifest
change — requires fresh consent, because declared builds execute
source-controlled build logic. Consent for one fingerprint never covers
another.

The consent card must show, before anything runs: the source and resolved
pin, every declared build argv, the toolchains it requires, the extension
ids provided with their capability envelopes, and (on update) the manifest
diff when the manifest changed. Consent decisions are recorded as provenance
events like every other permission decision.

Presentation has two profiles, a user-level setting:

- `standard` (default): one combined card covering fetch, build, and
  execution grant; a single approval completes the install.
- `granular`: separate confirmation at each stage (fetch, build, grant) for
  users who want to inspect intermediate results.

Both profiles gather the same consents; the profile changes prompt
granularity, never authority. **First contact is never automatic**: no
profile, tier, posture, or project trust may skip consent for a fingerprint
the user has not approved on this machine. Automation is permissible only
for re-materializing an already-consented fingerprint (for example,
reinstalling on a store wipe).

## Scoped Grants

Capability modes are the coarse gate. **Scoped grants** sit above `ask`: when a
request matches an active session, project, or user grant, the gate allows it
without re-prompting. `always-deny` still denies even if a grant exists.
`session-allow` remains capability-wide and does not require a grant match —
except for a request the danger walk flags or a sensitive path, which
`mode_for_request` escalates to `ask` (see "Static command safety").

Grant lifetime and pattern:

| Scope | Lifetime | Pattern |
|-------|----------|---------|
| `once` | this request only | none |
| `session` | current session | optional `ScopePattern` |
| `project` | workspace project config | optional `ScopePattern` |
| `user` | every session, every project (durable) | `ScopePattern` (prefix rule) |

`ScopePattern` is an opaque bounded string:

- **Unscoped** (empty pattern): whole capability (legacy `AllowSession`).
- **`shell-exec`**: command first token (`cargo`, `git`).
- **`fs-write`**: workspace-relative directory prefix (typically the path's
  top-level directory). Matching is prefix: `src` covers `src` and `src/lib.rs`.

Derivation of a display/default pattern from a live request (first token, top
level dir) is a caller concern; core stores and matches opaque patterns.

### Decisions

A decider may return:

- allow once (`once`);
- allow session-scoped (`session` + pattern, possibly unscoped);
- allow project-scoped (`project` + pattern);
- allow user-scoped (`user` + pattern — a durable prefix rule);
- deny;
- deny with **instruction** text — guidance for the exact active run blocked
  by this permission request.

In the interactive UI, deny-with-instruction is a narrow permission response,
not ordinary composer submission. The UI captures the permission's active
durable run identity when the ask opens and admits the instruction only as
same-run steering for that identity. It never infers steering versus follow-up
from timing, and it never converts the instruction into a later run when the
blocked run has moved or terminalized. If the run identity cannot be proven,
the prompt and instruction remain visible and no permission reply is sent.
The instruction may be acknowledged to the decider only after that exact
steering enqueue is durable.

Legacy verdicts map as: `Allow` → `once`, `AllowSession` → `session` unscoped,
`Deny` → deny without instruction.

### Project grants

Project grants persist under the workspace at `.euler/grants.json` (see
`docs/contracts/persistence.md`). Installing a project grant is an explicit
config write: the approval that grants project scope **must** be recorded as a
`permission.decision` event with `grant_scope: "project"` (and pattern when
set). Silent project-config mutation is forbidden.

The workspace file is repo-controlled content and is **never authority on its
own**. A project grant is active only when it appears in BOTH the workspace
file AND the user's consent store — a per-root file under the user-owned
euler home (`<home>/project-grants/<sha256(canonical root)>.json`) written
when the user approves the grant on this machine. A cloned repository that
ships `.euler/grants.json` therefore grants nothing until this user approves
each entry; deleting either side deactivates the grant. Sessions opened
without a resolvable consent directory disable project grants entirely —
reads and writes both fail closed.

A malformed or unreadable grants file — on either the workspace side or the
consent side — fails closed exactly like a missing consent directory: no
project grants load, project-grant writes are disabled for the session, and
a reload over a corrupted file clears any previously active project grants.
A corrupt file is never broader authority than a missing one. Load rejects
oversize files, unknown versions, unknown capabilities, and control-bearing
patterns; unknown JSON fields are ignored; duplicate entries collapse to
one.

### User rules (durable prefix rules)

User rules are the "don't ask again for commands starting with `cargo`"
tier: they persist across sessions AND projects, in a single store at
`<home>/user-grants.json` under the user-owned euler home (same atomic-write
and 0600 discipline as the other grant stores). Unlike project grants they
need **no consent intersection** — the store is user-authored in the user's
own home and is never repo-controlled content, so there is no second party
whose entries could preseed authority. Sessions opened without a resolvable
user grant dir disable user rules entirely — reads and writes both fail
closed. A malformed or unreadable user-grants file is treated the same way:
no rules load, the store stays unloaded (durable installs fail), and a
corrupt file never yields broader permissions than a missing one.

Installing a user rule is an explicit durable-config write and **must** be
recorded as a `permission.decision` event with `grant_scope: "user"` (and
the pattern). Silent user-store mutation is forbidden.

Pattern semantics for `shell-exec` are a **command prefix over the parsed
first token** — a rule `cargo` covers any command whose first token is
`cargo`, exactly as session/project token scopes match. Coverage composes
per segment (issue #78, see "Static command safety"): a prefix rule covers
a compound command iff it parses into plain segments and every segment is
either statically safe or prefix-covered. Unparseable commands (redirects,
substitution, subshells) are never covered and always re-ask.

A run covered by an existing user rule executes under that original
decision: no fresh `permission.decision` event, and the tool result carries
`grant_source: "user"` so the ledger can tag the run `· user rule`.

The approval panel offers the rule as `u  Allow <prefix> * always`,
alongside once/session/project — and only when it is honest: a prefix must
derive from a simple shell command AND the session must hold a loaded user
store. Unscoped or compound asks never show the option, and a session
without a resolvable user grant dir hides it entirely.

### Static command safety

Core performs static analysis of `shell-exec` command lines
(`euler-core/src/command_safety.rs`). Execution is `sh -c <command>`, so the
analysis reasons about the whole line. It parses with **tree-sitter-bash**
and uses **two parsers over that syntax tree**: one conservative parser that
may prove a command safe, and one permissive walk that may only find danger.
Neither can do the other's job, and a lexical approximation of shell can do
neither: redirections glued to a word, `${...}`, `$'...'`, brace expansion,
and here-documents all evade a tokenizer while an AST reports them as
distinct nodes.

**Parser 1 — prove-safe grammar (conservative).**

- **Parsing.** The parse must succeed with no error node, and every node in
  the tree must be one of `program`, `list`, `pipeline`, `command`,
  `command_name`, `word`, `string`, `string_content`, `raw_string`,
  `number`, `concatenation`, joined only by `&&`, `||`, `;`, `|`. Anything
  else — a redirection, here-document, here-string, substitution,
  expansion, subshell, brace group, control flow, background `&`, variable
  assignment — makes the command **not statically analyzable**. Such
  commands are never auto-approved and never covered by scoped grants; they
  fall to the ask path. False negatives cost a prompt; false positives are
  forbidden.
- **Literal words.** Every word must be exactly what the binary will
  receive: no `* ? [ ] { } ~ $ ` \ ^ #`, and no word beginning with `=`.
  A word the shell may rewrite is never proof, for any binary. A backslash
  ANYWHERE in the line makes it unprovable: `tree-sitter-bash` treats a
  backslash-newline as whitespace while `sh` treats it as a line
  continuation, so `cat .en\<newline>v` parses as three harmless words and
  executes as `cat .env`.
- **Options are an allowlist.** Each binary in the read-only set declares
  the exact option spellings it accepts. An unknown option, a GNU long
  abbreviation (`--recu`, `--dereference-rec`, `--fol`), an attached short
  value (`-oout.bin`, `-i.env`, `-n50`), or a bundle containing an unlisted
  letter (`-rS`, `-Do`) is simply not provable. A denylist has to enumerate
  every harmful spelling on every platform and misses one; this polarity
  cannot.
- **Read-only set.** `true false pwd whoami id uname echo expr seq which
  cat head tail wc ls nl paste rev cut tr stat uniq grep base64`, plus
  `find` (only the enumerated read-only predicates, so `-exec`, `-delete`,
  `-L`, `-follow` are rejected by omission) and `sed` (only the print-range
  form `sed -n Np [file]`). Recursive readers are excluded: no `grep -r`, and no
  `rg` at all, since `rg` recurses by default. A tree walk reads files the
  per-operand sensitive check never saw — `grep -r PASSWORD .` printed
  `.env`, and `rg PASSWORD` reads `deploy.pem`, `id_rsa`, and
  `credentials.json`. Under the `workspace` profile (row P) the sandbox runs
  them without a prompt; under `locked` they prompt. `uniq` accepts at most one operand, because the
  second operand is an output file it truncates. Binaries outside the set
  are never provable — `sort` (`-o` writes a file), `tee`, and every
  interpreter included.
- **`git` is not in the set**, even for `status`/`log`/`diff`/`show`. Those
  subcommands execute repository-controlled programs through
  `diff.external`, `core.fsmonitor`, `core.pager`, and clean and smudge
  filters, all selected by `.git/config` — the very file audit F34 showed a
  "read-only" command could write. Proving `git` safe needs the sandbox,
  not a parser: under the `workspace` profile (row P) the `git_*` tools run
  without a prompt inside the sandbox.
- **`cd` is not in the set**: it moves the directory later commands resolve
  against while confinement keeps checking the root the command started in.
- **Wrappers.** `[sh|bash|zsh] -c|-lc <script>` is provable only by
  recursively proving `<script>`, depth-capped at eight. No other wrapper
  form (`env`, `/bin/sh`, extra flags) is provable.
- **Workspace confinement.** Read-only is not harmless: `cat
  ~/.aws/credentials` writes nothing and still exfiltrates. Every operand
  and every option value — there is no exempt position, so a regex operand
  is checked like any other word — must stay inside the workspace root the
  command executes in: an existing path must canonicalize (symlinks
  resolved) under the canonicalized root; a non-existing argument must be
  relative with no `..` component and no leading `~`.
- **Sensitive paths.** One denylist, used by static shell analysis and by
  the fs-tool "Sensitive-basename ask" below (one list, not two), applied
  to the literal spelling AND to the canonicalized resolution — so an
  innocently named in-workspace symlink cannot read through it:
  - anything with a `.git` path component — the directory, the worktree
    pointer file, and everything under it;
  - `.gitmodules`, `.gitattributes`, `.gitconfig`, `.npmrc`, `.netrc`,
    `.cargo/config.toml` (and `.cargo/config`), `.config/git/config` and
    `.config/git/attributes`: configuration an
    interpreter or build tool honors on its next run;
  - shell startup files: `.bashrc`, `.bash_profile`, `.bash_login`,
    `.bash_logout`, `.profile`, `.zshrc`, `.zshenv`, `.zprofile`,
    `.zlogin`, `.zlogout`;
  - `.env*`, names containing `secret`/`credential`, `id_rsa`,
    `id_ed25519`, `*.pem`, `*.key`.
- A command is **statically safe** iff it parses under this grammar AND
  every command in it is read-only with rule-conforming options AND every
  operand and option value is confined and non-sensitive AND the danger
  walk below finds nothing.

**Parser 2 — find-danger walk (permissive).** A separate function visits
*every* command node in the tree — inside control flow, command and process
substitutions, expansions, redirection arguments, and the scripts carried by
`sh -c`, `eval`, `env -S`, and `trap` — and unwraps `sudo`, `doas`, `su`,
`env`, `nohup`, `time`, `timeout`, `nice`, `ionice`, `chrt`, `stdbuf`,
`setsid`, `flock`, `xargs`, `command`, `builtin`, and `exec`. `command`'s
POSIX options `-p`, `-v`, and `-V` are recognized, so a `command -v X`
lookup is not itself danger (its operands are still walked as a command).
It **must
never be used to prove safety**. It fails closed on a parse error, on a
dynamic command name, on a dynamic argument of a dangerous or wrapper
command, on an unrecognized `env` option, and past a wrapper depth of eight;
both walks are iterative and node-bounded, so nesting cannot exhaust the
stack.

Its danger predicate is an intentionally **extensible table** of commands
that destroy data with no undo: `rm` with `-r`/`-R`/`-f`/`--recursive`/
`--force` or any GNU abbreviation of those (`run_shell` closes stdin, so
`rm -r` never gets its interactive confirmation and is as destructive as
`rm -rf`), `find` with `-delete`/`-exec`/`-execdir`/`-ok`/`-okdir`, `git
clean -f`, `reset --hard`, `rm -f`, `checkout` that forces, marks paths with `--`, or
restores an unmistakable path operand (branch creation and ref-shaped
names like `feature/x` stay clean),
`restore <path>`, `branch -D` (and `-d` with `-f`), `stash drop`/`clear`,
`shred`, `truncate`, `wipefs`, `dd of=`, and the `mkfs*` family. An
interpreter (`sed`, `awk`, `perl`, `python`, `ruby`, `node`, `patch`, `ed`)
whose operands, program text included, mention a sensitive path is not on
this list: it sets the request's `sensitive_path` flag instead (ADR 0026
resolved question 15; "Execution profiles" above). "Mention" splits the
operand into path-shaped tokens and applies the sensitive list to each, so
`open(".env")` counts and `os.environ` does not. Command names are matched
case-insensitively, because the default macOS filesystem is.
It also flags **writes an interpreter later honors**: a redirect target or a
`cp`/`mv`/`tee`/`install`/`ln`/`rsync` destination on the sensitive list, an
in-place edit (`sed -i`, `perl -pi`, `ruby -i`, `awk -i inplace`, `patch
FILE`, `ed FILE`) of a file on it, so
`printf x > .git/hooks/pre-commit` and `echo x > .bashrc` ask exactly as
`write_file` on those paths does (audit F34, write side), including when
the redirection hangs off a compound statement
(`{ printf x; } > .bashrc`).

An extension-declared `shell-exec` request is walked like `run_shell` when
the invocation names a command, and treated as unreadable — never
grant-covered, always prompted — when it does not. The walk runs before the
capability mode is consulted, so a blanket `session-allow` does not skip
it.

A **truncated** command still blocks scoped grant matching, but the walk
itself reads the full command text, so an ordinary multi-kilobyte command is
not flagged merely for being long.

A flagged command is **never auto-approved, in any mode** (ADR 0021 decision
D), and is never routed to the guardian reviewer: it must reach a human. No grant covers it — scoped or unscoped, session, project, or user —
and every capability mode short of `always-deny` is escalated to `ask` for
that one request, so a forced `rm` prompts even under a blanket
`session-allow`. `always-deny` still denies without prompting, and a
never-prompt decider denies on the same path. The approval panel offers no
grant for it: it shows only allow once and deny, with a muted line saying why
(see `docs/contracts/ui.md`, Permissions UI).

**Auto-approval under `ask`.** When `shell-exec` is in `ask` mode, a
statically-safe command runs without a prompt. The run is recorded as a
fresh `permission.decision` with `mode: "static-safe"`, `allowed: true`,
`grant_scope: "once"`, parented to the `tool.call` — allowed-once
semantics; **no grant is installed** and no prompt event is emitted. The
static-safe check precedes grant-coverage matching, so the ledger
attributes such runs to the analysis rather than to an unrelated grant.
Static safety never bypasses `always-deny`, and a capability denial earlier
in the same turn still short-circuits the tool call. A command truncated at
the retention bound is never analyzed (and never matches scoped grants):
any permission decision must be based on exactly what will execute, or fail
closed to the ask path.

**Ledger treatment.** The decision event keeps provenance honest, but the
transcript does not render it as a standalone record — the
standalone-record-per-call noise is exactly what covered grants eliminated
(review v2 §8). Instead the `tool.result` carries `static_safe: true` and
the tool header shows a dim `· safe` tag, matching the covered-grant
`· session grant` header treatment.

**Segment-aware grant coverage.** Scoped `shell-exec` grant matching uses
the same segment analysis: a command is covered iff it parses into plain
segments and EVERY segment either has a granted first token or is
statically safe — with at least one segment actually matching a granted
token (an all-safe command is attributed to the static-safe path, never to
an unrelated grant). Tokens pool within one store: `cargo test && npm run
lint` is covered when the session store (or the project store) grants both
`cargo` and `npm`; a compound whose segments straddle the two stores falls
back to ask so the ledger's single `grant_source` tag stays honest.
Unparseable commands are never covered. The approval panel offers a token
scope only when the gate could actually grant it for the live command:
when every non-statically-safe segment shares one first token, that token
is offered; otherwise (distinct unsafe tokens, unparseable command) only
allow-once / unscoped / deny are offered.

### Revocation and listing

Core exposes list and revoke APIs over session, project, and user grant
stores for surfaces such as `/permissions`. Revoking a project grant rewrites
`.euler/grants.json`; revoking a user rule rewrites `<home>/user-grants.json`.
Child-agent capability attenuation remains exact flat subset semantics;
scoped grants do not change child capability sets.

`provenance-read` gates host-mediated bounded provenance queries. It is not
raw filesystem read access. The v0 pull-based event feed uses this same
capability because it reads the accepted durable provenance prefix through the
same bounded host API. The current process-local wake primitive is not exposed
to extension or child-agent code and therefore adds no new capability. Future
live push/background subscription may require a separate capability if it
exposes materially different privilege or timing semantics.

`diagnostics-read` gates host-mediated bounded reads of the current session's
diagnostics log lines. It is not raw filesystem read access.

`artifact-write` gates host-mediated extension artifact writes. It is not raw
filesystem write access and does not permit arbitrary extension state writes.

`extension-state` gates `HostApi::state_dir`, which returns the calling
extension's session-private directory. The directory is namespaced by
extension id, but it is intentionally one read/write scope: native and
managed-process extensions are trusted code rather than OS-sandboxed peers, so
claiming separate host-enforced read and write authority after returning a raw
path would be dishonest. This does not grant workspace file access.

`agent-record` gates host-mediated immediate child-agent completion records.
It lets an extension command ask the host to append one validated `agent.spawn`
event followed by one terminal `agent.result` event through the owning session
writer. It is not live model invocation, a scheduler, a durable background
worker, an observer daemon, or an arbitrary child-process launcher. Child
capabilities use the same exact flat subset semantics as core child agents:
the child set may be empty or equal to the command grant, duplicates are
normalized, and escalation fails before the host appends any agent events.

`agent-spawn` gates host-mediated live child-agent execution
(`HostApi::spawn_agent`): the host runs one child session to completion for a
validated `AgentTask` and records the same `agent.spawn`/`agent.result` pair
the session companion path records. Everything `agent-record` is not, this is
also not — except live model invocation of exactly one child per call. Child
capability attenuation uses the same exact flat subset semantics; the child
set must be a subset of the invoking command's granted capabilities. Children
do not receive an extension host and cannot spawn (depth one in v0.1).
`spawn_agent` is synchronous per call. `HostApi::spawn_agents` (v0.2) runs a
batch of single-round, tool-free, empty-capability child briefs concurrently
under the same gate and per-command quota; see the multi-agent contract for
its determinism and event-ordering invariants. Headless
`euler exec --auto-approve` tiers set `agent-spawn` to session-allow in both
tiers: children's own tool calls remain gated by the parent's tier-configured
modes, so allowing spawn cannot escalate beyond the tier.

Extension event-feed checkpoints are private extension state:

- `HostApi::load_event_feed_checkpoint` requires `fs-read`.
- `HostApi::store_event_feed_checkpoint` requires `fs-write`.
- `fs-write` permits the host to perform internal directory reads needed for
  safe checkpoint replacement, quota checks, and file-type validation, but it
  does not grant the extension read access to existing checkpoint contents.
- Extensions that use durable cursors must declare these capabilities even
  when they access checkpoints only through host APIs. The authority remains session-private checkpoint state, not
  arbitrary path access.

- Native extension manifests declare the maximum capability envelope for the
  extension. Each command declares its own required-capability set on its
  `CommandDescriptor`; that is the sole source of command capabilities. An
  empty set means no capabilities — there is no inheritance from the manifest
  envelope. A command's set must be a subset of the manifest envelope,
  enforced at registration.

`context-slot` gates host-mediated extension context slot updates. It permits an
extension command to append bounded `context.slot.updated` events for slots
namespaced to that extension id. It does not grant raw provenance reads,
arbitrary canvas control, or cross-extension slot writes.

`config-write` gates `HostApi::write_extension_config`, which replaces the
calling extension's own project or user config document (ADR 0022; extension
SDK contract, "Extension Config Documents v0"). Reading those documents needs
no capability: config is data, not authority. A user-tier write, and a
project-tier write from a `user` command the human invoked, follow the normal
modes: the command's approved `config-write` covers them. A project-tier
write started by an agent-invoked model tool always asks a human, once per
write: the host asks the configured decider directly, so no session
allowance, `session-allow` mode, or session, project, or user grant covers it,
the guardian never adjudicates it, and no grant is installed whatever scope
the human picks. A decider that cannot prompt denies it, so unattended runs
(headless `euler exec`) cannot commit a shared config change; they can still
propose one as an artifact. A configured `always-deny` denies without a
prompt. Lifecycle hooks (terminal idle, request tick, observer) never prompt,
so their project-tier writes are refused even with standing authority. Writes
whose value matches a known secret are refused before any prompt.

`plan-presentation` gates host-mediated typed plan presentation. It permits an
extension command to append bounded, attributed `plan.update` events. It does
not grant arbitrary event emission, context-slot writes, direct canvas
admission, or authority over another extension's workflow state.

`fs-read` defaults to `session-allow`: read tools execute without prompting,
but every execution records a permission decision event. Root-session
`extension-state`, `context-slot`, and `plan-presentation` also default to
`session-allow`; each is extension-attributed and bounded as described above.
Root-session `artifact-write` also defaults to `session-allow`: the host
writes the bytes content-addressed into the calling extension's
session-private artifact directory, records an attributed `extension.artifact`
event, and grants the extension no filesystem authority.
`fs-write`, `shell-exec`, and root-session `agent-spawn` default to `ask`;
the `workspace` profile runs an unflagged `fs-write` or `shell-exec` ask
without a prompt ("Execution profiles", row P).
Unconfigured capabilities remain `always-deny`; child-agent gates start
deny-all and inherit only their explicit attenuated envelope. Headless
auto-approve tiers override these root defaults with the explicit mapping
above; the root-session extension defaults (`extension-state`,
`context-slot`, `plan-presentation`, `artifact-write`) are outside that
mapping and stay `session-allow`.

**Sensitive-basename ask.** A blanket `session-allow` never covers a tool
request whose path names a categorically sensitive file. When a path-taking
tool request (`read_file`, and the write tools' paths equally) targets a
path on the sensitive list — the same list static command safety enforces
(anything under a `.git` component, git/npm/cargo/shell configuration,
`.env*`, names containing `secret`/`credential`, `id_rsa`, `id_ed25519`,
`*.pem`, `*.key`; see "Static command safety" above for the full list) —
the gate escalates that single request from `session-allow` to `ask`. The check applies to the literal argument AND its
canonicalized workspace resolution, so an innocently named symlink cannot
evade it. The escalated ask flows through the ordinary permission braid: a
covering session/project/user grant satisfies it silently (the tool result
carries the usual `grant_source` tag), otherwise a `permission.prompt` is
emitted whose reason names the file and why it is sensitive, and the
configured decider resolves it — allow-once, a session grant (which then
covers later sensitive reads), or deny. This is an ask, not a deny:
`always-deny` is never weakened and no new denial surface exists. Both
permission gates — root session and companion loop — apply the same
escalation. In headless auto-approve tiers the escalated ask reaches the
fail-closed headless decider and is therefore denied unless a durable grant
covers it. Under an execution profile with `approvals: never` (`full`) the
escalation does not happen, and a write tool's path on the
interpreter-honored part of the list is a dangerous request instead (see
"Execution profiles").

**Sensitive-path shell ask.** A `run_shell` request also carries the
sensitive-path flag when its command text names a path on the same list,
anywhere (operands, nested scripts, redirections, here-documents), with `@`
read as a separator so `curl -d @.env` counts, or when an interpreter's
operands mention one (`python3 x.py .env`). Under `approvals:
escalation-only` (`workspace`) the flag asks like the structured-tool
escalation above, grant-coverable and guardian-routable; under `approvals:
always` (`locked`) it leaves the capability's mode alone, as before
profiles; under `approvals: never` (`full`) it does not ask.
