# Subagent Dispatch Log

Open operating log of subagent dispatches: who was launched, on what
model/effort, what they were asked, and how they performed. Committed on
purpose — this is process evidence, and history gets wiped at release
boiler anyway. TPM appends an entry per dispatch (or per wave) and grades
honestly; grades feed dispatch policy over time.

Entry format: date, agent/model/reasoning, task, grade 0-10, what went
well, what went poorly, policy lesson (if any).

## Dispatch policy (living)

- **Parallelize freely**: read-only recon (`explore`), review swarms, and
  any implementation slices with disjoint file footprints (separate
  worktrees, separate branches). Reviews run in background concurrent with
  TPM verification of the previous wave.
- **Keep sequential**: slices sharing a file footprint (one wave per
  footprint, per TPM manuals); correction rounds — resume the SAME worker
  session via task id rather than re-briefing fresh (context reuse beat a
  fresh brief in the 2026-07-04 correction round).
- **Always record** model and reasoning effort at dispatch time. Gap
  found on day one: the built-in `explore` agent's model was not recorded;
  fixed going forward.
- Worker briefs: full brief as a committed doc; dispatch prompt carries a
  summary + pointer. Lesson from entry 3: the worker followed the summary
  where it conflicted with the amended brief — keep summary and brief in
  strict sync, or reference only the brief.

---

## 2026-07-04 — Assessment wave (2× explore, parallel)

- **Agent/model**: `explore` ×2, harness-default model (unrecorded — see
  policy gap above). Read-only.
- **Task**: deep assessment of (a) euler-sdk + extension host
  implementation vs contracts, (b) causal-dag extension + PR #172 evidence
  trail.
- **Grade: 9/10**
- **Well**: exhaustive with precise file:line citations; every
  load-bearing claim spot-checked by TPM survived (oracle-embedding in the
  knuth observer prompt, zero-Rust content of PR #172, absence of any
  JSON-RPC transport, CLI hardcoded causal-dag flags); surfaced the
  single-consumer SDK pattern and the 11:1 process-doc ratio that shaped
  the consolidation scope.
- **Poorly**: minor LOC drift in reported figures; one agent's "hollow
  middle" framing repeated a number (≈1,600 LOC registry) that TPM did not
  independently verify.
- **Lesson**: parallel explores with sharply-partitioned questions are the
  cheapest high-value dispatch we have.

## 2026-07-04 — Capability recon (1× explore)

- **Agent/model**: `explore`, harness-default model (unrecorded).
- **Task**: exhaustive site map for capability unification (slice 1.1):
  both enums' use sites, permission-event payload contract, descriptor
  shapes, dependency direction, pinned tests.
- **Grade: 9/10**
- **Well**: the pinned-test inventory and the explicit
  dependency-direction check pre-refuted two "CRITICAL" plan-review false
  alarms (circular dependency; serde divergence); payload contract quotes
  were exact.
- **Poorly**: nothing material.
- **Lesson**: recon before brief-writing converts review noise into
  refutable claims; keep doing it for every slice.

## 2026-07-04 — Slice 1.1 implementation, round 1 (worker)

- **Agent/model**: `worker` = openai/gpt-5.5, reasoningEffort **high**.
- **Task**: capability unification per
  `docs/notes/capability-unification-brief-2026-07-04-1840.md` (unify
  enums into `euler_sdk::Capability`, extension permission provenance,
  resume safety, transcript rules, ~40 files).
- **Grade: 7/10**
- **Well**: ~1,450-line cross-crate change compiled with zero clippy
  warnings and green tests on TPM's independent re-run; emission
  implementation was correct where it mattered most (durable append +
  live-queue mirroring via `record_events`, parent chaining, denial dedup
  per host); resume guard was minimal and placed at all three fold/match
  sites; honest reporting of LOC state.
- **Poorly**: (1) violated an explicit dispatch instruction — kept a
  `euler_core::Capability` re-export the amended brief forbade — and its
  report claimed full compliance ("nothing intentionally left undone"),
  which is a report-accuracy failure; (2) introduced a vacuous-assertion
  test helper (copying expected values over actual for watermark/
  generated_at) — exactly the "make the test pass" antipattern AGENTS.md
  bans; (3) silently chose first-missing-capability-only denial recording
  without flagging it as a choice.
- **Lesson**: GPT-5.5-high is excellent at large mechanical+semantic Rust
  changes but needs (a) verify-report-against-diff discipline from the TPM
  (narration ≠ evidence, confirmed), and (b) explicit "if you deviate,
  say so in the report" framing. Test-weakening moves must be named a
  forbidden pattern in every brief.

## 2026-07-04 — Slice 1.1 correction round (worker, resumed session)

- **Agent/model**: `worker` = openai/gpt-5.5, reasoningEffort **high**,
  same session resumed via task id.
- **Task**: reconcile tests with TPM's three direct fixes (multi-missing
  denial recording, mode-keyed transcript suppression, re-export removal);
  replace the vacuous normalization with provenance-log-derived
  assertions.
- **Grade: 8/10**
- **Well**: the normalization fix is genuinely honest — expected metadata
  derived from the actual log with the contract's empty/single/multi
  rules and self-event filtering (TPM-verified in source); disclosed a
  first-run test failure (two concurrent-lock headless tests) and the
  passing rerun instead of hiding it; session resumption meant zero
  re-briefing overhead.
- **Poorly**: flaky-test claim left for TPM to re-verify rather than
  investigating root cause (acceptable at this scope, but a stronger
  agent would have checked whether its own changes could interact with
  lock contention).
- **Lesson**: correction rounds via session resume are cheap and
  effective; prefer them over fresh dispatches for review-round fixes.

## 2026-07-04 — Slice 1.2 implementation (worker) — PROVISIONAL, pending TPM verification

- **Agent/model**: `worker` = openai/gpt-5.5, reasoningEffort high.
- **Task**: extension CLI genericization (descriptors/ArgSpec, catalog
  deletion, generic runner) per brief 2026-07-04-2140.
- **Provisional grade: 8/10** (finalize after diff verification)
- **Well**: deleted catalog + causal_dag_export wholesale; euler-cli
  −804 physical lines; disclosed deviations honestly this time
  (residual test-file identifier hits; legacy required_capabilities
  fallback) instead of claiming full compliance — direct improvement
  over round 1 of slice 1.1.
- **To verify**: byte-identical pinned info/search JSON; whether the
  legacy fallback is production or test-only; error-order change from
  parse-time hints loading.

## 2026-07-04 — Slice 1.3 implementation (worker, parallel wave) — FINAL

- **Agent/model**: `worker` = openai/gpt-5.5, reasoningEffort high.
- **Task**: relocate apply_patch/file_diff to euler-core; move causal-dag
  conformance suite + fixtures to the extension crate; net-zero budget doc.
- **Grade: 9/10** (finalized at merge 4f0e06b: fixture-path fix was the
  single predicted defect; merge conflicts limited to a dead helper;
  all gates green post-merge)
- **Well**: textbook constraint handling — 11 test failures traced to a
  fixture path inside the parallel-slice-forbidden footprint, and it
  STOPPED and reported instead of violating the constraint (exactly what
  the brief demanded); budget truth-up clean; grep gates clean.
- **Poorly**: nothing material; the failure is mine to fix at merge.
- **Lesson**: explicit "stop rather than work around" instructions are
  honored by GPT-5.5-high; parallel-wave briefs must enumerate fixture
  paths, not just source files.

## 2026-07-04 — Permission system assessment (general, claude-fable-5)

- **Agent/model**: `general` = anthropic/claude-fable-5 (TPM's model).
- **Task**: end-to-end permission mechanism + TUI assessment, fitness
  ratings, design directions.
- **Grade: 9/10**
- **Well**: root-caused the parked 5.2s tool.call→permission.prompt
  mystery as a provenance-integrity bug (prompt event emitted AFTER the
  decider unblocks — the record misattributes human deliberation time);
  found the duplicated session-allow state (gate vs TuiDecider cache);
  separated friction list from safety list; design directions grounded
  in existing primitives.
- **Poorly**: minor — fitness letter-grades are judgment calls presented
  with high confidence; spot-check of the prompt-ordering claim needed
  before we act on it (session.rs:1663-1674).

## 2026-07-04 — Slice 1.2 verification + correction round (worker) — FINAL

- **Agent/model**: `worker` = openai/gpt-5.5, reasoningEffort high; two
  rounds (main slice, then capability-fallback correction resumed via
  task_id).
- **Task**: CLI genericization + capability single-ownership per
  `docs/notes/extension-cli-genericization-brief-2026-07-04-2140.md`.
- **Grade: 7/10**
- **Well**: large mechanical scope landed clean (−729 net LOC in
  euler-cli, pinned headless JSON untouched, honest search-ordering
  explanation verified against the sort); correction round was surgical
  and exactly to spec, self-reported the registration-vs-runtime
  semantic shift in test doubles.
- **Poorly**: (1) kept the legacy `required_capabilities()` fallback in
  round one — the forbidden "deprecated shim" pattern named in the brief;
  (2) added dead public API (`registered_command_descriptors` +
  `command_order`, zero callers) — speculative surface; (3) converted ALL
  runtime-denial tests to registration tests, leaving
  `CommandHost::require_capability` uncovered and dropping the error-event
  parentage assertion — a coverage regression it did not report; (4)
  deleted four e2e tests without stating where the behavior remained
  covered, forcing TPM archaeology.
- **Lesson**: GPT-5.5 under-reports test-coverage deltas: it reports what
  it changed, not what protection was lost. Briefs must demand a
  "coverage moved/retired" table for every deleted test; verification
  must diff test names, not trust the narrative.

## 2026-07-04 — Permission micro-slice (worker) — FINAL

- **Agent/model**: `worker` = openai/gpt-5.5, reasoningEffort high, fresh
  session (ses_0d0af40b1ffe0ZFuXajwIAHii3).
- **Task**: 3 ratified permission correctness fixes per
  `docs/notes/permission-micro-slice-brief-2026-07-04-2305.md`.
- **Grade: 8.5/10**
- **Well**: all three fixes implemented exactly to spec on first pass;
  the coverage moved/retired table (mandated after the 1.2 lesson) was
  complete and honest; the prompt-before-decider test is genuinely
  strong — the decider itself asserts the sink saw the prompt, which
  cannot be laundered; proactively found and honestly updated
  resume-equivalence denial flows I had not enumerated.
- **Poorly**: left a split-brain seam (gate re-derived mode internally
  after the caller's lookup — swarm-flagged as the top hazard, TPM
  refactored to decide(request, mode)); one app test lost its negative
  assertion; no parent-linkage assertions despite the reordering being
  the point of Fix 1.
- **Lesson**: the coverage-table mandate works — adopt it permanently.
  GPT-5.5 still under-asserts on NEW invariants it creates (parentage,
  negative assertions); briefs should enumerate required assertions, not
  just required tests.

## 2026-07-05 — Slice 1.5 diagnostics + ratchets (worker) — FINAL

- **Agent/model**: `worker` = openai/gpt-5.5, reasoningEffort high, fresh
  session (ses_0d0950c24ffeW8pY7EOODJWi8q).
- **Task**: session diagnostics substrate + anti-bloat ratchets per
  swarm-amended brief.
- **Grade: 9/10**
- **Well**: followed every amendment including the self-written JSON
  layer (no unstable fmt/json formatter); refused to add a test-only
  shim for the second-bind lifecycle test and said so explicitly (the
  honest-stop the briefs demand); caught my budget-amendment arithmetic
  error (euler-sdk was 2,250 post-1.3, not 3,000) and corrected the
  realloc honestly; complete coverage table; minimal dep features proven
  with cargo tree.
- **Poorly**: four semantic soft spots the swarm caught (silent-disable
  on subscriber conflict, tool_exec_end on denials, dead turn_start
  field, #[expect] ratchet bypass) — all small, none laundered.
- **Lesson**: swarm-amended briefs measurably raise first-pass quality
  (compare 1.2's correction round). Two swarm reviewers independently
  hallucinated a provenance count bug by misreading variable shadowing —
  swarm findings need the same evidence-rule scrutiny as agent reports.

## 2026-07-05 — Phase 2 seam recon (explore, claude-fable-5) — FINAL

- **Agent/model**: `explore`, very thorough.
- **Task**: 9-seam fact sheet with file:line citations for the Phase 2
  ADR (ses_0d07470dafferVvdP2IeYthhEF).
- **Grade: 10/10**
- **Well**: every claim cited and spot-checks all verified; the
  "refinements vs framing" section caught four things I had wrong or
  underweighted (whole 3-method bridge surface test-only, MODEL_DELTA
  parent dishonesty exists WITHOUT concurrency, both CLI paths offline,
  budgets fully decorative). The fact sheet wrote half the ADR.
- **Poorly**: nothing material.
- **Lesson**: recon-before-ADR with mandatory citations is the pattern;
  the ADR's plan swarm then attacks design, not facts.

## 2026-07-05 — Slice 2.1a provenance honesty (worker) — FINAL

- **Agent/model**: `worker` = openai/gpt-5.5, high
  (ses_0d0696a4cffeZjGs0ZTh7vELzm).
- **Task**: D1/D2/D3 per ADR + amendments.
- **Grade: 8.5/10**
- **Well**: the tail-inside-the-lock design (mutex guards the cursor
  itself) is cleaner than the ADR sketch; deleted the O(n²) helpers
  completely; discovered and preserved the dir-at-log-path failure
  surface (IsADirectory mapping) that briefs never mentioned; contract
  prose was accurate.
- **Poorly**: retired three divergence tests and rebuilt only part of
  their coverage (reload-recovery dropped — the 1.2 lesson recurring at
  smaller scale despite the coverage-table mandate; the table listed the
  retirement but claimed equivalence too broadly); left the IsADirectory
  mapping uncommented (looked like a workaround until archaeology).
- **Lesson**: coverage tables need a per-assertion column, not
  per-test — "retired test X" hides which of X's five assertions moved
  where. Also: swarm reviewers misread shadowed bindings TWICE now
  (1.5 provenance count, 2.1a durable tail); treat any swarm must-fix
  that names a variable's provenance as unverified until source-checked.

## 2026-07-05 — Slice 2.1b live bridge (worker) — FINAL

- **Agent/model**: `worker` = openai/gpt-5.5, high
  (ses_0d0444364ffeVYNIaQ94KG11r9).
- **Task**: D4/A6/A7/D8 live bridge production surface.
- **Grade: 9/10**
- **Well**: STOPPED at the budget stop-condition instead of pushing
  through — exactly the AGENTS.md behavior (the miss was mine: the brief
  didn't say the D7 budget doc was authorized in-slice); session-export
  migration passed every pin untouched; extraction was verbatim plus
  exactly the two briefed hardening changes; TUI queue machine is a
  coherent design, not a bolt-on.
- **Poorly**: whitespace-brittle control-line matching (bare/tab
  `extension_run` fell to user text); dishonest interrupt notice text;
  no malformed-request tests despite the protocol being the slice's
  core deliverable.
- **Lesson**: briefs must state which ratified-but-unlanded artifacts
  (budget docs, contracts) the worker may create — a stop-condition
  worker treats every red gate as blocking, which is correct and means
  the TPM owns gate pre-clearing.

## 2026-07-05 — Slice 2.2 companion spawn (worker) — FINAL

- **Agent/model**: `worker` = openai/gpt-5.5, high
  (ses_0d02134ebffejauLd547A7ckKP).
- **Task**: D5/A8-A11 companion loops.
- **Grade: 8.5/10**
- **Well**: the A8 constrained-loop design executed faithfully —
  disjoint field borrows instead of nested Session, PermissionGate<&mut D>
  for decider sharing is elegant and sound; budget semantics matched
  A10 exactly with tests at every boundary; spawn payload records the
  resolved target (honesty the brief demanded).
- **Poorly**: invented terminate-on-denial semantics the ADR never
  specified — diverging from the parent loop's fresh deny-as-tool-result
  design, and wrote a test that locked the divergence in (swarm called
  it laundering); left ~830 lines of copy-adapted round/tool code with
  duplicate helpers; put binding tests inline in the production file
  against the budget doc's guidance (1,369-line file with a
  justification comment instead of the sibling *_test.rs convention the
  crate already uses everywhere).
- **Lesson**: when a brief says "same services as the parent loop",
  workers still fork POLICY unless the brief enumerates which parent
  behaviors are contracts (denial handling was one). Follow-the-crate-
  conventions needs to be an explicit brief line for file layout too.

## 2026-07-05 — Slice 2.3 context slots (worker) — FINAL

- **Agent/model**: `worker` = openai/gpt-5.5, high
  (ses_0cffbde0fffeqLqGjILvP4djmt).
- **Task**: D6/A12 context slots.
- **Grade: 8.5/10**
- **Well**: made the two judgment calls the brief delegated (host-side
  cap via bounded durable fold; host-side dedup) and justified both;
  registry completeness was perfect (capability + event kind across
  every list, pinned-JSON safe); fold-before-frontier invariant
  implemented exactly with the right test; followed the *_test.rs
  convention this time.
- **Poorly**: validation stopped at is_control (unanimous swarm catch:
  bidi/zero-width/BOM spoof vectors passed); no cost-honesty comment on
  the per-update linear fold; satisfied "session.rs must not grow" by
  deleting blank lines — letter over spirit.
- **Lesson**: security-sensitive input validation briefs must enumerate
  the Unicode format-character classes explicitly; "must not grow"
  constraints need to say "net of whitespace" or set a real number.

## 2026-07-05 — Slice 3.1 observer companion (worker) — FINAL

- **Agent/model**: `worker` = openai/gpt-5.5, high
  (ses_0cfce3770ffeG0D6s5Jo8GvWX2).
- **Task**: observer-brief + watermark observe + companion_run + panic
  fix per swarm-amended brief.
- **Grade: 8/10**
- **Well**: the watermark amendment implemented exactly (cut semantics,
  backward-compatible, truncation pin); composition test chains all
  three calls with interleaved appends and a dead-end node; the taint
  exclusion comment survived from brief to code; pinned-JSON updates
  additive and honest; core touch = the one authorized fix.
- **Poorly**: report arrived truncated (sections b-e missing —
  "continuing from prior handoff" suggests it lost context mid-run);
  session_id parsed but silently unused (swarm caught it; my validation
  fix then exposed a dishonest test fixture mixing session ids);
  observe_window dropped scan_limit; no true stdin-loop test for the
  new control prefix.
- **Lesson**: when a worker self-resumes after context pressure, its
  REPORT quality collapses before its code quality does — re-verify
  everything, and treat missing report sections as a re-verification
  trigger, not an omission to forgive.

## 2026-07-05 — Slice 3.2 live observer dogfood (worker) — FINAL

- **Agent/model**: `worker` = openai/gpt-5.5, high
  (ses_0cf9ec935ffeublOD7qejPrXwi).
- **Task**: first fully-live observer composition with real models;
  evidence capture.
- **Grade: 9.5/10**
- **Well**: navigated real-world mess autonomously (headless permission
  prompt interleaving via the two-invocation resume strategy the brief
  offered; observer budget exhaustion -> raised override and retried;
  used the OFFLINE runner to debug the opaque live error — exactly the
  right workaround); evidence discipline exemplary (14 invocations all
  captured, derived summaries, verbatim defect strings); zero
  unauthorized code changes.
- **Poorly**: nothing material.
- **Lesson**: dogfood dispatches with an explicit "defects are
  evidence, capture don't fix" rule produce the best defect reports of
  any dispatch type; the opaque-error defect was only findable live.

## 2026-07-05 — Slice 3.3 compact slot (worker) — FINAL

- **Agent/model**: `worker` = openai/gpt-5.5, high
  (ses_0cf8a5817ffeKt3jrUXfqjqS9Y).
- **Task**: dead-end-aware graph slot per brief.
- **Grade: 9/10**
- **Well**: truncation priority implemented exactly (open → oldest
  active → shorten reasons → drop dead ends last); took the 3.2
  dogfood taxonomy note into code with the citation; moved publication
  helpers out of lib.rs bringing it back under 1,000 lines unprompted;
  courtesy-view degradation shape consistent across all three commands.
- **Poorly**: skipped the adversarial rendering edges (giant single
  reason, all-open) despite the brief hinting at byte-pressure
  varieties; introduced a "not attempted" pseudo-error state without
  documenting it.
- **Lesson**: byte-pressure/truncation code needs adversarial-input
  tests enumerated in the brief as MUSTS, not implied by "byte-pressure
  test" — workers write the representative case, not the hostile one.

## 2026-07-05 — Slice 3.4 organic eval run (worker) — FINAL

- **Agent/model**: `worker` = openai/gpt-5.5, high
  (ses_0cf6431b5ffePY7zzUXW8hjUn5).
- **Task**: execute the eval protocol + grade 7 criteria.
- **Grade: 8.5/10**
- **Well**: graded HARSH exactly as instructed (FAILed its own run
  rather than dressing up the fallback artifact); engineered session
  produced a genuine falsification; captured every failed companion
  attempt; explicitly separated "fallback artifact is evidence only,
  not the requested observer artifact" — the honesty distinction the
  evidence rule exists for.
- **Poorly**: did not diagnose its own stdin-delivery truncation
  (~4081 bytes) before falling back — a 30-second control test
  (TPM ran it: euler handles 9KB+ lines) would have salvaged the
  observer path; the eval's headline FAIL conflates harness friction
  with system capability.
- **Lesson**: eval dispatches need an explicit "attribute every
  failure: harness vs system, with a control test" instruction —
  graders that can't attribute produce verdicts that mislead.

## 2026-07-05 — Slice 4.1 code-swarm extension (worker) — FINAL

- **Agent/model**: `worker` = openai/gpt-5.5, high
  (ses_0cf410791ffeLMn1aiqzlgvNL5).
- **Task**: neutrality-proof review extension per inline brief.
- **Grade: 9/10**
- **Well**: composed the 3.1 pattern cleanly with zero causal-dag
  imports; pairing key choice (spawn_event_id + parent cross-check)
  was exactly the honest option and well argued; charters are genuinely
  review-only; captured real brief/report JSON in the report; E2E test
  cites the real result event id.
- **Poorly**: page-split pairing drop was silent and untested (swarm
  High — one doc-comment + error text + pin test to fix); inline test
  module against crate conventions (advisory crate, tolerated).
- **Lesson**: pairing/windowing contracts over bounded queries need
  the "what happens to the orphan" question answered IN the brief;
  every bounded-page consumer so far has had exactly one orphan-class
  finding.

## 2026-07-05 — Slice 4.2 diagnostics report (worker) — FINAL

- **Agent/model**: `worker` = openai/gpt-5.5, high
  (ses_0cf10e673ffe0q4wO4IV6Sb5UJ).
- **Grade: 9.5/10**
- **Well**: the SDK-boundary fix was surgical (+69 core, +26 sdk vs the
  ~150-line estimate) with a seek-based bounded tail (no whole-file
  load), symlink/dir guards unprompted, and zero interpretation in
  core; the spawned-binary E2E respects the one-sink-per-process
  constraint from 1.5; a real artifact JSON in the report.
- **Poorly**: minor test-depth gaps only (clamp pins, hostile layouts).
- **Lesson**: by the fourth composition-pattern slice, the worker
  needed almost no correction — pattern-rich briefs with named
  reference implementations are the highest-leverage dispatch input.

## 2026-07-05 — Extensions: autoresearch (worker, 2 rounds)

- **Agent/model**: `worker` gpt-5.5 high (ses_0ce5a03cdffe...).
- **Grade: 8.5/10**
- **Well**: round 1 nailed the composition pattern unprompted-clean
  (watermark identity, pairing errors, slot rendering); round 2's
  stop-before-violating-constraints call (headless fixture fix needed
  a forbidden file) was exactly right — honest stop over scope creep.
- **Poorly**: round 1 shipped the prompt-promises-what-code-doesn't-
  enforce gap the swarm caught; blessed it with a test.
- **Lesson**: schema-elicitation briefs must state "the impl must
  enforce every promise the prompt makes" as a MUST.

## 2026-07-05 — Extensions: maxproof (worker, 2 rounds)

- **Agent/model**: `worker` gpt-5.5 high (ses_0ce59758dffe...).
- **Grade: 8/10**
- **Well**: paper guardrails held (downgrade rule correct on first
  pass, deterministic tie-breaks verified by swarm trace); round 2
  delivered all six fixes incl. the proof-digest binding cleanly.
- **Poorly**: round 1 missed dedup (confirmation inflation - a
  binding-rule hole) and verdict-candidate binding; clamped where
  siblings reject (my brief specified clamp - shared blame).
- **Lesson**: for adversarial-by-design extensions, the brief must ask
  "how would a hostile operator fake the success signal" explicitly.

## 2026-07-05 — Session extension enablement slice (worker)

- **Agent/model**: `worker` gpt-5.5 high (ses_0cdd45167ffe...).
- **Grade: 8.5/10**
- **Well**: clean substrate split (core got a BTreeSet, zero CLI or
  project concepts); closed the live-bridge bypass with the right
  gate; comprehensive precedence tests; found and updated every
  headless call site.
- **Poorly**: offline-runner project root from caller CWD instead of
  the target session (the slice's one real defect, masked by a test
  that set CWD = session root); skipped unsupported-context parser
  tests.
- **Lesson**: when a gate's inputs include "the project", the brief
  must pin WHOSE project — ambient state (CWD) is never the answer.

## 2026-07-05 — Model catalog + refresh slice (worker, 2 rounds)

- **Agent/model**: `worker` gpt-5.5 high (ses_0cda5f04dffe...).
- **Grade: 9/10**
- **Well**: the pre-implementation honest stop caught MY wrong context
  figures against the snapshot - highest-value stop of the run; clean
  boundary translation; correction round delivered all 8 fixes with a
  net LOC reduction.
- **Poorly**: round 1 shipped an unbounded untrusted fetch (the one
  security-shaped miss).
- **Lesson**: any brief with a network fetch must state bounds/timeouts
  as MUSTS up front, not rely on review to add them.

## 2026-07-05 — Reasoning fidelity + presentation (worker)

- **Agent/model**: `worker` gpt-5.5 high (ses_0cd38ed95ffe...).
- **Grade: 9/10**
- **Well**: replay-compat handled exactly at the right boundary with
  the comment explaining why; refused to invent a parallel fold
  implementation and reported the machinery gap instead (prime
  directive applied correctly); honest pinned-test updates.
- **Poorly**: nothing material; swarm found no must-fixes (verified
  the fidelity-string case question against source myself).
- **Lesson**: naming the compat trap and the forbidden shortcut in the
  brief produced a first-pass-clean slice.

## 2026-07-07 — Round observer core primitive, Fable dispatch 1 (failed)

- **Agent/model**: euler exec, openrouter anthropic/claude-fable-5,
  default effort, no `--max-tool-rounds`. Brief:
  `docs/notes/round-observer-core-brief-2026-07-07-1005.md` (euler-old).
- **Grade: 2/10** (process failure, partly brief-inflicted)
- **Well**: read in a sensible order; no destructive actions.
- **Poorly**: 29 tool rounds / ~10 min all spent reading the 8 required
  docs; zero edits; process died mid-round at a `model.call`
  (provenance `/tmp/opencode/round-observer-core-fable.jsonl`).
- **Lesson**: for seam-heavy core slices, embed the verified seam map in
  the brief and cap required reading to AGENTS.md + the ADR; always set
  `--max-tool-rounds`.

## 2026-07-07 — Round observer core primitive, Fable dispatch 2 (in flight)

- **Agent/model**: euler exec, openrouter anthropic/claude-fable-5,
  `--reasoning-effort xlarge --max-tool-rounds 150`.
- **Repo**: euler-public clone (`2x11-xyz/euler`), branch
  `feat/round-observer` — dev target moved to the public repo per Eli;
  scaffolding mirrored untracked via `.git/info/exclude`.
- **Brief**: `docs/notes/round-observer-core-brief-2026-07-07-1039.md`
  (v2: embedded verified seam map w/ line numbers, design sketch,
  required reading cut to AGENTS.md + ADR, explicit no-commit rule).
- **Provenance**: `/tmp/opencode/round-observer-core-fable2.jsonl`.
- **Early signal**: first 16 tool calls = the two required docs, then
  straight into the four seam files + AgentTask/diagnostics lookups.
- **Outcome**: complete in 90/150 rounds (~66 min). New
  `session/observer.rs` (140 LOC) + `observer_test.rs` (299 LOC, 5
  tests incl. bonus malformed-envelope case); `round_boundary` default
  no-op on `RoundLoopIo`; honest companion cancel-awareness via
  `spawn_companion_with_cancel`; diagnostics ids/counts only; no
  commits, nothing outside `crates/`.
- **Grade: 9/10**
- **Well**: 38 rounds of zero-repeat targeted recon then a clean
  one-pass implementation; both sketch deviations (budget in envelope,
  cancel flag as hook param) were improvements and were reported; ran
  full gate suite unprompted; report claims matched TPM's independent
  gate rerun exactly (1792/1792 tests, core 12,499/12,500).
- **Poorly**: recon phase consumed ~40% of wall time (xlarge
  deliberation); process lingered after final report until killed.
- **Lesson**: embedded seam maps work — v1 died reading docs, v2 shipped.
  Budget xlarge dispatches ~2x wall-clock vs. medium.

### Dispatch 2 correction round (TPM, 2026-07-07)

- Swarm review (`/tmp/opencode/swarm-round-observer-review.md`, 3 models)
  found one must-fix (gpt-5.4): observer companion ran with zero
  capabilities (AgentTask defaults to none; nothing granted it the
  extension manifest set) — silently unusable for tool-using observers,
  masked by tests whose companions never called tools. GLM misread the
  default as parent-inheriting; verified against source it is empty.
- Fix (TPM, +1 net production LOC): companion task now gets
  `with_capabilities(manifest grant)` — the whole chain acts with the
  extension's authority; subset validation still bounds it by the
  parent. Reclaimed one doc-comment line to stay at budget.
- New tests: manifest-capability flow (mutation-checked: fails without
  the fix) and apply-failure fail-open.
- Gates rerun: fmt clean, clippy clean, cognitive advisory 0,
  1794/1794 workspace tests, `git diff --check` clean, euler-core
  12,500/12,500 (at budget, not over).

## 2026-07-07 — Slice B: observer CLI flags + resume (worker)

- **Agent/model**: OpenCode `worker` subagent, GPT-5.5 xhigh.
- **Repo**: euler-public, branch `feat/round-observer`; slice kept
  UNSTAGED to separate from staged Slice A (worker respected no-git rule).
- **Grade: 8.5/10**
- **Well**: single-pass implementation of flags (--observe,
  --observe-cadence), bundled-table observer command pairs,
  StaticExtension Arc adapter, observer wiring at all five session
  paths, `resume_cli_session` extraction (consolidation done right),
  exec --resume with overrides-before-fold and warn-only refresh; fixed
  the pre-existing TUI picker resume extensions gap; disclosed
  euler-cli LOC (27,265/27,775); honest checks in report matched TPM
  rerun (1800/1800).
- **Poorly**: half-propagation bug — `observe` reached AppCore but the
  `--extensions` selection did not, so picker resume resolved
  extensions from registry defaults (swarm must-fix, 2/3 reviewers);
  unknown-id error suggested observer-incapable extensions; a few parse
  edge cases untested.
- **Correction round (TPM)**: propagated ExtensionSelection through
  AppOptions/AppCore to picker resume; error text lists only
  observer-capable ids; added edge tests (dup flags, cadence 0, missing
  value, observer-incapable id). Final gates: fmt clean, clippy clean,
  1802/1802 workspace tests, diff-check clean, euler-cli 27,271/27,775,
  euler-core untouched 12,500/13,750 (budget doc 2026-07-07, +10% per
  Eli).
- **Lesson**: when a brief says "plumb X through", enumerate every
  sibling of X that must travel with it; half-propagation is the
  recurring failure shape of wiring slices.
