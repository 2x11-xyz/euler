# ADR 0025: Gateway provider discovery and login

## Status

Accepted (2026-11-05). Proposed 2026-10-27 and revised 2026-10-29, when the
maintainer chose the direction: a product-neutral `discovery` setting in
`providers.json`, run by core, plus `euler login --provider <custom-id>` for
custom providers. An extension that contributes providers is an alternative
(see Alternatives). The questions the direction left open were resolved by
the maintainer on 2026-11-05 (question 5 changed from its proposal) and are
listed at the end; the Decision text cites them as "resolved question n".

## Context

A user reaches an OpenAI-compatible gateway today by hand-writing
`~/.euler/providers.json`: one provider with `api_family`, `base_url`, a
secret spec for the key, and a hand-maintained `models` array. This works
against a LiteLLM proxy (verified 2026-10-27 with three models under the
default sandbox, key absent from provenance), but every model must be listed
by hand and every teammate must learn the file format. The proxy already
publishes that data: `GET /model/info` returns one row per deployment
(several hundred KB on a production-sized proxy) with `litellm_provider`,
`mode`, `supported_endpoints`, token limits, and reasoning and tool support;
`GET /v1/models` returns bare ids on any OpenAI-compatible gateway.

What already shipped, per the provider contract: custom providers accept
`openai_chat_completions`, `anthropic_messages`, and `openai_responses`, a
model entry may override the provider's `api_family`, and one `/v1` base URL
serves every family. `models` is optional when `default_model` is set, and
`providers.json` lives in the Euler home. The auth file
(`<euler home>/auth.json`) already holds optional stored API-key credentials
keyed by provider id, but `euler login` covers only ChatGPT, so a custom
provider's key must be a literal, a `$NAME` spec, or a `!command` spec.

The goal: a teammate runs one command, enters a base URL and a key, and the
gateway's models appear, routed to the right family, with nothing to install
and no file to edit.

## Decision

### Setup

```text
$ euler login --provider litellm
Base URL: https://litellm.example.com
API key: ********
Found 42 models from /model/info: 12 anthropic_messages,
3 openai_responses, 27 openai_chat_completions.
Added provider litellm to ~/.euler/providers.json.
Stored the key for litellm in ~/.euler/auth.json.
```

The models then appear in `/model` and `euler models`. Users who keep keys
in the environment or a password manager can write the same entry by hand
with an `api_key` spec instead of logging in.

### 1. Login for custom providers

`euler login --provider <id>`, for an id that is not a built-in provider,
and `/login <id>` in the TUI (resolved question 2). Built-in ids keep
today's behavior.

- **No entry for `<id>` in `providers.json`:** prompt for the base URL
  (visible) and the key (hidden). Normalize the base URL to `<root>/v1`
  (trailing slashes and a trailing `/v1` removed, then `/v1` appended) and
  print the result; a URL that ends in a family path such as
  `/chat/completions` is refused with an error, and an `http` URL to a
  non-loopback host prints a warning that the key travels unencrypted. Run
  discovery (section 2) with that key. On success, write the key, then the
  entry below, then the cache, and print the counts. The entry follows the
  key so a provider never points at a missing key; if the entry write
  fails, login removes the key it just stored and reports the error. A
  failed cache write is a warning, and the next launch fetches. On
  failure, write nothing and name the
  endpoint and the status or transport error (DNS, connection, TLS,
  timeout); when every request answered 401 or 403, say the key was
  rejected.
- **Entry exists without `api_key`:** prompt for the base URL with the
  current value as the default (Enter keeps it) and for the key, then
  verify and store as above. Login sets `base_url` and `discovery: true`
  and leaves every other field of the entry as written.
- **Entry exists with an `api_key` spec:** fail with an error naming the
  spec and saying to remove `api_key` to use a stored key. A stored key
  never silently overrides configuration.

The entry login writes:

```json
"litellm": {
  "api_family": "openai_chat_completions",
  "base_url": "https://litellm.example.com/v1",
  "auth_header": true,
  "discovery": true
}
```

The entry sends the key as `Authorization: Bearer`, which LiteLLM accepts
for every family. A gateway that wants `x-api-key` for Messages needs a
hand edit to `headers`.

Keys (resolved question 3): the key is stored in the auth file as an API-key
credential under the provider id, with the file's owner-only permissions. A
custom provider with `auth_header: true` and no `api_key` uses it, and the
value seeds the redactor like other auth-file values; a key stored by
`/login` during a session is registered with that session's redactor at
once. An `api_key` spec in `providers.json` wins. A provider that needs a
stored key and has none fails its first request with an error naming
`euler login --provider <id>`. `euler logout --provider <id>` removes the
stored key, leaves the entry, and says so; `euler auth status` lists the
key like other stored keys. The secrets contract's auth-file section widens
from subscription tokens to stored API keys; it stays a plain local file,
not a vault.

Writing `providers.json`: login parses the file, adds or updates the one
entry, keeps every other entry, and replaces the file atomically with its
permissions preserved. A file that fails to parse is left untouched and the
error names it. Login is a command a human runs, so the config-edit
guardrail, which governs agent edits to `providers.json` and the auth file,
is unchanged.

**New prompts (flagged):** the base URL and key prompts in `euler login`
and `/login <id>`, which a user runs to set up. No new block is added.
`euler logout`, `euler auth status`, `euler models`, and `euler models
refresh` gain custom-provider output but no prompt.

### 2. The `discovery` setting

A custom provider may set `"discovery": true` (resolved question 1). Core
then adds the gateway's models to the provider's. The provider contract's rule
that a provider needs `models` or `default_model` gains a third way to
satisfy it: `discovery: true`. Configured `models` stay listed alongside
discovered ones, a configured entry with a discovered id replaces the
discovered descriptor whole, and `default_model` may name a discovered id.
A discovery provider's `base_url` must end in `/v1` (trailing slashes
allowed); any other shape ignores the provider with a warning that says so.

Requests go to the gateway root (`base_url` with trailing slashes and the
`/v1` removed), only at the base URL's origin, with the provider's auth and
header specs. Redirects are not followed, each request has a 5-second deadline,
one discovery run has a 10-second overall deadline, and a body over 4 MiB
fails the request. Failure messages name the endpoint and the status or
transport error, never a response body.

1. **Discover.**
   - `GET <root>/model/info`. A 200 whose body is a list of rows with
     `model_name` is the source. A wildcard route (a `model_name`
     containing `*`) expands to the `GET <root>/v1/models` ids it matches,
     each taking the metadata of the matching wildcard row with the longest
     literal prefix; an explicit row wins and unmatched ids are not added.
     If the expansion request fails, the explicit rows stand and the
     warning names the dropped wildcard routes (resolved question 6). If
     no chat model survives mapping, discovery fails and says the gateway
     lists no chat models.
   - Any other answer (a non-200 status, a body that is not a list of
     rows, an empty list): `GET <root>/v1/models`. Gateways without
     `/model/info` land here, and every id they list appears, including
     non-chat ones, which fail with the gateway's error when selected.
   - A non-200, malformed, or empty answer from `/v1/models` fails
     discovery and changes nothing.
2. **Map.** Drop non-chat modes (`embedding`, `image_generation`,
   `completion`, audio, rerank). Group rows by `model_name`; drop and list
   routes that fail the `providers.json` model-id rule. Merge conservatively
   across deployments: `context_window_tokens` is the minimum
   `max_input_tokens`, `max_output_tokens` the minimum `max_output_tokens`,
   and `supports_tools` (`supports_function_calling`) and
   `supports_reasoning` are true only when every deployment says so; a field
   missing anywhere is omitted. Ids from `/v1/models` carry no claims and
   are treated like an unlisted `--model`: the provider's family and no
   declared metadata.
3. **Route** each model (resolved question 5) through its per-model
   `api_family`, keeping a family only when every deployment of the route
   agrees:
   - `anthropic_messages` when `mode` is `chat` and the deployment is a
     Claude model behind an Anthropic-hosting provider, which is either of:
     - `litellm_provider` is `anthropic` or `vertex_ai-anthropic_models`,
       which host only Claude;
     - `litellm_provider` is `bedrock`, `bedrock_converse`, or
       `vertex_ai`, and the underlying model contains `claude`, ignoring
       case. The underlying model is the row's `litellm_params.model`, or
       its `model_info.key` when that is absent; for an id expanded from a
       wildcard route, it is the expanded id instead.

     `supported_endpoints` is not consulted. This path replays Claude
     thinking blocks, which chat completions drops, wherever the gateway
     hosts Claude.
   - `openai_responses` when `mode` is `responses`. A chat route whose
     `supported_endpoints` lists `/v1/responses` stays on chat completions.
   - `openai_chat_completions` otherwise, including disagreeing routes,
     which are listed. Every `/v1/models` id uses the provider's family.

   A configured `models` entry with a discovered id replaces the
   discovered descriptor whole, so a user can override the family for one
   model.

   Only models routed to chat completions that support reasoning get
   `compat.reasoning.request_format: "openai_reasoning_effort"`, since the
   other families ignore `compat`.

   All families share the provider's `<root>/v1` base URL.

Not included: `/health` as a further fallback (it makes the proxy probe
every deployment and reports backend names rather than routes), pricing,
budgets, vendor-specific request quirks, image models, and SSO login.

### 3. Cache and refresh

A full successful discovery is written atomically to
`<euler home>/cache/provider-discovery/<provider-id>.json`, owner-only,
recording the format version, base URL, fetch time, source endpoint, and
mapped models. A cache whose format version or base URL does not match the
entry is a miss, so a refresh that finishes after a `base_url` edit writes
a cache the next launch ignores. Concurrent writers are last-writer-wins.
Startup routes from the cache. A failed or empty discovery never replaces a
cache.

Refresh is automatic, as approved for the earlier Q12, and the same for
every key type, including `!command` specs, and for interactive and headless
runs alike (resolved question 4). Discovery sends the key and header specs,
so it resolves them the way a model request in that launch does. A new or
changed `!command` that needs first-execution approval asks in an
interactive launch (the wait does not count toward the deadline); in a
headless launch it is not run, and discovery is skipped with a warning
naming `euler models refresh`. The secrets contract gains that a discovery
refresh needs the provider's secrets; its rule that availability checks only
test auth presence still holds for `/model` and `euler models`, which read
the cache.

- **Cache older than 24 hours:** startup refreshes it in the background. The
  current session uses the cached list; later sessions see the update.
- **No cache:** startup fetches in the foreground under the 10-second
  overall deadline. On failure the provider loads with only its configured
  `models` or `default_model`, or with no listed models (an explicit
  `--model` still works as an unlisted model), and a warning names the
  endpoint, the status or transport error, and `euler models refresh`.
- **Failed refresh:** the cached list keeps routing, and the warning is
  shown once per launch. Until a refresh succeeds, every launch retries,
  so a `!command` key may run more than once a day while the gateway is
  down.

The existing `euler models refresh` also refreshes every discovery provider
now and reports counts per family, ids added and removed, and failures.
`euler models` lists discovered models with their provider and family.
Login refreshes the cache when it stores a key, so a key change takes
effect at once.

### 4. Recording

Selecting a discovered model records `model_source` on `session.start`,
`model.switched`, and `session.resumed`: `discovered`, with the cache's
source endpoint, fetch time, and the SHA-256 of the cache file (resolved
question 7). It is an additive optional field, which the events contract
allows without a version bump. A resume after a refresh proceeds, records
the new source, and reports the change; a model the refresh removed is
treated like an unlisted `--model` and fails with the gateway's error if the
gateway dropped it.

### 5. Boundary exception

The setting, the cache, refresh, and login are product-neutral, and
`/v1/models` is the generic OpenAI-compatible listing. Reading `/model/info`
means core knows one product's endpoint and field names, which the boundary
contract reserves for extensions, so it is a documented exception:

- **Invariant:** adopting a gateway takes one command, with nothing to
  install.
- **Why the extension boundary is insufficient:** an extension needs an
  install step (install consent and git sources are unimplemented, so setup
  is clone, link, enable, with Python on a Unix host) and six new core
  surfaces before it can contribute a provider.
- **Exit:** move the `/model/info` reader into an extension once extension
  install is one command and extensions can contribute providers, or
  sooner if a second gateway format is proposed for core.

## Alternatives

- **A `litellm` extension contributing provider configuration (formerly
  Option A):** the extension would run discovery and write a provider
  contribution document that core loads at startup. It keeps LiteLLM out
  of core, but setup needs an install step (clone, link, enable, then a
  sync command), and core would need six new surfaces to serve one
  integration: the contribution document, its validation and precedence, a
  host `http_get` with host-resolved auth headers, declared environment
  passthrough, session-less `euler extension run` with a run log for its
  capability decisions, and `provider_source` recording. Its custom-provider
  login is kept here, in section 1. The questions that only this option
  raised (host `http_get`, environment passthrough, session-less extension
  runs, blocking model tools and hooks from writing contributions) are out
  of scope.
- **Generic `/v1/models` alone:** ids only, no limits or claims, no
  Messages routing. Kept as the fallback step.
- **A machine-maintained model list in `providers.json`:** mixes machine and
  hand edits, the problem ADR 0016 removed from `models.json`. Login writes
  one entry once; the model list lives in the cache.
- **Fetching on every launch with no cache:** slows every start and fails
  offline. The cache with a daily refresh avoids both.
- **An extension-supplied transport:** ruled out by ADR 0015 and the
  provider contract.
- **Gateway models in the ADR 0016 catalog:** the catalog is public,
  built-in-only metadata; gateway membership is account-scoped.

## Rollout

1. Stored keys for custom providers: `euler login`, `logout`, and `auth
   status` for an existing entry, and auth resolution, with `provider.md`
   (a custom provider's key may come from the auth file), `secrets.md`, and
   the quickstart guide. Useful on its own for
   hand-written providers.
2. The `discovery` setting, mapping, routing, cache, refresh, `euler models`
   changes, and `model_source`, with `provider.md` (the models rule, the
   `/v1` base URL rule, and the boundary exception), `secrets.md` (refresh
   resolves the key), and `events.md`. Fixture tests from recorded
   `/model/info` and `/v1/models` bodies, covering Claude behind each
   Anthropic-hosting provider, and one live smoke per family against a
   LiteLLM proxy.
3. First-run login that prompts for the base URL and writes the entry, with
   the quickstart and headless guides.

## Consequences

- A teammate's setup is one command, with no install and no model list.
- Claude routes to Messages wherever the gateway hosts it (Anthropic,
  Bedrock, or Vertex) and keeps its thinking blocks across turns.
- Core carries one gateway's metadata format as a documented exception, and
  fixes to it ship with Euler releases.
- Startup may run a `!command` key spec about once a day, and on every
  launch while refresh keeps failing.
- `providers.json` gains entries written by a command, not only by hand.

## Resolved questions (2026-11-05)

Settled earlier: Option B plus custom-provider login (2026-10-29), and, from
the earlier Q12, the documented boundary exception, automatic daily refresh
for every key type with a short timeout and cache fallback, and whole
replacement by user `models` entries. Entries marked **changed** differ from
the proposal in the draft.

1. The setting is a boolean, `"discovery": true`, with the fixed chain
   `/model/info` then `/v1/models`. As proposed. One value is easiest to
   write, keeps product names out of the config, and gateways without
   `/model/info` fall through on their own. A string can replace it later
   if a second format arrives.
2. Login: a new id prompts for the base URL and key, runs discovery, and
   writes the key, entry, and cache only on success; an existing entry
   prompts with its base URL as the default and gains `discovery: true`; an
   entry with an `api_key` spec fails with an error; an `http` URL to a
   non-loopback host warns rather than fails; `/login <id>` works the same
   in the TUI. As proposed. Verifying before writing turns a bad URL or key
   into one clear error instead of a broken provider.
   Note (2026-11-05, rollout step 3): login refuses an `http` URL to a
   non-loopback host, since `providers.json` ignores such an entry, and an
   existing entry whose gateway lists no models may store the key with the
   entry left as written.
   Note (2026-11-06): login sets `default_model` on an entry that has none,
   so the first `euler --provider <id>` after login needs no `--model`. It
   picks the newest `anthropic_messages` model whose id contains `sonnet`
   (ignoring case), else the newest `anthropic_messages` model, else the
   newest chat completions model that declares tool support, never a
   Responses model, and prints the pick. Newest prefers an id starting with
   `claude-sonnet` (then `claude`), then the highest id in numeric-aware
   order with a trailing 8-digit date ignored, then the shorter id, then
   the later date; when no model qualifies it sets none. An existing `default_model` is never replaced.
   A launch that needs the default of a discovery provider without one (a
   hand-written entry) picks by the same rule and prints a one-line notice
   instead of failing; with no qualifying model it fails as before.
3. The key is stored in `auth.json` under the provider id, an `api_key` spec
   wins, and `logout` removes only the key. As proposed.
4. Stale caches refresh in the background, a missing cache is fetched in the
   foreground under a 10-second overall deadline, and headless runs follow
   the same rule. As proposed. A first headless run works without a
   separate refresh step (unless its key is a `!command` that still needs
   first-execution approval), and no launch waits more than 10 seconds on
   the network for discovery.
5. **Changed.** Claude routes to `anthropic_messages` whenever LiteLLM
   metadata identifies a Claude model behind an Anthropic-hosting provider
   (`anthropic`, `bedrock`, `bedrock_converse`, `vertex_ai`, or
   `vertex_ai-anthropic_models`), not only behind `anthropic`, and
   `supported_endpoints` is not consulted for it (section 2, step 3).
   Thinking continuity matters wherever Claude is hosted, and both the
   `anthropic_messages` and `openai_responses` families are live-verified
   as custom-provider families (#263, #264). Responses stays narrow:
   `openai_responses` only on `mode: responses`, not when
   `supported_endpoints` lists `/v1/responses` and not through LiteLLM's
   Responses bridge. Everything else uses chat completions. A configured
   `models` entry overrides the family for one model.
6. When wildcard expansion fails, the explicit rows stand with a warning
   rather than failing discovery. As proposed. A partial list that routes
   beats no update.
7. `model_source` is recorded when a discovered model is selected. As
   proposed.
