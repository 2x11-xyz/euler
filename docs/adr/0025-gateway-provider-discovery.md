# ADR 0025: Gateway provider discovery

## Status

Proposed (2026-10-27). Draft for the maintainer's review. It specifies two
options, a core discovery setting (Option A) and extension-contributed
provider configuration (Option B). Question 1 at the end recommends one; the
other questions carry proposed answers, marked "(proposed, Qn)" in the text.

Both options depend on custom-provider work in flight: `anthropic_messages`
and `openai_responses` as custom-provider `api_family` values with a
per-model `api_family` override and its base-URL rule
(`core/custom-provider-anthropic-messages`,
`core/custom-provider-openai-responses`), optional `models`
(`core/providers-optional-models`), and `providers.json` under `EULER_HOME`
(`core/providers-euler-home`). This ADR emits the shapes those changes
define; it does not define them. No contract changes until the rollout PRs
land.

## Context

A user reaches an OpenAI-compatible gateway today by hand-writing
`~/.euler/providers.json`: one provider with `api_family`, `base_url`, a
secret spec for the key, and a hand-maintained `models` array. This works
against a LiteLLM proxy (verified 2026-10-27 with three models under the
default sandbox, key absent from provenance), but every model must be listed
by hand and its limits copied from somewhere. The proxy already publishes
that data. On a production-sized proxy, `GET /model/info` returns dozens of
deployment rows, several hundred KB in total, carrying `litellm_provider`,
`mode`, `supported_endpoints`, `max_input_tokens`, `max_output_tokens`,
`supports_reasoning`, and `supports_function_calling`. `GET /v1/models`
returns bare ids.

The goal is that a team adopts any LiteLLM proxy with minimal setup, the way
`pi-provider-litellm` (v4.5.0, MIT) works in pi: set a base URL and a key, and
the models are discovered. That package is an in-process TypeScript module
whose wire protocol is pi-ai's. Euler cannot host that shape: ADR 0015
forbids in-process plugin loading, and the provider contract makes adapters
the only owners of transport. What transfers is the knowledge: the discovery
chain, the metadata mapping, and per-model protocol routing.

Two facts constrain where that knowledge can live:

- **Distribution.** Today `euler extension install` takes only a local
  directory, and installed packages stay inert; only linked packages run.
  ADR 0015's git sources are not implemented, and prebuilt binaries are an
  ADR 0015 non-goal. Using an extension therefore means cloning
  euler-extensions, building it (Rust toolchain) or creating its virtual
  environment (Python), then `link` and `enable`. Managed-process
  extensions also run only on Unix hosts today.
- **Boundary.** The boundary contract keeps product endpoints out of core,
  while `EULER.md` assigns wire compatibility to provider adapters, which
  live in core and already carry vendor knowledge (`openai_chat_completions`,
  `anthropic_messages`, the built-in OpenRouter and xAI adapters). Those
  name wire protocols many vendors implement; LiteLLM's `/model/info` is one
  product's management API. ADR 0016 says provider-specific discovery stays
  outside the Euler runtime, for the built-in catalog.

## Adoption requirements

Any answer must deliver:

1. **Onboarding:** one command or a few config lines; never a hand-written
   model list.
2. **Key entry without pi:** an environment variable, an OS keychain, or an
   Euler-managed credential.
3. **Refresh:** new proxy models appear without editing config, and Euler
   stays usable offline and headless.
4. **Distribution:** works for someone without a Rust toolchain.

## Discovery, mapping, and routing (either option)

The `litellm` dialect is a port of pi's `discoverModels`. The proxy root is
the configured base URL with trailing slashes and a trailing `/v1` removed.
Every request carries the provider's key as `Authorization: Bearer`.

1. **Discover.**
   - `GET <root>/model/info`. A 200 with at least one row is the source. A
     wildcard route (a `model_name` containing `*`) expands to the concrete
     `GET <root>/v1/models` ids it matches, each inheriting the row's
     metadata; an explicit row wins over a wildcard match, ids that match no
     row are not added, and a failed expansion fetch fails discovery.
   - On 401, 403, 404, or an empty list: `GET <root>/v1/models`. An empty
     list here ends the chain.
   - On 401, 403, or 404 from that: `GET <root>/health`, with a longer
     deadline, taking the route names of its healthy endpoints. `/health`
     makes LiteLLM probe every deployment, so it is slow and the last resort.
   - Any other status, a redirect (never followed), a timeout, a malformed
     body, or zero usable models fails discovery with the endpoint and status
     named, and changes nothing. When every step answered 401 or 403, the
     error says the key was rejected.
2. **Map** `/model/info` rows. Drop known non-chat modes (`embedding`,
   `image_generation`, `completion`, audio, rerank). Group the rest by
   `model_name`, since one route can have several deployments; routes that
   fail the `providers.json` model-id rule are dropped and listed. Merge
   conservatively across a route's deployments: `context_window_tokens` is
   the minimum `max_input_tokens`, `max_output_tokens` the minimum
   `max_output_tokens`, and `supports_tools` (`supports_function_calling`)
   and `supports_reasoning` are true only when every deployment says so. A
   field missing from any deployment is omitted, which makes no claim.
   Chat-completions models that support reasoning get
   `compat.reasoning.request_format: "openai_reasoning_effort"`, matching the
   configuration verified today. Ids from `/v1/models` or `/health` carry no
   metadata and make no capability claims, the treatment an unlisted model
   gets today.
3. **Route** each model to a family by applying these rules to each
   deployment and keeping the result only when every deployment yields the
   same family:
   - `anthropic_messages` when `litellm_provider` is `anthropic`, `mode` is
     `chat`, and `supported_endpoints` is null or lists `/v1/messages`. This
     path replays Claude thinking blocks, which chat completions drops today.
   - `openai_responses` when `mode` is `responses` (proposed, Q4).
   - `openai_chat_completions` otherwise, including routes whose deployments
     disagree, which are listed.

   Messages is addressed at the proxy root and the OpenAI families at
   `<root>/v1`, through the per-model base-URL rule.

Not ported: models.dev enrichment, pi's bundled catalog lookups, vendor
compat quirks (Moonshot, Azure version gating), prices, budgets, MCP, the
Skills Gateway, image models, and SSO login.

## Option A: a discovery setting in `providers.json`

A custom provider gains one optional field, `discovery`, naming a listing
dialect the way `api_family` names a wire protocol:

```json
{
  "version": 1,
  "providers": {
    "litellm": {
      "api_family": "openai_chat_completions",
      "base_url": "https://litellm.example.com/v1",
      "auth_header": true,
      "api_key": "$LITELLM_API_KEY",
      "discovery": "litellm"
    }
  }
}
```

- **Dialects.** `openai_models` reads `GET <base_url>/models` for ids only,
  with the provider's `api_family`, and serves any OpenAI-compatible gateway.
  `litellm` runs the chain above. The code lives in `euler-provider` beside
  the adapters. No endpoint is hardcoded; the user supplies the URL.
- **Merge.** With `discovery` set, `models` is optional. Discovered models
  fill the list; a `models` entry with the same id replaces the discovered
  descriptor whole (proposed, Q6), and `default_model` is the user's. The
  user's file is never written.
- **Cache.** A full successful discovery is written atomically (temp file
  and rename; concurrent refreshes, last writer wins) to
  `<euler home>/cache/provider-discovery/<provider-id>.json`: a format
  version, base URL, dialect, `api_family`, time, and mapped models, with no
  secrets. A cache whose version, base URL, dialect, or `api_family` differs
  from the config is ignored, and entries pass the `providers.json` model
  parser at load. Startup and the picker read only the cache. The cache does
  not identify the key: after a key change the list can be stale until the
  next refresh, and a model the new key cannot use fails with the proxy's
  error.
- **Refresh** (proposed, Q3). `euler models refresh`, which today checks the
  ADR 0016 catalog, also refreshes every provider with `discovery`
  (`--provider <id>` for one). An interactive launch refreshes in the
  background when a cache is missing or older than 24 hours, as ADR 0016
  does for the catalog, and reports the result in the status line; the
  picker shows the provider with a refreshing note until it lands. Headless
  runs never fetch: with a cold cache, `--model <id>` still works with the
  defaults of an unlisted model, and without `--model` the provider's
  `default_model` is used or the run fails naming both fixes. A failed
  refresh keeps the last cache. Running sessions keep the list they loaded.
- **Secrets.** The automatic background refresh runs only for keys that
  need no shell command (an environment variable or the Euler-managed
  credential), so it never executes a `!command` spec and stays inside the
  secrets contract's availability-check rule. A `!command` key refreshes
  when the user selects that provider in the picker or runs
  `euler models refresh`: user actions that need the key, like a model
  call.
- **Transport limits.** `https`, or `http` to a loopback host; redirects
  not followed; body capped at 4 MiB; 30-second deadline (120 seconds for
  `/health`). Each refresh is recorded content-free in diagnostics.
- **Boundary.** `openai_models` reads an OpenAI-standard endpoint and needs
  no exception. The `litellm` dialect puts one product's nouns in core, so it
  is a boundary exception under `boundaries.md` (proposed, Q2): the
  invariant is zero-install adoption of a user-configured gateway; the
  extension boundary is insufficient because no extension installs today
  without cloning and a local build or environment, and only on Unix; the
  exit criterion is one-command, build-free extension install, after which
  the dialect moves to Option B. It also amends ADR 0016's consequence line
  for user-configured providers only. Dialect fixes ship with Euler
  releases.

Onboarding: export `LITELLM_API_KEY` and add the six lines above. No
extension, no toolchain, no new host API, and no new block. The only new
prompt is the optional managed credential (Q5).

## Option B: extension-contributed provider configuration

An extension contributes provider configuration, and a `litellm` extension
in euler-extensions runs the dialect. Core gains two substrates. Each
decision below is proposed under Q7.

### B1. Contributions are provider documents, never transport

An enabled extension may contribute one document with exactly the
`providers.json` schema. It selects among the core `api_family` adapters and
cannot add one. It may define only provider ids equal to its extension id or
prefixed `<extension-id>_`. Extension ids cannot contain `_`, so two
extensions cannot collide, and every contributed provider names its owner.

### B2. Storage and host API

The host derives `<id>` from the calling command's registration, as for
ADR 0022 config documents, and stores the document at
`<euler home>/extensions/<id>/providers.json`: owner-only (`0600`), written
by temp file and rename, machine-managed, and sharing the extension
directory's lifecycle.

- `HostApi::read_provider_contribution()` returns the document (or `null`)
  and its load warnings, including shadowing. No capability.
- `HostApi::write_provider_contribution(document | null)` requires
  `config-write` and is allowed only from a `user` command a human invoked.
  Model tools and lifecycle hooks are refused (**new block**). The host adds
  to the command output, verbatim, every secret spec that is new or changed
  since the previous document.

"Human-invoked" is the command origin the host sees, as in ADR 0022. A model
that runs `euler extension run` through `run_shell` is confined by the
default sandbox (no network, real home read-only).

### B3. Strict validation at write

The write runs `ProviderConfigRegistry::with_json`, and any validation
warning refuses the whole write, including an `api_family` this Euler does
not know. Added rules: the id rule, a 256 KiB cap (also at load), and
secrets only as `$NAME` or `!command` specs. A literal, or a string that
fails `write_extension_config`'s known-value and token-shape check, is
refused. A contributed spec prompts exactly when the same spec in
`providers.json` would. Extensions are trusted code (capability contract);
the spec echo exists for specs built from untrusted data by mistake.

### B4. Precedence

Built-in ids stay reserved. On an id collision the user's `providers.json`
entry wins whole, with a warning naming both files. Contributions load only
while their extension is enabled; a session that selects or resumes a
provider whose extension is disabled fails with an error naming the
extension and the fix. Loading runs no extension code.

### B5. `HostApi::http_get`

GET only, with the Option A transport limits. A header may be
`{"secret": "<spec>", "prefix": "Bearer "}`, which the host resolves and
registers with the redactor, so the key never enters the extension. Secret
headers bind to one origin (scheme, host, port) per invocation, and the host
adds that origin to the command output. The response body passes the
session redactor. Requires `network` (and `secret-resolve` for secret
headers), human-invoked `user` commands only.

### B6. Recording

Selecting a contributed provider records `provider_source`: the extension
id and the SHA-256 of the document.

### The `litellm` extension

It declares `network`, `secret-resolve`, and `config-write`, and two `user`
commands, with no model tools or hooks, so it adds nothing to the prompt.

- `litellm.sync` takes `{"base_url": "...", "api_key": "<secret spec>"}` on
  its first run (optionally `provider`, `default_model`, `dry_run`), stores
  the settings in its ADR 0022 user config, refuses a literal key, runs the
  dialect through `http_get`, and writes the contribution. Output: the
  discovery source, counts per family, ids added and removed, and any
  shadowing. A failed or empty discovery writes nothing.
- `litellm.status` reports offline: settings, key-spec kind, last sync time
  and source, counts per family, and whether the contribution is active,
  shadowed, or invalid.
- Refresh is explicit: only `litellm.sync` touches the network.

Onboarding today: clone euler-extensions, build or create the package
environment, `euler extension link`, `euler extension enable`, then run
`/extension run litellm.sync {...}` in the TUI (headless `euler extension
run` takes a session path). After ADR 0015 git sources land, install becomes
one command, but it still builds on the user's machine.

## Key entry (both options)

The spec grammar already covers the first two; the third is new
(proposed, Q5).

- **Environment variable:** `"api_key": "$LITELLM_API_KEY"`. The documented
  default.
- **OS keychain:** a command spec, for example
  `!security find-generic-password -s litellm -w` on macOS or
  `!secret-tool lookup service litellm` on Linux.
- **Euler-managed credential:** `euler login --provider <custom-id>` reads
  the key from a hidden prompt and stores it in `~/.euler/auth.json`, which
  already holds optional stored API-key credentials, with owner-only
  permissions. A provider with `auth_header: true` and no `api_key` uses it.
  This is a new prompt, but only inside a command the user runs to enter a
  key. It stays a plain local file, not the vault or keychain abstraction
  the secrets contract rules out; the contract's auth-file section widens
  from subscription tokens to user-entered custom-provider keys.

A pi user can point a command spec at pi's credential store. Euler does not
read another tool's credentials unless the user names them in a spec.

## Comparison

| | Option A: core `discovery` | Option B: extension |
|---|---|---|
| Setup today | Six config lines and an env var | Clone, build or venv, link, enable, sync |
| Toolchain | None | Rust or Python; Unix only |
| New core surface | A provider field, dialect code, a cache | Two host APIs, a contribution store, `provider_source` |
| New block or prompt | None (one opt-in prompt if Q5) | Model tools and hooks refused (plus Q5 if chosen) |
| Refresh | Automatic when stale (interactive, non-shell keys), explicit command | Explicit `litellm.sync` only |
| Headless | Works from the cache, or unlisted-model defaults | Works from the last written contribution |
| Boundary | LiteLLM listing format in the provider layer | LiteLLM nouns stay out of core |
| Release coupling | Dialect fixes wait for an Euler release | Extension ships independently |
| Other gateways | `openai_models` for ids; new dialects in core | Any gateway can ship its own extension |

## Alternatives rejected

- **Generic `/v1/models` alone.** Ids only: no limits, no tool or reasoning
  claims, no Messages routing, so Claude loses thinking replay. Option A
  keeps it as the `openai_models` dialect, but it does not meet the goal by
  itself.
- **Writing the user's `providers.json`.** Machine and hand edits would
  share one file, the problem ADR 0016 removed from `models.json`.
- **An extension-supplied transport, like pi's module.** Ruled out by
  ADR 0015 and the provider contract.
- **Proxy models in the ADR 0016 catalog.** That catalog is public metadata
  for built-in adapters; a private proxy's membership is account-scoped.
- **`HostApi::resolve_secret` returning the value** (Option B variant). The
  key would enter extension memory.
- **Print and paste.** No refresh, and hand merges. It stays the fallback.

## Rollout

- **Option A:** one Euler PR after the dependencies: the `discovery` field,
  both dialects, the cache, the refresh paths, and the managed credential if
  Q5 is accepted. Contracts: the provider-config section of `provider.md`,
  `secrets.md`, and the headless guide. Fixture tests from recorded
  `/model/info`, `/v1/models`, and `/health` bodies (mapping, grouping,
  routing, fallbacks), cache and headless-no-network tests, and one live
  smoke per family against a proxy.
- **Option B:** Euler (B1 to B6, with `extension-sdk.md`, `capabilities.md`,
  `secrets.md`, `provider.md`, `events.md`), then the `litellm` extension
  with the MIT notice and the same fixtures, then the headless guide.

## Consequences

- Either option removes the hand-written model list and routes Claude to
  Messages.
- Option A adds LiteLLM knowledge to core and couples its fixes to Euler
  releases; Option B adds two host substrates and depends on extension
  distribution for ease of use.
- A user's hand-written provider with the same id keeps working: under A it
  gains `discovery` by adding one field; under B it shadows the contribution
  until removed.

## Open questions

1. **Which option?** **Proposed: Option A now.** Option B cannot meet the
   onboarding requirement today (clone, build or environment, link, enable,
   sync, Unix only), while A needs six config lines and an environment
   variable. The trade-off is a documented boundary exception: one product's
   listing format in core, with fixes on Euler's release cadence; we port a
   bounded subset of pi's discovery rather than its full quirk set. Option B
   stays specified here as the exit path once extensions install in one
   command without a local build.
2. (A) Accept the `litellm` dialect as a boundary exception with the
   invariant, reason, and exit criterion stated in Option A? **Proposed:
   yes.** Calling it ordinary provider-layer knowledge would blur the line
   ADR 0022 just restored.
3. (A) Refresh automatically at interactive launch when the cache is
   missing or older than 24 hours, only for keys that need no shell
   command, never headless, plus `euler models refresh` and picker
   selection? **Proposed: yes.**
4. Route to Responses only on `mode: responses`, or also when
   `supported_endpoints` lists `/v1/responses` (pi's wider rule, which moves
   most GPT routes)? **Proposed: `mode: responses` only** for v1; revisit
   after the `openai_responses` live checks.
5. Add the Euler-managed credential (`euler login --provider <custom-id>`)
   next to environment variables and keychain commands? **Proposed: yes.**
   It is the easiest key entry for people who manage neither.
6. (A) A user `models` entry replaces the discovered descriptor whole, and
   v1 has no exclude list? **Proposed: yes.** It matches how
   `providers.json` and `models.json` treat descriptors; a user pinning one
   model copies the fields it needs. Hiding routes waits for a real need.
7. If Option B is chosen, accept its decisions as written? **Proposed: yes**
   to each: (a) the `<extension-id>_` id rule; (b) the user's
   `providers.json` wins whole; (c) contributions load only while enabled;
   (d) `http_get` rather than `resolve_secret`; (e) literal keys refused;
   (f) `provider_source` recorded; (g) the new block on model tools and
   hooks; (h) refresh only through `litellm.sync`.
