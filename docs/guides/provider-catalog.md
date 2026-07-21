# Provider catalog and model updates

Euler separates provider code from provider and model metadata. Provider
adapters, authentication, endpoints, and request behavior ship in the Euler
binary. The public
[`euler-provider-catalog`](https://github.com/2x11-xyz/euler-provider-catalog)
repository publishes the model IDs and metadata those built-in adapters can
use.

This gives Euler an offline baseline while allowing model metadata to advance
between Euler releases.

## The lifecycle

```text
Official provider APIs and reviewed official sources
                         │
                         ▼
             Daily catalog observation
                         │
                         ▼
        Validated candidate and promotion review
                         │
                  maintainer merge
                         ▼
           Versioned GitHub catalog release
                   ┌─────┴─────┐
                   ▼           ▼
        next Euler release   installed Euler
        embeds the catalog   TUI: background check
                             CLI: euler models refresh
                                   │
                                   ▼
                           verified local cache
```

Daily observation is not an automatic rewrite of users' installations. A
candidate must pass the catalog repository's validation and promotion policy,
and a maintainer reviews and merges the stable-state change before GitHub
publishes an immutable catalog release.

## What happens on a fresh install

Every Euler release binary contains a verified catalog snapshot. Consequently,
the following command works immediately without network access:

```console
euler models
```

Before creating a new Euler release tag, a maintainer synchronizes the latest
published catalog into the Euler source tree. The release workflow refuses to
build or publish a new release from a tag whose committed embedded catalog is
not the latest release. The binary is then built from those committed bytes,
so downloading a prebuilt binary and building Euler from the same release tag
begin with the same catalog.

Existing binaries are never rewritten when the catalog changes. They can
accept a newer compatible catalog through the runtime refresh path described
below. A new Euler binary is required when a change needs a new provider
adapter, authentication flow, endpoint, or other executable behavior.

## Runtime behavior

Euler has three deliberately different command paths:

| Path | Network behavior | Result |
| --- | --- | --- |
| `euler models` | Always offline | Lists the effective local catalog |
| `euler models refresh` | Explicit bounded GitHub check | Installs a newer compatible catalog, or reports that the current catalog is latest |
| Full-screen `euler` / `euler tui` | Non-blocking background check when due | Reloads the model picker if a newer catalog is accepted |

After any successful check, the automatic path is due again after 24 hours.
After a failed check, the TUI may retry after one hour. The usable interface
never waits for the network. Line-oriented and headless commands do not perform
an implicit refresh.

To update without opening the TUI, run:

```console
euler models refresh
```

The command is sessionless and does not use provider API keys. It downloads
only public catalog release files from GitHub.

## Validation and failure behavior

Euler first resolves the latest release manifest, then downloads the catalog
from that manifest's versioned release-specific location. Before accepting it,
Euler checks the release identity, SHA-256 digest, byte length, schema,
minimum Euler version, release ordering, timestamps, and catalog invariants.
Requests are bounded by host allowlists, size limits, redirect limits, and a
deadline.

Accepted releases are stored under:

```text
~/.euler/catalogs/provider-v1/
```

An unavailable network, malformed release, incompatible catalog, downgrade,
or write failure leaves the last-known-good catalog untouched. If no cached
release is usable, Euler falls back to the catalog embedded in the binary.

The trust root is the official GitHub repository over HTTPS. The release ID and
digest detect mismatched or corrupted assets; they do not provide a trust root
or maintainer signature independent of that repository.

## Catalog scope

The catalog can publish public metadata such as:

- provider-scoped model IDs and display names;
- context and output limits;
- tool and reasoning support;
- reasoning-effort choices, aliases, defaults, lifecycle state, and pricing
  when known.

The catalog cannot add executable provider code or change endpoints,
authentication, headers, secret resolution, request formats, prompts, or
session behavior. Euler accepts catalog entries only for provider adapters
already compiled into the binary. Custom provider transport remains
user-owned configuration in `~/.euler/providers.json`.

Provider API keys used by the catalog repository's scheduled observation job
are operator discovery credentials. They are injected into that GitHub Actions
job only and are never catalog content. Euler's refresh client neither reads
nor resolves them.

## Effective catalog precedence

Euler assembles the model list in this order:

1. the catalog embedded in the binary;
2. the newest valid compatible cached release, when one exists;
3. the user's optional `~/.euler/models.json` additions and same-ID metadata
   overrides.

Transport-bearing custom providers from `~/.euler/providers.json` are a
separate configuration surface.

## Maintainer release sync

In the Euler repository, maintainers update the embedded snapshot with:

```console
python3 scripts/sync_provider_catalog.py
python3 scripts/sync_provider_catalog.py --check
```

The sync command validates the public release before replacing the tracked
catalog, manifest, and generated catalog README. A pushed new-version tag runs
the latest-release check again before any release binary is built. Rebuilding
an already-published Euler release instead validates that tag's committed
snapshot offline so an old release remains reproducible.

For the architectural decision and full trust model, see
[ADR 0016](../adr/0016-github-provider-catalog.md). For observation,
promotion, and publication mechanics, see the
[`euler-provider-catalog` README](https://github.com/2x11-xyz/euler-provider-catalog).
