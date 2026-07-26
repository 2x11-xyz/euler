---
name: euler-provider-change
description: "Diagnose and implement Euler provider or model compatibility changes, including catalog-versus-adapter ownership, safe failure inspection, request shaping, stream parsing, retry classification, and live validation."
---
# Euler provider changes

1. Decide ownership before editing:
   - the provider catalog owns model membership, display metadata, limits, reasoning levels, pricing, aliases, and defaults;
   - the provider adapter owns auth, endpoints, transport, headers, request shaping, wire compatibility, stream parsing, and provider-error classification.
2. Read `docs/contracts/provider.md`, `docs/contracts/secrets.md`, and relevant provider tests.
3. Inspect session files minimally. Prefer route, event kind, stop reason, stable error code, and parameter metadata; do not dump prompts, reasoning, credentials, or arbitrary provider messages.
4. Reproduce with an isolated `EULER_HOME`. Copy only the credential file needed for the probe with mode 600, and remove the temporary store afterward.
5. Test request-shape behavior and every accepted error envelope. Error categories must match retry semantics: only transient transport or throttling failures are retryable; entitlement and unsupported-request failures are terminal rejections.
6. Keep provider compatibility rules in adapter code unless the current catalog schema explicitly owns that metadata. Do not widen the catalog trust boundary as part of a runtime bug fix.
7. Validate plain text and, when relevant, a complete tool round. Record only safe event metadata in reports.
8. Run provider tests, formatting, Clippy, catalog synchronization tests, the workspace suite, doctests, and a locked release build for CLI-facing changes.
