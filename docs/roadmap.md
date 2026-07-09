# Roadmap

Directions, not commitments. Ordered roughly by intent; no dates.

## Near term (v0.2)

- **Warm Ledger TUI.** Core CLI transcript layout/interaction system (ADR 0010,
  `docs/contracts/ui.md`, plan `docs/notes/warm-ledger-tui-plan-2026-07-09.md`).
  Docs/contracts first; then multi-slice implementation on a long-lived branch
  for user dogfood before PR. Sequence (scheduled, not opportunistic): ledger
  foundation + themes → diff renderer → thinking/markdown → live composer/queue
  → scoped approvals → failures → workspace checkpoints → search/@/timestamps →
  slash/extension surfaces → resume polish → recaps/notify → companion
  sub-ledger (after multi-agent projection contract). Themes stay swappable
  profiles. Core/extension boundary unchanged: DAG and rich viz stay extension-
  owned; `/dag` is dispatch only.
- **Out-of-process extensions.** The extension SDK's second lane: extensions
  as separate processes over a stdio JSON-RPC transport, in any language.
  v0.1.0 ships native Rust extension crates only.
- **Headless session resume.** `exec --resume`: continue an interrupted run
  from its provenance log instead of re-exploring from scratch.
- **Honest cap summaries.** When a session hits its round limit, report what
  was done and where it stopped, not a canned line.
- **Causal-DAG ergonomics.** Friendlier paging for long logs, non-mutating
  read commands, degrade-instead-of-fail on stale hint pointers.

## After that

- **Richer retention tiers.** Auto-compaction beyond `off`/`stubs`:
  structured indexes over demoted content, and extension-owned compactors
  that summarize with domain knowledge (the causal DAG as the first one).
- **DAG visualization pipeline.** First-class export from a session's causal
  DAG to interactive renderers (2D and 3D), suitable for embedding and
  side-by-side comparison of runs.
- **Observer hook.** Move causal-DAG maintenance off the working agent
  entirely: a second model observes the session and maintains the graph, so
  the agent pays no notebook tax.
- **More bundled extensions.** Literature review and writing/synthesis
  workflows on the same SDK.

## Principles that shape all of it

- Core stays small; workflows live in extensions.
- The model canvas stays clean; provenance stays exhaustive.
- Forgetting is user-chosen and visible — degrade content, never facts.
- Honest failure over silent failure.
