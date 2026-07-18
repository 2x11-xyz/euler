# ADR 0008: Persisted Model Target and Effort Changes

## Status

Accepted. Decided before this repository's v0.1.0 import; the original ADR
text was not imported. This stub was reconstructed in the 2026-07-18 ADR
cleanup from the decision's surviving citation (`docs/contracts/events.md`,
`model.effort.changed`) so that reference resolves. The normative statements
live in the contract below, not here.

## Decision (reconstructed summary)

Model-target and reasoning-effort changes are durable session events
(`model.switched`, `model.effort.changed`) persisted in the stream and folded
on resume, rather than ephemeral runtime configuration. A resumed session
therefore continues against the same target and effort the user last chose,
and the change points remain auditable in provenance.

## Normative contracts

- `docs/contracts/events.md` — `model.switched`, `model.effort.changed`,
  and their resume-fold semantics (implemented by euler-core's
  `fold_model_target` / `fold_reasoning_effort`)
