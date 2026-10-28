# ADR 0025: Gateway provider discovery

## Status

Proposed (2026-10-27). Draft for the maintainer's review. It specifies two
options: a `litellm` extension that contributes provider configuration
(Option A, the current lean) and a core discovery setting in
`providers.json` (Option B, the fallback). Every choice is an open question
at the end with a proposed answer, marked "(proposed, Qn)" in the text.

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
by hand. The proxy already publishes that data: `GET /model/info` returns
one row per deployment (several hundred KB on a production-sized proxy) with
`litellm_provider`, `mode`, `supported_endpoints`, token limits, and
reasoning and tool support; `GET /v1/models` returns bare ids.

The goal is that a team adopts any LiteLLM proxy with minimal setup, the way
`pi-provider-litellm` (v4.5.0, MIT) works in pi: set a base URL and a key,
and models are discovered. Pi gives that package a host surface Euler lacks:

| Pi mechanism | What it does there | Euler boundary today |
|---|---|---|
| `pi.registerProvider` | In-process registration at activation | Out-of-process, consent-gated extensions; providers resolve at startup from config files, with no extension code running |
| Per-model pi-ai `api` (`openai-completions`, `anthropic-messages`, `openai-responses`) | The extension never implements a wire protocol | Same idea: per-model core `api_family` (in flight) |
| `/login` framework | Prompts for base URL and secret, stores them in pi's credential file | `euler login` covers ChatGPT only; custom providers use specs |
| Discovery at `session_start` (3-second seed cap, 5-second request timeout), stored model list as fallback | Models appear without a command | No startup hook may run an extension, prompt, or resolve secrets |
| `/litellm-refresh` | On-demand refresh | `user` commands, but `euler extension run` requires a session |
| `LITELLM_BASE_URL`, `LITELLM_API_KEY`, `LITELLM_API_KEY_HELPER` | Zero-config for env users | Managed processes get only `PATH` and `EULER_MANAGED_PROCESS_PROTOCOL` |

Distribution is simpler than it first appears. Python extensions in
euler-extensions run `python3` against the SDK checked into that repository,
with no build step. Today `euler extension install` records an inert entry
that `enable` refuses (the install consent flow and git sources are
unimplemented; the link lane is complete), so the working path is `git clone`, `euler extension link <dir> --scope user`, and
`euler extension enable`. No Rust toolchain is needed; managed-process
extensions run only on Unix hosts.

## Discovery, mapping, and routing (either option)

The `litellm` dialect is a port of pi's `discoverModels`. The proxy root is
the base URL with trailing slashes and a trailing `/v1` removed. Requests
carry the key as `Authorization: Bearer`.

1. **Discover.**
   - `GET <root>/model/info`. A 200 with at least one row is the source. A
     wildcard route (a `model_name` containing `*`) expands to the concrete
     `GET <root>/v1/models` ids it matches, each inheriting the row's
     metadata; an explicit row wins, unmatched ids are not added, and a
     failed expansion fails discovery.
   - On 401, 403, 404, or an empty list: `GET <root>/v1/models`. An empty
     list ends the chain.
   - On 401, 403, or 404 from that: `GET <root>/health` with a longer
     deadline, taking the route names of healthy endpoints. It makes LiteLLM
     probe every deployment, so it is the last resort.
   - Any other status, a redirect (never followed), a timeout, a malformed
     body, or zero usable models fails discovery with the endpoint and
     status named, and changes nothing. If every step answered 401 or 403,
     the error says the key was rejected.
2. **Map.** Drop non-chat modes (`embedding`, `image_generation`,
   `completion`, audio, rerank). Group rows by `model_name`; drop and list
   routes that fail the `providers.json` model-id rule. Merge conservatively
   across deployments: `context_window_tokens` is the minimum
   `max_input_tokens`, `max_output_tokens` the minimum `max_output_tokens`,
   and `supports_tools` (`supports_function_calling`) and
   `supports_reasoning` are true only when every deployment says so; a field
   missing anywhere is omitted. Chat-completions models that support
   reasoning get `compat.reasoning.request_format:
   "openai_reasoning_effort"`. Ids from `/v1/models` or `/health` carry no
   claims, the treatment an unlisted model gets today.
3. **Route** each deployment, keeping a family only when all deployments of
   the route agree:
   - `anthropic_messages` when `litellm_provider` is `anthropic`, `mode` is
     `chat`, and `supported_endpoints` is null or lists `/v1/messages`. This
     path replays Claude thinking blocks, which chat completions drops.
   - `openai_responses` when `mode` is `responses` (proposed, Q9).
   - `openai_chat_completions` otherwise, including disagreeing routes,
     which are listed.

   Messages is addressed at the proxy root and the OpenAI families at
   `<root>/v1`, through the per-model base-URL rule.

Not ported: models.dev enrichment, pi's catalog lookups, vendor quirks
(Moonshot, Azure version gating), prices, budgets, MCP, the Skills Gateway,
image models, and SSO login.

## Option A: a `litellm` extension contributing provider configuration

Onboarding:

```sh
git clone https://github.com/2x11-xyz/euler-extensions
euler extension link euler-extensions/extensions/litellm --scope user
euler extension enable litellm
export LITELLM_BASE_URL=https://litellm.example.com   # pi users: already set
export LITELLM_API_KEY=...                            # or: euler login --provider litellm
euler extension run litellm.sync                      # or /litellm.sync in the TUI
```

Euler core adds six product-neutral pieces (A1 to A6); the extension (A7)
holds every LiteLLM noun.

### A1. Provider contribution document

An enabled extension may contribute one document with exactly the
`providers.json` schema, stored by the host at
`<euler home>/extensions/<id>/providers.json` (owner-only, temp file and
rename, the config documents' symlink and file-type rules, machine-managed). The host derives `<id>` from the calling command,
as for ADR 0022 config documents. Contributed models name a core
`api_family`; a contribution cannot add transport. Provider ids must equal
the extension id or start with `<extension-id>_`; extension ids cannot
contain `_`, so contributions never collide (proposed, Q2).

`HostApi::read_provider_contribution()` returns the document and its load
warnings. `HostApi::write_provider_contribution(document | null)` requires
`config-write` and runs only from a `user` command a human invoked; model
tools and lifecycle hooks are refused (**new block**, proposed, Q11). The
host adds to the command output, verbatim, every secret spec that is new or
changed.

### A2. Validation and precedence

The write runs `ProviderConfigRegistry::with_json` strictly: any validation
warning refuses the whole write, including an unknown `api_family`. Added
rules: the id rule, a 256 KiB cap (about a thousand models), and secrets only as `$NAME` or `!command`
specs or the host credential (A3); literals are refused (proposed, Q3). At
load the same parser runs leniently, like `providers.json`.

Precedence (proposed, Q2): built-in ids stay reserved; on an id collision
the user's `providers.json` entry wins whole, with a warning naming both
files; contributions load only while their extension is enabled, and a
session that selects or resumes a provider whose extension is disabled fails
with an error naming the extension. Contributions load at startup with
`providers.json`; no extension code runs then.

### A3. Host-owned credential for custom providers

`euler login --provider <custom-id>` (and `/login <custom-id>` in the TUI)
reads an API key from a hidden prompt and stores it in `~/.euler/auth.json`,
which already holds optional stored API-key credentials (quickstart guide),
keyed by provider id; `euler logout --provider <custom-id>` removes it
(proposed, Q4). A provider with `auth_header: true` and no `api_key` uses
it, and the stored value seeds the redactor like other auth-file
credentials. This is pi's
`/login` without the base-URL prompt: the base URL is configuration, not a
secret. The login works before the provider exists, so a user can log in and
then sync. It is a new prompt, but only inside a command the user runs to
enter a key, and it stays a plain owner-only file, not the vault the secrets
contract rules out; that contract's auth-file section widens from
subscription tokens to custom-provider keys. It serves hand-written `providers.json` providers too.

### A4. Network: `HostApi::http_get`

The `network` capability gains a host primitive (proposed, Q5). GET only;
`https`, or `http` to a loopback host; redirects not followed; body capped
at 4 MiB and passed through the redactor (a session-less run builds one with
the same seeding a session uses); 30-second deadline, up to 120 on request. Auth headers are never values the extension holds: a header
is `{"secret": "<spec>", "prefix": "Bearer "}` or
`{"credential": "<provider-id>", "prefix": "Bearer "}` for a provider the
extension owns, and the host resolves it. Secret headers bind to one origin
(scheme, host, port) per invocation; a credential header also goes only to
the origin of that provider's current base URL once a contribution exists.
The host adds the origin to the command output.
`secret-resolve` is required for secret headers. Human-invoked `user`
commands only. In a session, calls are recorded content-free in
diagnostics.

### A5. Declared environment passthrough

A manifest may list environment variable names for the host to report to the
managed process (`"environment": ["LITELLM_BASE_URL", "LITELLM_API_KEY",
"LITELLM_API_KEY_HELPER"]`); `enable` shows the list with the argv
(proposed, Q6). A name the secrets contract's classifier treats as secret
(an `*_API_KEY` suffix or a `KEY`, `TOKEN`, or `SECRET` segment, which
covers both `LITELLM_API_KEY` and `LITELLM_API_KEY_HELPER`) arrives only as
set or unset; other names arrive with their values. Secret variables reach
requests only through specs the host resolves, never as values in the
extension. This lets pi's variable names work with nothing new to set.

### A6. Session-less user commands

`euler extension run <id>.<command> <SESSION>` requires a session today;
code-swarm's `configure-user` therefore runs as a TUI slash command or with
a session path. The session positional becomes optional (proposed, Q8).
Without it, the host runs the command with no session: session-scoped host
APIs (provenance, artifacts, extension state, context slots, plan
presentation, agent spawn and record, diagnostics) fail with an error naming
the capability. User-tier config, `http_get`, and contribution writes work.
Such a run has no event log: its record is the command output (with the
spec and origin echoes) and the contribution, whose digest `provider_source`
captures when the provider is used. A command whose declared
capabilities all need a session is refused before launch.

### A7. The `litellm` extension

A Python managed-process package declaring `network`, `secret-resolve`, and
`config-write`, the three environment names above, and two `user`
commands. No model tools or hooks,
so nothing enters the prompt.

- `litellm.sync`, the counterpart of `/litellm-refresh`. Optional JSON input
  (`base_url`, `api_key` spec, `provider`, `default_model`, `dry_run`) is
  stored in the extension's user config and wins over the environment.
  - Base URL: input, else `LITELLM_BASE_URL`, else an error naming both.
  - Key, in pi's order: the input spec; else `!eval "$LITELLM_API_KEY_HELPER"`
    when that variable is set (the host's shell expands it at request time);
    else `$LITELLM_API_KEY` when set; else the host credential. An
    unresolved key fails with an error naming all the ways to provide one.
  - Runs the dialect through `http_get`, writes the contribution, and
    reports the source, counts per family, ids added and removed, and any
    shadowing. A failed or empty discovery writes nothing. `dry_run` writes
    nothing.
- `litellm.status`, offline: settings, key source kind, last sync time and
  source, counts per family, and whether the contribution is active,
  shadowed, or invalid.

### A8. Refresh

The contribution is pi's stored model list and is what Euler routes from
at startup. In v1 it changes only when the user runs `litellm.sync`
(proposed, Q7). Pi's startup discovery has no counterpart: it would need a
new lifecycle surface that launches an extension at startup with standing
network and secret authority and no prompt. `litellm.status` shows the
snapshot's age. A stale snapshot still routes, and a removed model fails
with the proxy's error.

### A9. Recording

Selecting a contributed provider records `provider_source`: the extension
id and the SHA-256 of the document (proposed, Q10).

## Option B (fallback): a discovery setting in `providers.json`

A custom provider gains `"discovery": "litellm"` or `"openai_models"` (ids
only from `GET <base_url>/models`), and `euler-provider` runs the dialect
itself:

```json
"litellm": {
  "api_family": "openai_chat_completions",
  "base_url": "https://litellm.example.com/v1",
  "auth_header": true,
  "api_key": "$LITELLM_API_KEY",
  "discovery": "litellm"
}
```

Discovered models fill `models`; a user entry with the same id replaces the
discovered descriptor whole. A full successful discovery is cached
atomically under `<euler home>/cache/provider-discovery/`, keyed by format
version, base URL, dialect, and `api_family`, and startup reads only the
cache; after a key change the list may be stale until the next refresh.
`euler models refresh` refreshes on demand; an interactive launch
refreshes in the background when the cache is missing or older than 24
hours, only for keys that need no shell command. Headless runs never fetch.
The `litellm` dialect puts one product's nouns in core, so it is a boundary
exception (invariant: zero-install adoption; reason: no build-free extension
install; exit: move to Option A). It needs only A3 from the list above, and
no extension.

## Comparison

| | Option A: extension | Option B: core setting |
|---|---|---|
| Setup | Clone, link, enable, env vars or login, sync | Six config lines and an env var |
| Toolchain | `python3`; Unix only | None |
| Core additions | A1 to A6, A9 | Discovery field, dialect code, cache, A3 |
| New block or prompt | Model tools and hooks refused; login prompt | Login prompt |
| Refresh | `litellm.sync` (startup hook deferred) | Automatic when stale, plus a command |
| Boundary | No product nouns in core | Documented exception |
| Release coupling | Extension ships independently | Dialect fixes wait for Euler releases |
| Reuse | Any gateway can ship an extension on A1 to A6 | Other gateways need core dialects |

## Alternatives rejected

- **Generic `/v1/models` alone:** ids only, no limits or claims, no Messages
  routing. Kept as Option B's `openai_models` dialect.
- **Writing the user's `providers.json`:** mixes machine and hand edits, the
  problem ADR 0016 removed from `models.json`.
- **An extension-supplied transport, like pi's module:** ruled out by
  ADR 0015 and the provider contract.
- **Proxy models in the ADR 0016 catalog:** public, built-in-only metadata;
  proxy membership is account-scoped.
- **`HostApi::resolve_secret` returning the value:** the key would enter
  extension memory.
- **Passing secret environment variables to the extension:** same problem;
  A5 passes only non-secret names.

## Rollout

- **Option A:** Euler A3 and A6 first (useful on their own), then A1, A2,
  A4, A5, and A9 with `extension-sdk.md`, `capabilities.md`, `secrets.md`,
  `provider.md`, and `events.md`; then the `litellm` extension with the MIT
  notice, fixture tests from recorded `/model/info`, `/v1/models`, and
  `/health` bodies, and one live smoke per family; then the headless guide.
- **Option B:** one Euler PR with the field, dialects, cache, refresh, and
  A3, with the same fixtures.

## Consequences

- Either option removes the hand-written model list and routes Claude to
  Messages.
- Option A grows the extension SDK with substrates any gateway can use and
  keeps LiteLLM out of core; models refresh on command until a startup hook
  exists.
- Option B is the shortest setup but carries a boundary exception and
  release coupling.

## Open questions

1. **Which option?** **Proposed: Option A**, with B as the fallback if the
   core additions stall. Python makes A toolchain-free and the steps are a
   one-time setup; B is shorter but puts LiteLLM in core.
2. Contributed provider ids limited to `<extension-id>` or
   `<extension-id>_*`; the user's `providers.json` wins whole on collision;
   contributions load only while enabled? **Proposed: yes.**
3. Strict write validation, secrets only as specs or the host credential,
   literals refused? **Proposed: yes.**
4. Add `euler login --provider <custom-id>` API-key entry stored in
   `~/.euler/auth.json` (a user-run prompt)? **Proposed: yes.** It is pi's
   `/login` for people who manage neither env vars nor keychains.
5. Give the `network` capability a host `http_get` with host-resolved auth
   headers, one origin per invocation, human-invoked commands only?
   **Proposed: yes.**
6. Let a manifest declare environment names, passing values for non-secret
   names and only set or unset for secret-shaped ones? **Proposed: yes**, so
   `LITELLM_BASE_URL`, `LITELLM_API_KEY`, and `LITELLM_API_KEY_HELPER` work
   unchanged.
7. Refresh only through `litellm.sync` in v1, deferring a startup refresh
   hook? **Proposed: yes.** Proxy model lists change rarely, and a startup
   hook needs standing network and secret authority without a prompt.
8. Make the session optional in `euler extension run`, with session-scoped
   APIs failing clearly? **Proposed: yes.**
9. Route to Responses only on `mode: responses`, not whenever
   `supported_endpoints` lists `/v1/responses` (pi's wider rule)?
   **Proposed: yes** for v1; revisit after the `openai_responses` live
   checks.
10. Record `provider_source` when a contributed provider is selected?
    **Proposed: yes.**
11. Accept the new block: model tools and lifecycle hooks cannot write
    contributions or call `http_get`? **Proposed: yes.** No workflow needs
    the model to change providers.
12. If Option B is chosen: the `litellm` dialect as a documented boundary
    exception, automatic refresh only for non-shell keys, and whole
    replacement by user `models` entries? **Proposed: yes.**
