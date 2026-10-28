# ADR 0025: Extension-contributed provider configuration

## Status

Proposed (2026-10-27). Draft for Eli's review; the open questions at the end
carry proposed answers, marked "(proposed, Qn)" in the Decision text. Depends
on the custom-provider work in flight: `anthropic_messages` and
`openai_responses` as custom-provider `api_family` values with a per-model
`api_family` override and its base-URL rule
(`core/custom-provider-anthropic-messages`,
`core/custom-provider-openai-responses`), optional `models`
(`core/providers-optional-models`), and `providers.json` under `EULER_HOME`
(`core/providers-euler-home`). This ADR emits the shapes those changes define;
it does not define them. No contract changes until the rollout PRs land.

## Context

A user reaches an OpenAI-compatible gateway today by hand-writing
`~/.euler/providers.json`: one provider with `api_family`, `base_url`, a
secret spec for the key, and a hand-maintained `models` array. This works
against a LiteLLM proxy (verified 2026-10-27 with three models under the
default sandbox, key absent from provenance), but every model must be listed
by hand and its limits copied from somewhere. The proxy already publishes
that data: on Eli's proxy `GET /model/info` returns 61 deployment rows
(about 400 KB) carrying `litellm_provider`, `mode`, `supported_endpoints`,
`max_input_tokens`, `max_output_tokens`, `supports_reasoning`, and
`supports_function_calling`; 46 distinct routes are chat-capable. `GET
/v1/models` returns 51 bare ids.

Pi solves this with `pi-provider-litellm` (v4.5.0, MIT), an in-process
TypeScript module that discovers models and registers a provider whose wire
protocol is pi-ai's. Euler cannot host that shape: ADR 0015 forbids in-process
plugin loading, and the provider contract makes adapters the only owners of
transport, so a shim would bypass the attempt and liveness boundary and
redaction. What transfers is the knowledge: the discovery chain, the metadata
mapping, and per-model protocol routing.

The boundary contract puts LiteLLM's nouns (`/model/info`, `litellm_provider`,
`mode`) in an extension. What core lacks is a sanctioned way for an extension
to hand the provider registry data, plus a way to authenticate its discovery
requests without holding the key.

## Decision

### 1. Contributions are provider documents, never transport

An enabled extension may contribute one provider document: a JSON object with
exactly the `providers.json` schema (`version`, `providers`). It can define
endpoints, auth header choice, secret specs, default model, model metadata,
`compat`, and per-model `api_family`, and nothing else. The core
`api_family` adapters stay the only wire implementations; a contribution
selects among them and cannot add one.

A contribution may define only provider ids equal to its extension id or
prefixed `<extension-id>_` (proposed, Q1). Extension ids are unique and
cannot contain `_`, while provider ids can, so two extensions cannot
collide and every contributed provider names its owner. Built-in ids stay
reserved, as in `providers.json`.

### 2. Storage and host API

As with ADR 0022 config documents, the host derives `<id>` from the calling
command's registration; an extension reads and writes only its own
contribution. The host stores the document at
`<euler home>/extensions/<id>/providers.json`, next to the ADR 0022 user
config document, owner-only (`0600`) because it may
hold `!command` specs. It is machine-managed: the extension replaces it whole
and the user does not edit it. Writes use a same-directory temp file and
rename, as ADR 0022 config writes do, and the file shares the extension
directory's lifecycle. The user's own `providers.json` is never written.

- `HostApi::read_provider_contribution()` returns the stored document (or
  `null`) and its current load warnings, including shadowing (Decision 4).
  No capability.
- `HostApi::write_provider_contribution(document | null)` replaces or removes
  it. It requires `config-write` and is allowed only from a `user` command a
  human invoked (TUI, headless `extension_run`, `euler extension run`). Model
  tools and lifecycle hooks are refused with a clear error. This is stricter
  than ADR 0022's user tier because the document changes which endpoints
  Euler calls and which secret commands it runs. The write returns the same
  warnings the read does, and the host adds to the command output, verbatim,
  every secret spec that is new or changed since the previous document, so
  the human who ran the command sees what Euler will execute or read.

"Human-invoked" is the command origin the host sees, as in ADR 0022. A model
that runs `euler extension run` through `run_shell` is confined by the
default sandbox (no network, real home read-only); the full-access preset
carries the same residual risk ADR 0022 accepts for project-tier writes.

**New block (flag for review).** The refusal from model tools and hooks is a
new block. It adds no prompt: a human-invoked write follows the ordinary
`config-write` modes.

### 3. Validated exactly like `providers.json`, strictly at write

Validation reuses `ProviderConfigRegistry::with_json`, the same parser and
rules as `providers.json`. At write time any validation warning refuses the
whole write, including an `api_family` this Euler does not know (a newer
extension on an older Euler), and the error lists every warning, so a bad
document never lands. A precedence note such as shadowing is not a
validation warning. At load time the same parser runs leniently with
warnings, like `providers.json`.

Three rules are added at write: the id rule (Decision 1); a size cap of
256 KiB; and secrets only as specs. `api_key` and every header value must be
an environment (`$NAME`, `${NAME}`) or command (`!command`) spec. A literal
is refused, and so is any key or string that fails the known-value and
token-shape check `write_extension_config` applies (proposed, Q5). A spec
written by a human-invoked command is user-authored config under the secrets
contract's config-edit guardrail, not agent-authored. It resolves at request
time like any custom provider secret and prompts exactly when the same spec
in `providers.json` would; a contribution adds no prompt of its own. An
oversize document is refused with its size and the cap, and the same cap
applies at load.

Extensions are trusted code (capability contract): one that wants to run a
command can already do so as a process. The verbatim spec echo above exists
for the other risk, a spec built from untrusted data by mistake.

### 4. Precedence

1. Built-in provider ids: reserved against both the user's `providers.json`
   and contributions, as today.
2. The user's `providers.json`: on an id collision the user's entry wins
   whole. No field merge. The contributed provider is ignored with a warning
   naming both files, and the read and write calls report it as shadowed
   (proposed, Q2).
3. Contributions from extensions that are enabled in the registry. A
   contribution from a disabled, removed, or `needs-review` extension does not
   load. A session that selects or resumes such a provider fails with an error
   naming the extension and the fix (enable it, or pick another model)
   (proposed, Q3). The id rule makes contributions disjoint, so no order
   among extensions is needed.

Loading a contribution runs no extension code. Contributions are read at the
same points `providers.json` is read, so a sync, disable, or removal applies
to new and resumed sessions, not to running ones.

### 5. Authenticated discovery without the key: `HostApi::http_get`

Discovery needs the proxy key, and a managed-process extension gets no
ambient secrets. Core adds a narrow, product-neutral network primitive
(proposed, Q4):

- GET only. URLs must be `https`, or `http` to a loopback host (the
  `base_url` scheme rule), with no embedded credentials. No redirects are
  followed; a 3xx is returned as its status, and callers report it as an
  unfollowed redirect.
- Request headers are literal, or `{"secret": "<spec>", "prefix": "Bearer "}`
  (prefix optional) resolved by the host at request time and registered with the session redactor. The value never
  crosses into the extension. Secret headers bind to one origin (scheme,
  host, port) per command invocation: after the first secret-bearing
  request, a secret header sent to any other origin is refused, so a bug that
  builds a URL from proxy data, or follows a redirect by hand, cannot carry
  the key to a second origin. The host adds that origin to the command
  output, next to the spec echo, so the human sees where the key went.
- The response is status, content type, and body. The body is capped at
  4 MiB and passes the session redactor (known values and token shapes)
  before delivery. The deadline defaults to 30 seconds; a caller may ask for
  up to 120.
- Requires `network`, plus `secret-resolve` when a secret header is present.
  Available only to human-invoked `user` commands (same block as Decision 2).
  A managed process can still open sockets itself (capabilities are not an OS
  boundary); `http_get` is the only egress that can carry a host-resolved
  secret.
- Each call is recorded content-free in diagnostics: origin, path, status,
  bytes, duration. No headers, query strings, or bodies.

### 6. Recording

The event that selects a provider (session start, or a model-target change)
records `provider_source` for contributed providers: the extension id and
the SHA-256 of the contribution document. A postmortem can tell which
snapshot routed a run (proposed, Q8). User `providers.json` providers record
nothing new.

## The `litellm` extension (first consumer, euler-extensions)

A managed-process extension `litellm` declares `network`, `secret-resolve`,
and `config-write`, and two `user` commands. It declares no model tools and
no lifecycle hooks, so it adds nothing to the prompt and the model cannot
trigger it.

### `litellm.sync`

First run takes JSON input:
`{"base_url": "https://proxy.example.com", "api_key": "<secret spec>"}`,
optionally `"provider"` (default `litellm`, or `litellm_<name>` for a second
proxy), `"default_model"`, and `"dry_run"`. The proxy root is `base_url` with
trailing slashes and a trailing `/v1` removed. An `api_key` that is not an
environment or command spec is refused before anything is stored, with the
same suggestion as Decision 3. The settings and the last sync summary go to
the extension's ADR 0022 user config, so later runs take no input; input
on a later run replaces the stored settings. A `dry_run` stores nothing.
Then:

1. **Discover** through `http_get` with
   `Authorization: {"secret": <spec>, "prefix": "Bearer "}`, ported from pi's
   `discoverModels`:
   - `GET <root>/model/info`. A 200 with at least one row is the source. A
     wildcard route (a `model_name` containing `*`) expands to the concrete
     `GET <root>/v1/models` ids it matches, each inheriting the row's
     metadata; an explicit row wins over a wildcard match, ids that match no
     row are not added, and a failed expansion fetch fails the sync.
   - On 401, 403, 404, or an empty list: `GET <root>/v1/models`. An empty
     list here ends the chain.
   - On 401, 403, or 404 from that: `GET <root>/health` with the 120-second
     deadline, taking the route names of its healthy endpoints. `/health`
     makes LiteLLM probe every deployment, so it is slow and the last resort.
   - Any other status, timeout, or malformed body, or zero usable models at
     the end, fails the sync with the endpoint and status named and writes
     nothing.
2. **Map** (`/model/info` rows): drop rows whose `mode` is a known non-chat
   mode (`embedding`, `image_generation`, `completion`, audio, rerank). Group
   the rest by `model_name`, since one route can have several deployments.
   Routes that fail the parser's model-id rule are dropped and listed.
   Merge conservatively across a route's deployments:
   `context_window_tokens` is the minimum `max_input_tokens`,
   `max_output_tokens` the minimum `max_output_tokens`, and `supports_tools`
   (`supports_function_calling`) and `supports_reasoning` are true only when
   every deployment says so. A field missing from any deployment is
   omitted, which makes no claim. Chat-completions models that
   support reasoning get `compat.reasoning.request_format:
   "openai_reasoning_effort"`, matching the configuration verified today.
   `/v1/models` and `/health` results carry ids only and make no capability
   claims, which is the treatment an unlisted model gets today. `status`
   labels them "no metadata".
3. **Route each model to a family**, applying these rules to each deployment
   and keeping the result only when every deployment yields the same family:
   - `anthropic_messages` when `litellm_provider` is `anthropic`, `mode` is
     `chat`, and `supported_endpoints` is null or lists `/v1/messages`. This
     path replays Claude thinking blocks, which chat completions drops today.
   - `openai_responses` when `mode` is `responses` (proposed, Q7).
   - `openai_chat_completions` otherwise, including routes whose deployments
     disagree; the sync output lists those routes.

   Messages is addressed at the proxy root and the OpenAI families at
   `<root>/v1`, expressed through the Phase 1 per-model base-URL rule.
4. **Write** the document with `write_provider_contribution`. A previous
   default model is kept if it still exists, else cleared with a note.
   `dry_run` runs steps 1 to 3, reports the result, and writes nothing.

Output: the discovery source, the model count per family, ids added and
removed since the last snapshot, and any shadowing warning. Not ported from
pi: models.dev enrichment, pi's bundled catalog lookups, vendor compat quirks
(Moonshot, Azure version gating), prices, budgets, MCP, the Skills Gateway,
image models, and SSO login.

### `litellm.status`

Offline. Reports the configured base URL and key-spec kind (environment or
command), the last sync time and source, model count per family, the default
model, and whether the contribution is active, shadowed, or failing
validation. When unconfigured, it prints the exact `litellm.sync` input that
would configure it.

### Key source

- **Today:** the user's `providers.json` uses
  `"api_key": "!jq -r .litellm.key ~/.pi/agent/auth.json"`. Euler runs it per
  request, never copies the value, and redacts it.
- **With the extension:** the user passes the same spec, or `$LITELLM_API_KEY`,
  or `!security find-generic-password -s litellm -w`, once to `litellm.sync`.
  The spec lands in the extension's user config and the contribution's
  `api_key`. The host resolves it for discovery (`http_get`) and for model
  requests (the adapter). The extension never sees the value. It does not
  read pi's `auth.json` unless the user names it in a spec; the unconfigured
  message suggests that spec as an example (proposed, Q6).

### Refresh semantics

- Only `litellm.sync` touches the network (proposed, Q9). No startup, picker,
  background, or headless refresh, following ADR 0016's rule that headless
  runs gain no implicit network dependency.
- A sync replaces the snapshot whole. Models gone from the proxy disappear,
  and new ones appear. A failed or empty discovery writes nothing and keeps
  the last snapshot.
- A stale snapshot still routes. A model the proxy has since removed fails at
  request time with the proxy's error; `status` shows the snapshot's age.
- Running sessions keep the provider config they loaded; new and resumed
  sessions see the new snapshot (Decision 4).

## Alternatives considered

- **Generic `/v1/models` discovery in core.** Rejected. It yields ids only:
  no context window, tool, or reasoning metadata, and no family routing, so
  Claude would stay on chat completions without its thinking replay. Richer
  discovery needs gateway nouns, which the boundary test puts in an
  extension. A discovery call at startup or in the picker would also add an
  implicit network dependency and run `!command` secrets for an availability
  check, which the secrets contract forbids. Each gateway's listing differs
  anyway (LiteLLM, OpenRouter, vLLM, Portkey), so core would pick one and
  still miss metadata.
- **The extension edits the user's `providers.json`.** Rejected. Machine and
  hand edits would share one file, the problem ADR 0016 removed from
  `models.json`.
- **An extension-supplied transport, like pi's module.** Rejected by ADR 0015
  (no in-process plugins) and the provider contract (adapters own transport).
- **Proxy models in the ADR 0016 catalog.** Rejected. That catalog is public
  metadata for built-in adapters, and a private proxy's membership is
  account-scoped.
- **`HostApi::resolve_secret` returning the value.** Smaller, but the key
  would enter extension memory and every extension would reimplement
  authenticated fetches.
- **Print and paste.** Works today with no core change, but gives no refresh
  and leaves hand merges. It stays the fallback.

## Rollout

1. **Euler:** contribution storage, read and write host APIs, strict write
   validation, load precedence, `provider_source`, and `http_get`. Contracts:
   `extension-sdk.md`, `capabilities.md`, `secrets.md`, the provider-config
   section of `provider.md`, and `events.md`. Tests cover each precedence row,
   strict-write refusal per rule, the model-tool and hook refusals, a disabled
   extension's error, a secret header never reaching the extension, the
   one-origin rule, the `!command` echo, and the redirect, size, and scheme
   limits.
2. **euler-extensions:** `litellm`, with the MIT notice for the ported logic,
   fixture tests built from recorded `/model/info`, `/v1/models`, and `/health`
   bodies (mapping, grouping, routing, fallback statuses), and one live smoke
   per family against a proxy.
3. **Docs:** the headless guide shows `litellm.sync` next to the hand-written
   `providers.json` example.

## Consequences

- Any gateway can get a removable discovery extension without core learning
  its name. LiteLLM is the first.
- The user's `providers.json` stays user-owned and keeps the last word.
- Core gains two small substrates: provider contributions and an
  authenticated GET. Both are limited to human-invoked commands.
- Eli's current hand-written `litellm` entry shadows the contribution until
  he removes or renames it; `status` and `sync` say so.

## Open questions for Eli

1. Restrict contributed provider ids to `<extension-id>` or
   `<extension-id>_*`? **Proposed: yes.** It rules out cross-extension
   collisions and makes ownership visible in every provider id.
2. On a collision with the user's `providers.json`, does the user's entry win
   whole, with a warning? **Proposed: yes**, no field merge.
3. Load contributions only while the extension is enabled, and fail a session
   on a disabled extension's provider with an error naming it? **Proposed:
   yes.**
4. Use host-mediated `http_get` with secret-spec headers, limited to
   human-invoked commands and to one secret origin per invocation, rather
   than `resolve_secret`? **Proposed: yes.** The key never enters extension
   memory and cannot leave for a second origin.
5. Refuse literal keys in contributions? **Proposed: yes**, with an error that
   suggests a `$NAME` or `!command` spec. A literal stays allowed in the
   user's own `providers.json`.
6. Should the extension auto-detect pi's `auth.json`? **Proposed: no.** The
   unconfigured message prints the `!jq` spec for the user to pass.
7. Route to Responses only on `mode: responses`, or also when
   `supported_endpoints` lists `/v1/responses` (pi's wider rule, which would
   move most GPT routes)? **Proposed: `mode: responses` only** for v1;
   revisit after the `openai_responses` live checks.
8. Record `provider_source` (extension id and document digest) when a
   contributed provider is selected? **Proposed: yes.**
9. Keep refresh explicit (`litellm.sync` only)? **Proposed: yes.**
10. Accept the new block: `write_provider_contribution` and `http_get` refuse
    model tools and lifecycle hooks instead of prompting? **Proposed: yes.**
    No workflow needs the model to change providers, and a refusal is a
    clearer error than a prompt that unattended runs would deny anyway.
