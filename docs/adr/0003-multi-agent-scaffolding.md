# ADR 0003: Multi-Agent Scaffolding

## Status

Accepted. Decided before this repository's v0.1.0 import; the original ADR
text was not imported. This stub was reconstructed in the 2026-07-18 ADR
cleanup from the decision's surviving citations (ADR 0010, the multi-agent
contract) so those references resolve. The normative statements live in the
contract below, not here.

## Decision (reconstructed summary)

Agent fan-out (companions, spawned agents) is event-sourced through the one
canonical session stream — `agent.spawn` / `agent.message` / `agent.result` —
rather than through parallel channels or core companion lifecycle types.
UI treatment of companions is presentation only: sub-ledgers render from
events and carry no authority of their own.

## Normative contracts

- `docs/contracts/multi-agent.md` — spawn semantics, reporting, result joins
- `docs/contracts/events.md` — the agent event kinds
