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
resolution as auth, sessions, extensions, and skills. A missing file means no
custom providers. When the Euler home cannot be resolved (for example a
relative `EULER_HOME`), no custom providers load and line-oriented commands
print the resolution error as a `warning: ignored provider config:` line on
stderr.

Each provider lists its models in a non-empty `models` array of objects with
an `id`. A provider may omit `models` when it sets `default_model`; the parser
then records one model with that id and no declared metadata (context window,
output limit, tool and reasoning support, compat), which is how an unlisted
`--model` is treated. A provider whose `models` is empty, is not an array, or
holds no valid entries, or that omits `models` without a valid string
`default_model`, is ignored with a warning that names both fixes.

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
