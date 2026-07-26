---
name: euler-change-delivery
description: "Deliver a focused Euler change through branch setup, implementation, verification, commit, review response, and pull request preparation. Use for repository changes that will be committed or proposed upstream."
---
# Euler change delivery

1. Read `AGENTS.md`, `EULER.md`, and the contracts or ADRs governing the changed surface.
2. Start from current `origin/main` on a focused branch. Use a separate worktree when independent writing or review work is already active.
3. Keep the change within one architectural owner. If ownership is unclear, apply `docs/contracts/boundaries.md` before coding.
4. Update implementation, deterministic tests, and the owning contract together. Amend an ADR when the decision itself changes.
5. Run focused checks first, then the full repository gate from `EULER.md`. Do not hide failed or flaky runs.
6. Inspect `git diff --check`, changed paths, and the staged diff for secrets or unrelated edits.
7. Keep commit and PR language about the fix or feature itself. Do not include investigation tooling, unrelated repositories, credentials, or private session details.
8. Do not push, force-push, open a PR, merge, or delete branches without explicit user approval. Use `--force-with-lease` only when rewriting an already-approved topic branch.
9. When responding to review, verify the finding against code and contracts. Add the narrow fix and regression coverage, rerun affected gates, and update the existing commit or add a follow-up according to the user's direction.
