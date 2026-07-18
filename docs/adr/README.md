# Architecture Decision Records

## Convention

- One decision per file: `NNNN-kebab-case-title.md`, four-digit zero-padded,
  numbered monotonically by decision date. Numbers are never reused and files
  are never renumbered once cited.
- Every ADR opens with `# ADR NNNN: Title` followed by a `## Status` section
  stating one of **Proposed**, **Accepted**, or **Superseded by ADR NNNN** —
  with the decision date — before any other content. Later sections are
  typically Context, Decision, Consequences.
- ADRs record decisions; they are not kept normative against drift. When a
  contract in `docs/contracts/` and an ADR disagree, the contract wins, and
  the ADR gains a supersession note saying exactly which parts fell (see
  ADR 0010's status block for the model).
- Citing an ADR from code or docs is encouraged (`ADR NNNN`); a citation
  freezes the number forever.

## The pre-repo gap (0001–0008)

The numbering continues a decision sequence that started before this
repository's v0.1.0 import, and most of those early ADR texts were not
imported. Numbers cited by surviving code or docs got reconstructed stubs
that point at the now-normative contracts; uncited numbers stay vacant
rather than being backfilled or reused.

## Index

| ADR | Title | Status |
|---|---|---|
| 0001 | — | Pre-repo; text lost, no surviving citations. Number retired. |
| [0002](0002-provenance-canvas-separation.md) | Provenance / canvas separation | Accepted (pre-repo; reconstructed stub) |
| [0003](0003-multi-agent-scaffolding.md) | Multi-agent scaffolding | Accepted (pre-repo; reconstructed stub) |
| 0004 | — | Pre-repo; text lost, no surviving citations. Number retired. |
| 0005 | — | Pre-repo; text lost, no surviving citations. Number retired. |
| 0006 | — | Pre-repo; text lost, no surviving citations. Number retired. |
| [0007](0007-ui-and-reasoning-display.md) | Terminal UI and reasoning display | Accepted (pre-repo, imported 2026-07-09; amended) |
| [0008](0008-persisted-model-target-and-effort-changes.md) | Persisted model target and effort changes | Accepted (pre-repo; reconstructed stub) |
| [0009](0009-companion-roundloop-and-round-observer.md) | Companion RoundLoop + round-boundary observer | Accepted 2026-07-06 (numbered in the 2026-07-18 cleanup) |
| [0010](0010-warm-ledger-tui.md) | Warm Ledger terminal UI | Accepted 2026-07-09; partially superseded by `contracts/ui.md` |
| [0011](0011-permissions-v2-guardian.md) | Permissions v2 — guardian reviewer | Accepted 2026-07-11 |
| [0012](0012-parallel-swarm-fanout-event-ordering.md) | Parallel CodeSwarm fan-out and event ordering | Accepted 2026-07-11 |
| [0013](0013-operation-scoped-permissions.md) | Operation-scoped permission approval | Accepted 2026-07-14 |
| [0014](0014-linux-workspace-subprocess-sandbox.md) | Linux workspace subprocess sandbox | Accepted 2026-07-14 |
| [0015](0015-extension-distribution.md) | Extension distribution | Accepted 2026-07-18 |
| [0016](0016-github-provider-catalog.md) | GitHub provider catalog distribution | Accepted 2026-07-18 |

Next number: **0017**.
