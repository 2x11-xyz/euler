# Provider Attempt and Liveness Contract

Provider adapters own authentication, request shaping, wire compatibility,
transport I/O, and response parsing. Core owns the session boundary around one
physical provider attempt: cancellation, inactivity policy, retry eligibility,
and content-free lifecycle observation.

## Semantic and control paths

`ModelStreamEvent` is a semantic-only channel. Its closed variants are text,
reasoning, tool calls, and the provider-neutral finish record. Transport bytes,
heartbeats, socket polls, retry notices, deadlines, and cancellation signals
must never be represented as model events. They therefore cannot become
assistant content, transcript content, model-canvas content, or meaningful
model progress by accident.

The attempt observer is a separate, content-free control path. It may report:

- attempt started;
- response headers received;
- first response byte received;
- first semantic output received;
- one terminal outcome and timing summary.

An attempt id identifies one physical dispatch, not a logical model round. A
retry has a fresh attempt id. Observer payloads contain only identifiers,
stages, outcomes, and durations; provider payload bytes and model content are
forbidden.

The live `Session` observer composes attempt telemetry with provider/model
identity, retry ordinal/category/backoff, and one explicit runtime scope:
`Root`, `Companion`, `ParallelReviewer`, or `Compaction`. Only `Root` denotes
the foreground model turn. A UI may opt into separately labelled background
status, but child, reviewer, and compaction transitions must not replace,
advance, stall, or terminalize foreground activity. This observer is a
process-local attachment rather than `SessionConfig` or durable state, and
cloned runtime paths inherit it.

Observation is synchronous and must remain a prompt, nonblocking handoff. A
host may enqueue the control on its own channel, but must not perform provider
I/O, provenance I/O, or other unbounded work inside the callback. This keeps
the attempt boundary usable for cancellation races without making telemetry a
new source of provider-call latency.

A retry transition carries the failed physical attempt id when available, so
overlapping calls to the same provider/model remain correlatable. A premature
stream end is a terminal `stream_truncation` error carrying that same attempt
id; core must not replace it with an uncorrelated synthetic error.

An interactive host may route these controls through a non-content worker
channel to its existing live activity projection. Only `Root` controls may
change the foreground phase. Attempt start, headers, first byte, first
semantic output, retry, timeout, and cancellation are displayable stages, but
none resets meaningful-progress time or replaces the last completed
milestone. The route must not manufacture a session event or append anything
to transcript, visual-canvas history, provenance, or model input; canonical
replay remains a pure event-log fold.

## Inactivity clocks

Every session-owned provider call, including root, companion, parallel
reviewer, and shadow-compaction calls, uses the same attempt boundary. Three
configurable deadlines are enforced:

1. `response_headers`: from attempt start until the adapter returns an opened
   response stream;
2. `first_byte`: from response headers until the first body byte or WebSocket
   message;
3. `semantic_idle`: from the first response byte until the first meaningful
   model event, then from each meaningful model event to the next.

These stages are sequential rather than competing clocks. Before open, a
timeout is `response_headers`; after open and before any byte, it is
`first_byte`; only after a byte can `semantic_idle` fire. Configuration order
cannot cause a stall to be labelled as a later stage.

The defaults are 60 seconds for response headers, 60 seconds for the first
byte, and 5 minutes for semantic idle. A zero duration times out immediately.
An unrepresentable deadline fails closed instead of silently disabling the
boundary.

There is deliberately no total attempt-duration cap. A response may run for
arbitrarily long while it continues to produce semantic progress.

Raw transport activity updates transport timing only. SSE comments, HTTP body
bytes that do not parse into model output, WebSocket ping/pong frames, and
socket polling wakeups do not reset semantic idle. Empty text/reasoning deltas
are not semantic progress. Provider-opaque reasoning artifacts are adapter-owned
carry state and must not be interpreted as progress outside their adapter.
Nonempty text, readable nonempty reasoning, tool calls, and the terminal finish
record are semantic progress.

## Cancellation and bounded transport shutdown

The session boundary polls its canonical cancellation token and stops waiting
within a short fixed interval. Once cancelled, timed out, or abandoned, it
rejects every late provider event and closes the demand path to the adapter.

Built-in synchronous adapters additionally bound their socket operations so a
connected worker cannot remain stuck forever after detachment:

- HTTP adapters apply the header deadline plus a small shutdown grace to
  connect, read, and write syscalls. A body-read timeout is a control-plane
  polling wakeup; the reader continues only while the owning attempt remains
  live.
- The ChatGPT WebSocket adapter establishes TCP and the handshake under the
  same bound, polls stream reads at a finite interval, and shuts down its
  cloned control socket when ownership ends.

The process resolver used by these synchronous clients is outside this socket
boundary and cannot be synchronously preempted by portable Rust. A stalled OS
name lookup may therefore outlive logical detachment, but has no route back
into the session; header timeout still bounds what the session waits for.

Provider implementations that override `invoke_observed` must follow the same
rule: every blocking I/O operation is finite or explicitly interruptible, raw
transport observations carry no content, and `should_stop` ends the adapter.
The default implementation exists for in-process/compatibility providers that
have no observable transport; it receives logical session detachment but
cannot manufacture physical preemption for an arbitrary blocking
implementation.

Shadow compaction additionally uses the content-free `Attempt::Ended`
transition as its ready-versus-cancel race boundary. `Attempt::Started` marks
the physical attempt active; `Attempt::Ended` is published before its terminal
stream value returns to the compaction actor. Cancellation before that boundary
retains the short detach grace required for a blocked compatibility provider.
Cancellation after it prevents any retry from starting and settles the
worker's terminal result and usage before the session actor closes the shadow.
This settlement does not grant the candidate authority to replace the active
canvas: the caller's apply-versus-discard disposition remains decisive.

## Failure and retry

An inactivity timeout is a `transport` provider error with additive
`timeout_stage` and `provider_attempt_id` metadata. Timeout is not a new error
category. Provider adapters retain the canonical categories `auth`,
`transport`, `rate_limit`, `rejected`, and `stream_truncation`.

A transport or rate-limit failure may retry only if the logical round has not
observed provider-neutral progress. Once readable text/reasoning, a tool call,
or a finished record has entered the round, the request is never replayed
automatically because its remote outcome and emitted prefix cannot be
duplicated safely. Empty deltas, provider-opaque artifacts, and transport
control observations do not suppress an otherwise safe pre-semantic retry.

Inactivity timeouts in both ordinary model rounds and shadow compaction retry
by stage, not by category alone:

- `response_headers` and `first_byte` timeouts are retryable (subject to the
  progress rule and the retry budget): nothing was received, so the attempt
  is treated like any other transport failure.
- `semantic_idle` timeouts are never retried, even when no provider-neutral
  progress reached the round. The stage can only fire after the first
  response byte, so the provider had accepted and was working the request
  (for example a long reasoning phase with no readable summary). Replaying it
  would bill the caller again for an attempt that already ran; the round
  fails with the timeout error instead.

Adapters must therefore make the reasoning phase observable where the API
allows it (the ChatGPT Responses adapter requests `reasoning.summary: auto`)
so that legitimate long thinking resets semantic idle rather than tripping it.
Each retry is separately observed and its backoff is recorded in diagnostics.

Root readable text is durably checkpointed before it is forwarded as live UI
content. A handled provider failure or cancellation flushes every observed
text suffix before the canonical terminal error. Checkpoints do not broaden
the retry window: any nonempty semantic output still forbids automatic replay.

## Diagnostics and provenance

Attempt diagnostics record request start, response headers, first byte, first
semantic output, retry scheduling, timeout/cancellation/completion outcome, and
the final transport/semantic timing summary. Diagnostics must remain
content-free and follow the secrets contract. The final record distinguishes
unavailable provider usage from known zero usage and includes the locally
observed output byte count. `usage_available: false` omits token-count fields;
when true, input/output counts are numeric scalars even when either known
value is zero.

The canonical `model.call` plus its ordinary `model.result` or terminal
`error` remains the durable semantic history. A provider terminal error may
carry `provider_attempt_id` and `timeout_stage`; those fields support
postmortems without introducing high-volume heartbeat events. Ordinary raw
transport observations are never persisted.

## User provider configuration

User-owned custom provider transport is read from `providers.json` in the
Euler home: `$EULER_HOME/providers.json` when `EULER_HOME` is set, otherwise
`$HOME/.euler/providers.json`. The path resolves through the same Euler home
resolution as the auth file (`auth.json`, see `secrets.md`), sessions,
extensions, and skills. A missing file means no
custom providers. When the Euler home cannot be resolved (for example a
relative `EULER_HOME`), no custom providers load and line-oriented commands
print the resolution error as a `warning: ignored provider config:` line on
stderr.

A saved model preference lives in `preferences.json` in the same Euler home
(`$EULER_HOME/preferences.json` when `EULER_HOME` is set, otherwise
`$HOME/.euler/preferences.json`), written by `/model`. The theme, timestamp,
and notification preferences share that file. A file under `$HOME/.euler` is
not read or migrated when `EULER_HOME` points elsewhere; the new location
starts empty. When the Euler home cannot be resolved, startup loads no saved
preference and `/model` reports the resolution error instead of saving. A
saved model preference may name a custom provider. Startup resolves it
against built-in providers and the loaded `providers.json`, and prints
`warning: ignored model preference with unknown provider <id>` only when
neither defines that id.

Each provider lists its models in a non-empty `models` array of objects with
an `id`. A provider may omit `models` when it sets `default_model`; the parser
then records one model with that id and no declared metadata (context window,
output limit, tool and reasoning support, compat), which is how an unlisted
`--model` is treated. A provider that sets `"discovery": true` may omit both
(see Gateway discovery). A provider whose `models` is empty, is not an array,
or holds no valid entries, or that omits `models` without a valid string
`default_model` or `"discovery": true`, is ignored with a warning that names
the three fixes.

### Stored keys

A provider with `auth_header: true` sends `Authorization: Bearer <key>`. The
key is its `api_key` spec (see the secrets contract) when the entry sets one,
otherwise the API-key credential stored in the auth file under the provider
id. An `api_key` spec always wins. The stored key is read when the provider
validates its auth at launch or `/model`, and again at each request, so a key
stored during a session is used by its next request. A provider that needs a
stored key and has none fails with an auth error naming
`euler login --provider <id>`.

`euler login --provider <id>`, for an id that is not built-in, sets up a
gateway (ADR 0025 section 1). It prompts for the base URL (visible) and the
key (without echo; a non-terminal stdin supplies one line each), so a new
user needs no file edit:

- **No entry for the id:** the base URL is required. It is normalized to
  `<root>/v1` (trailing slashes and a trailing `/v1` removed, then `/v1`
  appended) and printed. A URL ending in a family endpoint path
  (`/chat/completions`, `/messages`, `/responses`) is refused, and so is
  one this file would ignore, including HTTP to a non-loopback host (the
  error suggests the `https` URL). Login then runs gateway discovery with
  the typed key. On success it stores the key, then adds the entry
  `{"api_family": "openai_chat_completions", "base_url": "<root>/v1",
  "auth_header": true, "discovery": true}`, then writes the discovery
  cache, and prints the model counts per family. On failure it stores
  nothing and names the endpoint and the status or transport error; when
  every request answered HTTP 401 or 403, the error says the key was
  rejected.
- **An entry with `auth_header: true` and no `api_key`:** the prompt shows
  the current `base_url` as the default (Enter keeps it), and the same
  normalization, verification, and writes apply; login sets `base_url` and
  `discovery: true` and leaves every other field as written. When
  discovery fails for a reason other than a rejected key (the gateway lists
  no models or cannot be reached) and the typed base URL normalizes to the
  entry's own (Enter, or the same URL in another form), login prints the
  error and asks "Save the key without model discovery? [Y/n]" (Enter or
  end of input means yes); yes stores the key unverified, leaves the entry unchanged, and
  names the `base_url` the key is sent to; no stores nothing.

When login writes or updates an entry that has no `default_model`, it sets
`default_model` to a model picked from the discovered list (a configured
`models` entry replaces a discovered one with its id, as at launch) and
prints `Default model: <id> (change with --model or /model)`. The pick is
the newest `anthropic_messages` model whose id contains `sonnet` (ignoring
case), else the newest `anthropic_messages` model, else the newest
`openai_chat_completions` model that declares tool support; a Responses
model is never picked, and when none qualifies no `default_model` is set.
Newest, independent of list order: an id that starts with the tier's prefix
(`claude-sonnet`, then `claude`, none for chat completions, ignoring case)
wins; then the highest id in natural order (digit runs compare as numbers,
so `claude-sonnet-5-10` is above `claude-sonnet-5-9`), ignoring a trailing
`-` segment of 8 digits; then the shorter id, so `claude-sonnet-5-5` wins
over `claude-sonnet-5-5-20261001`; then the higher full id, so the later of
two dated snapshots wins; then the lower id in byte order. An entry's
existing `default_model` is never replaced.

Login refuses, storing nothing and before any prompt, an id that breaks the
`providers.json` id rule (lowercase ASCII letters, numbers, `-`, `_`). Base
URL errors name the problem but never echo the typed text, which may be a
key sent on the wrong line or a URL with embedded credentials. Login also
refuses, storing nothing, when the entry sets an `api_key` spec (the
error says to remove `api_key` to use a stored key), when it does not set
`auth_header: true` (a stored key would not be sent), when `providers.json`
has an entry for the id that failed to load, when `providers.json` is not
valid JSON (the file is left untouched and the error names it), or when the
URL or key is empty. Keys are stored as literals (a key starting with `$`
or `!` gets the `$` escape, so it is never read back as an environment or
command reference). The key is written before the entry so a provider never
points at a missing key; if the entry write fails, login restores the
credential the key replaced (or removes it) and reports the error. A failed
cache write is a warning, and the next launch fetches. `providers.json` is
updated by parsing it, changing the one entry, keeping every other entry and
field, and replacing the file atomically with its permissions preserved
(owner-only for a new file); it is rewritten as formatted JSON with object
keys in sorted order. `euler logout --provider
<id>` removes the stored key, leaves the entry, and says so. `euler auth
status` lists each stored custom-provider key after the built-in providers,
without its value. Built-in ids other than `chatgpt` still reject `login` and
`logout`.

### Gateway discovery

A custom provider may set `"discovery": true` (ADR 0025). Core then adds the
gateway's models to the provider's. Configured `models` stay listed beside
discovered ones, a configured entry with a discovered id replaces the
discovered descriptor whole (so it can override one model's family), and
`default_model` may name a discovered id; a `default_model` the gateway does
not list still loads as an unlisted model. A launch that needs the default
model of a discovery provider without `default_model` (a hand-written entry)
uses the model login would pick from its listed models and prints to stderr `note:
provider <id> has no default_model; using <model> (change with --model or
/model)`; when none qualifies, the launch fails with an error saying the
provider has no `default_model` and to pass `--model`. A discovery provider's `base_url`
must end in `/v1` (trailing slashes allowed); any other shape ignores the
provider with a warning.

Requests go to the gateway root (`base_url` without trailing slashes and
`/v1`), with the provider's auth and header specs resolved as for a model
request. Redirects are not followed, each request has a 5-second deadline,
one discovery run has a 10-second overall deadline, and a body over 4 MiB
fails the request. Failure messages name the endpoint and the status or
transport error, never a response body, and begin "the key was rejected"
when every request answered HTTP 401 or 403.

1. `GET <root>/model/info`. A 200 whose `data` (or bare list) holds rows
   with a string `model_name` is the source. A wildcard route (a
   `model_name` containing `*`) expands to the `GET <root>/v1/models` ids it
   matches, each taking the deployments of the matching wildcard route with
   the longest literal prefix; an explicit row wins, even one dropped below
   as non-chat, and unmatched ids are not added. If that request fails, the
   explicit rows stand and a warning names the dropped wildcard routes that
   have a chat row. If no chat model survives mapping, discovery fails.
2. Any other answer, or a transport failure: `GET <root>/v1/models`. Every
   listed id appears with the provider's family and no declared metadata,
   like an unlisted `--model`. A non-200, malformed, or empty answer fails
   discovery.

Mapping `/model/info` keeps deployments whose `model_info.mode` is `chat`,
`responses`, or unset and drops every other mode (embedding, image
generation, completion, audio, rerank, and the rest). Rows are grouped by
`model_name`; routes that fail the model-id rule are dropped and listed.
Deployments merge conservatively: `context_window_tokens` is the minimum
`max_input_tokens`, `max_output_tokens` the minimum `max_output_tokens`, and
`supports_tools` (`supports_function_calling`) and `supports_reasoning` are
true only when every deployment says so; a field missing from any deployment
is omitted. A deployment routes to:

- `anthropic_messages` when its mode is `chat` and it is Claude behind an
  Anthropic-hosting provider: `litellm_provider` is `anthropic` or
  `vertex_ai-anthropic_models`, or it is `bedrock`, `bedrock_converse`, or
  `vertex_ai` and the underlying model (`litellm_params.model`, else
  `model_info.key`, or the expanded id for a wildcard route) contains
  `claude`, ignoring case. `supported_endpoints` is not consulted;
- `openai_responses` when its mode is `responses`;
- `openai_chat_completions` otherwise.

A route keeps a family only when every deployment agrees; a disagreeing
route uses `openai_chat_completions` and is listed. Only chat-completions
models that support reasoning get
`compat.reasoning.request_format: "openai_reasoning_effort"`.

A full successful discovery is written atomically and owner-only to
`<euler home>/cache/provider-discovery/<provider id>.json`, recording the
format version, base URL, fetch time, source endpoint, and mapped models in
the model-entry shape. A cache whose format version or base URL does not
match the entry is a miss. Concurrent writers are last-writer-wins. A failed
or empty discovery never replaces a cache.

Every launch of a session (`run`, `tui`, `exec`, `--resume`) routes from
the cache, headless or interactive alike:

- **No cache:** the launch fetches before it starts, all discovery providers
  in parallel, each under the 10-second deadline. On failure the provider
  loads with only its configured `models` or `default_model` (or no listed
  models; an explicit `--model` still works as an unlisted model), and a
  warning names the endpoint, the status or transport error, and
  `euler models refresh`.
- **Cache older than 24 hours:** the launch refreshes it in the background
  and uses the cached list; later sessions see the update. A failed
  background refresh keeps the cache and its warning prints once, when the
  command exits. Until a refresh succeeds, every launch retries.

`euler models refresh` also refreshes every discovery provider and prints,
per provider, its model count per family, the ids added and removed, and
mapping notes; a failed provider keeps its cache, is reported, and makes the
command fail after the others run. `euler models` reads the cache and lists
discovered models with their provider, family, and `"source":
"discovered"`. `euler login --provider <id>` writes a discovery
provider's cache from the discovery that verified the key, so a key change
takes effect at once. `/model` lists discovered models from the cache too. Known limit: the
running session keeps the provider list it launched with, so after a
background refresh `/model` can offer an id that this session sends as an
unlisted model (the provider family, no `model_source`) until the next
launch. A refresh interrupted by process exit can leave an owner-only
`.<provider id>-*.tmp` file in the cache directory; the cache itself is
replaced only by an atomic rename.

A discovered model reports its cache as the provider's `model_source`
(endpoint, fetch time, and the cache file's SHA-256), which core records on
`session.start`, `model.switched`, and `session.resumed` (see the events
contract). A configured entry, including a configured replacement of a
discovered id, has no `model_source`. A resume whose recorded source
differs from the current one proceeds, records the new source, and prints a
note; a model a refresh removed is sent as an unlisted model.

Boundary exception (boundary contract, Exceptions). The setting, cache,
refresh, and `/v1/models` are product-neutral, but reading `/model/info`
means core knows one gateway product's endpoint and field names
(`model_name`, `litellm_params`, `model_info`, `litellm_provider`):

- **Invariant:** adopting a gateway takes one command, with nothing to
  install.
- **Why the extension boundary is insufficient:** an extension needs an
  install step and new core surfaces before it can contribute a provider.
- **Exit:** move the `/model/info` reader into an extension once extension
  install is one command and extensions can contribute providers, or sooner
  if a second gateway format is proposed for core.

### API families and endpoints

A provider's `api_family` is `openai_chat_completions`,
`anthropic_messages`, or `openai_responses`. A model entry may set its own
`api_family`, which overrides the provider's for that model; an unlisted
`--model` uses the provider's. A model entry whose `api_family` is unknown is
ignored with a warning.

Every family shares the provider's `base_url`, auth, and headers. Each family
derives its endpoint from `base_url`:

- `openai_chat_completions` appends `/chat/completions`;
- `anthropic_messages` appends `/messages` when `base_url` ends in `/v1`, and
  `/v1/messages` otherwise;
- `openai_responses` appends `/responses`.

A `base_url` that already ends with a family's path is that family's full
endpoint and is used as is. A full endpoint serves only its own family: a
provider whose own `api_family` the full endpoint cannot serve is ignored
with a warning, and so is a model entry that overrides the provider's family
to one the full endpoint cannot serve. So one `/v1` root serves every
family, and a bare host root also serves Messages.

`anthropic_messages` requests use the built-in Anthropic Messages adapter
(request shape, `anthropic-version: 2023-06-01`, SSE parsing). Auth is the
provider's: `auth_header: true` sends `Authorization: Bearer <api_key>`, and
an `x-api-key` entry in `headers` sends the key the way Anthropic's own API
expects. A configured `anthropic-version` header replaces the default. A
model that declares `supports_reasoning: true` requests adaptive thinking
with the same effort mapping as the built-in `anthropic` provider; any other
model requests no thinking. When a request sets no `max_output_tokens`,
`max_tokens` is 32000, capped at the model entry's declared
`max_output_tokens`. Errors read from the stream name the custom provider.
Recorded thinking blocks are replayed with their signatures only to the same
provider and model; reasoning recorded without a signature, or with a Chat
Completions `reasoning_details` array in place of one, is not replayed. The
same rule applies to the built-in `anthropic` provider.

`openai_responses` requests use the built-in ChatGPT Responses adapter
(request shape with `store: false`, flat function tools, SSE parsing) without
its ChatGPT-only headers or its check of the effort against the built-in
catalog, so an effort the gateway rejects fails as a rejected request. Auth is
the provider's. A model that declares `supports_reasoning: true` requests
`reasoning: {effort, summary: "auto"}` with the same effort mapping as the
built-in `chatgpt` provider; any other model sends no `reasoning` field. A
request's `max_output_tokens` is sent as `max_output_tokens`. Errors read from
the stream name the custom provider. As on the built-in `chatgpt` provider,
recorded reasoning is not replayed on later turns.

`compat` applies only to `openai_chat_completions`; on a Messages or
Responses model it is ignored with a warning. On every family, an HTTP
rejection is classified by status and its body is not read.
