---
name: euler-terminal-ui-change
description: "Implement and validate Euler terminal UI changes while preserving logical canvas ownership, transcript commit boundaries, native scrollback, deterministic input handling, resize behavior, replay, and PTY reliability."
---
# Euler terminal UI changes

1. Read `docs/contracts/ui.md`, `docs/contracts/canvas.md`, and the relevant terminal architecture tests before editing.
2. Keep logical canvas assembly in core and terminal projection in the CLI. Do not move semantic state into renderer-only structures.
3. Preserve the boundary between committed transcript history and the active interaction region. Native scrollback, replay, folding, and resize must not duplicate or lose history.
4. Prefer deterministic synchronization and semantic readiness signals over sleep-based timing.
5. Cover pure state transitions with unit tests, rendering behavior with snapshot or projection tests, and terminal lifecycle behavior with PTY integration tests.
6. Run focused tests repeatedly when touching resize, interrupt, replay, or folding paths. If an unrelated test flakes, report the failed run, reproduce it independently, and rerun the full gate honestly.
7. Do not weaken architecture ratchets, remove assertions, or expand allowlists merely to make a UI test pass.
8. Validate non-interactive and narrow-terminal behavior where applicable, then run the full repository gate from `EULER.md`.
