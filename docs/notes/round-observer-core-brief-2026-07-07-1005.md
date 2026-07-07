# Round Observer Core Primitive Brief

Date: 2026-07-07 10:05 UTC

## Role

You are an implementation worker in the Euler old engineering repo. Read
`AGENTS.md` first, then the specific docs named below. This is a core seam
slice with tight LOC pressure; smallest correct change wins.

## Required Context

- `docs/adr/companion-roundloop-and-round-observer-2026-07-06.md`
- `docs/contracts/boundaries.md`
- `docs/contracts/extension-sdk.md`
- `docs/contracts/multi-agent.md`
- `docs/contracts/canvas.md`
- `docs/contracts/causal-dag.md`
- `docs/budget-2026-07-06.md`

## Goal

Implement the product-neutral core primitive for a live round-boundary
observer hook. The primitive must make it possible for a session to run this
generic composition at a cadence boundary:

1. Execute an enabled extension command that builds an observer/companion brief.
2. Run one companion task from that brief.
3. Execute an enabled extension apply command with the companion output folded
   into the brief-provided apply input.

The causal-DAG extension is the first consumer, but core must not learn any
causal-DAG vocabulary.

## Scope

Expected files, but use judgment if the seam requires a nearby test file:

- `crates/euler-core/src/session.rs`
- `crates/euler-core/src/session/round_loop.rs`
- `crates/euler-core/src/session/companion.rs` only if cancel-aware companion
  execution can be made small and clean
- `crates/euler-core/src/session/extension_bridge.rs` only if a helper belongs
  there
- focused tests under `crates/euler-core/src/session_test.rs` or existing
  session/companion tests

## Non-Goals

- Do not add CLI flags in this slice.
- Do not add causal-DAG-specific core code, event kinds, names, or schemas.
- Do not alter `causal-dag` extension semantics unless a focused core test
  absolutely requires a tiny fixture helper.
- Do not add broad compatibility layers.
- Do not make the observer default-on.
- Do not add new `#[allow]` attributes.

## Known Seams

Live extension execution exists:

- `Session::execute_extension_command` in
  `crates/euler-core/src/session/extension_bridge.rs` executes an enabled
  extension command with a caller-supplied granted capability set and returns
  raw `serde_json::Value`.
- It calls `persist_new_events()` before host construction and publishes queued
  extension events back into the session bus after execution.
- Capability grants are explicit; do not bypass them.

Companion execution exists:

- `Session::spawn_companion(AgentTask) -> Result<AgentResultSummary,
  SessionError>` in `crates/euler-core/src/session/companion.rs`.
- Empty provider/model in the `AgentTask` inherits the active session target.
- `AgentBudget.max_tokens` counts input + output; `max_turns` maps to
  `RoundLoopConfig.max_rounds`.
- Current companion cancellation is weak: it uses a local never-tripping cancel
  flag. If making it cancel-aware is small and honest, do it; otherwise report
  it as a remaining blocker instead of inventing a fake fix.

Round loop seam:

- `RoundLoop::run` in `crates/euler-core/src/session/round_loop.rs` calls
  `io.round_completed()` after each clean model round.
- Add an Io-side hook method rather than hard-coding observer work into
  `RoundLoop`. Companion loop must remain no-op so observer companions cannot
  recursively observe themselves.

## Design Requirements

- `SessionConfig` gains an optional round observer config. Keep it small,
  cloneable, debug-friendly, and product-neutral.
- The config must be sufficient for core to run the generic brief -> companion
  -> apply chain. If an executable extension object cannot reasonably live in
  `SessionConfig`, choose the smallest clean alternative and explain it in the
  report.
- Observer cadence is every N driver rounds for this slice. If the event-count
  force-trigger would require large plumbing, leave it explicitly deferred in a
  test/documented TODO only if the implemented shape does not foreclose it.
- Hook is fail-open: any observer failure is recorded in diagnostics or a
  bounded session error event if that is already the local pattern, but it must
  not fail the primary model turn.
- Hook output must be provenance-recorded through the existing extension and
  companion machinery; no parallel logs.
- Flush the live sink after hook work so TUI/headless sinks can observe emitted
  events in order.
- Do not put raw provenance in canvas assembly.

## Test Requirements

Add focused tests that prove real behavior, not test-only production shape:

1. Hook triggers after the configured round cadence and runs brief -> companion
   -> apply exactly once for the tested turn.
2. Hook failures are fail-open: the driver turn completes and emits the normal
   assistant/tool outcome despite an observer command or companion failure.
3. Companion rounds do not recursively trigger the observer hook.

Use fixture/scripted providers and tiny test extensions where possible. Do not
assert on values that were copied directly from expected constants into the
implementation solely for the test.

## LOC / Budget

`scripts/loc_report.sh` before this slice shows `euler-core` at 12,304 / 12,500
LOC. Keep the core increase below the remaining 196 LOC if possible. If the
honest implementation exceeds it, stop and report with the smallest diff you
believe is correct rather than hiding budget pressure.

## Required Checks

Run at least:

- `cargo fmt --check`
- focused relevant tests for the changed crate(s)
- `scripts/loc_report.sh`

If full `cargo test --workspace` or clippy are too slow for the worker window,
state exactly what was and was not run. The TPM will rerun gates.

## Report Back

Final report must include:

- changed files
- implementation summary
- raw check commands and final output lines
- current `scripts/loc_report.sh` euler-core actual/budget line
- any remaining blockers, especially cancel-awareness or event-count trigger
  if not implemented
