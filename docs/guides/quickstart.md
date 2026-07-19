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

Anthropic, OpenAI (API key), OpenRouter, and xAI use environment variables,
not `euler login`:

```sh
export ANTHROPIC_API_KEY='...'
export OPENAI_API_KEY='...'
export OPENROUTER_API_KEY='...'
export XAI_API_KEY='...'
euler auth status
```

Local models and other OpenAI-compatible endpoints can be wired as custom
providers via `~/.euler/providers.json` — see the
[headless guide](headless.md).

Check the offline model catalog:

```sh
euler models
```

Euler ships a verified provider catalog in the binary, so this works on a
fresh offline install. The TUI checks the public
[`euler-provider-catalog`](https://github.com/2x11-xyz/euler-provider-catalog)
GitHub release channel in the background once per day after a successful
check. A failed check may retry after one hour. To check on demand without
starting a session:

```sh
euler models refresh
```

Refresh does not use provider API keys. It downloads only the public release
manifest and catalog, verifies their identity, digest, schema, compatibility,
and monotonic release time, then keeps the last-known-good catalog on any
failure.

The catalog contains only public model metadata. Provider credentials are
never downloaded into, copied into, or resolved by the catalog client.

If an older Euler generated `~/.euler/models.json`, the new loader recognizes
its exact `"generated_by": "euler models refresh"` marker and ignores that
obsolete machine-owned overlay. It leaves the file in place in case it was
hand-edited; remove it after reviewing any local changes.

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

Headless `exec` takes one prompt argument, or reads a prompt from stdin. It creates
an indexed non-interactive home session by default, so the run appears in
`/resume`. Pass `--provenance` when you want a standalone log at a specific
path instead of the home session store. (Older builds defaulted to
`./euler-provenance.jsonl` in the cwd.)

```sh
euler exec \
  --provider chatgpt \
  --model gpt-5.5 \
  'Read README.md and summarize what Euler is in five bullets.'
```

No-credential smoke test with the built-in fixture provider:

```sh
euler exec \
  --provider fixture \
  --model echo \
  --extensions none \
  'Say hello without tools.'
```

## Where data lives

Default local state is under `~/.euler`:

- `~/.euler/auth.json` — ChatGPT OAuth credentials and optional stored API-key credentials.
- `~/.euler/sessions/<session-id>/events.jsonl` — interactive session event log.
- `~/.euler/sessions/<session-id>/blobs/` — large payload blobs for that session.
- `~/.euler/sessions/<session-id>/session.json` — session sidecar metadata.
- `~/.euler/sessions/index.jsonl` — session index.
- `~/.euler/catalogs/provider-v1/` — verified, machine-managed catalog releases
  downloaded from GitHub.
- `~/.euler/models.json` — optional user-owned metadata/default overrides,
  applied after the official catalog.

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
scrub
```

`login` and `logout` require `--provider chatgpt`; Anthropic and OpenRouter do
not use those commands.
