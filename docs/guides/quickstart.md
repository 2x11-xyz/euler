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

On Linux, install Bubblewrap (`sudo apt install bubblewrap`). On macOS, Euler
uses the system Seatbelt launcher. `run_shell` and the `git_*` tools run inside
the platform backend by default and fail closed when it cannot be applied.
Check the boundary before your first session:

```sh
euler --check-sandbox
```

If it reports `unavailable`, the diagnostic names the cause and the host
change that fixes it. Common Linux causes are unprivileged user namespaces
disabled by a sysctl or AppArmor; on macOS another application sandbox can
prevent Seatbelt from nesting.

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

To use an OpenAI-compatible gateway such as a LiteLLM proxy, log in with a
provider id of your choice:

```sh
euler login --provider litellm
```

```text
Base URL: https://litellm.example.com
Using https://litellm.example.com/v1
API key:
Found 42 models from /model/info: 12 anthropic_messages, 3 openai_responses, 27 openai_chat_completions.
Added provider litellm to ~/.euler/providers.json.
Stored the key for litellm in ~/.euler/auth.json.
```

Login asks for the base URL and then the key without echoing it (or reads
one line each from a pipe), checks both by listing the gateway's models, and
only then stores the key in `~/.euler/auth.json` and adds the provider to
`~/.euler/providers.json` with `"discovery": true`. The models then appear in
`/model` and `euler models`, each routed to its API family. A wrong URL or a
rejected key stores nothing and says why. Running login again for the same
id offers the current URL as the default and replaces the key.

Local models and other endpoints can also be wired by hand in
`~/.euler/providers.json`; see the [headless guide](headless.md). For a
hand-written entry that sets `"auth_header": true` and no `api_key`, the same
command stores its key; if you keep its URL and that gateway does not list
its models or cannot be reached, login asks whether to save the key without
model discovery and leaves the entry as written. An `api_key` in the entry
wins, so login refuses an entry that sets one. `euler logout --provider litellm` removes the stored key and leaves the
entry.

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

See [Provider catalog and model updates](provider-catalog.md) for the complete
publication, binary-embedding, runtime-refresh, and failure lifecycle.

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

## Personal instructions

Put standing rules you want in every session (your name, writing preferences)
in `~/.euler/AGENTS.md`, or `$EULER_HOME/AGENTS.md` when `EULER_HOME` is set:

```sh
printf 'Call me Ada. Keep answers short.\n' > ~/.euler/AGENTS.md
```

Euler loads it into every new session ahead of project `EULER.md` files, with
no prompt. Project instructions are more specific and win when the two
conflict. The file is limited to 32 KiB; a larger one is skipped. Edits apply
to the next new session.

## Where data lives

Default local state is under `~/.euler`:

- `~/.euler/AGENTS.md`: optional personal instructions loaded into every session.
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
extension
scrub
```

`login` and `logout` require `--provider chatgpt` or a custom provider id;
Anthropic, OpenAI, OpenRouter, and xAI do not use those commands.
