# Headless runs

`euler exec` is the unattended mode for long-horizon runs and fleets. Keep the
prompt short, put the real assignment in a file, and monitor provenance.

## Exec anatomy

```sh
euler exec \
  --provider openrouter \
  --model anthropic/claude-sonnet-5 \
  --reasoning-effort large \
  --auto-approve trusted-local \
  --extensions none \
  --max-tool-rounds 100 \
  --provenance ./runs/job-001.jsonl \
  --auto-compaction stubs \
  --compaction-budget-bytes 640000 \
  'Read BRIEF.md in this directory and carry it out fully.'
```

Flags:

- `--provider <id>` / `--model <id>` choose the route.
- `--reasoning-effort xsmall|small|medium|large|xlarge` sets Euler's reasoning knob.
- `--auto-approve read-only|trusted-local` controls headless permissions.
  - `read-only` is the default: file reads are allowed; writes and shell are denied.
  - `trusted-local`: file reads, file writes, and shell are session-allowed.
- `--extensions <ids>` enables a comma-separated list. Use `--extensions none`
  for no extensions.
- `--max-tool-rounds N` sets a positive hard ceiling. Default is unlimited.
- `--provenance <path>` writes a standalone append-only JSONL event log at
  `<path>` instead of creating an indexed non-interactive home session. By
  default, `exec` stores runs under `~/.euler/sessions/<session-id>/` so they
  appear in `/resume`. (Pre-v0.1.2 default was `./euler-provenance.jsonl` in
  the cwd; scripts that depended on that path should pass `--provenance`
  explicitly.)
- `--auto-compaction off|stubs` chooses canvas retention. Default: `stubs`.
- `--compaction-budget-bytes <n>` sets the rendered-canvas byte budget. Default:
  `640000`.

`exec` requires a prompt argument or piped stdin.

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
  --auto-approve trusted-local \
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

Any OpenAI-compatible chat-completions endpoint can be registered as a
provider in `~/.euler/providers.json`:

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
      "api_key": "$GATEWAY_API_KEY"
    }
  }
}
```

Then:

```sh
euler exec --provider local --model qwen3:32b --provenance ./run.jsonl "..."
```

Rules, enforced at load with warnings for anything malformed:

- `api_family` must be `openai_chat_completions` (the only family in this
  release); `base_url` is required.
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
- Optional per-model entries under `"models"` can declare
  `context_window_tokens`, `max_output_tokens`, `supports_tools`, and
  `supports_reasoning`.

## Detached launches

Use OS-level detachment plus an outer timeout:

```sh
mkdir -p runs
timeout 12h setsid -f sh -c '
  nohup euler exec \
    --provider openrouter \
    --model anthropic/claude-sonnet-5 \
    --auto-approve trusted-local \
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
    --auto-approve trusted-local \
    --extensions none \
    --max-tool-rounds 100 \
    --provenance ./session.jsonl \
    --auto-compaction stubs \
    "Read BRIEF.md and complete it." \
    > ./stdout.log 2> ./stderr.log
'
```

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
