# Euler repository audit work log

Started: 2026-09-05. Baseline: `main` at `9dfb881` (`fix: make process tool outcomes truthful (#210)`).

## Scope and working rules

User requested a deep understanding of the repository, followed by a comprehensive macro-to-micro bug and optimization audit. User subsequently requested this root-level Markdown log to preserve continuity. This is an analysis task: no production-code changes, commits, PRs, or external publication are planned. Findings must distinguish reproduced bugs, source-backed risks, intended tradeoffs, and measurement-dependent opportunities.

Read repository guidance: `EULER.md`, `docs/vision.md`, `docs/contracts/boundaries.md`, README, roadmap, ADR index, security policy, relevant contracts. No `AGENTS.md` found in repository/ancestor locations. The initial review was solo; three subagents subsequently completed bounded reviews after the user's explicit request.

**Current status: audit complete.** The finalized findings, implementation order, and verification limits are in [REPO_AUDIT_REPORT.md](REPO_AUDIT_REPORT.md). Reproducible synthetic evidence is preserved in [audit/2026-09-05](audit/2026-09-05/README.md). Earlier sections below retain the investigation chronology; the final completion record supersedes pending statuses and exploratory measurements.

## Repository map

- Seven Rust crates: `euler-event` (canonical envelopes/outcomes), `euler-sdk` (extension interfaces), `euler-managed-process` (stdio JSON-RPC extension runtime), `euler-agents` (agent records/budgets), `euler-core` (session, tools, permissions, provenance, compaction, extensions, project context), `euler-provider` (adapters/auth/catalog), `euler-cli` (CLI/TUI and integration wiring).
- Approximately 191,950 lines across files in crates/scripts, including tests, catalog JSON, snapshots; this is not a production Rust LOC count.
- Largest implementation files include core `session.rs` (4,069 lines), CLI `ui/app.rs` (3,511), `ui/transcript.rs` (2,326), core `tools.rs` (1,746), `extensions.rs` (1,737), provider `catalog.rs` (1,728).
- Intended architecture: canonical append-only provenance; transcript and working canvas are distinct projections; core owns invariants, extensions own workflows.
- Existing positives: detailed contracts, many regression/integration tests, explicit permission events, bounded extension protocol, content-addressed blobs, append reconciliation, cancellation-aware provider boundary, recent UI incremental rendering work.

## Verification record

- `cargo fmt --all -- --check`: PASS.
- `python3 -m unittest scripts.test_sync_provider_catalog`: PASS, 21 tests.
- Installed toolchain: Rust/Cargo 1.97.1. `cargo nextest` unavailable initially.
- First standard Cargo attempt could not write outside workspace to `~/.cargo`; switched to isolated writable cache and build directories without requesting permission.
- Test command: `env -u EULER_HOME CARGO_HOME=/private/tmp/euler-audit-cargo CARGO_TARGET_DIR=/private/tmp/euler-audit-target cargo test --workspace --locked`.
- Full output: `/private/tmp/euler-audit-tests.log`. CLI unit suite passed 1,042 tests; headless suite passed 106 with 1 ignored; other early suites passed. Core unit suite: 922 passed, 9 failed. Cargo stopped before later crates/integration suites.
- Nine failures are in `project_context::tests`: user skill admission/off policy, user/project duplicate names, user skill changes/digests, user path diagnostics, rendered catalog, indeterminate boundaries, declined snapshot skills. One explicitly reports sandbox `Operation not permitted` creating a non-UTF-8 user skill path. Need inspect fixture root construction and rerun isolated/with appropriate temp root before calling these product bugs.
- Python test generated `scripts/__pycache__/`; remove generated cache before completion (no user files were present initially).

## Coverage so far

- [x] Architecture, dependency/entrypoint map, CI/release workflow, contracts and roadmap.
- [x] Core tool dispatch, subprocess supervisor, prepare/apply file writes, result preview/rehydration, workspace snapshots.
- [x] Static shell safety parser and permission dispatch path (continue validating semantic gaps).
- [x] Provenance append, query pagination, eager rehydration; resume preflight/fold/model recovery.
- [x] Canvas pair selection, swaps, demotion, context projection; shared model round loop.
- [x] Provider shared HTTP path, ChatGPT/SSE, cancellation wrapper (partial; adapters/parsers/catalog need more).
- [x] Managed-process I/O and request deadlines; extension host APIs and event append (partial).
- [x] Auth/config/grants/scrub security and durability completion.
- [x] Project-context discovery/folding and failing tests.
- [x] Agent isolation/budgets, parallel spawn/observer lifecycle.
- [x] CLI/TUI source review and correctness sweep; live UI performance explicitly unmeasured.
- [x] Independent synthetic reproducers and scale benchmarks.
- [x] Final prioritized report with exact code locations, implementation steps, acceptance checks, and validation limits.

## Candidate findings and current confidence

These are investigation notes, not final adjudicated findings.

1. **Automatic safe approval can mutate files (high-confidence source finding; reproduce next).** `command_safety.rs` includes `uniq` in `READ_ONLY_BINARIES` (around 411), but `uniq input output` creates/truncates output. `tool_dispatch.rs` authorizes statically-safe commands under `ShellExec=Ask` without calling the decider. Do not imply this bypasses `AlwaysDeny`; that branch blocks static-safe approval.
2. **Static path analysis does not track actual read scope (high-confidence; reproduce next).** Unquoted globs allowed for unconditional read-only binaries; `cat *.txt` can expand to a sensitive file or outside symlink, while literal `*.txt` passes `arg_confined`. Recursive `grep -R`/`rg --follow` on a safe directory can follow outside symlinks. `cd sub && cat x` validates every segment against original cwd, rather than the shell's changed cwd. Group/partition findings by independent fixes after reproduction. All probes must use synthetic temp files.
3. **Prepared writes do not revalidate target and use truncating writes (source confirmed; reproduce next).** `tools.rs:665-684` calls `fs::write` on saved path/content. Add/create checks occur during preparation. An intervening file creation is overwritten; an intervening edit is lost. A directory/symlink swap can invalidate confinement. Proposed direction: stable handles, create-new/no-clobber for additions, optimistic preimage validation and atomic replacement for modifications, preserve metadata and durable preimage before destructive write.
4. **Canvas pairs keyed by provider tool ID globally (source confirmed; reproduce next).** `canvas.rs:901-948` uses `calls_by_id` and `paired_call_ids` keyed by payload `id`; later model rounds reusing that ID are omitted. Envelope parent identifies exact call already. Need validate scope guarantees and build two-round scripted case.
5. **Provider cancellation is logical, not physical (documented tradeoff; optimization).** `provider/lib.rs:714-784` detaches blocked sync HTTP worker after cancel. Shared HTTP builder (`chat_completions_provider.rs:205`) and ChatGPT HTTP builder set no explicit read/overall deadline and recreate agents each request. Need check underlying ureq defaults before claiming unbounded waits; propose bounded transport cancellation/deadlines and connection reuse. Avoid falsely saying UI cancellation itself hangs: the wrapper releases it promptly.
6. **Managed-process timeout cannot preempt synchronous host call (source-backed risk).** `runtime.rs:587` invokes host dispatch inline, checks deadline only after return; host spawn may consume >60 seconds or block. Need distinguish intended host-call semantics from actual timeout contract; inspect tests/docs before classifying.
7. **Replay/query scaling (source confirmed; benchmark pending).** `query_provenance` reopens at beginning for each after-event-ID cursor; scan-limit applies after locating cursor. Repeated pagination is quadratic in total history. Context-slot and plan APIs use full-log queries. `read_provenance` and resume eagerly read whole log and all blobs. Canvas assembly reconstructs whole-log indexes repeatedly; `active_swap` validates each historic swap and performs nested linear event-ID searches.
8. **Memory/I/O bounded only after acquisition (source confirmed; optimize).** `read_file` reads entire file before slicing; shell captures all output in unbounded Vecs before redaction/blob externalization; file diffs built fully before truncation. Each shell invocation captures entire workspace before and after (up to limits); snapshot overflow/IO failure results in no observed changes. Need measure and expose incomplete coverage instead of treating empty as no changes.
9. **Torn-tail resume is deliberately inspection-only (NOT a new bug).** Contract explicitly says readable final fragment permits inspection but fences all new appends. Consider an explicit recovery/repair UX, not silent truncation. UTF-8 tears may still block prefix inspection because readers call `read_to_string` before cutting to newline; investigate separately.
10. **Partial non-success provider stops may be presented as successful turns (candidate).** `session.rs:630-656` errors on MaxTokens/Refusal/Error only when content is empty; partial content becomes assistant message and normal completion. Need compare contract/intended semantics; may be better modeled as typed incomplete outcome. Empty non-success emits an error parented to model.result; verify resume doesn't misclassify duplicate terminal.
11. **Docs/toolchain drift (confirmed low priority).** README recommends Rust 1.80 although code uses `Option::is_none_or` and dependencies likely need newer Rust; workspace has no `rust-version` and CI has no pinned/tested MSRV. ADR index says next 0018 though 0018 file exists; roadmap lists headless resume as future though `ExecArgs.resume_path`/`run_exec_resume` exist. Update based on current shipping behavior.

## Next actions

1. Inspect user-skill fixture helper; resolve verification environment without reading real user skills/credentials.
2. Create standalone Cargo probe project under `/private/tmp/euler-repo-audit/` depending on local crates. Reproduce safe-command, write-race, repeated tool-ID, UTF-8 torn-tail, and partial stop behavior against unmodified production source.
3. Complete tests (nextest if installed into temp root, or isolated/serial Cargo fallback), clippy, docs tests; record exact outcomes and exceptions.
4. Finish remaining source sweep, run synthetic replay/pagination and workspace snapshot benchmarks, then write root-level final audit report.

## Progress update — verified findings, delegation, and measurements

The user explicitly authorized subagents after the initial solo review. Three bounded read-only reviews are now running: `provider_auth_audit`, `context_agents_audit`, and `extensions_ui_audit`. Their detailed evidence files will be incorporated into the final report; agents have not edited production source or accessed real credentials.

### Updated verification

- User upgraded Homebrew Rust during the audit: active toolchain is now Rust/Cargo 1.98.0. Earlier results began on 1.97.1; the final workspace rebuild completed on the new toolchain.
- Initial nine project-context failures adjudicated: eight result from tests passing noncanonical macOS `/var` tempfile paths to a deliberately canonical-only discovery API. CLI production paths come from canonicalized EulerHome. The remaining test assumes Unix filesystems allow byte `0xff` in a filename; APFS rejects it. These are test portability defects, not nine production regressions.
- Full workspace fallback command completed successfully: `env -u EULER_HOME TMPDIR=/private/tmp CARGO_HOME=/private/tmp/euler-audit-cargo CARGO_TARGET_DIR=/private/tmp/euler-audit-target cargo test --workspace --locked -- --skip project_context::tests::user_skill_path_diagnostics_do_not_change_project_acknowledgment_digest`.
- Result: **2,753 passed, 0 failed, 3 pre-existing ignored, 1 explicitly filtered** across 35 test-suite results, including doc tests. Log: `/private/tmp/euler-audit-tests-canonical.log`.
- `cargo nextest` still unavailable; do not claim the exact nextest gate ran.
- Clippy running, log `/private/tmp/euler-audit-clippy.log`.
- Independent local Cargo probe package: `/private/tmp/euler-repo-audit/`. Uses local production crates and a copied baseline lockfile (initial exploratory run used newly resolved dependencies; repeated against baseline graph before confirmation). Final probe rerun log `/private/tmp/euler-audit-probes-final.log`.

### Confirmed root probes

- `uniq input.txt output.txt` overwrites output with **FsWrite=AlwaysDeny, ShellExec=Ask, zero decider calls**; recorded `static-safe`.
- `cat *.txt`, `cd nested && cat view.txt`, and `rg --follow SYNTHETIC .` all pass static analysis and read a synthetic outside-workspace file via symlinks. Direct `cat public.txt` correctly rejects the same symlink. `grep -R` did not follow the fixture symlinks on this macOS; do not claim that specific variant was reproduced here.
- Prepared create overwrites an intervening user-created file. Prepared edit overwrites an intervening user edit.
- Two valid sequential model tool rounds reusing a provider call ID leave two canonical tool results but only the first result in canvas.
- Empty MaxTokens result causes `Session::run_turn` failure and a subsequent resume failure: `DuplicateModelTerminal`. The extra provider error is parented to an already-terminal model.result and recovery matches it to the closed call.
- Partial MaxTokens result returns normal `Ok` and assistant.message. Treat as outcome/API design gap unless contract establishes it as a defect; provenance does retain stop_reason.
- Torn UTF-8 final fragment makes replay/resume reject the entire log even though query_provenance successfully returns the valid complete prefix.
- Newly added probe verifies revoked unscoped session grant is revived by resume (pending final probe result). Source: session.rs:1598-1604 revokes in memory only; resume.rs:408-440 restores historical allow with no revocation fold. Subsequent test attempts a write with denying decider.

### Synthetic scale measurements

Debug build; local machine; relative scaling observations, not production latency guarantees. Script: `/private/tmp/euler-repo-audit/src/bin/scaling.rs`; output `/private/tmp/euler-audit-scaling.log`.

| Workload | Measurement |
| --- | --- |
| Full paginated query of 1,000 events (256/page) | 17.277 ms |
| Same, 5,000 events | 241.303 ms |
| Same, 10,000 events | 941.912 ms |
| Canvas 100 rounds / 10 layer-1 swaps | 1.310 ms median of 3 |
| Canvas 500 rounds / 50 swaps | 12.387 ms |
| Canvas 1,000 rounds / 100 swaps | 40.387 ms |
| One workspace snapshot: 1,000 files × 16 KiB | 513.103 ms |
| 4,097 files, one actual edit | Zero observed changes (documented incomplete-snapshot behavior; improve observability) |
| Read 1 line / 16 bytes from 32 MiB file | 63.888 ms; 96 output bytes incl. marker; whole file acquired |

### Agent findings to reconcile into final report

- Context/agents: no-parent-canvas companion discards its own prior tool results on later rounds (reproduced); sequential companion lacks token admission used by parallel path (reproduced: 8 KiB task enters 10-token context); compaction summary can carry excluded project context into a `project_context:none` child (deterministic synthetic marker reproduced). Notes `/private/tmp/euler-context-agents-audit.md`.
- Extensions/UI: TUI Add first links, then attempts install, causing deterministic ModeConflict; Add key unavailable when registry empty. Failed native extension registration leaves earlier commands installed; descriptor panic escapes catch. Managed-process nonzero shutdown exit reaps leader before abort, leaving descendant alive (synthetic child wrote marker later, then exited). Pending explicit run revocation is source-backed, not reproduced. Notes `/private/tmp/euler-extensions-ui-audit.md`.
- Providers/secrets: shared workspace checkpoint scrub breaks other sessions' references (reproduced); checkpoint preimages bypass known-value redactor (reproduced); short-before-long known-value redaction leaks suffix (reproduced); request-time/legacy auth taint gap; permissive SSE drops malformed content; custom config syntax/compat options mismatch. Agent is producing final notes `/private/tmp/euler-provider-audit.md`.
- Refuted candidate: TUI does NOT silently ignore headless-only model/compaction flags; parser rejects them explicitly.
- Managed-process synchronous host-call deadline overrun is explicitly documented at extension-sdk.md:495-497; classify as architectural limitation, not a new bug.

### Finalization plan

Finish clippy and final root probes, read and adjudicate agent notes, collect exact locations, and write `REPO_AUDIT_REPORT.md` at repository root. Report must connect macro priorities (durable authorization, actor-scoped context, bounded data paths, transactional extension lifecycle, authoritative secret taint) to focused implementation changes and acceptance checks. Preserve positive architecture/test coverage and clearly disclose untested live-provider/Linux behavior. Remove generated Python cache only; leave audit Markdown files for user review.

## Completion record — 2026-09-05

Completed [REPO_AUDIT_REPORT.md](REPO_AUDIT_REPORT.md): repository model and strengths, macro-to-micro implementation map, **33 individually classified findings**, **8 optimization opportunities**, ordered focused-change roadmap, regression criteria, verification, and explicit limits. Findings distinguish reproduced bugs, source-backed risks, an outcome/API design gap, documented limitations, and maintenance issues. No claim of formal line-by-line proof or live-provider validation is made.

Three subagents completed independent reviews of context/agents, providers/auth/secrets, and extensions/runtime/TUI. Provider and extension reviewers additionally checked the final report for overclaims and remediation mistakes. Their corrections are incorporated, including original-input overlap-span redaction, exact parser locations, process cleanup timing, and immediate launch revalidation.

### Final verification

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets --locked`: passed; [preserved output](audit/2026-09-05/outputs/workspace-clippy.log).
- Python catalog tests: 21 passed.
- Workspace Cargo tests including docs: **2,753 passed, 0 failed, 3 existing ignored, 1 explicit filesystem-incompatible fixture skip**, across 35 suite results. [Final log](audit/2026-09-05/outputs/workspace-tests.log) and [initial failed run](audit/2026-09-05/outputs/workspace-tests-initial.log) are preserved. The initial nine failures were fixture portability issues, as documented above.
- `cargo nextest` unavailable; Cargo fallback used. Do not describe this as the exact nextest gate.
- Final toolchain: `rustc 1.98.0 (88d9e12ae 2026-08-18) (Homebrew)`.
- Root defect probes: **10 passed**. Revoked-grant restoration confirmed; the additional failed-stream probe confirmed captured partial text is present in memory but absent from durable provenance after transport failure.
- Consolidated evidence: all seven binaries built and ran, retaining their observed outputs. The package was copied into the repository, formatted, and checked through `cargo test --manifest-path audit/2026-09-05/Cargo.toml --locked --all-targets` with isolated Cargo/build directories and canonical TMPDIR: passed. [Portable package test output](audit/2026-09-05/outputs/portable-tests.log).
- Audit report/log/evidence README link checks passed; report has consecutive unique F01–F33 and O1–O8 identifiers.

The defect probes intentionally assert the observed baseline failures. They are evidence, not desired-behavior regression tests. Convert the relevant probe when fixing an issue.

### Final release measurements

The earlier debug figures remain historical notes and are superseded by these optimized results for recommendations. Rust 1.98.0, local synthetic workload; pagination and canvas medians of three, snapshot/read single captures. [Source and instructions](audit/2026-09-05/README.md), [release output](audit/2026-09-05/outputs/scaling-release-baseline.log).

| Workload | Release measurement |
| --- | ---: |
| Full pagination, 1,000 events, 256/page | 1.691 ms |
| Full pagination, 5,000 events | 24.514 ms |
| Full pagination, 10,000 events | 81.489 ms |
| Full pagination, 20,000 events | 311.193 ms |
| Canvas: 100 rounds / 10 swaps | 0.228 ms |
| Canvas: 500 rounds / 50 swaps | 1.748 ms |
| Canvas: 1,000 rounds / 100 swaps | 5.187 ms |
| One snapshot: 1,000 files × 16 KiB | 47.050 ms |
| Request one line from 32 MiB | 23.503 ms |
| 4,097 files with one edit | Zero reported changes; documented incomplete-capture behavior |

### Delivered artifacts and final workspace state

- [Report](REPO_AUDIT_REPORT.md): final adjudicated findings and proposed implementation sequence.
- This log: continuity, investigation chronology, final verification.
- [Evidence package](audit/2026-09-05/README.md): standalone Cargo package outside the production workspace, synthetic probes, Python peer fixture, recorded results, and validation logs. No compiled binaries, caches, credentials, or real session data included. Temporary originals may be discarded later; the useful evidence is now preserved locally in the repository.
- Removed the audit-generated `scripts/__pycache__/`.
- Tracked production source and root lockfile unchanged. Only the two root Markdown files and `audit/2026-09-05/` are new. No commits, PRs, extension installation, real model requests, or external publication performed.

No audit work remains pending. Implementation of the proposed fixes is a separate phase; this request was for investigation and recommendations.

## Subsequent PR stack work

The user's later request to inspect and unblock PRs #211–#220 is tracked separately in [PR_STACK_LOG.md](PR_STACK_LOG.md), preserving this audit's baseline and conclusions. That log records live GitHub state, merge requirements, targeted fixes, and the #217 product split assessment.

## Second-pass verification — 2026-09-05

An independent agent re-verified the completed report on the same baseline (`9dfb881`, Rust 1.98.0) at the user's request, using six parallel read-only reviewers plus a fresh rebuild of the ten root probes. Full record: [REPO_AUDIT_REPORT.md § Second-pass verification](REPO_AUDIT_REPORT.md#second-pass-verification).

- **Outcome:** all 33 findings confirmed against the source; none refuted. Ten probes reproduced from a clean target directory. F09, F11, F12, F13 additionally exercised through the built CLI with the fixture provider.
- **Re-graded:** F12 raised to P1 (silent context loss with no warning). F08, F13, F28, F29 lowered to P3 (latent or narrow incremental impact; reasons recorded in each finding).
- **New findings:** F34 static-safe write into `.git/config` plus static-safe `git status` reaches code execution; F35 explicit permission mode changes are not durable and scoped grants are dropped on resume; F36 checkpoint is captured from the prepare-time preimage and stored after the write; F37 canvas assembly has no per-actor filter for ordinary tool rounds and rehydration ignores the parent-canvas flag; F38 no production HTTP or WebSocket transport has a read timeout.
- **Amendments recorded inside existing findings:** ChatGPT adapter's provider-local error scrub (F05); `.env.local` path-gate gap (F06); `AmbiguousModelTerminal` variant and untested resume for the existing empty-stop test (F09); MaxTokens treated as success by root but failure by companion/parallel (F11); Anthropic parser shares SSE's duplicate-terminal and empty-id defects (F17); `OutputLimitExceeded` shares F25's reaping gap; two more accepted-but-unread `supports_*` flags (F19); false "not a network call" doc comment (F18); false "side-effect-free" register doc comment (F29); six persistence-contract references (F33).
- **Roadmap:** the first tranche was rewritten into a twelve-item day-one tranche of small independent edits and a five-item contract tranche. Lowered items were moved to the "Next" table.
- **Workspace state:** only the two root Markdown files changed. Production source, `docs/`, and `audit/2026-09-05/` untouched. Scratch fixtures under `/private/tmp/ev-f09`, `/private/tmp/ev-f12`, `/private/tmp/ev-f13`.
