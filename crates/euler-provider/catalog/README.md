# Embedded provider catalog

These files are copied byte-for-byte from the immutable GitHub Release
[`catalog-v1-20260721t000130z-2834472ff45442f2617675c8e2ed9c2cbca2e33b2e193c799cb4e68e49a2683b`](https://github.com/2x11-xyz/euler-provider-catalog/releases/tag/catalog-v1-20260721t000130z-2834472ff45442f2617675c8e2ed9c2cbca2e33b2e193c799cb4e68e49a2683b)
in the public
[`2x11-xyz/euler-provider-catalog`](https://github.com/2x11-xyz/euler-provider-catalog)
repository.

- `catalog-v1.json`: `152593` bytes,
  SHA-256 `bf3cb671579a1008d81cc15180a37311c8c864a9d073f16009bcbddfdb1ff197`
- `manifest-v1.json`: the release manifest used to authenticate the catalog
  identity and digest

The runtime embeds both files so a fresh install has a complete offline
baseline. Interactive sessions perform a bounded, best-effort GitHub refresh
when due; headless commands stay offline unless refresh is explicitly requested.

Do not hand-edit these generated files. Maintainers update to GitHub's latest
stable catalog with:

```console
python3 scripts/sync_provider_catalog.py
python3 scripts/sync_provider_catalog.py --check
```

`--release-id <id>` selects an immutable release explicitly. New tag builds
must pass the latest-release check before compiling. A manual rebuild is only
allowed for an existing release and validates that tag's committed manifest
identity, catalog digest, and generated README entirely offline.
Source observations and field-level provenance remain in the catalog repository
and its release assets; Euler packages no provider credentials or observation
responses.
