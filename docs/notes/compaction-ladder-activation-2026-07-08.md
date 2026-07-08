# Spec: Activate the compaction ladder (slice 2)

Date: 2026-07-08  
Status: implemented in tree (P0–P3; P4 hot-set deferred; long-run dogfood metrics pending)  
Owners: core (retention, policy, tools); CLI (status chrome)  
Supersedes / extends: `docs/adr/canvas-retention-and-auto-compaction-2026-07-06.md` (slice 1 shipped; this is slices 1.5 + 2 entry)  
Related: `docs/contracts/canvas.md`, `docs/contracts/tools.md`, `docs/contracts/provenance.md`

## 1. Problem

Slice 1 fixed silent N=8 round drops: every tool round stays on canvas; under
byte budget, tool-result *content* demotes to a stub with a provenance handle.

Dogfood (home sessions + controlled `exec` runs, 2026-07-08) shows:

| Regime | Behavior |
|---|---|
| Short / mid sessions (canvas ≪ 640KB) | Healthy. Zero demotions. |
| Marathon coding (e.g. 400+ model calls) | Canvas pins ~620–640KB; demotions climb into the hundreds; provider input tokens pin near full; hot files re-read 10–15×. Session can still finish, but working memory is hollow. |
| Layer-1 / full `canvas.swap` | **Never observed** in production dogfood. |

Root causes (code + logs, not speculation):

1. **Only stubs run.** Token-threshold auto-compact (`auto_compact_if_triggered`)
   requires `SessionConfig.context_limit`. Default is `None` → path always no-ops.
2. **Stubs are not rehydratable.** Handles are `blob:` / `event:` text in the
   stub; no core tool resolves them. Recovery is re-`read_file` / re-run shell —
   recreating the thrash the ADR aimed to stop.
3. **Single estimator.** Demotion is driven by *rendered canvas bytes* (default
   640_000). Provider *input tokens* can already be near the real window while
   we carefully stay under the byte budget. Dual scales drift.
4. **No user-visible pressure.** Demotion is only in `canvas.snapshot`
   provenance. Long sessions look “fine” until the model starts thrashing.

Principle from the ADR still holds: **degrade content, never facts.** This
spec does not reopen that decision. It activates the rest of the ladder and
closes the handle/recovery hole.

## 2. Goals

1. Make the **intelligent** compaction path real in default interactive and
   headless sessions (layer-1 before full swap; stubs remain the always-on
   byte backstop).
2. Make demotion **recoverable** without full re-execution of tools.
3. Align pressure signals with **provider token usage** where available, not
   only rendered-byte estimates.
4. Make pressure **interpretable** in the TUI (and exec diagnostics).
5. Stay inside core budget and contracts: no silent round removal, no canvas
   dump of raw provenance, no secrets in stubs/handles.

## 3. Non-goals

- Shipping full `structured` indexes or `assisted` extension compactors
  (roadmap; this spec only leaves seats for them).
- Changing provenance fidelity or blob layout.
- Provider-specific signed-thinking replay policy (still adapter-owned per ADR).
- Raising the default byte budget as the primary fix (may retune after
  ladder works; not the first lever).
- Multi-session / cross-session memory.

## 4. Design summary

```
                    every model request
                           │
                           ▼
              assemble_canvas (full facts)
                           │
          ┌────────────────┼────────────────┐
          │                │                │
   active canvas.swap   stubs tier      off tier
   (frontier / L1)      byte demote     hard stop
          │                │                │
          └────────────────┼────────────────┘
                           ▼
                    model prompt

After model.result (or turn start), if context_limit known:
  if used_tokens > window - reserve:
      prefer layer-1 (read_file previews)
      else full projection swap (explicit event)
```

**Ordering invariant:** layer-1 and full swap run on the *session event stream*
and affect later assemblies. Stub demotion remains an *assembly-time* pure
function of current events + policy. Never remove rounds.

## 5. Work items

### W1. Wire `context_limit` from model metadata

**Problem:** `compaction_context_window()` returns `None` unless
`SessionConfig.context_limit` is set → auto layer-1/swap never fires.

**Spec:**

- On session start and on model switch, set `context_limit` from the resolved
  model descriptor’s `context_window_tokens` when present (catalog / provider
  metadata already expose this for built-ins, e.g. gpt-5.5 ~1.05M,
  Claude windows, etc.).
- If the model has no known window: leave `context_limit` unset; stubs still
  apply; do **not** invent a fake window. Record in `session.start` payload:
  `context_limit: null` vs `{ "limit_tokens": N, "source": "catalog"|"config" }`.
- Allow explicit override later via config/CLI (out of scope for first PR;
  field already exists on `SessionConfig`).
- **Default reserve** remains `compaction_reserve_tokens = 16_384` unless
  overridden. Threshold: `used_tokens > limit_tokens - reserve`.

**Acceptance:**

- New interactive and `exec` sessions with catalogued models have non-`None`
  `context_limit` at start (assert in unit/integration test with fixture catalog).
- A synthetic session with high fake usage emits `canvas.swap` with
  `layer1_compacted_event_ids` or a full projection when over threshold.
- Models without window metadata still run; no panic; stubs-only.

**Files (expected):** `session.rs` construction paths, CLI session launch
(`main.rs` / `session_lifecycle.rs`), provider catalog types already carrying
`context_window_tokens`.

### W2. Prefer layer-1 before full swap; keep stubs as backstop

**Already sketched in code** (`compact_for_threshold`): select layer-1
candidates → if estimated tokens ≤ threshold after L1, emit layer-1 swap;
else `try_compact(heuristic_projection)`.

**Spec adjustments:**

1. **Eligibility (layer-1):** keep `read_file` only for v1. Do not add
   `run_shell` / git tools until outputs are known non-stale or versioned.
2. **`keep_recent`:** default 4 remains. Document as “full content for the
   last N tool results of any type; layer-1 only considers older `read_file`.”
3. **Estimator honesty:** replace whitespace word-count `estimated_tokens`
   with the same **bytes/4** proxy used for the canvas byte budget (or share
   one helper). Dual proxies for the *same* decision are a defect.
4. **Interaction with stubs:** after a layer-1 swap, assembly still may
   demote other tool outputs under byte budget. Layer-1-marked results should
   not be double-processed into worse stubs if already `⟨compacted⟩` (skip
   demotion when `compacted == true`).
5. **Full swap:** remains last resort when L1 cannot get under threshold.
   Projection stays heuristic until structured tier; must stay deterministic
   and model-free in this slice.

**Acceptance:**

- Integration test: many large `read_file` results + high usage → layer-1 swap
  first; latest `keep_recent` reads stay full; older show `⟨compacted⟩` preview.
- No test may assert silent absence of a tool round.

### W3. Rehydrate demoted / compacted tool results

**Problem:** stubs cite `handle blob:…` or `event:…` but nothing resolves them.

**Spec — core tool (preferred):**

Add a narrow retrieval tool to the default coding palette, e.g.
`tool_result_get` (name bikeshed OK; one owner):

| Field | Rules |
|---|---|
| Input | `event_id` (required) **or** `blob_hash` (optional alternative) |
| Scope | Current session provenance only; refuse foreign session ids |
| Output | Original tool result payload (or blob bytes as text with encoding note) |
| Bounds | Honor existing capture caps; if stored content was already truncated at
  capture, return what was stored + honest note. Support optional `max_bytes`
  / line range if payload is large (tools contract: truncation needs a
  narrower handle). |
| Permissions | Same class as `read_file` (read-only local session data). |
| Canvas effect | Full content may re-enter as a new tool result fact; do not
  silently mutate historical events. |

**Stub text contract (tighten, do not invent a parallel format):**

Keep one-line stubs. Ensure every demoted stub includes a resolvable handle:

```text
[tool {name} event {event_id}: {ok|failed} — content demoted, {N}B, handle {blob:hash|event:id}{, path PATH}]
```

Model-facing tool description for `tool_result_get` must say: *when you see
`handle event:…` or `blob:…`, call this instead of re-running the original tool
if the original inputs would be expensive or non-idempotent.*

**Layer-1 previews:** the existing `⟨compacted⟩` block already says
“re-read to recover”. Update copy to prefer `tool_result_get` with the event
id when available, else re-read path.

**Acceptance:**

- Unit: demote a result → stub contains handle → `tool_result_get` returns
  original content from the bus/blob store.
- Integration: after demotion, model-visible tool list includes the getter;
  permission path works under `read-only` auto-approve.
- Refuse cross-session and missing ids with honest errors (no path leaks).

### W4. Dual-threshold pressure (bytes + tokens)

**Spec:**

| Signal | Role |
|---|---|
| Rendered canvas **bytes** vs `budget_bytes` | Assembly-time stub demotion (unchanged role) |
| Provider **used_tokens** vs `limit - reserve` | Session-time layer-1 / full swap |

Additions:

1. When `latest_model_usage` exists and `context_limit` is set, **also**
   allow stub demotion to use a token-aware soft target: if
   `used_tokens > 0.8 * limit` (configurable later), treat assembly as under
   extra pressure by using `min(budget_bytes, token_proxy_budget)` where
   `token_proxy_budget = (limit - reserve) * 4` (bytes/4 proxy). Prevents
   “under 640KB but provider already full” on large-context models where
   640KB is far below the real window *and* the inverse case where structure
   fills tokens before bytes.
2. Do **not** demote more aggressively than today when usage is missing.
3. Telemetry on `canvas.snapshot`: add optional
   `used_tokens`, `limit_tokens`, `pressure: "none"|"byte"|"token"|"both"`.

**Acceptance:**

- Fixture with small limit + high usage triggers L1 even if rendered bytes
  &lt; default 640KB.
- Snapshot payload includes pressure fields when usage known.
- No demotion regression when usage absent.

### W5. TUI / exec observability

**TUI status (minimal):**

When any demotion has occurred in the latest assembly **or** a `canvas.swap`
exists in-session, show a compact status fragment, e.g.:

```text
Context ~62% · 48 demoted · stubs
```

or, if percent unknown:

```text
Canvas 612KB/640KB · 48 demoted · stubs
```

- Update on each turn completion (from last `canvas.snapshot` / session fold).
- No new sidebar; status line / existing activity chrome only (`docs/contracts/ui.md`).

**Slash command (thin):**

`/compaction` → read-only report: tier, budget_bytes, limit_tokens, reserve,
last retained_bytes, demoted_items, last swap id/time if any.  
Tier switches (`/compaction stubs`) can wait for a later PR; this slice is
**show**, not **mutate**.

**Exec:** no new required flags. Snapshots already land in provenance; ensure
headless guide mentions demotion fields for operators.

**Acceptance:**

- Unit/render test for status fragment with demoted &gt; 0.
- `/compaction` returns structured text without requiring network.

### W6. Hot-set retention (small, optional in same slice if budget allows)

If W1–W3 land under budget, add **hot-set** protection:

- Maintain the set of file paths from the last `K` successful `read_file` /
  write-shaped tools (K default 8).
- Under stub demotion, **prefer demoting non-hot paths first** (still
  oldest-first within class). Write-shaped remains last overall.
- Does not pin unbounded content; only delays demotion of currently relevant
  paths.

If LOC pressure is high, defer hot-set to a follow-up; W3 rehydrate is more
important.

## 6. Phasing

| Phase | Scope | Exit criteria |
|---|---|---|
| **P0** | W1 + W2 estimator fix + tests | Catalogued models get `context_limit`; synthetic high-usage emits layer-1 swap |
| **P1** | W3 rehydrate tool + stub copy | Demoted content recoverable by event/blob id |
| **P2** | W4 dual-threshold + snapshot fields | Token pressure visible and effective |
| **P3** | W5 status + `/compaction` | Operators can see pressure without grepping JSONL |
| **P4** | W6 hot-set (optional) | Fewer re-reads of active paths under demotion |

Do not start P2 before P0 (otherwise token pressure has no L1 to call).  
P1 can parallelize with P0 after tool palette design is agreed.

## 7. Contracts / docs to update when implementing

- `docs/contracts/canvas.md` — state that layer-1 is live when limit known;
  rehydrate tool is the preferred recovery path.
- `docs/contracts/tools.md` — document `tool_result_get` (or final name):
  bounds, session scope, handle formats.
- `docs/contracts/events.md` — any new snapshot fields; no change to
  `canvas.swap` authority.
- `docs/guides/headless.md` — short “reading demotion telemetry” note.
- ADR canvas-retention — add “Implementation status” footnote: slice 1.5/2
  activated per this note (do not rewrite ratified D1–D5).

## 8. Explicit non-regressions (tests that must keep passing)

- No tool round silently removed at any budget (including `budget_bytes = 0`).
- Write-shaped demotes last; path retained on stub when derivable.
- `off` tier still hard-fails at budget (honest stop).
- Provenance retains full tool results regardless of canvas demotion.
- Companion path uses the same assembly policy as the root session.

## 9. Risks

| Risk | Mitigation |
|---|---|
| Large-context models (1M) make token threshold fire very late; stubs dominate | Dual-threshold (W4); later `--compaction-at <pct>` from ADR |
| Rehydrate tool becomes a side-channel for huge blobs | `max_bytes` / range; same caps as capture |
| Layer-1 on stale files misleads | L1 only `read_file`; copy says may be stale; prefer rehydrate or re-read |
| LOC / euler-core budget | Prefer small direct wiring over new subsystems; stop if over allocation |
| Signed-thinking providers | Compaction only at assembly boundary; adapters own replay barriers |

## 10. Success metrics (dogfood)

After ship, run ≥3 long coding sessions (target ≥150 tool rounds or until
canvas &gt; 80% budget) on ChatGPT gpt-5.5:

| Metric | Slice-1 baseline (one marathon) | Target |
|---|---|---|
| `canvas.swap` count | 0 | ≥1 layer-1 before canvas pins at byte cap for long runs with known limits |
| Peak demoted_items | 172 | Lower for same task shape *or* equal demotions with fewer same-path re-reads |
| Same-path `read_file` ≥5 | 8 paths | Down meaningfully when rehydrate used |
| Task completion | Often still works | No regression |
| Status visible | No | Demotion/swap visible without reading JSONL |

## 11. Open questions (resolve in implementation PR, not by silent choice)

1. **Tool name:** `tool_result_get` vs `provenance_get` vs `canvas_rehydrate`?
   Prefer session-local, non-provenance-product naming (`tool_result_get`).
2. **Should `/compact` manual path also set layer-1 before full projection?**
   Recommend yes for consistency.
3. **Default `compaction_reserve_tokens` on 1M-context models** — 16k may be
   too small a headroom fraction; consider `max(16k, 2% of limit)` later.
4. **Whether stub demotion should ever run *before* first model result**
   (assembly-only bytes) — keep current behavior (yes); token path needs usage.

## 12. Implementation checklist (for the implementing agent)

- [ ] Read this note + canvas ADR + `canvas.md` / `tools.md` before coding
- [ ] Run `scripts/loc_report.sh`; disclose euler-core / euler-cli delta in PR
- [ ] P0: context_limit wiring + shared token estimate helper + L1 test
- [ ] P1: rehydrate tool + tests + tool palette registration
- [ ] P2: dual-threshold + snapshot fields
- [ ] P3: status fragment + `/compaction` read-only
- [ ] Update contracts/guides as listed
- [ ] Dogfood evidence: raw provenance snippets or session ids, not narration

## 13. One-line intent

**Turn on the ladder we already designed: know the window, shrink reads
intelligently, let stubs be undoes, and show the pressure — without ever
deleting the fact that work happened.**
