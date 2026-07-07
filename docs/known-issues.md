# Known issues — v0.1.0

Honest list of defects and rough edges we know about at release. Items here
are queued, not forgotten.

## TUI

- **Growing the terminal mid-session leaves a blank gap.** After enlarging
  the window, the active area can stay pinned low with a blank region above
  it instead of re-anchoring to the transcript. Shrinking is safe (a
  destructive variant of this was fixed just before release); no content is
  lost. A layout rework (top-anchored active surface, full repaint on
  resize) is queued next.

## Headless

- **`exec` output is block-buffered when piped.** Progress is not visible on
  stdout until a turn completes. Monitor the provenance JSONL instead; it is
  written continuously.
- **No session resume for `exec`.** If a headless run dies mid-task
  (provider outage, kill), it cannot be resumed; the workaround is a fresh
  run whose prompt points at the on-disk state. Resume is a v0.2 priority.
- **Hitting `--max-tool-rounds` produces a generic message**, not a summary
  of what was accomplished before the cap.
- **`run_shell` has no memory limit.** A runaway subprocess can exhaust
  system memory before the OS intervenes. Use OS-level limits for untrusted
  workloads.

## Extensions

- **`causal-dag observe`/`export` page bounds are strict.** Logs over 256
  events require explicit `--after-event-id` cursors, and `observe` refuses
  truncated pages outright.
- **`extension run` appends lifecycle events to the target log it reads.**
  Read-shaped commands mutate their input file; work on a copy when the log
  must stay pristine.
- **Hint `source_refs` with stale `payload_pointer`s fail hard** in
  `observe` instead of degrading.

## Platform

- Developed and exercised on Linux and macOS. Windows is untested.
