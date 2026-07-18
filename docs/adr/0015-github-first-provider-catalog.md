# ADR 0015: GitHub-first provider catalog distribution

## Status

Proposed (2026-07-18).

## Context

Euler's built-in provider adapters and its model catalog change at different
rates. Adapter transport, authentication, and wire compatibility belong in the
Euler binary. Model identity and advisory capability metadata change often
enough that waiting for a new Euler release leaves otherwise-routable models
absent from the picker.

Euler already has `euler models refresh`, but it fetches `models.dev` directly
and writes a generated overlay to `~/.euler/models.json`. That is useful as a
manual escape hatch, but it has four structural limits:

- every installation independently fetches and translates an upstream feed;
- one third-party aggregate is the source for every built-in provider;
- the overlay can add or replace entries but cannot remove stale built-ins;
- the same path is both a machine-managed snapshot and the user's local
  metadata override surface.

The supported sources also differ materially. OpenRouter publishes rich model
and capability metadata. Anthropic and xAI publish model-list APIs with
different field coverage. OpenAI's model-list API provides basic identity and
availability while its model documentation carries capability and limit data.
The ChatGPT subscription route is a separate Euler adapter without a public,
stable discovery API suitable for this job.

The catalog must remain metadata-only under the provider and provider-config
contracts. A remote catalog must never become a provider factory or a way to
change authentication, endpoints, headers, request compatibility, or provider
reasoning-artifact ownership.

## Decision

### Authority and hosting

Create a separate public repository, provisionally
`2x11-xyz/euler-provider-catalog`, as the source and publication authority for
Euler's built-in provider/model metadata.

GitHub is the bootstrap trust and availability boundary:

- source policy, curated overrides, schemas, generators, fixtures, and the
  normalized catalog are reviewable in the repository;
- GitHub Actions performs scheduled observation and publication;
- versioned catalog artifacts and their manifest are published as GitHub
  Release assets;
- Euler's stable bootstrap URL points directly to the repository's latest
  GitHub release, not to a separately operated service.

Cloudflare is not part of the first implementation. It may later mirror the
immutable catalog bytes for bandwidth or latency, but it is never the catalog
authority. On first use Euler obtains the manifest and artifact from GitHub.
A future mirror is acceptable only when its bytes match a digest obtained from
the GitHub-hosted manifest; mirror failure falls back to GitHub or the local
last-known-good snapshot.

### What the repository owns

The repository owns only metadata for provider adapters that already exist in
Euler:

- canonical provider and model ids;
- display names;
- context and output token limits;
- tool and reasoning support;
- supported canonical reasoning-effort values when known;
- aliases or pseudo-routes deliberately supported by Euler;
- Euler's curated default model for each built-in provider;
- lifecycle state needed to distinguish active, deprecated, and removed
  observations.

It does not own provider transport, base URLs, authentication, secret
references, headers, wire-format compatibility, session behavior, or custom
providers. Adding a provider to this repository does not make it executable;
adapter wiring remains an Euler code change. User-defined providers remain in
`~/.euler/providers.json`.

### Source policy

Every provider has an explicit field-ownership policy checked into the catalog
repository. Source precedence is:

1. an official provider API for fields it actually publishes;
2. official provider model documentation for capabilities and limits omitted
   from that API;
3. a named secondary aggregate, initially `models.dev`, only for documented
   gaps;
4. small Euler-curated overrides for product defaults, subscription-only
   routes, aliases, and documented router pseudo-models.

Conflicting lower-priority values never silently replace higher-priority
values. Each published provider section records source URLs, observation time,
input digests, and generator version in a separate provenance artifact. The
runtime catalog stays compact and does not expose source machinery to model
selection.

Only documented machine-readable endpoints may drive unattended catalog
changes. Human-readable documentation backs reviewed structured overrides;
HTML scraping does not silently alter the stable catalog.

Initial provider policy is:

| Euler provider | Automated membership source | Capability/limit source | Curated portion |
|---|---|---|---|
| `openrouter` | [OpenRouter Models API](https://openrouter.ai/docs/api/api-reference/models/get-models) | Same API, with documented secondary enrichment for missing fields | Euler-supported aliases and router pseudo-models |
| `anthropic` | [Anthropic List Models API](https://platform.claude.com/docs/en/api/models/list) | Official model docs, then documented secondary enrichment | Default and adapter support filter |
| `openai` | [OpenAI List Models API](https://platform.openai.com/docs/api-reference/models/list) | [Official model catalog](https://developers.openai.com/api/docs/models) and documented secondary enrichment | Default and adapter support filter |
| `xai` | [xAI Models API](https://docs.x.ai/developers/rest-api-reference/inference/models) | Official model pages, then documented secondary enrichment | Default and adapter support filter |
| `chatgpt` | None suitable for unattended public discovery | Euler/Codex route evidence | Entire supported subscription-route list and operational context policy |

The output is an **Euler-supported catalog**, not a claim to enumerate every
model visible to every account. Account-scoped API results are observations,
not universal availability proof.

### Daily workflow and promotion

The repository runs one pinned, least-privilege GitHub Actions workflow daily
and on `workflow_dispatch`:

1. fetch bounded upstream responses, failing closed if a required source is
   unavailable or malformed;
2. retain the raw responses as workflow evidence with bounded retention;
3. normalize deterministically from recorded inputs;
4. validate schema and catalog invariants;
5. compare the candidate with the current stable catalog;
6. open or update one bot pull request when the normalized catalog changed;
7. auto-merge routine additive or metadata-only changes after all gates pass;
8. require human review for defaults, built-in provider membership, source
   policy, suspicious count changes, and removals;
9. publish a versioned GitHub Release only from merged `main`.

The generator never converts a fetch failure into an empty provider list.
Removals require repeated observation or explicit review so a transient API or
account-entitlement change cannot erase the stable catalog. Git history and
release artifacts provide the audit and rollback path.

Credentials needed by official list endpoints are narrow GitHub Actions
secrets used only for discovery requests. The workflow never performs paid
inference and never logs request headers or secret values.

At minimum, validation enforces:

- known provider ids and unique, bounded model ids;
- deterministic ordering and byte-identical output for identical inputs;
- each curated default exists and cannot be changed by discovery;
- all published models satisfy Euler's adapter support policy, including tool
  use where required;
- positive, sane token limits and valid canonical reasoning-effort sets;
- minimum per-provider counts and bounded shrink thresholds;
- a strict schema and artifact-size ceiling;
- absence of transport, auth, secret, prompt, and executable fields.

### Published artifacts

Each changed release contains:

- `catalog-v1.json`: deterministic runtime metadata;
- `manifest-v1.json`: schema version, release id, artifact byte length,
  SHA-256 digest, generation time, and minimum compatible Euler version;
- `provenance-v1.json`: provider source URLs, observation times, source
  digests, generator revision, and normalized diff summary.

The digest detects corrupt, truncated, mismatched, or untrusted mirror bytes.
Authenticity comes from the pinned GitHub repository/release channel and its
protected publication workflow; a digest served beside an artifact is not by
itself a signature.

### Euler bootstrap and local ownership

Euler ships an embedded catalog snapshot produced by the same schema and
generator. First launch therefore remains usable offline and does not block on
GitHub, Cloudflare, or any provider.

On first interactive launch, after the usable UI is available, Euler performs
one bounded best-effort refresh from the GitHub release channel and reports the
result visibly. Failure retains the embedded snapshot. Headless commands do
not acquire this implicit network dependency. Later interactive sessions may
offer an update when the managed snapshot is stale; `euler models refresh`
remains the explicit on-demand path, and bare `euler models` remains offline.

Downloaded state moves to a distinct machine-managed path, for example
`~/.euler/catalogs/provider-v1.json`. `~/.euler/models.json` remains the
user-owned advisory override surface. Effective precedence is:

1. embedded release snapshot;
2. verified downloaded full snapshot, replacing model membership for the
   built-in providers it contains;
3. user `models.json` additions and same-id metadata/default overrides.

This replacement boundary allows a new stable catalog to remove stale models
without allowing local config to hide built-ins accidentally. A legacy
`models.json` bearing the exact `euler models refresh` generator marker is
recognized once at the load boundary and ignored when a new managed snapshot
exists; user-authored files keep their existing semantics.

Refresh validates the manifest and catalog before an atomic write. Unsupported
schema, digest mismatch, timeout, malformed content, suspicious catalog shape,
or write failure leaves the last-known-good file untouched. Catalog fetching
does not resolve provider secrets and does not create session or provenance
state.

The large generated model arrays should ultimately leave Rust source. Euler
embeds the release snapshot as data, while small adapter-owned policy such as
ChatGPT effective-context handling remains code.

## Consequences

- New routable models can reach users independently of an Euler binary
  release, while first launch remains offline-safe.
- GitHub provides a public audit trail, immutable versions, rollback, and the
  initial endpoint without introducing a service to operate.
- Provider-specific discovery stays outside Euler runtime and its model
  canvas.
- Machine-managed state no longer competes with user-owned `models.json`.
- The publication repository becomes release infrastructure and needs branch
  protection, pinned actions, narrow secrets, and failure alerts.
- Some metadata remains curated because no official source provides a complete
  machine-readable contract. The provenance artifact makes that limitation
  explicit rather than pretending the process is fully automatic.

## Implementation sequence

1. Create the public catalog repository with schema, source-policy files,
   deterministic fixtures, and an OpenRouter generator.
2. Add daily candidate generation, guarded bot PRs, and GitHub Release
   publication.
3. Add the managed-snapshot loader and GitHub refresh client to Euler while
   retaining the embedded fallback and manual override contract.
4. Generate Euler's embedded snapshot from the release artifact and delete the
   giant hand-maintained model arrays.
5. Add Anthropic, OpenAI, and xAI source adapters, then migrate the curated
   ChatGPT section.
6. Add first-launch/background UX only after offline, timeout,
   last-known-good, and headless-no-network tests enforce the stop conditions.
7. Consider a Cloudflare mirror only after measured GitHub bandwidth,
   reliability, or latency justifies the extra operational surface.

## Verification gates

Mechanically checkable client behavior must prove:

- a fresh offline launch uses the embedded snapshot;
- first-use GitHub failure does not block or erase the catalog;
- a valid newer artifact replaces managed provider membership;
- checksum, size, schema, and invariant failures preserve last-known-good
  state;
- user overrides retain final precedence;
- legacy generated overlays are normalized only at the load boundary;
- headless commands make no implicit catalog request;
- catalog content cannot affect provider transport, authentication, or secret
  resolution.

Publication tests must prove deterministic generation, fail-closed source
handling, guarded removals/defaults, schema validity, and byte-for-byte digest
agreement with every published artifact.
