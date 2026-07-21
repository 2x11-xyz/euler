# Roadmap

Directions, not commitments. Ordered roughly by intent; no dates.

## Landed since v0.1.0

Kept here (rather than deleted) so the roadmap stays honest about what moved
from intent to shipped; details in `CHANGELOG.md`.

- **Warm Ledger TUI** — shipped through the Warm Spine v2.1 iteration (ADR
  0010, `docs/contracts/ui.md` normative; #132, #134–#136), plus the
  follow-on performance work (#137–#141) and mid-turn steering (#147).
- **Out-of-process extensions** — the managed-process runtime: extensions as
  separate processes over stdio JSON-RPC, Python SDK included (#130, #131),
  including managed-process round observers (#133).
- **Observer hook** — the round-boundary observer (`--observe`,
  `--observe-cadence`) moves causal-DAG maintenance off the working agent.
- **DAG visualization pipeline** — `causal-dag.export --format html`
  produces the self-contained interactive 2D/3D viewer
  (`docs/examples/knuth-gpt55-xhigh.html`).

## Near term (v0.2)

- **Headless session resume.** `exec --resume`: continue an interrupted run
  from its provenance log instead of re-exploring from scratch.
- **Honest cap summaries.** When a session hits its round limit, report what
  was done and where it stopped, not a canned line.
- **Causal-DAG redesign.** The extension is paused for a behavior-first
  redesign in euler-extensions (its schemas and golden fixtures are the
  spec); the old ergonomics issues fold into that redesign as inputs.

## After that

- **Richer retention tiers.** Auto-compaction beyond `off`/`stubs`:
  structured indexes over demoted content, and extension-owned compactors
  that summarize with domain knowledge (the causal DAG as the first one).
- **More first-party extensions.** Literature review and writing/synthesis
  workflows on the same SDK, distributed through euler-extensions rather
  than bundled into the binary.

## Principles that shape all of it

- Core stays small; workflows live in extensions.
- The model canvas stays clean; provenance stays exhaustive.
- Forgetting is user-chosen and visible — degrade content, never facts.
- Honest failure over silent failure.
