# Swarm review triage: outstanding fleet-dogfood issues (2026-07-06)

Input: `/tmp/opencode/issues-swarm-brief.md` (full-context brief for third-party
review). Output: `/tmp/opencode/issues-swarm-report.md` (3/3 models:
fable-5, glm-5.2, gpt-5.4). This doc records accepted/deferred verdicts and
the resulting plan. TPM: Fable (OpenCode session).

## Convergent findings (all three reviewers)

1. **Do not ship window=64 without a token/size backstop.** 64 items ×
   unbounded results risks trading item-starvation for context overflow,
   provider 400s, cost spikes. GLM: token budget is a prerequisite, not a
   follow-up. ACCEPTED — with one correction: tool results are already
   bounded per-result (`DEFAULT_MAX_BYTES = 16KB`, tools.rs), so worst case
   at 64 is ~1MB text; pathological but bounded. Fix ships with a total
   canvas tool-result byte ceiling as backstop.
2. **A/B is n=1; replication needed before "proven."** ACCEPTED — replication
   on the footer task is in flight; issue language stays "strongly
   evidenced" until then. Curve characterization (16/32) deferred: the byte
   ceiling makes the item count a soft knob, and fleet time is better spent
   on replication.
3. **Observability required**: log effective canvas config + retained
   count/bytes/dropped per round. ACCEPTED — added to canvas.snapshot
   payload and session.start.
4. **#203 needs a deterministic fallback summary** assembled from system
   state (rounds, files modified, last action, session id), with the model
   summary as optional icing. ACCEPTED — deterministic floor ships first.
5. **#202 resume is the riskiest item**: replay source of truth, dangling
   event policy, lock semantics, config-override semantics all
   underspecified. ACCEPTED — resume gets a mini-ADR before implementation;
   #204 lock-flake diagnosis is promoted to a resume prerequisite.
6. **File the four unfiled observations now** (memory cap most dangerous;
   unexplained death matters for provenance trust; heartbeat cheap/valuable;
   refusal cheap to triage). ACCEPTED.

## Notable single-reviewer points

- gpt-5.4: artifacts-the-agent-WROTE deserve stronger retention than reads
  (the report-clobber case is qualitatively worse). ACCEPTED into the
  token-budget/index follow-up design issue, not the minimal fix.
- gpt-5.4: structured artifact index (path/read-or-written/hash) beats
  narrative summaries. ACCEPTED into follow-up design issue.
- gpt-5.4: define which SessionConfig fields are immutable on resume.
  ACCEPTED into resume mini-ADR.
- glm-5.2: #199 may be a canary for canvas-size fragility on the direct
  Anthropic path; add a regression check when raising the window. PARTIALLY
  ACCEPTED — direct-Anthropic regression check noted in #210; #199 diagnosis
  itself stays parked.
- fable-5: eviction-thrash signature should be demonstrated from logs
  (re-read distribution), not just outcomes. ACCEPTED — cheap provenance
  query, added to #210 evidence.
- fable-5: write-tool overwrite guard (cat >/write over non-empty file the
  agent hasn't read this session) as an independent mitigation. DEFERRED to
  the follow-up design issue; tool-side guards need care to avoid nanny
  behavior.

## Revised fix order

1. #210 minimal fix: default 64 + total byte ceiling backstop + config/
   telemetry in provenance + exec flag. (this PR)
2. #203 honest cap output: deterministic fallback always; optional bounded
   model summary; accurate wording per mode.
3. #204 diagnosis (promoted): flake in product lock code vs test harness —
   must be answered before resume ships.
4. #202 headless resume: mini-ADR first (replay source of truth, dangling
   events, locks, config immutability), then implementation.
5. #201 exec line-buffering (trivial, anytime).
6. New issues filed: run_shell rlimits; exec heartbeat; token-budget/
   artifact-index canvas design; unexplained process death; empty-content
   refusal.

## Evidence anchors

- A/B: banner-fable.jsonl (w8: 150 rounds/0 edits) vs banner-fable-w64.jsonl
  (w64: 74 rounds/9 edits/committed, gates green per run log).
- Starvation trio: banner-gpt55.jsonl, banner-fable.jsonl,
  tui-footer-fable2.jsonl. Clobber: RETRO-session.jsonl.
- Replication in flight: footer-fable-w64.jsonl.
