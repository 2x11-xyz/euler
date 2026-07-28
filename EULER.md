# Working on Euler

## Start here

Read `docs/vision.md` before editing code. For architecture-affecting work,
also read `docs/contracts/boundaries.md`, the relevant contracts under
`docs/contracts/`, and any governing ADRs under `docs/adr/`.

Contracts describe implemented behavior. Update the owning contract in the same
patch when behavior changes. Add or amend an ADR before changing a system
boundary or introducing a new architectural owner.

## Ownership boundaries

- Core owns reusable substrates: sessions, tools, permissions, provenance,
  provider routing, canvas assembly, extensions, and agent scaffolding.
- Extensions own optional interpretation and workflow policy.
- Provider catalogs own model metadata and defaults.
- Provider adapters own authentication, transport, headers, request shaping,
  wire compatibility, and response parsing.

Apply `docs/contracts/boundaries.md` when ownership is unclear.

## Working practices

- Keep each branch and PR focused on one change.
- Use a separate Git worktree for independent concurrent writing work.
- Prefer one canonical implementation over compatibility layers or parallel
  paths.
- Do not add production structure solely to make a test observable.
- Treat auth files, sessions, diagnostics, prompts, and tool arguments as
  potentially sensitive. Inspect only the minimum necessary metadata.
- Keep commit and PR prose focused on the change and its verification.
- Use an applicable skill from the catalog for detailed procedures.

## Verification

Run checks appropriate to the changed surface. The full repository gate is:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
python3 -m unittest scripts.test_sync_provider_catalog
env -u EULER_HOME cargo nextest run --workspace
cargo test --workspace --doc
```

For release-affecting CLI changes, also run:

```text
cargo build --release --locked -p euler-cli
```

Never report a check as passing unless its command completed successfully. If a
test flakes, reproduce it independently and report both failed and successful
runs honestly.
