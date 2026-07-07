# Quickstart

Install, authenticate, start one interactive session, then run one headless task.

## Install

```sh
git clone https://github.com/2x11-xyz/euler
cd euler
cargo build --release
./target/release/euler models
```

Use `./target/release/euler` in the examples below, or put it on your `PATH` as
`euler`.

## Authenticate

Euler has one browser/OAuth login path today: ChatGPT.

```sh
euler login --provider chatgpt
euler auth status
```

Anthropic, OpenAI (API key), and OpenRouter use environment variables, not
`euler login`:

```sh
export ANTHROPIC_API_KEY='...'
export OPENAI_API_KEY='...'
export OPENROUTER_API_KEY='...'
euler auth status
```

Local models and other OpenAI-compatible endpoints can be wired as custom
providers via `~/.euler/providers.json` — see the
[headless guide](headless.md).

Check the offline model catalog:

```sh
euler models
```

## First interactive session

Start the full-screen TUI:

```sh
euler
# or explicitly:
euler tui
```

`euler` defaults to the TUI only when stdin and stdout are both terminals. For
line-oriented operation:

```sh
euler run --no-tty --provider chatgpt --model gpt-5.5
```

Line-oriented decorative color honors `NO_COLOR` and `TERM=dumb`:

```sh
NO_COLOR=1 euler run --no-tty --provider chatgpt --model gpt-5.5
```

The full-screen TUI keeps color enabled because color carries interface state.

## First headless run

Headless `exec` takes one prompt argument, or reads a prompt from stdin. It writes
provenance to `euler-provenance.jsonl` by default; pass `--provenance` when you
want a named log.

```sh
euler exec \
  --provider chatgpt \
  --model gpt-5.5 \
  --provenance ./first-session.jsonl \
  'Read README.md and summarize what Euler is in five bullets.'
```

No-credential smoke test with the built-in fixture provider:

```sh
euler exec \
  --provider fixture \
  --model echo \
  --extensions none \
  --provenance ./fixture-session.jsonl \
  'Say hello without tools.'
```

## Where data lives

Default local state is under `~/.euler`:

- `~/.euler/auth.json` — ChatGPT OAuth credentials and optional stored API-key credentials.
- `~/.euler/sessions/<session-id>/events.jsonl` — interactive session event log.
- `~/.euler/sessions/<session-id>/blobs/` — large payload blobs for that session.
- `~/.euler/sessions/<session-id>/session.json` — session sidecar metadata.
- `~/.euler/sessions/index.jsonl` — session index.

`exec --provenance <path>` writes the event log at that path instead of the home
session store.

## Real command surface

Top-level commands are:

```text
run
tui
exec
login
logout
auth status
models
session-export
extension
```

`login` and `logout` require `--provider chatgpt`; Anthropic and OpenRouter do
not use those commands.
