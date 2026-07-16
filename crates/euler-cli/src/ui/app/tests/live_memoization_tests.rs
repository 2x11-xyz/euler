//! Memoization of the committed prefix of a streaming answer (perf fix).
//!
//! While an assistant message streams, the TUI repaints the live transcript on
//! every frame — and the spinner forces a repaint ~11x/second for the whole
//! turn. Re-parsing the (append-only) committed prefix each time is quadratic
//! in the answer's length. These tests pin the cache down: a frame with no new
//! committed content re-renders nothing, new content or a width/gutter/theme
//! change re-renders, and the cached rows are byte-identical to the old
//! unmemoized full render.

use super::super::visual::{live_committed_render_probe as probe, ratatui_lines_to_canvas};
use super::*;
use crate::ui::text::with_timestamp_gutter;
use crate::ui::transcript::render_items_for_history;
use crate::ui::visual_canvas::CanvasLine;

fn push_text_delta(core: &mut AppCore, delta: &str) {
    core.transcript.push_event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", delta.into())]),
    ));
}

/// The exact expression the pre-memoization `visual_canvas_blocks` used to
/// compute the committed live block, so equivalence is a real golden.
fn unmemoized_committed_lines(core: &AppCore, width: u16) -> Vec<CanvasLine> {
    let show_ts = core.show_timestamp_gutter;
    let items = core.transcript.live_committed_items();
    ratatui_lines_to_canvas(with_timestamp_gutter(show_ts, || {
        render_items_for_history(&items, &core.theme, width)
    }))
}

#[test]
fn same_committed_revision_and_width_reuses_cached_lines_without_rerender() {
    let mut core = core();
    // A fenced code block exercises the syntax-highlight path that made the
    // per-frame cost quadratic; the closing fence commits the whole block.
    push_text_delta(
        &mut core,
        "# Heading\n\nHello **world** with `code`.\n\n```rust\nlet x = 1;\n```\n\n",
    );
    assert!(
        core.transcript.live_committed_revision().is_some(),
        "the fenced block should have committed"
    );

    probe::reset();
    let first = core.live_committed_history_lines(80);
    assert_eq!(probe::count(), 1, "first frame is a cache miss");

    let second = core.live_committed_history_lines(80);
    assert_eq!(
        probe::count(),
        1,
        "an identical frame must not re-render the committed block"
    );
    assert_eq!(first, second, "cached lines must match the rendered lines");
    assert!(!first.is_empty(), "committed content should render rows");
}

#[test]
fn cached_committed_lines_are_byte_identical_to_the_unmemoized_render() {
    let mut core = core();
    push_text_delta(
        &mut core,
        "## Section\n\n- item one\n- item two\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\nTrailing prose.\n\n",
    );

    for width in [80_u16, 48, 120] {
        let expected = unmemoized_committed_lines(&core, width);
        let memoized = core.live_committed_history_lines(width);
        assert_eq!(
            memoized, expected,
            "memoized committed render must be byte-identical at width {width}"
        );
        // And the second call (a cache hit) is still identical.
        let hit = core.live_committed_history_lines(width);
        assert_eq!(hit, expected, "cache hit must stay identical at width {width}");
    }
}

#[test]
fn new_committed_content_and_width_changes_re_render() {
    let mut core = core();
    push_text_delta(&mut core, "First paragraph.\n\n");

    probe::reset();
    let first = core.live_committed_history_lines(80);
    assert_eq!(probe::count(), 1, "initial miss");

    // More committed content at the same width → re-render, extended output.
    // Long enough to wrap differently at width 40 vs 80 (the resize check).
    push_text_delta(
        &mut core,
        "Second paragraph here with plenty of words so that it wraps across a forty column terminal.\n\n",
    );
    let extended = core.live_committed_history_lines(80);
    assert_eq!(probe::count(), 2, "new committed content forces a re-render");
    assert!(
        extended.len() > first.len(),
        "the extended render should carry the new paragraph's rows"
    );

    // Re-render is now cached again at width 80.
    let _ = core.live_committed_history_lines(80);
    assert_eq!(probe::count(), 2, "unchanged frame stays a hit");

    // A width change invalidates the cache and reflows.
    let narrow = core.live_committed_history_lines(40);
    assert_eq!(probe::count(), 3, "a resize forces a re-render");
    assert_ne!(narrow, extended, "reflowed rows differ at a narrower width");
}

#[test]
fn round_boundary_epoch_prevents_stale_alias_at_equal_committed_length() {
    // Both rounds commit exactly five bytes ("aaaa\n" / "bbbb\n"). Keyed on
    // committed_len alone the second round would falsely hit the first round's
    // cached render; the epoch (bumped on clear) is what keeps them distinct.
    let mut core = core();
    push_text_delta(&mut core, "aaaa\n");
    let first = core.live_committed_history_lines(80);

    // Round boundary, then immediately recommit to the same length WITHOUT an
    // intervening (empty-committed) render that would have dropped the cache.
    core.transcript.clear_transient_live_tail();
    push_text_delta(&mut core, "bbbb\n");
    let second = core.live_committed_history_lines(80);

    assert_ne!(
        first, second,
        "a new round at an equal committed length must not reuse the prior render"
    );
    assert!(
        second
            .iter()
            .any(|line| line.text().contains("bbbb")),
        "the second round's own content must render: {second:?}"
    );
}

#[test]
fn no_committed_content_drops_the_cache_and_renders_nothing() {
    let mut core = core();
    push_text_delta(&mut core, "committed line\n");
    let _ = core.live_committed_history_lines(80);

    core.transcript.clear_transient_live_tail();
    let empty = core.live_committed_history_lines(80);
    assert!(
        empty.is_empty(),
        "with nothing committed the committed block is empty"
    );
}
