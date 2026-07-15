# ADR 0010: Warm Ledger Terminal UI

## Status

Accepted for implementation on branch `feat/warm-ledger-tui` (2026-07-09).

**Partially superseded (2026-07-15).** The decision below — Warm Ledger as the
layout and interaction system, with `docs/contracts/ui.md` normative over it —
stands. Three items in "Ledger rules (normative summary)" do not: they were
written against the v1 layout and were reversed by the Warm Spine v2.1 design
spec (2026-07-10) and by later amendments. The ADR is kept as the record of the
decision that was made; **`docs/contracts/ui.md` is the current normative
statement** wherever the two disagree.

| Rule below | Superseded by |
|---|---|
| 9-character timestamp gutter by default | Timestamps **off** by default; the 2-cell anchor spine carries the ledger. `/timestamps` opts the gutter in (spec §0/§5.5). |
| Hairline under each meaningful event | **No hairline per event** — separation is the spine plus one blank line (spec §0/§1). |
| Nearest-block `ctrl+o` | `ctrl+o` is a **global** toggle, not per-cell targeting (issue #49; see "Fold" in `ui.md`). |

## Decision

The core CLI transcript adopts **Warm Ledger** as its **layout and interaction
system**. Themes remain **swappable color profiles** over a fixed set of
semantic and structural tokens. Boxed Zot-style artifact chrome is superseded
in practice; methodology (clear user rail, quiet agent prose, foldable tool
output) remains.

Warm Ledger is specified by:

- `docs/contracts/ui.md` (normative UI contract; absorbed the implementation
  program formerly kept as a working note)
- Design package *Euler TUI Spec* (option 3a); Spec text wins over Concepts mockups

## Ledger rules (normative summary)

- Chronological transcript of meaningful events.
- Default fixed **9-character** timestamp gutter (`HH:MM:SS`, faint).
- Hairline under each meaningful event; tool children and output tails nest
  without their own timestamps or hairlines.
- **No box-drawing borders** in the flow except **approval panels**.
- Universal fold key: **nearest-block `ctrl+o`** only (viewport-center closest
  foldable; tie → later block).
- Composer always accepts input (queue while working; live during approval).
- One mono family; hierarchy from color and weight, never size.
- Color via **roles** (user/success, fail, attention, read/companion) plus
  neutrals/structural tokens — not ad-hoc hues in renderers.

## Themes

- Profiles map roles → colors (`warm-ledger`, `gruvbox-dark/light`, later
  Solarized family, etc.).
- Renderers never hardcode palette hex; only tokens.
- Light profiles invert neutral lightness, keep role hues, validate before ship.

## Non-goals

- Sidebars, dashboards, or a second chat pane in core CLI.
- Workflow logic in core (e.g. causal-DAG semantics beyond dispatch to an
  extension command).
- Core `Companion` lifecycle types (UI may present extension/agent events as
  nested sub-ledgers).
- Dumping the user-visible ledger into the model canvas (ADR 0002).
- Fake scoped permission labels without real grant scopes.
- Checkpoint UI without workspace pre-image substrate.

## Consequences

- Implementation proceeds as multi-slice work on **one long-lived branch**; no
  intermediate PRs until the user dogfoods and requests one.
- Render fixtures (vt100 / transcript tests) are required for visual changes.
- Box chrome is deleted or replaced, not kept in parallel forever.
- Reasoning display policy is refined in ADR 0007 amendment + `ui.md`; opaque
  provider artifacts are never rendered by core.
- Later slices that need new authority (scoped grants, workspace checkpoints,
  extension slash registration) update contracts **before** or **with** the
  first honest UI for those features.

## Related

- ADR 0002 (provenance / canvas separation)
- ADR 0007 (UI and reasoning display — amended for Warm Ledger)
- ADR 0003 (multi-agent scaffolding — companion UI is presentation only)
