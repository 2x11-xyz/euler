# PR stack follow-up log

Started 2026-09-05 (local timezone), following the repository audit. Repository: `2x11-xyz/euler`. Root checkout is now on `main` at **`c3249adcb3a512e96780ade3fa3011bdca664ad8`**, containing the requested #214 → #211 → #212 merges. The original audit files retain their `9dfb881` baseline.

## Current handoff

- **Merged:** #214, #211, #212, in the requested order, with fresh green checks and no protection bypass. Local main was fast-forwarded to the final squash commit.
- **#213:** fixed a confirmed shadow-compaction retry defect, pushed `ff70bb6`, fresh CI green. Still needs retarget/rebase onto main after #215.
- **#216:** PTY synchronization and response persistence corrections are pushed; final head `ff37987` is green in fresh CI `34011528944`.
- **#218:** corrected ADR 0020 at `7bd4ee6`, fresh CI green. The user's lifecycle revert and retained-result deadlock fix remain intact.
- **#219/#220:** current heads green; sibling branches require combined integration as they advance through the stack.
- **#217:** hold and split; unresolved compatibility decisions affect both macOS and Linux workflows.

The requested initial merge handoff is reached. Do not merge the remaining train automatically: next is #215's rebase, then #213's retarget/rebase, followed by #215 → #213 → #216 → #218 → #219 → #220 with fresh checks at each updated head.

| PR | Verified remote head / squash | Handoff state |
| --- | --- | --- |
| #214 | squash `ab087bf` | Merged |
| #211 | squash `f602756` | Merged |
| #212 | squash `c3249ad` | Merged |
| #215 | `0a215f8` | Green on existing head; update onto main, preserving #214 compatibility |
| #213 | `ff70bb6` | Correction green; retarget/rebase from feat/activity-projection to main |
| #216 | `ff37987` | Final head green; preserve all fixes during stack rebase |
| #218 | `7bd4ee6` | Green; preserve deadlock fix, lifecycle revert, and corrected ADR |
| #219 | `6fde560` | Green; update after #218 lands |
| #220 | `fa2a165` | Green; integrate #219 sibling changes before its turn |
| #217 | `c3fcdbf` | Hold; product decision and split remain |

These are head-specific checks, not a promise that stale branches are merge-ready. Saved [PR state](audit/pr-stack-2026-09-05/pr-state.json) and [eight successful CI runs](audit/pr-stack-2026-09-05/ci-results.json) provide the verification snapshot.

## Requested sequence

User supplied pushes for #218 and #219, then squash merges #214 → #211 → #212. After these land, the stated handoff is to rebase #215 onto main and retarget/rebase #213 off #211; later order is #215 → #213 → #216 → #218 → #219 → #220, each green. #217 remains a product decision, with a recommendation to split and hold macOS shell removal.

## Live state at inspection

| PR | Head | Base | Observation |
| --- | --- | --- | --- |
| #214 | `baa7eef` | main | Open, mergeable, green; behind base |
| #211 | `af3f8f1` | main | Open, mergeable, green |
| #212 | `a766ac5` | main | Open, mergeable, green; behind base |
| #215 | `0a215f8` | main | Open, mergeable, green; behind base |
| #213 | `aa7c478` | feat/activity-projection | Open, mergeable, green |
| #216 | `ddcf9ec` | fix/model-call-liveness | Open, CI failed |
| #218 | `be7d6ae` | feat/partial-response-durability | Remote already equals local; CI running |
| #219 | `6fde560` | feat/durable-run-queue-core | Remote already equals local; CI running |
| #220 | `fa2a165` | feat/durable-run-queue-core | Head CI green; mergeability being computed |
| #217 | `c3fcdbf` | main | Conflicts; product scope unresolved |

The requested #218/#219 pushes had already happened before inspection, so no redundant force-push was performed. Both named worktrees are clean.

## Review and actions

- Read current PR bodies, heads, checks, commit lists, and changed-file lists for the first three merges; inspected skill admission/advisory changes, TUI activity/recap wiring, and #212's touch/fallback correction.
- Delegated independent read-only checks of #216's CI failure, #218's revert/deadlock/ADR semantics, and #217's policy scope.
- #216's initial failure was the PTY fold/resize test (`post-resize expand did not reveal folded output`). The synchronization diagnosis and correction below are distinct from the reverted #218 lifecycle expectation.
- Merge commands will pin the reviewed head with `--match-head-commit`, preserve branches for stacked descendants, and include the required Codex co-author trailer. No admin bypass or broad permission-rule change is planned.

## Blockers and corrections

### Strict main rules require a fresh base

The first requested #214 merge was rejected: the head was not up to date with `main`. Repository ruleset **Protect main** (`18937760`) requires `checks` with `strict_required_status_checks_policy: true`. This explains why green/mergeable PRs marked BEHIND cannot merge. The regular branch-protection endpoint returns 404 because protection is configured through a ruleset.

Updated #214 with `gh pr update-branch 214 --rebase`, producing `0a062973d319795fe2b7a7b7992f1ef05c929c64`. Fresh CI run: `34010220171`. A successful squash of each PR will advance main and require the next branch to incorporate it and rerun checks. No admin bypass used.

- **#214 merged:** fresh checks succeeded, then squash commit **`ab087bfa3a179ae08084f3e1fb6bb0c10dd5e2cc`** landed at 2026-09-06 03:59:48 UTC. Verified the requested subject and Codex trailer.
- **#211 update:** GitHub's automatic rebase failed with a rebase conflict. Created isolated worktree `/private/tmp/euler-wt/merge-211`, branch `codex/merge-211`, and merged current main into the existing PR history. This merged cleanly, required no manual source resolution, and preserves downstream ancestry. Formatting and diff checks passed. Pushed normally to `feat/activity-projection` at **`088c47ad9d3c0e008e24dcfcc2068735c9e23bc4`**; CI run **`34010429849`**.
- **#211 merged:** CI `34010429849` succeeded; squash **`f6027560c2884c2f93ecefbae84e22cc93e3c415`** landed with subject `feat(ui): project truthful run activity in the TUI (#211)`.
- **#212 updated and merged:** isolated worktree `/private/tmp/euler-wt/merge-212`, branch `codex/merge-212`; merged main cleanly, passed formatting/diff checks, and pushed normally at **`b26a447f4b1cdb003721cd4d818a7240afdeeec1`**. Fresh CI `34010670557` passed; squash **`c3249adcb3a512e96780ade3fa3011bdca664ad8`** landed with subject `fix(session): harden projections and record runtime identity (#212)`.
- All squash messages include the Codex trailer. Branches were preserved for the stack; root main was fast-forwarded to `c3249ad`.

### PR #216: PTY completion synchronization

- Failed run `34007641391`, job `101417676043`: `tui_pty_fold_toggle_replay_after_resize_keeps_history_intact`, `post-resize expand did not reveal folded output`.
- The test/resize/Ctrl+O handlers were unchanged by #216. Its supposed completion barrier checked the first streamed fragment; the existing harness documents why that is insufficient and provides a persisted-event barrier.
- Three targeted baseline macOS runs passed (4.11s, 4.14s, 4.45s). This does not reproduce or disprove the Linux failure; evidence points to timing, not a provider retry or partial-response regression.
- Added `wait_for_home_session_event_count(..., ASSISTANT_MESSAGE, 1)` before fold operations. Kept the immediate resize → toggle sequence and history assertions intact. Corrected the earlier assertion's wording to “answer did not start streaming.”
- Fixed targeted test passed locally (7.30s); `cargo fmt --all -- --check` passed. Source changes are limited to the test.
- Committed and pushed normally, without force: **`8d56daed97046214dc4a897830404d14c26c26a5`**. Fresh Linux CI run `34010316759` passed.

### PR #213: shared retry policy for shadow compaction

- Confirmed using a public Session probe with a synthetic provider: `SemanticIdle` is categorized as Transport, and the worker's separate category-only predicate dispatched three times with retries=2. The ordinary round loop already excluded semantic-idle timeouts as required by the provider contract.
- Made the existing `provider_failure_is_retryable` predicate available to the sibling worker and reused it there. Added worker-level response-header/first-byte/semantic-idle stage coverage with a successful second response; clarified that the provider contract applies to both execution paths.
- Committed and pushed normally: **`ff70bb67ff65ae3e4c0f10334889c16175f56d8a`**. Fresh CI **`34010766743` passed**.
- Local verification: four compaction tests, four round-loop tests, formatting, core all-target Clippy passed. The public probe now records FirstByte=3 dispatches/2 retries and SemanticIdle=1 dispatch/0 retries.
- Evidence: `/private/tmp/euler-shadow-retry-probe/`, its `before-aa7c478.txt`, and `/private/tmp/euler-pr-stack-20260905/pr213-*.log`.
- Branch remains based on `feat/activity-projection`; preserve this correction when retargeting/rebasing onto main.

### PR #216: response persistence must fence unrelated activity

- Confirmed against `8d56dae` in a disposable source archive. The existing checkpoint FileSync test supplied only one fixture response, so an unauthorized second dispatch failed from fixture exhaustion and satisfied the weak assertion.
- With two valid fixture responses, the second turn succeeds: root dispatches increase **1 → 2**, events **5 → 12**, while the original call has **zero semantic terminals**. Later lifecycle reopen correctly recovers the original 23-byte response. The live session should have required that reopen before accepting unrelated activity.
- Implemented: retain root response ownership until a semantic terminal append succeeds; if an error abandons that owner, latch the existing terminalization fence. Reject fresh turn admission before side effects and expose the fence through `can_accept_turn`.
- Ownership refinement: clear `response_checkpoint` immediately after successful model.result, provider-error, or cancellation-terminal append. This preserves ordinary exact-batch reconciliation for later tool-result/assistant-message failures and the independent queued-admission retry owner. A broad fence covering all later appends was rejected in review.
- Strengthened regression verifies dispatch count, accepted event count, unchanged file bytes, blocked turn/rename/compaction, and valid reopen recovery. Added checkpoint suffix, reasoning, result, provider-error, and cancellation sync faults plus positive controls for durable failure follow-up and post-terminal append reconciliation.
- Final-state verification: **244 session unit tests**, including 11 steering lifecycle tests, **53 resume integration tests**, **151 session-loop integration tests**, core all-target Clippy, formatting, and diff checks passed.
- Committed and pushed normally: **`67abc785240028040527605f30c4a27f0b3e11b4`**. Fresh CI **[`34011113794`](https://github.com/2x11-xyz/euler/actions/runs/34011113794) passed**, including the Linux workspace test gate. The worktree is clean.
- Claude's independent review prompted a final simplification: use `result.is_err() && response_checkpoint.is_some()` directly. Pending admission cannot coexist with an open response in current call paths, and checkpoint accounting errors can theoretically precede a dirty writer. The implementation agent and root independently confirmed the simpler condition preserves post-terminal retry ownership and existing cancellation ordering. Follow-up **`ff37987de3f64f09e44fb79e1b88d71cff00936a`** was committed and pushed normally; all **448 affected tests**, core Clippy, formatting, and diff checks passed again. Final fresh CI **[`34011528944`](https://github.com/2x11-xyz/euler/actions/runs/34011528944) passed**.
- Baseline evidence: `/private/tmp/euler-216-fence-probe/`, `/private/tmp/euler-216-fence-probe-result.txt`, `/private/tmp/euler-216-fence-probe-test.patch`. Production source in the probe is unchanged; only its test was strengthened.
- Adjacent source-reviewed limitation: a failed `model.call` sync in `prepare_model_request` occurs before response ownership is assigned. Later exact backlog reconciliation can leave an unterminated call even though no provider dispatched. This predates #216's response feature and remains outside this patch; reproduce separately before broadening call-admission lifecycle changes.
- Selected probe and validation records are preserved in [audit/pr-stack-2026-09-05](audit/pr-stack-2026-09-05/README.md).

### PR #218: reverted finding was incorrect; ADR still described it

- At `be7d6ae`, the lifecycle fold rejects ordinary run-less root errors after lifecycle activity before session listing status selection. Restoring the removed override would contradict the contract.
- The retained-result/deferred-terminal deadlock fix correctly permits only exact retained result settlement while maintaining admission, pending-terminal, and invalid-state fences. Its regression exercises repeated sync failure and exactly one result/terminal settlement.
- Corrected ADR 0020's stale consequence paragraph: validation precedes status; a run-less ordinary error is Invalid, not a late override of a completed terminal; legacy status fallback remains.
- Documentation-only commit pushed normally: **`7bd4ee65e5fce00713c60580ecf3a14f3a6feffe`**. Fresh CI run `34010325194` passed. No lifecycle code or tests were reintroduced.
- Optional future coverage: double-failure retry with a bound steering queue, and a direct assertion that the rejected run-less error fixture projects Invalid.

Local Git had no author identity. New commits use per-command `user.name=Codex` / `user.email=codex@openai.com` plus the required co-author trailer; no global Git configuration was changed.

## Independent review and remaining integration

- The user requested headless Claude Code as an additional reviewer. Installed Claude Code 2.1.263 was run in safe/restricted plan mode without persistence, plugins, hooks, or MCP. The initial restricted launch could not use its configured login helper; an authorized Read/Grep/Glob launch returned no output for over ten minutes and was stopped. A subsequent tool-free, single-response review of supplied final patches and supporting source completed successfully within a three-minute deadline. No credential values were inspected and no automatic approval rejection occurred.
- Claude found no defect in #213 and raised three conditional #216 concerns. Full-source adjudication rejected the proposed pending-admission and cancellation triggering sequences as currently unreachable. Its remaining observation concerned theoretical checkpoint accounting overflow, not a reproduced storage failure. Adopted the simpler response-ownership condition; retained cancellation ordering. [Raw review](audit/pr-stack-2026-09-05/claude-review.md), [exact final prompt](audit/pr-stack-2026-09-05/claude-final-patch-prompt.md), and [adjudication](audit/pr-stack-2026-09-05/claude-adjudication.md) are preserved. Do not count these as three new confirmed bugs.
- #215 narrow source review found no new concrete blocker. During rebase preserve #214's derived skill names, mismatched-name advisory, Unicode description cap, separate diagnostic budgets, and loaded-warning UI, alongside #215's frozen descriptions/catalog/help/activation behavior. Run combined compatibility and activation tests. Existing audit F14 remains separate.
- #219 `6fde560` and #220 `fa2a165` received bounded source review against #218 `be7d6ae`; no concrete blocker identified. #219 cutoff, failure latch, standing authority, and canvas admission were traced; #220 expected-head reservation under queue lock and exact retry ownership were traced. They are siblings, so fresh integrated verification is still required. #220's cooperative retry budget cannot preempt a synchronous filesystem call; this is documented.
- When propagating #216 into #218, run the response-persistence fault/reopen regressions together with queued-admission and retained-result/deferred-terminal tests. The green standalone heads do not establish that the combined retry owners and run terminalization compose correctly.

## Workspace and publication record

- Root main is `c3249ad`; its tracked files are clean. The root Markdown logs/reports and `audit/` evidence remain untracked local artifacts for the user.
- Worktrees touched for #211/#212 integration and #213/#216/#218 fixes are clean. #219/#220 worktrees remain clean and unchanged.
- Existing branches were preserved. No new PRs, PR comments, issues, or other messages were published. Only the authorized initial three merges and focused corrections to existing PR branches were performed.
- No global Git configuration or broad permission rule was changed. Every new commit and squash message includes the required Codex co-author trailer.
- This phase is complete: initial merges, focused corrections, independent review adjudication, fresh CI, and local evidence are finished. Remaining rebases/merges and #217's product decision are the explicit handoff above.

## PR #217 product assessment and concrete split

Recommendation: hold #217 from this train and split it. Its existing “Accepted” ADR amendment does not resolve the user's pending product decision.

| Change in current #217 | Consequence |
| --- | --- |
| Reject non-Linux subprocess execution | macOS shell and built-in Git subprocesses fail while remaining advertised; structured tools and intercepted apply_patch still work |
| Mandatory no-network Bubblewrap on Linux | Shell git fetch/push, gh, package downloads, localhost, and networked builds lose access; provider/managed-extension networking is a separate path |
| Remove disabled/host-mode fallback | Full Access no longer permits ordinary unsandboxed execution |
| Clear environment and implicit home visibility | Existing credentials, configuration, caches, and home-managed toolchains become unavailable without an explicit design |
| Require complete pre-snapshot | Shell and Git views fail when capture bounds are exceeded; the bound is 4,096 granular entries per root, with separate opaque namespace accounting, not simply any repo with 4,096 files |
| Add root/mount/worktree authority and resume rules | Compatibility and migration changes extend well beyond a stale-write fix |

Suggested focused changes:

1. **Structured stale-write/path hardening:** extract create-new O_EXCL, descriptor-based confined opening, regular-file/link checks, exact preimage comparison, and failed-write observation against the current primary-root model. Avoid importing shell policy, attachment roots, `.worktrees` restrictions, or wholesale resume changes. This does not finish atomic-write durability: current #217 still truncates/writes the opened inode and stores the checkpoint after application.
2. **Git capability/helper hardening:** remove Git from static-safe classification, require ShellExec for Git views, disable optional locks/fsmonitor/external diff/textconv. Preserve explicit ordinary permission behavior; these settings are not proof that all repository-selected execution is harmless.
3. **Mutation observation:** keep typed process outcomes and explicit incomplete-observation reporting separate from the policy that incomplete snapshots block every command.
4. **Authority/product proposal:** hold mandatory sandbox defaults, non-Linux blocking, network policy, environment/runtime roots, additional writable roots, special-node/mount/worktree restrictions, and coordinated resume/checkpoint/scrub behavior together for an explicit compatibility/migration decision. Require a macOS replacement strategy, Linux workflow tests, and large-repository measurements.

Holding shell removal leaves macOS subprocesses with the host user's authority. File-tool hardening does not constrain shell filesystem/network access. The audit's uniq/glob/changed-directory/recursive-follow automatic-approval issues still need separate conservative fixes; #217 addresses only the Git portion of that allowlist. Keep this residual behavior accurately disclosed.

## Merge train completion — 2026-09-06 (Claude session)

Continued from the handoff above. Each PR was rebased onto the then-current `main`, verified locally (fmt, clippy, full `euler-core` + `euler-cli` suites; the only failures were the 16 known macOS `project_context` fixture tests, audit F31), pushed with a lease, re-checked green on fresh CI, and squash-merged. Strict protection required a fresh rebase after every predecessor landed.

| PR | Squash | Notes |
| --- | --- | --- |
| #213 | `6163ceb` | Rebased onto main; two import-only conflicts. |
| #215 | `7ccabdf` | Two rebases (CHANGELOG, then `app.rs` imports). |
| #216 | `fcc2e4d` | Conflicts in `canvas_test.rs`, `provenance.rs`, `provenance_test.rs` (all additive). |
| #218 | `704e77d` | See integration notes below. |
| #219 | `ff0e876` | Clean rebase. |
| #220 | `9fd5f12` | Two rebases; reconciliation commit `1468632` (see below). |

### Cross-feature defects found only by integrating

None of these were visible in any PR's own CI; each surfaced when two independently developed branches were combined.

- **#216 × #218: duplicate `preflight_session`.** Both added a different helper with that name and different return types; git merged them into one file with no conflict marker. Renamed #218's to `preflight_recovery_lifecycle`.
- **#212 × #218: scrub invalidated the runtime-identity digest.** On macOS temp roots live under `/private/var`; a scrub whose value list contained `private` rewrote `session.start`'s `root`, and the stored projection digest no longer matched, making the session permanently `Invalid`. Any secret that is a substring of a workspace path would do the same. Fix: `resync_session_start_projection_digest` in `runtime_identity.rs`, called from `provenance/scrub.rs` after a `session.start` payload rewrite. Scrub is a writer-owned, audited mutation, so re-deriving the digest preserves the check's purpose (detecting external drift).
- **#215 × #218: skill activation bypassed the admission pipeline.** #218 restructured user admission into `prepare_user_admission` → `user_admission_events`; #215's `user_message_payload` (skill expansion) was no longer called. Wired it into `user_admission_events`, which now returns `Result` so unknown skills reject before any pending admission is installed. Tests updated for the fallible queue APIs; the rejected-skill test now asserts deterministic-rejection invariants rather than pre-lifecycle queue contents (a failed run's terminal boundary releases volatile steering rows).
- **#215 × #220: skill activation during a running turn.** #220 replaced implicit mode selection with the steer/follow-up chooser. Extracted `queue_input_during_turn` so composer submission and skill activation share one path; the test parks a real turn so the queue has an admitted run.

### Still open

- **#217** remains held. The split plan was run through swarm-factory (three decorrelated reviewers) and adjudicated against `c3fcdbf`'s source; the result, with decisions A–E, four units, a hard-gated prerequisite, and the required test matrix, is in [audit/pr-217-split-plan.md](audit/pr-217-split-plan.md) (raw swarm output: [audit/pr-217-swarm-review.md](audit/pr-217-swarm-review.md)). A second pass compared decisions A–E against Codex's implementation ([audit/pr-217-codex-comparison.md](audit/pr-217-codex-comparison.md)); the approval table in [audit/pr-217-decision-summary.md](audit/pr-217-decision-summary.md) is revised accordingly, most notably: macOS gets a Seatbelt backend in Unit 2 instead of an unsandboxed interim, Linux fails closed without userns, and the static allowlist is replaced by a two-parser design.
- **#219 latency bound** — the request-tick "slot admission" commit fixed a TOCTOU in owner discovery but did not add the aggregate per-round latency bound the original review asked for. Still open.
- Roadmap day-one items 1, 2, 5, 6 (static shell allowlist, `.git/` write chain, F09 terminal, F12 pairing) are not in any open PR.

Worktrees for this train live under `/private/tmp/euler-wt/`; they can be removed once #220 merges.

## Subsequent personal authorship preference

At the user's request, installed the validated personal skill
`/Users/eli.bressert/.codex/skills/github-user-authorship/SKILL.md`, with a
discovery symlink under `~/.agents/skills` and a global `~/.codex/AGENTS.md`
instruction to load it for Git/GitHub authoring. The preference preserves
Eli Bressert's authenticated `ebressert` identity and omits optional assistant
co-authorship. Effective author and committer identity were checked without
creating a commit. Global Git configuration and existing history were unchanged.

The active session still requires the Codex co-author trailer. The skill
explicitly respects that higher-priority requirement; it does not claim to
disable mandatory attribution. Installation follows the official
[skill discovery](https://developers.openai.com/codex/skills/) and
[global instruction](https://developers.openai.com/codex/guides/agents-md/) guidance.
