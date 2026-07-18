# Euler Vision

Euler is a research agent (coding agent included) and an open-ended,
runtime-extensible platform for long-range, evidence-grounded work.

Small core. Long memory. Infinitely extensible. Multi-agent and multi-model
by design.

## North star

Euler superpowers researchers. It is a fast, extensible, provenance-aware
agentic workspace for work that spans days, models, and agents, with an
extension system that continuously expands what it can do.

Euler's identity is not a coding agent, a science agent, or a project
management agent. Coding is one important capability. Scientific work is the
major pressure test. The product boundary is broader: researchers working
across code, data, writing, experiments, literature, simulations, and
analysis. The core does not know what physics, mathematics, or literature
review are, and it never should. It provides the scaffolding that lets
users, agents, and extensions build those workflows reliably.

Euler should be extensible enough that AI can improve Euler itself through
extensions.

## The bar

A scientific paper should be able to cite an agent-driven finding, and an
independent researcher should be able to inspect how the system arrived
at it.

That is the whole test. A result you cannot reconstruct is a result you
cannot trust, extend, or audit. Narration is not evidence; artifacts are.
Dead ends are data: the paths that failed constrain the search space for
every future attempt (human or agent), but only if they were kept.

In Euler this is structural, not aspirational. Every session is one
canonical event stream. Every event carries its causal parent. Which input
caused which tool call, which result caused which decision, which branch
produced which conclusion: the chain is recorded as it happens, survives
compaction, and reconstructs on resume. This is not logging. It is a causal
graph of reasoning that can be walked, queried, and reproduced.

## Three surfaces

Euler separates three surfaces and never conflates them:

```text
Provenance surface:
Everything that happened.

Transcript surface:
What the user and assistant meaningfully said or did.

Working canvas:
What the next model turn is allowed to reason over.
```

The rule that binds them: preserve the messy truth without making the model
eat the log. Provider retries, partial streams, failed tool calls, and raw
finish metadata belong in provenance. The working canvas stays clean,
bounded, and replayable. Under context pressure, content may degrade to
compact stubs, but facts about what happened are never silently lost.
Forgetting is user-chosen and visible. Degrade content, never facts.

## Core owns invariants. Extensions own behavior.

The core guarantees what every long-duration agent system needs: session
durability, tool-call integrity, permissions and budgets, the compaction
substrate, provenance, extension hosting, and multi-agent scaffolding.
That is the entire job. If a feature is domain-specific, workflow-specific,
or an integration with an external system, it is an extension.

Extensions define what the system does: commands, context slots, artifacts,
checkpoints, capabilities, and companion-agent patterns, built on one host
API, in any language (extensions run as separate processes when they want
to be). Research records, causal graphs, review swarms, and observers that
maintain understanding off the working agent's back all live outside the
core, where they can be added, replaced, and discarded without
renegotiating the core's guarantees. When a capability is tempting to
hard-wire, that temptation is the signal to strengthen the SDK instead.

## Accuracy comes from structure

Multi-agent systems are not automatically more accurate. Euler makes them
manageable and auditable through independent contexts, model diversity,
explicit assumptions, artifact-backed claims, critic and verifier roles,
reproducible traces, and provenance-preserving synthesis. Agents stay
isolated by default (decorrelated reasoning is worth protecting) and
coordinate through shared artifacts and explicit messages, not through a
shared soup of context.

The goal is not more agent chatter. The goal is better-supported
conclusions.

## How Euler holds itself to this

Improvement is empirical. Agent behavior is an empirical system: a change
that claims to improve quality, reliability, or cost gets a baseline first
and a comparison after.

Behavior is governed by contracts. The documents in `docs/contracts/` say
what is true now, and they outrank code, enthusiasm, and old decisions.
When reality drifts, the drift is reconciled, not tolerated.

Failure is honest. Caps report what was done and where work stopped.
Errors surface with their evidence. A feature that cannot fail loudly is
not done.

Trust is mechanical. Permission decisions are recorded events. Session
ownership is enforced by the operating system, not by convention. Secrets
are redacted at the boundary. Attention is a budget: compact names,
schemas, and discoverable commands beat multi-thousand-token prompts, and
if a feature needs extensive instructions to be usable, the interface is
the thing to fix.

## Horizon

The direction is a platform where long-horizon agent work is ordinary:
sessions that resume instead of restart, observers that keep understanding
current while the working agent works, and one team's workflow becoming
everyone's capability through the SDK. Concrete steps live in
`docs/roadmap.md`. The principles above are the parts that do not move.
