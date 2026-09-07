# PR 216 root response persistence fence

Worktree: `/private/tmp/euler-wt/stack-216`
Base HEAD: `8d56daed97046214dc4a897830404d14c26c26a5`
Status: changes left uncommitted; no push or PR metadata changes.

## Patch

- `crates/euler-core/src/session.rs`: retain the existing reopen-only fence when an open root response exits with unpersisted or unresolved provenance. Clear checkpoint ownership only after a successfully appended canonical model result, provider error, or cancellation terminal. Respect pending user-admission retry ownership. Reject new turns before other work and report the fence through `can_accept_turn()`.
- `crates/euler-core/src/session_test.rs`: strengthen the original checkpoint fault test with two successful scripted responses, one physical-dispatch assertion, no added events/admission/log bytes, blocked control operations, and correct recovery. Cover buffered suffix, reasoning, model-result, provider-error, and cancellation-terminal sync failures. Positive controls preserve ordinary follow-up after durable provider failure and exact reconciliation after post-terminal assistant-message/tool-result append failures.
- `docs/contracts/provenance.md`: clarify response ownership through the accepted canonical terminal, reopen handling for reasoning/terminal failures, and retention of an already-recorded terminal outcome.

## Final-state verification

All Cargo commands ran from the worktree with:

```text
env -u EULER_HOME CARGO_HOME=/private/tmp/euler-audit-cargo CARGO_TARGET_DIR=/private/tmp/euler-pr216-fix-target
```

Recorded results from tool output (compile chatter omitted):

```text
cargo test --offline --locked -p euler-core --lib session:: --quiet
test result: ok. 244 passed; 0 failed; 0 ignored; 0 measured; 719 filtered out; finished in 14.93s

cargo test --offline --locked -p euler-core --test session_loop --test resume --quiet
test result: ok. 53 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.57s
test result: ok. 151 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 6.89s

cargo clippy --offline --locked -p euler-core --all-targets
Finished `dev` profile [unoptimized + debuginfo] target(s) in 6.27s
Exit status: 0

cargo fmt --all --check
Exit status: 0

git diff --check
Exit status: 0
```

The 244 session unit tests include all 11 `session::steering::lifecycle_tests` cases. These independently passed as a focused suite before the final full-session run. Final format/diff checks were repeated after the documentation clarification.

Full Clippy output: `/private/tmp/euler-pr216-fix-clippy.log`
Patch export: `/private/tmp/euler-pr216-response-fence.patch`
Original failing reproducer evidence: `/private/tmp/euler-216-fence-probe-result.txt`

## Contract assessment and limits

The original defect violated the documented lifecycle-reopen boundary for an ambiguous root checkpoint. The patch retains response ownership through durable terminal acceptance and never synthesizes a successful terminal. Reopen preserves physically complete canonical outcomes; an open prefix receives exactly one interrupted recovery closure. Completed root calls release ownership so later tool-result, presentation, and queued-admission failures keep their existing exact retry semantics.

Adjacent pre-existing limitation, deliberately outside this patch: root `prepare_model_request` assigns the response checkpoint only after the `model.call` append succeeds. A post-write sync error of `model.call` itself can therefore leave an unterminalized bus call which later backlog reconciliation accepts before another turn. No provider dispatch occurred for that call. This is a broader pre-dispatch call/admission lifecycle gap against the eventual-terminal requirement, not the confirmed partial-response defect; a separate regression and adjudication should precede widening this fix.

Validation ran on the local macOS host with synthetic providers and isolated build output. Full workspace CI, Linux, and PTY suites were not rerun for this bounded core fix. Existing PR 216 PTY synchronization changes were preserved.

## Final ownership simplification at ff37987

After adjudicating Claude's supplied-source review, the condition was simplified to
`result.is_err() && io.response_checkpoint.is_some()`. No test fixtures changed.
The same 244 session, 53 resume, and 151 session-loop tests passed again, as did
core all-target Clippy, formatting, and diff checks. Commands used the environment
above plus `TMPDIR=/private/tmp`; output is saved in `pr216-final-session.log`,
`pr216-final-integration.log`, and `pr216-final-clippy.log` in this directory.
The correction was committed and pushed normally at
`ff37987de3f64f09e44fb79e1b88d71cff00936a`. See the root PR log for final CI.
