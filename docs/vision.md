# Euler — Vision and Design Philosophy

This document memorializes why Euler exists and the principles that shape
every decision in it. The roadmap says what comes next; the contracts say
what is true now; this says what stays true throughout.

## Mission

Euler exists to make agent work **trustworthy enough to build on**. It is a
research agent — coding agent included — and a runtime-extensible platform
whose defining commitment is that problem solving is a first-class artifact:
every session is a durable, reconstructable record of what was tried, what
died, and what survived.

## The wager

Agents are becoming capable of long, consequential work — research programs,
codebases, investigations that outlive any single session. The industry's
default is to treat the work's *output* as the product and the *process* as
disposable exhaust. Euler wagers the opposite: the process record is where
the durable value accumulates.

- A result you cannot reconstruct is a result you cannot trust, extend, or
  audit. Narration is not evidence; artifacts are.
- Dead ends are data. The paths that failed constrain the search space for
  every future attempt — human or agent — but only if they were kept.
- Tools that keep honest records compound; tools that keep impressions of
  records decay. Provenance is not a feature of Euler. It is the product.

## What Euler is — and is not

Euler **is** a small, provenance-bearing core: one canonical event stream,
a disciplined projection of it into model context, and a host API that lets
extensions add commands, context slots, artifacts, checkpoints, capabilities,
and companion-agent patterns without changing the core.

Euler **is not** a workflow product. Workflows — causal graphs, research
pipelines, review swarms, whatever comes next — live in extensions, where
they can be added, replaced, and discarded without renegotiating the core's
guarantees. When a capability is tempting to hard-wire, that temptation is
the signal to strengthen the SDK instead.

## Design principles

1. **Provenance is exhaustive; the canvas is clean.** The append-only event
   log captures everything at provider fidelity. The model sees a curated
   projection of it, never the ledger itself. These are different artifacts
   with different masters, and they must never be conflated (ADR 0002).

2. **Forgetting is user-chosen and visible.** Under budget pressure, content
   may degrade to compact stubs — but facts about what happened are never
   silently lost. Any forgetting an agent does on its own behalf is a bug.

3. **Contracts before authority.** New capability lands its contract update
   before or with the first honest surface that exercises it (ADR 0010).
   The contracts in `docs/contracts/` outrank code, ADRs, and enthusiasm;
   when reality drifts, the drift is reconciled, not tolerated.

4. **Honest failure over silent failure.** Caps report what was done and
   where work stopped. Errors surface with their evidence. A feature that
   cannot fail loudly is not done.

5. **Small core, strong seams.** The core stays small enough to audit and
   trust; power accrues at the extension boundary. Unwired scaffolding is
   not neutral — a helper lands with its consumer or not at all.

6. **The terminal is an honest surface.** Native scrollback, native text
   selection, real files, real exit codes. The TUI renders the ledger; it
   does not impersonate one. Chrome that implies state the system does not
   have gets deleted.

7. **Trust is earned mechanically.** Permission decisions, secret redaction,
   sandboxing, and session locking are event-recorded, contract-governed
   mechanisms — not policies in a README. Where ownership matters, the
   operating system enforces it, not a convention.

## How decisions get made

Decisions worth keeping become ADRs (`docs/adr/`); the truths they establish
become contracts (`docs/contracts/`); everything else must earn its place by
dogfood. Euler is developed in Euler-adjacent workflows daily, and the
ledger of its own construction — issues, ADRs, provenance of its sessions —
is held to the same standard the tool promises its users.

## The horizon

The direction is a platform where long-horizon agent work is ordinary:
sessions that resume across days, observers that maintain understanding off
the working agent's back, extensions that turn one team's workflow into
everyone's capability — and, always, records good enough that the next
attempt starts from evidence instead of memory. Concrete steps live in
`docs/roadmap.md`; the principles above are the parts that do not move.
