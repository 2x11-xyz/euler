# ADR 0002: Provenance / Canvas Separation

## Status

Accepted. Decided before this repository's v0.1.0 import; the original ADR
text was not imported. This stub was reconstructed in the 2026-07-18 ADR
cleanup from the decision's surviving citations (`docs/contracts/ui.md`,
`docs/contracts/events.md`, ADR 0007, ADR 0010) so those references resolve.
The normative statements live in the contracts below, not here.

## Decision (reconstructed summary)

The durable session event stream (provenance) and the model-facing request
canvas are separate projections with separate rules. The canvas is assembled
from events for the next model call — selected, compacted, budgeted — and
user-visible ledger presentation is never dumped into it: queued input,
denials, recaps, and UI toggles are presentation concerns that must not leak
into model context.

## Normative contracts

- `docs/contracts/canvas.md` — canvas assembly, compaction, budgets
- `docs/contracts/provenance.md` — the durable stream
- `docs/contracts/events.md` — the shared event vocabulary
