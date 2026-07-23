//! Shared `#[cfg(test)]` fixtures for the UI test suites.
//!
//! One canonical event constructor, hoisted out of the copies that had
//! accreted in transcript_tests.rs, transcript_patch_tests.rs, and
//! app/turn_recap.rs (the "`fn event()` copy-pasted in N files" form debt).
//! Import with `use crate::ui::test_support::{event, event_at};`.
//!
//! # Snapshots vs behavioral asserts (`insta`)
//!
//! Reach for `insta::assert_snapshot!` when a test pins the *shape of a
//! rendered surface* — a picker frame, an artifact cell, a transcript block —
//! and today does it with a cluster of positive `.contains(..)`/line-vector
//! equalities that must be hand-edited whenever the layout shifts. One
//! snapshot absorbs the whole cluster and re-baselines with `cargo insta
//! review`. Use inline `@".."` for short surfaces, file snapshots (the
//! `snapshots/` dirs) for multi-line frames.
//!
//! Do NOT snapshot:
//! - behavioral / invariant assertions — ordering (`a_index < b_index`),
//!   counts (`matches(..).count() == 2`), state enums (`SurfaceEvent::..`,
//!   `TranscriptItem::..`), style modifiers. These encode intent a snapshot
//!   cannot, and stay as explicit asserts.
//! - absence guards (`!rendered.contains("Session:")`). A snapshot only
//!   implicitly shows a string is gone; keep the explicit `!contains` so the
//!   regression it guards is self-documenting.
//! - surfaces carrying machine-local output (wall-clock stamps via `Local`,
//!   unbounded cwd/host paths) — a snapshot would bake in a non-hermetic value.
//! - a surface whose *trailing* blank rows / padding are load-bearing: insta
//!   trims trailing whitespace, so an exact line-vector `assert_eq!` is the
//!   honest tool there.
//!
//! The migration rule: replace the *positive full-surface* cluster with one
//! snapshot; keep the behavioral asserts and absence guards beside it. After
//! generating, diff each new snapshot against the substrings it replaced —
//! every one must still appear.

use euler_event::{EventEnvelope, JsonObject};

/// A session-scoped agent event with the default id/parent — the baseline
/// most transcript-projection tests want. Callers that need explicit
/// ids/parents (companion joins, child suppression) build `EventEnvelope`
/// directly.
pub(crate) fn event(kind: &'static str, payload: JsonObject) -> EventEnvelope {
    EventEnvelope::new("session", "agent", None, kind, payload)
}

/// `event` with an explicit provenance timestamp (RFC3339), for sequences
/// whose projection depends on inter-event timing.
pub(crate) fn event_at(kind: &'static str, payload: JsonObject, ts: &'static str) -> EventEnvelope {
    let mut event = event(kind, payload);
    event.ts = ts.to_owned();
    event
}
