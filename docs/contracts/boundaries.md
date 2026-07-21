# Boundary Contract

Core is fixed substrate. Extensions are evolving capability packages.

Core changes rarely and owns invariants. Extensions change freely and own
meaning. This contract gives the operational tests for deciding which side
of the line a feature lives on.

## The Boundary Test

Apply in order; the first test that fires decides.

1. If removing the feature would break Euler's ability to **host, govern,
   record, or extend** agent behavior, it is a core candidate.
2. If removing the feature would only remove **one workflow, integration,
   protocol, visualization, or domain behavior**, it belongs in an extension.
3. If defining the feature's contract requires **domain, protocol, workflow,
   or interpretation nouns**, it belongs in an extension.

Litmus question for multi-agent and workflow features: could a user delete
this and still build a *different* multi-agent workflow on what remains? If
yes, it is an extension. If deleting it means no multi-agent anything, it is
core.

## Host APIs Are Capability-Shaped, Not Product-Shaped

Core exposes generic, host-mediated primitives: file, process, network,
model invocation, provenance emit/query, tool registration, agent
spawn/await. Names are product-neutral.

Extensions own product semantics: protocol config shapes, external service
names, tool naming for integrations, workflow policy, and interpretation.
Core must not know endpoints, auth names, or workflow vocabulary of any
specific product, including first-party ones.

Stable host APIs are versioned; breaking changes require a deprecation path.

## Exceptions

A native or transitional exception to this contract must document:

- the invariant being protected,
- why the extension boundary is insufficient,
- exit criteria.

First-party extensions are not privileged core behavior. They use the
same SDK surface, remain removable, and prove the SDK works.

## Failure Isolation

Extension failure must not corrupt core session state, provenance integrity,
agent isolation, or UI responsiveness. Failures are bounded, surfaced, and
recorded in provenance with extension identity.

## No Out-of-Band Communication

Extensions must not communicate with each other or with agents outside
core-mediated channels. Cross-extension calls route through core tools,
events, or host APIs so permissions, budgets, redaction, and provenance
remain intact.

## Prompt Budget

Always-on instruction text is a product constraint, like the LOC budget but
for context. Core tools and first-party extensions prefer compact names, schemas,
and defaults over long prompt fragments. If a feature needs extensive
instructions to be usable, improve the interface before adding more prompt.
Detail belongs in on-demand help, tool descriptions, and extension context
loaded only when needed.
