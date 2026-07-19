# Embedded provider catalog

These files are copied byte-for-byte from the immutable GitHub Release
[`catalog-v1-20260718t221617z-d619088f6e7778720898f59eb19ef903bbbd712d8ebe24e66c668490ce26e5d9`](https://github.com/2x11-xyz/euler-provider-catalog/releases/tag/catalog-v1-20260718t221617z-d619088f6e7778720898f59eb19ef903bbbd712d8ebe24e66c668490ce26e5d9)
in the public
[`2x11-xyz/euler-provider-catalog`](https://github.com/2x11-xyz/euler-provider-catalog)
repository.

- `catalog-v1.json`: `142980` bytes,
  SHA-256 `0c791bd6d84b4f180ac569d91258f76b09bd89625f7d8df55a7e6198aaf740a4`
- `manifest-v1.json`: the release manifest used to authenticate the catalog
  identity and digest

The runtime embeds both files so a fresh install has a complete offline
baseline. Do not hand-edit the JSON. A future embedded update should copy a
published release, verify its bytes and digest, and update this note in the
same reviewed change. Source observations and field-level provenance remain
in the catalog repository and its release assets; Euler packages no provider
credentials or observation responses.
