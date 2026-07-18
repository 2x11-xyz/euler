# ADR 0015: Extension Distribution

## Status

Accepted (2026-07-18).

## Context

Euler ships six extensions as Rust crates compiled into the binary. This
couples extension development to core releases, bloats the core build, and
contradicts the product boundary (core owns invariants; extensions own
behavior). Meanwhile the managed-process runtime is proven: extensions run
as separate processes over stdio JSON-RPC, in any language, with provenance
query, artifacts, and round observation already crossing the boundary.

The design follows the shape package managers have converged on: pinned
sources, a shared store, per-project declarations, and explicit trust. In
Euler's vocabulary the unit is the extension; there is no separate
"package" concept.

## Decision 1: extensions install from sources

A source is a git ref or a local path. One source may provide several
extensions (each a directory with the existing `Euler.extension.json`
manifest and an argv entrypoint) along with themes and templates; a
source-level manifest lists what it provides. Installing a source
materializes its extensions into the store; users enable, disable, and
trust extensions individually, exactly as today.

## Decision 2: two tiers

- `~/.euler/extensions/` is the store: installed extensions,
  source-addressed, tracked by the existing registry (enablement log,
  fingerprints, consent).
- `.euler/` in a project declares sources and deltas (which extensions to
  activate). Project-local paths are permitted but load only after explicit
  per-project consent. Entering a directory never runs anything.

Project declarations apply as deltas over the user's global set; the
project entry wins on conflict; conflicts are surfaced, not silently
resolved.

## Decision 3: sources and pinning

Two source schemes at first, with syntax room for more:

- `git:<host>/<path>@<ref>` cloned into the store; the ref is pinned;
  `update` reconciles a clone without moving pins.
- `path:<dir>` referenced in place, for local development.

Extension identity is the manifest id; the source and resolved ref are
recorded and fingerprinted. The same id from two sources is a surfaced
conflict.

## Decision 4: multilingual materialization

Sources declare how to build their entrypoints (for example
`cargo build --release`, or `uv sync`). Materialization runs at install,
never at load, and only after consent against the pinned ref's
fingerprint: an install-time build is arbitrary code execution and is
treated as such. A changed fingerprint requires re-consent. Entrypoints
remain argv; the managed-process protocol is the only load boundary.

The managed-process wire protocol is the sole integration contract,
documented and versioned in the extension SDK contract; per-language SDKs
are conveniences that live in euler-extensions, never requirements.
Sources declare the toolchains their build requires; install verifies them
up front and fails with the missing tool named.

## Decision 5: CLI

The existing `euler extension` family grows distribution verbs and keeps
runtime ones: `install | remove | update | list` alongside
`enable | disable | run | audit`. `extension link` remains as sugar for a
`path:` source containing one extension.

## Decision 6: migration of the bundled six

The `euler-extensions` repository becomes the first source and the home of
all new extension work. Bundled crates convert one at a time to standalone
binaries over managed-process, starting small (session-export or
diagnostics-report) and ending with causal-dag, which gates on host-API
coverage. Bundled and installed extensions coexist in the registry
throughout; no flag day.

## Non-goals

- A registry or gallery (sources are git and path for now).
- Prebuilt per-platform binaries (declared builds are the honest v1).
- In-process plugin loading in any form.

## Consequences

- Contracts updated with or before the first implementation slice:
  `extension-sdk.md` (source manifest, materialization), `capabilities.md`
  (install consent), `ui.md` (manager surface shows extension provenance).
- Core gains the store/declaration/trust plumbing and the new distribution
  verbs; it loses six crates over time.
- Extension velocity decouples from core releases; a core release never
  again waits on an extension fix, and vice versa.
