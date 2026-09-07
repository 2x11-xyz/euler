# Evidence interpretation

Baseline: `9dfb881`. All fixtures are synthetic. The observations below must be distinguished from intentional behavior and platform limitations when turning them into production changes.

## Root probes

- `uniq input output` is statically approved under ShellExec=Ask, performs a write even with FsWrite=AlwaysDeny, and bypasses a denying decider. Static command safety needs command-specific argument semantics.
- Globs, changed cwd, and `rg --follow` pass the static path check yet read a synthetic outside file through symlinks. The direct literal symlink path correctly fails. Do not generalize the measured `rg` behavior to macOS `grep -R`, which did not reproduce it.
- Prepared creates and edits overwrite intervening user changes; apply-time preimage/no-clobber validation is missing.
- Reusing a provider tool call ID in a later round leaves both durable results but hides the second result from canvas. Pair by canonical call identity/model round.
- Session grant revocation is in-memory only and the historical allow revives on resume.
- An empty MaxTokens stop fails the turn and then causes DuplicateModelTerminal on resume; a partial MaxTokens stop returns normal completion. The latter is an outcome/API design gap unless a binding contract establishes stricter behavior.
- A torn UTF-8 tail blocks full replay/resume even though query pagination returns the valid complete prefix.
- Partial streamed text survives in memory but is absent from durable provenance after transport failure.

## Context and agents

- `companion.rs:814–825` gates all round history on parent-canvas inheritance, discarding the child's own prior tool results when inheritance is false. The probe observes two requests and zero tool outputs in request two.
- `companion.rs:887–960` lacks the token admission used by parallel reviewers. An 8 KiB explicit brief dispatches under a ten-token limit sequentially and rejects with zero calls in parallel.
- `session.rs:2714–2724,3912–3916` sends classified project guidance to the summarizer; `canvas.rs:399–405` emits the summary as an unclassified projection. A project_context:none child receives a unique project-only marker after compaction despite having zero typed project-context items.
- Static additional observation: `parallel_spawn.rs:307–322` uses the parent's context limit instead of the child target's model window. Centralize per-target request admission.
- Original nine project-context test failures were portability defects: eight noncanonical `/var` aliases, one APFS rejection of filename byte `0xff`. Production CLI canonicalizes EulerHome. The full workspace passed after canonical TMPDIR and an explicit skip of that incompatible fixture; these are not nine production regressions.

## Providers and secret lifecycle

- Short known secrets substituted before longer overlapping secrets expose the longer value's suffix.
- Checkpoint storage accepts an opaque synthetic value already known to the session redactor because its separate heuristic lacks that taint set.
- Scrubbing one closed session rewrites shared workspace checkpoint objects but leaves another session referencing the old hash.
- Legacy auth succeeds at the provider while startup AuthStorage rejects the legacy shape; validation does not feed the resolved-secret sink.
- SSE accepts malformed content or malformed tool JSON before reporting completion. Exact printed events describe the baseline behavior.
- Custom configuration has environment-expression and compatibility-flag inconsistencies; the loopback server prints the actual outbound request.
- A runtime catalog accepting a newly added reasoning effort can disagree with the adapter's embedded catalog check.

## Extension lifecycle

- A failed native extension registration leaves earlier commands executable, and descriptor panics escape the registration containment boundary. Register atomically after complete validation.
- TUI-style Add runs link then install against the same registry and deterministically produces ModeConflict, leaving the link materialization.
- A managed process exits nonzero during shutdown; its forked descendant still writes a marker after the command returns. Reaping the leader must not disable process-group cleanup.
- Managed-process synchronous host-call deadline overruns are documented architectural limitations, not newly proven timer-contract violations.

## Performance

Use `outputs/scaling-release-baseline.log` for release figures and the later harness output for a debug recheck. Pagination from an event-ID cursor repeatedly scans earlier pages; historical canvas swaps repeatedly rebuild indexes. Workspace snapshots and read_file acquire substantially more data than the returned bounded view. A 4,097-file workspace reports zero changes after a real edit because capture exceeded its bound; improve incomplete-capture observability.

No live-provider integration, Linux process behavior, adversarial filesystem race scheduling, or production concurrency load was verified by this bundle.
