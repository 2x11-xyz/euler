//! Memoization of the committed prefix of a streaming answer (perf fix).
//!
//! While an assistant message streams, the TUI repaints the live transcript on
//! every frame — and the spinner forces a repaint ~11x/second for the whole
//! turn. Re-parsing the (append-only) committed prefix each time is quadratic
//! in the answer's length. These tests pin the cache down at two levels:
//!
//! * the `LiveCommittedCache` component in isolation, driving it with a render
//!   closure that increments a local counter — so a hit provably runs zero
//!   render work and every cache-key field change is provably a miss. No
//!   globals, no `cfg(test)` on the production path: race-free under plain
//!   parallel `cargo test`, not just nextest's process-per-test isolation.
//! * the `AppCore` seam end-to-end, where the cached rows must stay
//!   byte-identical to the old unmemoized full render and a real width / gutter
//!   / theme / round change must reflow.

use super::super::visual::{ratatui_lines_to_canvas, LiveCommittedCache, LiveCommittedKey};
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

// --- Component-level tests: count render-closure calls directly. ---------

fn sample_lines(text: &str) -> Vec<CanvasLine> {
    vec![CanvasLine::plain_lossy(text)]
}

fn sample_key(epoch: u64, committed_len: usize) -> LiveCommittedKey {
    LiveCommittedKey::new(epoch, committed_len, 80, false, Theme::default_dark())
}

#[test]
fn identical_key_is_a_hit_and_never_re_renders() {
    let mut cache = LiveCommittedCache::default();
    let key = sample_key(1, 10);
    let mut renders = 0_usize;

    let first = cache.lines_with(key.clone(), || {
        renders += 1;
        sample_lines("committed")
    });
    assert_eq!(renders, 1, "the first frame is a cache miss");

    let second = cache.lines_with(key, || {
        renders += 1;
        sample_lines("committed")
    });
    assert_eq!(
        renders, 1,
        "an identical key must reuse the cached rows without re-rendering"
    );
    assert_eq!(first, second, "the hit must return the cached rows");
}

#[test]
fn every_cache_key_field_change_forces_a_re_render() {
    // Each variant differs from `base` in exactly one field — including the two
    // the reviewer called out, the timestamp gutter (`show_ts`) and the theme.
    let base = LiveCommittedKey::new(1, 10, 80, false, Theme::default_dark());
    let variants = [
        (
            "epoch",
            LiveCommittedKey::new(2, 10, 80, false, Theme::default_dark()),
        ),
        (
            "committed_len",
            LiveCommittedKey::new(1, 11, 80, false, Theme::default_dark()),
        ),
        (
            "width",
            LiveCommittedKey::new(1, 10, 40, false, Theme::default_dark()),
        ),
        (
            "timestamp_gutter",
            LiveCommittedKey::new(1, 10, 80, true, Theme::default_dark()),
        ),
        (
            "theme",
            LiveCommittedKey::new(1, 10, 80, false, Theme::default_light()),
        ),
    ];

    for (field, variant) in variants {
        let mut cache = LiveCommittedCache::default();
        let mut renders = 0_usize;
        let _ = cache.lines_with(base.clone(), || {
            renders += 1;
            sample_lines("base")
        });
        let _ = cache.lines_with(variant, || {
            renders += 1;
            sample_lines("variant")
        });
        assert_eq!(
            renders, 2,
            "a change to `{field}` must be a cache miss that re-renders"
        );
    }
}

#[test]
fn a_mutable_tail_only_frame_keeps_the_committed_cache_a_hit() {
    // The mutable tail is *not* part of the committed key, so a frame that only
    // grew the tail presents the same key and must not re-render the committed
    // prefix (the whole point of the split).
    let mut cache = LiveCommittedCache::default();
    let key = sample_key(1, 10);
    let mut renders = 0_usize;

    let _ = cache.lines_with(key.clone(), || {
        renders += 1;
        sample_lines("committed")
    });
    // Same committed key even though (in production) the mutable tail changed.
    let _ = cache.lines_with(key, || {
        renders += 1;
        sample_lines("committed")
    });
    assert_eq!(
        renders, 1,
        "a mutable-tail-only frame must not re-render the committed prefix"
    );
}

#[test]
fn clear_drops_the_entry_so_the_next_frame_re_renders() {
    let mut cache = LiveCommittedCache::default();
    let key = sample_key(1, 10);
    let mut renders = 0_usize;

    let _ = cache.lines_with(key.clone(), || {
        renders += 1;
        sample_lines("committed")
    });
    cache.clear();
    let _ = cache.lines_with(key, || {
        renders += 1;
        sample_lines("committed")
    });
    assert_eq!(
        renders, 2,
        "after `clear` the same key must be a miss again"
    );
}

// --- AppCore-level tests: byte-identical equivalence and real reflows. ----

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
        assert_eq!(
            hit, expected,
            "cache hit must stay identical at width {width}"
        );
    }
}

#[test]
fn new_committed_content_and_width_changes_reflow() {
    let mut core = core();
    push_text_delta(&mut core, "First paragraph.\n\n");
    let first = core.live_committed_history_lines(80);

    // More committed content at the same width → extended output.
    // Long enough to wrap differently at width 40 vs 80 (the resize check).
    push_text_delta(
        &mut core,
        "Second paragraph here with plenty of words so that it wraps across a forty column terminal.\n\n",
    );
    let extended = core.live_committed_history_lines(80);
    assert!(
        extended.len() > first.len(),
        "the extended render should carry the new paragraph's rows"
    );

    // A width change reflows.
    let narrow = core.live_committed_history_lines(40);
    assert_ne!(narrow, extended, "reflowed rows differ at a narrower width");
    // The reflowed rows are exactly the unmemoized render at the new width.
    assert_eq!(
        narrow,
        unmemoized_committed_lines(&core, 40),
        "a resize must re-render, never serve stale rows"
    );
}

#[test]
fn toggling_the_timestamp_gutter_re_renders_committed_lines() {
    let mut core = core();
    push_text_delta(
        &mut core,
        "committed prose line one.\n\ncommitted prose line two.\n\n",
    );

    core.show_timestamp_gutter = false;
    let without = core.live_committed_history_lines(80);
    assert_eq!(without, unmemoized_committed_lines(&core, 80));

    // Flipping the gutter is a cache-key change: the next frame must re-render
    // with the gutter, never serve the stale gutter-less rows.
    core.show_timestamp_gutter = true;
    let with = core.live_committed_history_lines(80);
    assert_eq!(
        with,
        unmemoized_committed_lines(&core, 80),
        "the gutter toggle must re-render, not serve stale cached rows"
    );
    assert_ne!(
        with, without,
        "the timestamp gutter must change the rendered rows"
    );
}

#[test]
fn switching_the_theme_re_renders_committed_lines() {
    let mut core = core();
    core.theme = Theme::default_dark();
    push_text_delta(
        &mut core,
        "# Heading\n\nHello **world** with `code`.\n\n```rust\nlet x = 1;\n```\n\n",
    );
    let dark = core.live_committed_history_lines(80);
    assert_eq!(dark, unmemoized_committed_lines(&core, 80));

    // A theme switch is a cache-key change: the next frame must re-render with
    // the new theme's styles, never serve the cached dark-theme rows.
    core.theme = Theme::default_light();
    let light = core.live_committed_history_lines(80);
    assert_eq!(
        light,
        unmemoized_committed_lines(&core, 80),
        "the theme switch must re-render, not serve stale cached rows"
    );
    assert_ne!(
        light, dark,
        "the two themes must style the rendered rows differently"
    );
}

#[test]
fn a_mutable_tail_only_update_keeps_the_committed_revision_stable() {
    let mut core = core();
    push_text_delta(&mut core, "committed line one.\n\n");
    let rev_before = core.transcript.live_committed_revision();
    let committed_before = core.live_committed_history_lines(80);

    // A tail delta with no trailing newline never commits: the committed prefix
    // — and thus the memo key — is unchanged, so the committed block is a hit.
    push_text_delta(&mut core, "partial tail with no trailing newline yet");
    let rev_after = core.transcript.live_committed_revision();
    assert_eq!(
        rev_before, rev_after,
        "a mutable-tail-only update must not advance the committed revision"
    );
    let committed_after = core.live_committed_history_lines(80);
    assert_eq!(
        committed_before, committed_after,
        "the committed block is unchanged by a mutable-tail-only update"
    );
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
        second.iter().any(|line| line.text().contains("bbbb")),
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
