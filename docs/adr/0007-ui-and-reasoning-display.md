# ADR 0007: Terminal UI and Reasoning Display

## Status

Accepted. Decided before this repository's v0.1.0 import and brought in with
its original number on 2026-07-09; amended since (see Amendments).

## Decision

Euler uses a Ratatui terminal UX as the baseline. The CLI representation is an
ordered event transcript, not a sidebar/dashboard interface.

**Warm Ledger** (ADR 0010) is the layout and interaction system for that
transcript: flat ledger cells, timestamp gutter, hairlines, one fold key,
theme profiles over semantic tokens. Codex remains the primary *product-bar*
reference for coding-agent UX density; Warm Ledger is the Euler-specific
visual system that replaces boxed Zot-style artifact chrome.

### Reasoning display

Euler will not invent or expose **private** chain-of-thought that the provider
did not surface as a displayable stream.

- **Activity / status** text is user-facing intent and progress — not hidden
  CoT and not a substitute for `model.reasoning`.
- **`model.reasoning` events** may carry raw, summary, or opaque/encrypted
  artifacts depending on the provider adapter.
- The **UI may render** only what the owning provider adapter classifies as
  **user-displayable and taint-safe** (typically summaries, or raw thinking
  the provider explicitly exposes for display). Rendering is collapsible and
  bounded (Warm Ledger thinking collapse).
- **Provider-opaque / encrypted / signature-only** reasoning artifacts are
  **never rendered or interpreted by core UI**. They may be retained for the
  owning adapter’s replay rules and provenance; they do not appear as
  transcript prose.
- Providers that expose nothing produce no reasoning UI. Core must not require
  reasoning tokens.
- **Provenance storage**, **transcript projection**, and **canvas eligibility**
  are separate decisions (see `events.md`, `provenance.md`, `canvas.md`,
  ADR 0002). Display in the ledger does not automatically put reasoning on the
  next model canvas.

## Rationale

Codex has the strongest coding-agent transcript UX: compact, readable, and
action-oriented. Euler needs research-grade visibility into *provider-exposed*
reasoning without depending on every model emitting it, and without violating
taint / opaque-artifact stop conditions.

Warm Ledger keeps the transcript calm and scannable while making tool work,
approvals, and folds first-class.

## Consequence

- Core CLI follows Warm Ledger layout rules (ADR 0010) and `docs/contracts/ui.md`.
- Rich sidebars, dashboards, graph views, and web UI remain extensions or
  companion processes.
- Tests must cover: opaque reasoning non-rendering; summary/raw display only
  via adapter-classified payloads; thinking collapse markers.
- Activity blocks must not become a back-channel for private CoT.

## Amendments

- 2026-07-09: Align with Warm Ledger (ADR 0010); clarify opaque vs displayable
  reasoning and separation from canvas/provenance.
