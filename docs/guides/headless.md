# Headless runs

`euler exec` is the unattended mode for long-horizon runs and fleets. Keep the
prompt short, put the real assignment in a file, and monitor provenance.

## Exec anatomy

```sh
euler exec \
  --provider openrouter \
  --model anthropic/claude-sonnet-5 \
  --reasoning-effort large \
  --extensions none \
  --max-tool-rounds 100 \
  --provenance ./runs/job-001.jsonl \
  --auto-compaction stubs \
  --compaction-budget-bytes 640000 \
  'Read BRIEF.md in this directory and carry it out fully.'
```

Flags:

- `--provider <id>` / `--model <id>` choose the route.
- `--reasoning-effort xsmall|small|medium|large|xlarge|max` sets Euler's reasoning knob. `max` is available on GPT-5.6 Luna, Sol, and Terra. Codex's `ultra` mode is not a provider effort: it combines wire-level `max` with proactive multi-agent delegation, which Euler does not yet expose as a driver workflow.
- `--profile <name>` selects the execution profile. The default for `exec` is
  `full` (or `defaults.headless` in `~/.euler/profiles.json`): commands run
  on the host without a sandbox or a prompt, with your `HOME`, credentials,
  and network. For CI or untrusted repositories, `--profile workspace` runs
  shell commands and workspace writes inside a sandbox with network, and
  `--profile locked` runs read-only without network, with the
  `--auto-approve` tier deciding writes and shell.
- `--auto-approve read-only|trusted-local` controls headless permissions under
  a profile that asks for every command (`locked`). Under `full` or
  `workspace` it has no effect and Euler prints a one-line notice.
  - `read-only` is the default tier (it applies only under a profile that asks for every command, such as `locked`): file reads are allowed; writes and shell are denied.
  - `trusted-local`: file reads, file writes, and shell are session-allowed.
- `exec` cannot ask anyone, so a request that would prompt is denied unless
  the guardian (`--permission-reviewer guardian`) allows it. That denial
  covers the one request: the agent gets a tool error and carries on, and a
  later request of the same kind in the same turn is decided again.
- `--extensions <ids>` enables a comma-separated list. Use `--extensions none`
  for no extensions.
- `--max-tool-rounds N` sets a positive hard ceiling. Default is unlimited.
- `--provenance <path>` writes a standalone append-only JSONL event log at
  `<path>` instead of creating an indexed non-interactive home session. By
  default, `exec` stores runs under `~/.euler/sessions/<session-id>/` so they
  appear in `/resume`. (Pre-v0.1.2 default was `./euler-provenance.jsonl` in
  the cwd; scripts that depended on that path should pass `--provenance`
  explicitly.)
- `--auto-compaction off|stubs` chooses the legacy headless policy bundle:
  `off` disables automatic compaction and stubs; `stubs` enables both. When no
  flag is supplied, both automatic compaction and recoverable tool stubs are
  enabled by default.
- `--compaction-budget-bytes <n>` sets the rendered-canvas byte budget. Default:
  `640000`.

`exec` requires a prompt argument or piped stdin.

## Team profiles

A team can share one `profiles.json`: keep it in a repository and copy it to
`~/.euler/profiles.json` (or `$EULER_HOME/profiles.json`). A profile extends a
preset (`full`, `workspace`, or `locked`) and may add permission `rules` and
an `ask_timeout`. Each repository can tighten it further with
`.euler/profile-overlay.json`, which may add `ask` and `deny` rules and an
`ask_timeout` no higher than the profile's, never an `allow`.

```json
{
  "defaults": { "interactive": "dev", "headless": "agent", "subagent": "inherit" },
  "profiles": {
    "dev": {
      "extends": "full",
      "rules": {
        "shell": [
          { "pattern": "git push *", "state": "ask" },
          { "pattern": "git push *--force*", "state": "deny",
            "reason": "Push a new branch instead of force-pushing." },
          { "pattern": "terraform apply *", "state": "deny" }
        ],
        "path": [
          { "pattern": "~/.euler/**", "access": "write", "state": "deny" }
        ]
      }
    },
    "agent": {
      "extends": "full",
      "reviewer": "guardian",
      "ask_timeout": 300,
      "rules": {
        "shell": [
          { "pattern": "git push *", "state": "ask" },
          { "pattern": "git push *--force*", "state": "deny" },
          { "pattern": "gh pr merge *", "state": "deny" },
          { "pattern": "kubectl *", "state": "deny" },
          { "pattern": "kubectl get *", "state": "allow" }
        ],
        "outside_workspace": [
          { "pattern": "/", "access": "write", "state": "ask" },
          { "pattern": "/tmp", "access": "write", "state": "allow" },
          { "pattern": "~/.cache", "access": "write", "state": "allow" }
        ],
        "path": [
          { "pattern": "~/.euler/**", "access": "write", "state": "deny" },
          { "pattern": "~/.ssh/**", "state": "deny" }
        ]
      }
    },
    "ci": {
      "extends": "workspace",
      "rules": {
        "shell": [
          { "pattern": "*", "state": "deny",
            "reason": "CI runs only the listed cargo commands." },
          { "pattern": "cargo build *", "state": "allow" },
          { "pattern": "cargo test *", "state": "allow" },
          { "pattern": "cargo clippy *", "state": "allow" }
        ],
        "tools": [
          { "pattern": "code-swarm.*", "state": "deny",
            "reason": "No paid model reviews in CI." }
        ]
      }
    }
  }
}
```

How rules decide:

- Surfaces: `shell` matches each command of a `run_shell` line (each part of
  a `&&`, `;`, or `|` list, as words, `*` matching anything); `path` matches
  the paths a tool call or a shell command names (globs, relative to the
  workspace unless absolute or `~/`); `outside_workspace` matches paths
  outside the workspace by directory prefix; `tools` matches a core tool name
  (`run_shell`) or an extension command (`<extension-id>.<command>`).
  `access` (`read`, `write`, or `any`) narrows a path rule.
- Within one surface the last matching rule wins, so a carve-out goes after
  the broad rule it carves out of. Across surfaces the strictest match wins:
  `deny`, then `ask`, then `allow`.
- `deny` refuses that one request without asking anyone. The agent gets a
  tool error with the `reason` and carries on with its other work.
- `ask` prompts even under `full`, and no saved grant settles it.
- `allow` only settles an ask that the profile's own approval policy would
  raise. It never lifts Euler's built-in asks (dangerous commands, sensitive
  paths, escalation) and never widens the sandbox.
- Rules guard against mistakes. They see the paths a command names, not
  every file a program touches; the sandbox of `workspace` and `locked`
  remains the boundary.
- A mistake in the file (an unknown key or state, a bad pattern, a core tool
  name that does not exist) is a startup error naming the file, profile,
  surface, and index.

The three profiles:

- `dev` is `full` at the keyboard with three hard stops and one review
  point: `git push` asks, while force pushes, `terraform apply`, and writes
  under `~/.euler` are denied. A shell command whose operand Euler cannot
  resolve asks you only if it could write under `~/.euler` (see below).
- `agent` is the default for `euler exec`. Under `full` nothing asks except
  Euler's built-in asks, the `ask` rules, and shell commands with an operand
  Euler cannot resolve that could reach a rule. An ask goes to the guardian,
  which may allow it once (an unresolved operand's ask skips the guardian
  when it could reach a deny, see below); plain
  `exec` cannot ask a person, so an ask the guardian does not settle is
  denied at once. Run interactively (`euler --profile agent`) and left
  unattended, an ask waits 300 seconds for a person and is then denied.
  Either way the deny covers that one request.
  `kubectl` is denied except `kubectl get`. A write outside the workspace
  asks, except under `/tmp` and `~/.cache`. Writes under `~/.euler` (the
  default Euler home) are denied, so the agent cannot edit its own rules
  through a file tool or a shell write Euler can resolve (see the next
  bullets for the rest), and `~/.ssh` is denied.
- Patterns match words, not Git semantics. In `dev` and `agent`,
  `git push --force` and `--force-with-lease` are denied, but `git push -f`
  still only asks and `git -C dir push --force` matches no rule. Add a
  pattern per spelling you want covered.
- A shell `~` is your home directory, so `cat ~/.ssh/id_ed25519` hits the
  `~/.ssh` deny. A glob or a variable is read up to its fixed directory:
  `ls src/*.rs` names paths under `src/`, which no rule above reaches, so
  it runs without an ask. A word that starts with a variable
  (`cat $HOME/.ssh/*`, `rm -rf $DIR`) could be any path. Such a word never
  matches an `allow`; where it could reach an `ask` or `deny` rule it asks,
  and where it could reach a `deny` only a person can answer (the guardian
  does not), so under plain `exec` it is denied. Variables that are not
  paths, such as `echo $HOME`, `git commit -m "$MSG"`, or
  `python x.py $OUT`, add nothing: for commands other than the readers and
  writers Euler knows, a variable counts as a path only when it contains
  `/` or starts with `.` or `~`. For `agent` under plain `exec`, have the
  agent write literal paths where it can.
- `ci` is `workspace` (sandboxed, network on so cargo can fetch crates) with
  a catch-all shell `deny` followed by `allow` carve-outs, so only the listed
  cargo commands run. Every other shell command is denied, including a
  compound command with one unlisted part and a command Euler cannot
  resolve. File tools keep the `workspace` behavior. Select it with
  `euler exec --profile ci`. The `code-swarm.*` deny keeps paid model reviews
  out of CI; when that extension is not installed, startup prints a notice
  and keeps the rule.
- Use `workspace`, not `locked`, for a catch-all CI profile. `locked` has
  `approvals: always`, so under `exec` the `--auto-approve` tier applies,
  and its default tier sets shell commands to always-deny (ADR 0027,
  resolved question 17). A configured always-deny is checked before any
  rule, so the `allow` carve-outs would never run a command.

Extension reviewers (`extension_reviewers`, ADR 0027) name extension
commands that review each ask before the guardian or a person. A reviewer can
deny or defer, never allow, and a named reviewer that is not installed and
enabled is a startup error that names the fix. Asks raised by child agents do
not reach them yet (#299).

## Brief-file pattern

Use the CLI prompt only as a pointer:

```sh
cat > BRIEF.md <<'EOF'
# Brief

Goal: compare the current implementation against the release README.

Constraints:
- Do not modify code.
- Verify claims against source files.
- Write findings to REPORT.md.

Deliverables:
- REPORT.md with discrepancies and evidence.
EOF

euler exec \
  --provider openrouter \
  --model anthropic/claude-sonnet-5 \
  --extensions none \
  --provenance ./research.jsonl \
  'Read BRIEF.md and complete it.'
```

This keeps shell history readable and makes the actual requirements versionable.

## Provenance monitoring

The provenance file is append-only JSONL: one event per line. Each line has the
event envelope (`v`, `id`, `ts`, `session`, `agent`, `parent`, `kind`, `payload`,
`blobs`). `kind`, `ts`, and `payload` are the fields you usually inspect first.

Count model rounds:

```sh
jq 'select(.kind == "model.result")' ./research.jsonl | wc -l
```

Watch tool calls:

```sh
jq -r 'select(.kind == "tool.call") | [.ts, .payload.name, (.payload.input|tostring)] | @tsv' ./research.jsonl
```

Spot errors:

```sh
jq -r 'select(.kind == "error") | [.ts, .payload.source, .payload.message] | @tsv' ./research.jsonl
```

Same checks without `jq`:

```sh
python3 -c 'import json,sys; print(sum(1 for l in open(sys.argv[1]) if json.loads(l)["kind"]=="model.result"))' ./research.jsonl
python3 -c 'import json,sys; [print(e["ts"], e["payload"].get("name"), e["payload"].get("input")) for e in map(json.loads, open(sys.argv[1])) if e["kind"]=="tool.call"]' ./research.jsonl
python3 -c 'import json,sys; [print(e["ts"], e["payload"].get("source"), e["payload"].get("message")) for e in map(json.loads, open(sys.argv[1])) if e["kind"]=="error"]' ./research.jsonl
```

`exec` stdout is not the reliable progress monitor when piped; current CLI output
is printed after the turn completes. Watch the provenance file.

## Auto-compaction policies

`off`:

- keeps full canvas history;
- if the rendered canvas exceeds the byte budget, Euler stops with a policy-named
  context-budget error instead of silently dropping history.

`stubs` (default):

- keeps every eligible round as a fact;
- demotes oldest tool-result content first;
- replaces demoted content with a one-line stub containing the tool name, event
  id, outcome, original byte count, and retrieval handle;
- demotes write-shaped results last and keeps the artifact path when it can.

Facts are never silently lost; only bulky result content is demoted.

## Custom providers (local models)

Any OpenAI-compatible Chat Completions or Responses endpoint, or
Anthropic-compatible Messages endpoint, can be registered as a provider in
`providers.json` under the Euler home: `$EULER_HOME/providers.json` when
`EULER_HOME` is set, otherwise `~/.euler/providers.json`:

```json
{
  "version": 1,
  "providers": {
    "local": {
      "api_family": "openai_chat_completions",
      "base_url": "http://localhost:11434/v1",
      "default_model": "qwen3:32b"
    },
    "gateway": {
      "api_family": "openai_chat_completions",
      "base_url": "https://llm.example.com/v1",
      "auth_header": true,
      "api_key": "$GATEWAY_API_KEY",
      "default_model": "large-model",
      "models": [
        { "id": "large-model", "context_window_tokens": 200000, "supports_tools": true },
        { "id": "small-model" }
      ]
    }
  }
}
```

Then:

```sh
euler exec --provider local --model qwen3:32b --provenance ./run.jsonl "..."
```

Rules, enforced at load with warnings for anything malformed:

- `api_family` is `openai_chat_completions`, `anthropic_messages`, or
  `openai_responses`; `base_url` is required. A model entry may set its own
  `api_family` to override the provider's (see the gateway example below).
- `http` URLs are accepted for loopback hosts only (`localhost`, `127.0.0.1`,
  `::1`); anything remote must be `https`.
- An `Authorization: Bearer` header is sent only when `auth_header` is
  `true`, and then `api_key` is required. The `api_key` value is a secret
  spec: `$NAME` or `${NAME}` reads an environment variable, `!command` runs a
  command and uses its output, anything else is a literal (escape a leading
  `$` or `!` as `$$` / `$!`). Resolved secrets are tainted values, redacted
  from logs and provenance by construction.
- Built-in provider ids (`chatgpt`, `anthropic`, `openai`, `openrouter`,
  `xai`, `fixture`) are reserved and cannot be overridden.
- `models` lists the provider's models; each entry needs an `id` and can
  declare `context_window_tokens`, `max_output_tokens`, `supports_tools`, and
  `supports_reasoning`. `models` is optional when `default_model` is set: the
  provider then offers that one model with no declared metadata, the same as
  passing an unlisted `--model`. A provider with neither field, or whose
  `models` is present but empty or not an array (even with `default_model`
  set), is ignored with a warning.

### Gateway setup with `euler login`

`euler login --provider <id>` adds a gateway without editing
`providers.json` (see the [quickstart](quickstart.md)). It reads the base URL
and the key as one line each from a non-terminal stdin, so a provisioning
script can run it once per machine:

```sh
printf '%s\n%s\n' https://litellm.example.com "$LITELLM_API_KEY" |
  euler login --provider litellm
```

Login verifies the URL and key by listing the gateway's models and exits
non-zero, storing nothing, when that fails for a new id. For an existing
entry whose URL is kept (an empty first line, or the same URL), a failure
other than a rejected key instead asks whether to save the key without
discovery, and end of input answers yes. On success it writes the key to the auth file, the
entry (with `"discovery": true`) to `providers.json`, and the model list to
the discovery cache, so the next headless launch routes from the cache
without a network wait. To keep the key out of the auth file, write the
entry by hand with an `api_key` spec instead.

### One gateway, several API families

A gateway such as a LiteLLM proxy serves Claude models natively over
Anthropic Messages (`POST /v1/messages`), OpenAI models over Responses
(`POST /v1/responses`), and everything over OpenAI Chat Completions
(`POST /v1/chat/completions`). Route Claude models to Messages so their
signed thinking blocks are replayed across tool rounds, and route
Responses-native models (such as Codex models) to Responses so the gateway
does not translate them to Chat Completions:

```json
{
  "version": 1,
  "providers": {
    "litellm": {
      "api_family": "openai_chat_completions",
      "base_url": "https://litellm.example.com/v1",
      "auth_header": true,
      "api_key": "$LITELLM_API_KEY",
      "default_model": "claude-sonnet",
      "models": [
        { "id": "claude-sonnet", "api_family": "anthropic_messages",
          "context_window_tokens": 1000000, "supports_tools": true, "supports_reasoning": true },
        { "id": "gpt-large", "supports_tools": true, "supports_reasoning": true,
          "compat": { "reasoning": { "request_format": "openai_reasoning_effort" } } },
        { "id": "gpt-codex", "api_family": "openai_responses",
          "supports_tools": true, "supports_reasoning": true }
      ]
    }
  }
}
```

All families share `base_url`. Chat Completions appends `/chat/completions`
and Responses appends `/responses`. Messages appends `/messages` to a root
ending in `/v1` and `/v1/messages` to any other root, so
`https://litellm.example.com/v1` reaches `/v1/chat/completions`,
`/v1/responses`, and `/v1/messages`. A `base_url` that is already a full
endpoint (ending in `/chat/completions`, `/responses`, or `/messages`) serves
only that family, so a model that overrides the family needs a root
`base_url`.

On a Messages model, `supports_reasoning: true` turns on adaptive thinking at
the requested `--reasoning-effort`, the same mapping as the built-in
`anthropic` provider. Messages requires `max_tokens`. Without
`--max-output-tokens`, Euler sends 32000, or the model entry's
`max_output_tokens` when that is lower, so declare that field for a model
with a lower output limit. On a Responses model, `supports_reasoning: true`
requests reasoning (with streamed summaries) at the requested
`--reasoning-effort`; without it no reasoning field is sent, so list a
reasoning model with `supports_reasoning: true`. Otherwise
`--reasoning-effort` is ignored and no reasoning summaries stream. Euler does
not check the effort on a Responses model. It sends `xsmall` as `minimal` and
`max` as `max`, and a level the upstream model does not support (often `max`,
sometimes `minimal`) fails as a rejected request, so pick one it accepts
(typically `small` to `xlarge`).
Recorded reasoning is not replayed on Responses models, as on the built-in
`chatgpt` provider. `compat` applies only to Chat Completions models. For
Anthropic's own API, which expects `x-api-key` rather than a bearer token, use
`"headers": { "x-api-key": "$ANTHROPIC_API_KEY" }` instead of `auth_header`.

## Detached launches

Use OS-level detachment plus an outer timeout:

```sh
mkdir -p runs
timeout 12h setsid -f sh -c '
  nohup euler exec \
    --provider openrouter \
    --model anthropic/claude-sonnet-5 \
    --extensions none \
    --provenance ./runs/job-001.jsonl \
    "Read BRIEF.md and complete it." \
    > ./runs/job-001.out 2> ./runs/job-001.err
'
```

Inside a session, the `run_shell` tool has a default timeout of `120000` ms and
accepts `timeout_ms` up to `600000` ms. The outer `timeout` is still useful for
fleet supervision.

## Worked example

```sh
mkdir -p fleet-runs/001
cd fleet-runs/001

cat > BRIEF.md <<'EOF'
# Research brief

Question: What command-line surfaces does this repository expose for auth and
headless runs?

Requirements:
- Verify claims against source files.
- Do not make network calls.
- Do not modify Rust code.

Deliverable:
- Write FINDINGS.md with commands, files inspected, and open questions.
EOF

timeout 6h setsid -f sh -c '
  nohup euler exec \
    --provider openrouter \
    --model anthropic/claude-sonnet-5 \
    --reasoning-effort large \
    --profile locked \
    --auto-approve trusted-local \
    --extensions none \
    --max-tool-rounds 100 \
    --provenance ./session.jsonl \
    --auto-compaction stubs \
    "Read BRIEF.md and complete it." \
    > ./stdout.log 2> ./stderr.log
'
```

`--profile locked --auto-approve trusted-local` keeps the run off the network,
as the brief asks, while still allowing writes and shell.

Monitor:

```sh
while sleep 30; do
  printf 'rounds='; jq 'select(.kind == "model.result")' ./session.jsonl | wc -l
  jq -r 'select(.kind == "tool.call") | [.ts, .payload.name] | @tsv' ./session.jsonl | tail -n 5
  jq -r 'select(.kind == "error") | [.ts, .payload.source, .payload.message] | @tsv' ./session.jsonl
done
```

Completion usually looks like:

- the process exits;
- `stderr.log` has no fatal error;
- the last `model.result` has `payload.stop_reason` of `completed`;
- the deliverable file exists;
- `session.jsonl` contains the evidence trail for model calls, tool calls, and
  any errors.
