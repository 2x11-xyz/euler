# Anchor spine implementation checklist (Spec v2 §0/§1/§2)

Working branch: `fix/warm-spine-anchor` off `feat/warm-ledger-tui`.
Spec §0 is normative; the design board frame labeled `canonical` shows the
target; SUPERSEDED frame is v1 — do not implement from it.

## Layout contract (v2)
- Flush-left single column: 2-character anchor spine (glyph + space) then
  content. No timestamp gutter by default.
- Anchors by event type: `•` default (prose, tools), `✱` thinking, `✓`/`✗`
  decision records (green/red — the anchor IS the decision color, text dim,
  no gold), `◆` companion, `↩` revert, `⚠` provider, `■` interrupt. User
  messages: tan `▌` rail instead of an anchor (both lines of wrapped
  messages keep the rail).
- Sub-steps (tool children, output tails, diff previews) indent under
  `├`/`└`; never get their own anchor. Every action event pairs with exactly
  one `└` result line when collapsed.
- Separation: one blank line between events. NO hairlines between events.
  Only rules left in the flow: turn dividers (`── Worked for Ns ──`) and the
  composer rule.
- Timestamps: none inline; `/timestamps` opts IN a 9-char gutter beside the
  spine (whole column shifts right together). Default off. Preference load
  in main.rs flips default.
- Theme id: keep `warm-ledger`, alias `warm-spine` — never break config.

## Implementation order
1. text.rs: spine constants (2-char), gutter default off; keep
   `with_timestamp_gutter` machinery as the opt-in (§5.5) — spine width added
   to gutter width when enabled.
2. glyphs.rs: `spine_anchor(kind)` accessors reusing the F25 GlyphSet
   (ASCII fallbacks: `*`, `ok/x`, `&`, `<-`, `!`, `#`?, per §2 table).
3. render.rs: replace `push_hairline` per-event with blank-line separation +
   `stamp_first_line`-style spine stamping (anchor chosen per
   TranscriptItem); remove hairline pushes (keep markdown h1/h2 underline —
   that one stays per §4). Continuation lines: two spaces.
4. cells.rs: artifact/tool cells — dim `•` anchor + single `└` result line
   when collapsed (fold marker moves under it); decision records re-anchored
   (drop the `({decision})` suffix wording per audit S3).
5. Composer flush-left: sending becomes a purely vertical transition
   (composer line becomes ledger entry in place; check composer/render
   left padding).
6. F27 rides along: shell running state (spinner + elapsed + 2-line tail)
   and done informative-line already exist — restructure to the `└` result
   pairing.
7. Tests: transcript_tests vt100 fixtures re-baselined; PTY tests updated
   (the 9-space indent assertions disappear); hairline test
   `hairline_uses_dedicated_theme_token_not_gutter` becomes a turn-divider /
   markdown-underline test.

## Coordination
- Do NOT touch status.rs/footer (footer-status agent) or markdown.rs table
  internals (table-density agent) until their branches merge.
- Audit items riding on the spine: decision-record color/wording (S3),
  explore tree alignment polish (verify post-spine), recap placement under
  the Worked divider (S3 recap item).
