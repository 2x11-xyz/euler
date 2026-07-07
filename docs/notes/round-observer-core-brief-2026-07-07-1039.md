# Round Observer Core Primitive Brief (v2)

Date: 2026-07-07 10:39 UTC
Repo: /home/exedev/code/2x11-xyz/euler-public (public repo working clone, branch `feat/round-observer`)

## Role

You are an implementation worker. This is a core seam slice with tight LOC
pressure; smallest correct change wins. The seam map below is verified against
this working tree — trust it and start designing/coding early instead of
re-deriving it. Read only these two docs before coding:

- `AGENTS.md` (engineering rules; it is untracked here on purpose)
- `docs/adr/companion-roundloop-and-round-observer-2026-07-06.md`

Other contracts exist under `docs/contracts/` if you need to check a specific
rule, but do not do a general survey.

## Repo Rules (important, different from usual)

- This clone is of the PUBLIC repo. Engineering scaffolding (`AGENTS.md`,
  `docs/adr/`, `docs/notes/`, `docs/budget-*.md`, `scripts/`, some contracts)
  is deliberately untracked via `.git/info/exclude`. Do not `git add` any of
  it, do not edit `.git/info/exclude`, and do NOT run `git commit` at all.
  Leave all changes uncommitted in the working tree; the TPM reviews and
  commits.
- Only modify files under `crates/`.

## Goal

Implement the product-neutral core primitive for a live round-boundary
observer hook. At a configured cadence of driver rounds, mid-turn, the session
must run this generic composition:

1. Execute an enabled extension command that builds an observer/companion
   brief.
2. Run one companion task from that brief.
3. Execute an enabled extension apply command with the companion output folded
   into the brief-provided apply input.

The causal-DAG extension is the first consumer, but core must not learn any
causal-DAG vocabulary, event kinds, names, or schemas. Core defines only a
small neutral JSON envelope for the brief command's output (see Design
Sketch).

## Non-Goals

- No CLI flags in this slice.
- No changes to the `causal-dag` extension.
- Observer is opt-in: absent config means zero behavior change.
- No new `#[allow]` attributes; no test-only fields on production structs.
- No broad compatibility layers.

## Verified Seam Map

### Round loop hook point — `crates/euler-core/src/session/round_loop.rs`

- `trait RoundLoopIo` is at lines 88-124. It already has
  `fn round_completed(&mut self);` (line 122) and `fn flush_events(&mut self);`.
- `RoundLoop::run` (line ~139) calls the io per round outcome:

  ```rust
  match self.run_round(cancel_flag)? {
      RoundOutcome::Complete(done) => {
          self.io.round_completed();          // line 154
          return Ok(done);
      }
      RoundOutcome::Continue => self.io.round_completed(),   // line 157
  }
  completed_rounds += 1;
  ```

- Add a new trait method `fn round_boundary(&mut self)` (name yours to
  choose) with a DEFAULT NO-OP body, called from the `Continue` arm only
  (mid-turn boundaries; a turn's final round is not a mid-turn boundary).
  Because the default is no-op, `CompanionLoop` (below) inherits non-observing
  behavior — that is the recursion guard.

### Driver-side io — `crates/euler-core/src/session.rs`

- `struct Session<D>` fields at lines 385-398 (note
  `extension_emission_degraded`, `provenance`, `open_agent_spawns`).
- `struct SessionRoundIo<'a, 'sink, F, D>` at lines 403-411: bundles
  `session: &'a mut Session<D>`, `sink: &'a mut EventSink<'sink, F>`,
  `turn_state: &'a mut TurnState`, `rounds: &'a mut u64`.
- `impl RoundLoopIo for SessionRoundIo` at line 413;
  its `round_completed` (lines 541-543) increments `*self.rounds`.
- Implement the new hook here: check config + observer extension present,
  cadence divides current round count, then run the brief -> companion ->
  apply chain. Flush the sink after hook work so TUI/headless sinks observe
  emitted events in order (see how the existing `flush_events` impl does it).

### Session config — `crates/euler-core/src/session.rs:98-125`

- `#[derive(Clone, Debug)] pub struct SessionConfig { ... }` is plain data
  (String/PathBuf/Option/BTreeSet fields). Add a small
  `Option<RoundObserverConfig>` here. Keep it product-neutral: cadence in
  rounds (nonzero), brief command name, apply command name. Nothing
  causal-DAG-specific.
- An `Arc<dyn Extension>` cannot live in this Clone+Debug plain-data config.
  Give `Session` an `observer_extension: Option<Arc<dyn Extension>>` field
  with a setter (CLI will wire it in the next slice). Config present but
  extension absent => hook inert.

### Extension execution — `crates/euler-core/src/session/extension_bridge.rs:81-112`

```rust
pub fn execute_extension_command(
    &mut self,
    extension: &dyn Extension,
    command: &str,
    input: Value,
    granted: impl IntoIterator<Item = Capability>,
) -> Result<Value, ExtensionExecutionError>
```

- Already handles enablement check, capability grants, provenance
  persistence, queued-event publication (publication failure wins and
  degrades emission sticky). Use it as-is; do not bypass.
- Grant the observer extension its own manifest-declared capabilities
  (`extension.manifest()`), nothing broader.

### Companion execution — `crates/euler-core/src/session/companion.rs`

- `Session::spawn_companion(&mut self, task: AgentTask) -> Result<AgentResultSummary, SessionError>`
  at line 73. Validates the task's capability set is a subset of the parent's,
  records spawn/result provenance, runs `CompanionLoop`.
- Empty provider/model in `AgentTask` inherits the active session target
  (`resolve_companion_target`, line ~113).
- `AgentBudget.max_tokens` counts input + output; `max_turns` maps to
  `RoundLoopConfig.max_rounds`.
- Companion cancellation is weak: local never-tripping `AtomicBool` at line
  207. If making it cancel-aware is small and honest, do it; otherwise report
  it as a remaining blocker. Do not fake it.
- `impl RoundLoopIo for CompanionLoop` at line 551 — must NOT override the new
  hook method (keep default no-op).

## Design Sketch (deviate only with a stated reason)

- `RoundObserverConfig { cadence_rounds: NonZeroU64, brief_command: String, apply_command: String }`
  (+ optional companion budget knobs if small) in `SessionConfig`.
- `Session::set_observer_extension(Arc<dyn Extension>)` (or equivalent).
- Neutral envelope, defined by core, returned by the brief command:
  a JSON object containing the companion task fields (task text, optional
  provider/model/budget) and an opaque `apply` value. Core builds the
  `AgentTask`, runs the companion, then calls the apply command with input
  `{ "apply": <opaque value>, "companion": { ...result summary... } }` (exact
  field names yours; keep them product-neutral and tested).
- Fail-open: any error in brief/companion/apply is recorded via the existing
  diagnostics pattern (see `crate::diagnostics::extension_command_end` usage)
  and MUST NOT fail the driver turn. No panics, no `?` escaping into the
  round loop from hook work.
- Event-count force-trigger (bounded-page pressure) is out of scope; leave the
  shape open for it (e.g. cadence check isolated in one place), note it in
  your report.

## Test Requirements

Focused tests proving real behavior (fixture/scripted providers and a tiny
test extension; existing companion/session tests show the pattern —
`crates/euler-core/src/session/companion_test.rs` and inline session tests):

1. With cadence N configured, a turn running >= N rounds triggers brief ->
   companion -> apply exactly once at the Nth boundary (and again at 2N if
   cheap to show).
2. Fail-open: observer brief command failure (and/or companion failure) —
   driver turn still completes with its normal assistant/tool outcome, and the
   sticky `extension_emission_degraded` is not tripped by a mere command
   error.
3. No recursion: companion rounds never trigger the observer hook.
4. No config => no observer events, byte-identical driver behavior.

Do not assert on constants copied from the implementation solely to pass.

## LOC / Budget

`euler-core` is at 12,304 / 12,500 LOC. Keep the core increase under the
remaining ~196 LOC if possible (`scripts/loc_report.sh` reports actuals; test
code in `#[cfg(test)]`/`*_test.rs` does not count against production lints but
does count in LOC — check the script's output either way). If the honest
implementation exceeds budget, stop and report with the smallest diff you
believe correct rather than hiding the pressure.

## Required Checks

A prewarm `cargo test --workspace --no-run` may still be running when you
start; if `cargo` blocks on the lock, wait it out — do not delete locks.

Run at least, and paste raw final output lines in your report:

- `cargo fmt --check`
- `cargo test -p euler-core` (plus any other crate you touched)
- `scripts/loc_report.sh`

If full `cargo test --workspace` or clippy are too slow for your window, state
exactly what was and was not run. The TPM reruns all gates.

## Report Back

- changed files
- implementation summary incl. any deviation from the design sketch and why
- raw check commands + final output lines
- `scripts/loc_report.sh` euler-core actual/budget line
- remaining blockers: cancel-awareness, event-count trigger, anything else
