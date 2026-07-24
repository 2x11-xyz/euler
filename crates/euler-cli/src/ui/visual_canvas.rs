const ESC: char = '\u{1b}';
use super::transcript::{item_wants_timestamp, parse_event_time, EventTiming, ProjectedEntry};
use super::transcript::{TimingClock, TranscriptItem};
use chrono::Local;
use ratatui::style::Style;
use std::borrow::Cow;
use std::sync::Arc;
#[derive(Clone, Debug, Default)]
pub struct VisualCanvasState {
    finalized: Vec<ProjectedEntry>,
    history_cache: Option<RenderedHistoryCache>,
    /// Finalized items whose rows are already committed to native scrollback.
    /// Items below this boundary must never be mutated or removed — their
    /// rendered rows are physically in the terminal and cannot be recalled.
    committed_items: usize,
    /// Running reference clock for stamping newly pushed items (review v2
    /// §6): every event gets its own honest time, not a blank column.
    clock: TimingClock,
}
impl VisualCanvasState {
    pub fn new(finalized: Vec<TranscriptItem>) -> Self {
        finalized
            .into_iter()
            .fold(Self::default(), |mut state, item| {
                state.push_finalized(item);
                state
            })
    }

    /// Build from already-timed entries (a full rebuild from event
    /// provenance — resume, new session, rollback). Timing is taken as-is,
    /// not recomputed, since `entries` already carries each item's real
    /// event time.
    ///
    /// `clock_seed` is the `TimingClock` as of the last of those entries
    /// (see `TranscriptState::timed_items`). Without it, the first item
    /// pushed after the rebuild would restart `since_start` at ~0 and lose
    /// `since_previous` continuity instead of picking up the session's real
    /// timeline where the rebuilt entries left off.
    pub fn new_with_entries(entries: Vec<ProjectedEntry>, clock_seed: TimingClock) -> Self {
        let mut state = Self {
            clock: clock_seed,
            ..Self::default()
        };
        for entry in entries {
            state.push_finalized_entry(entry.item, entry.timing);
        }
        state
    }

    pub fn push_finalized(&mut self, item: TranscriptItem) {
        self.push_finalized_with_ts(item, None);
    }

    /// Push a finalized item stamped from a real event's provenance time
    /// (`ts`, RFC3339) when available, else the current wall time. Live
    /// control chrome (`item_wants_timestamp` false) is never stamped.
    pub fn push_finalized_with_ts(&mut self, item: TranscriptItem, ts: Option<&str>) {
        let timing = self.stamp_for(&item, ts);
        self.push_finalized_entry(item, timing);
    }

    fn stamp_for(&mut self, item: &TranscriptItem, ts: Option<&str>) -> Option<EventTiming> {
        if !item_wants_timestamp(item) {
            return None;
        }
        let current = ts.and_then(parse_event_time).unwrap_or_else(Local::now);
        Some(self.clock.stamp(current))
    }

    fn push_finalized_entry(&mut self, item: TranscriptItem, timing: Option<EventTiming>) {
        // Merges/removals may only touch items past the committed boundary:
        // rows already in native scrollback cannot change, so mutating their
        // source item would shift every later row and re-emit stale content
        // (the duplicate-line audit finding, P1).
        let boundary = self.committed_items;
        if matches!(item, TranscriptItem::WorkedDuration(_))
            && self.finalized.len() > boundary
            && matches!(
                self.finalized.last().map(|entry| &entry.item),
                Some(TranscriptItem::WorkedDuration(_))
            )
        {
            return;
        }
        if let TranscriptItem::Exploration { summaries } = item {
            if self.finalized.len() > boundary {
                if let Some(ProjectedEntry {
                    item:
                        TranscriptItem::Exploration {
                            summaries: existing,
                        },
                    timing: existing_timing,
                }) = self.finalized.last_mut()
                {
                    for summary in summaries {
                        if !existing.contains(&summary) {
                            existing.push(summary);
                        }
                    }
                    if timing.is_some() {
                        *existing_timing = timing;
                    }
                    // In-place mutation of the last item: invalidate only its
                    // cached rows, never the whole session.
                    let last = self.finalized.len() - 1;
                    self.mark_history_dirty_from(last);
                    return;
                }
            }
            self.finalized.push(ProjectedEntry {
                item: TranscriptItem::Exploration { summaries },
                timing,
            });
            // Pure append: the next render appends just this item (below).
            return;
        }
        if let TranscriptItem::Companion { spawn_event_id, .. } = &item {
            if let Some(pos) = self.finalized[boundary..].iter().rposition(|entry| {
                entry.item.companion_spawn_event_id() == Some(spawn_event_id.as_str())
            }) {
                let index = boundary + pos;
                let entry = &mut self.finalized[index];
                let _ = super::transcript::merge_companion_item(&mut entry.item, item);
                if timing.is_some() {
                    entry.timing = timing;
                }
                self.mark_history_dirty_from(index);
                return;
            }
        }
        if let TranscriptItem::FileDiff { path, .. } = &item {
            if let Some(index) = self.finalized[boundary..].iter().rposition(|entry| {
                matches!(&entry.item, TranscriptItem::PatchApplied { path: p, .. } if p == path)
            }) {
                self.mark_history_dirty_from(boundary + index);
                self.finalized.remove(boundary + index);
            }
            if let Some(index) = self.finalized[boundary..].iter().rposition(|entry| {
                matches!(&entry.item, TranscriptItem::FileChange { path: p, .. } if p == path)
            }) {
                self.mark_history_dirty_from(boundary + index);
                self.finalized.remove(boundary + index);
            }
        }
        // Pure append (possibly after the removals above, which already
        // invalidated the cache from their point onward). The cache is not
        // dropped: the next render re-renders only from the earliest dirty or
        // newly appended item.
        self.finalized.push(ProjectedEntry { item, timing });
    }

    /// Invalidate the cached history render from finalized item `index`
    /// onward: that item (and everything after it) was mutated or removed, so
    /// its cached rows are stale and re-render on the next `render_history`.
    /// Items before `index` keep their cached rows byte-for-byte.
    ///
    /// The truncation point first walks back over any immediately preceding
    /// `Notice` run. A notice's trailing blank is suppressed only when the
    /// item after it is also a notice (the renderer's rhythm rule), so a
    /// notice adjacent to the mutated region must re-render too — its blank
    /// may need to reappear or vanish.
    ///
    /// The truncation point (and the walk-back) is clamped to the committed
    /// boundary: items below `committed_items` are physically in native
    /// scrollback and cannot be rewritten, so their cached rows are never
    /// truncated. Callers only ever touch items past the boundary; the clamp
    /// on `index` is a defensive floor so a stray earlier index can never
    /// retract a committed row.
    fn mark_history_dirty_from(&mut self, index: usize) {
        let boundary = self.committed_items;
        let mut from = index.max(boundary);
        while from > boundary
            && matches!(
                self.finalized.get(from - 1).map(|entry| &entry.item),
                Some(TranscriptItem::Notice(_))
            )
        {
            from -= 1;
        }
        if let Some(cache) = &mut self.history_cache {
            let keep = from.min(cache.item_end_offsets.len());
            let line_keep = keep
                .checked_sub(1)
                .map_or(0, |last| cache.item_end_offsets[last]);
            Arc::make_mut(&mut cache.lines).truncate(line_keep);
            cache.item_end_offsets.truncate(keep);
        }
    }

    /// Advance the committed-items boundary (monotonic; from the terminal's
    /// native-scrollback accounting after each draw).
    pub fn set_committed_items(&mut self, committed: usize) {
        self.committed_items = self
            .committed_items
            .max(committed.min(self.finalized.len()));
    }

    /// Reset the boundary (history replay purges native scrollback).
    pub fn reset_committed_items(&mut self) {
        self.committed_items = 0;
    }

    pub fn has_foldable_artifact(&self, output_limit_lines: usize) -> bool {
        self.finalized
            .iter()
            .any(|entry| entry.item.is_foldable_artifact(output_limit_lines))
    }

    pub fn finalized_items(&self) -> Vec<TranscriptItem> {
        self.finalized
            .iter()
            .map(|entry| entry.item.clone())
            .collect()
    }

    pub fn invalidate_history_cache(&mut self) {
        self.history_cache = None;
    }

    pub fn render<R>(
        &mut self,
        snapshot: VisualCanvasSnapshot,
        render_finalized: R,
    ) -> VisualCanvasFrame
    where
        R: FnOnce(&[ProjectedEntry], usize, u16) -> (Vec<CanvasLine>, Vec<usize>),
    {
        let (history, offsets) = self.render_history(snapshot.width, render_finalized);
        let mut frame = derive_frame_owned(history, snapshot.blocks, snapshot.focus);
        frame.history_item_offsets = offsets;
        frame
    }

    /// Render the finalized history at `width`, reusing the cache incrementally.
    ///
    /// - Width unchanged, nothing appended → serve the cached render.
    /// - Width unchanged, items appended → render only the new tail (with full
    ///   cross-item context) and splice it on; previously rendered rows stay
    ///   byte-identical, and the per-item offsets extend in lockstep.
    /// - Width changed (or the cache was dropped by a theme/config change) →
    ///   full re-render; this is the only path that pays the whole session's
    ///   markdown/highlight cost.
    ///
    /// `render_finalized(entries, render_from, width)` renders
    /// `entries[render_from..]` with offsets relative to that segment.
    ///
    /// The returned lines Arc-share the cache's buffer: `render` runs per
    /// keystroke and per spinner tick, so the frame must never deep-copy the
    /// whole rendered history (deep review P2-d). Mutation paths go through
    /// `Arc::make_mut`, which is in-place in production because the previous
    /// frame is dropped before the next render.
    fn render_history<R>(
        &mut self,
        width: u16,
        render_finalized: R,
    ) -> (Arc<Vec<CanvasLine>>, Vec<usize>)
    where
        R: FnOnce(&[ProjectedEntry], usize, u16) -> (Vec<CanvasLine>, Vec<usize>),
    {
        let width_matches = self
            .history_cache
            .as_ref()
            .is_some_and(|cache| cache.width == width);
        if width_matches {
            let cached_items = self
                .history_cache
                .as_ref()
                .map_or(0, |cache| cache.item_end_offsets.len());
            if cached_items >= self.finalized.len() {
                let cache = self
                    .history_cache
                    .as_ref()
                    .expect("cache present when width matches");
                return (Arc::clone(&cache.lines), cache.item_end_offsets.clone());
            }
            let (mut new_lines, new_offsets) =
                render_finalized(&self.finalized, cached_items, width);
            let retract_seam_blank =
                seam_retracts_trailing_blank(&self.finalized, cached_items, self.committed_items);
            let cache = self
                .history_cache
                .as_mut()
                .expect("cache present when width matches");
            let lines = Arc::make_mut(&mut cache.lines);
            if retract_seam_blank {
                if let Some(last) = cache.item_end_offsets.last_mut() {
                    if lines.len() == *last && *last > 0 {
                        lines.pop();
                        *last -= 1;
                    }
                }
            }
            let base = lines.len();
            lines.append(&mut new_lines);
            cache
                .item_end_offsets
                .extend(new_offsets.into_iter().map(|offset| offset + base));
            return (Arc::clone(&cache.lines), cache.item_end_offsets.clone());
        }
        let (lines, item_end_offsets) = render_finalized(&self.finalized, 0, width);
        let lines = Arc::new(lines);
        self.history_cache = Some(RenderedHistoryCache {
            width,
            lines: Arc::clone(&lines),
            item_end_offsets: item_end_offsets.clone(),
        });
        (lines, item_end_offsets)
    }
}

/// True when appending `finalized[boundary..]` onto a cache that already holds
/// `finalized[..boundary]` must retract the last cached item's trailing blank:
/// the cached last item and the first new item are both notices, and the
/// renderer stacks consecutive notices with no blank between them.
///
/// The retraction is refused when the preceding notice (`boundary - 1`) is
/// already inside `committed_items`: its rows — trailing blank included — are
/// physically in native scrollback and cannot be rewritten. The incremental
/// render must preserve everything above the committed boundary byte-for-byte,
/// even where a fresh full render would drop the blank; scrollback consistency
/// wins over the rhythm rule.
fn seam_retracts_trailing_blank(
    finalized: &[ProjectedEntry],
    boundary: usize,
    committed_items: usize,
) -> bool {
    let Some(prev_index) = boundary.checked_sub(1) else {
        return false;
    };
    if prev_index < committed_items {
        return false;
    }
    let (Some(prev), Some(next)) = (finalized.get(prev_index), finalized.get(boundary)) else {
        return false;
    };
    super::transcript::consecutive_notices(&prev.item, &next.item)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RenderedHistoryCache {
    width: u16,
    /// Arc-shared with every frame rendered from this cache; mutated only
    /// through `Arc::make_mut` (in-place while the allocation is unshared).
    lines: Arc<Vec<CanvasLine>>,
    item_end_offsets: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisualCanvasSnapshot {
    pub width: u16,
    pub blocks: Vec<VisualBlock>,
    pub status: CanvasStatusSnapshot,
    pub composer: CanvasComposerSnapshot,
    pub focus: FocusOwner,
}

impl VisualCanvasSnapshot {
    pub fn new(
        width: u16,
        blocks: Vec<VisualBlock>,
        status: CanvasStatusSnapshot,
        composer: CanvasComposerSnapshot,
        focus: FocusOwner,
    ) -> Self {
        Self {
            width,
            blocks,
            status,
            composer,
            focus,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisualCanvasFrame {
    /// Rendered finalized-history rows, Arc-shared with the canvas's
    /// incremental cache — never deep-copied per frame (render runs per
    /// keystroke and per spinner tick).
    pub history_lines: Arc<Vec<CanvasLine>>,
    /// Rows below the history: live transcript, chrome, composer, status.
    pub tail_lines: Vec<CanvasLine>,
    pub cursor: Option<CursorTarget>,
    pub required_height: u16,
    pub history_rows: usize,
    pub committable_rows: usize,
    pub pinned_rows: usize,
    pub prefer_stable_height: bool,
    /// Cumulative end-row offset of each finalized history item at this
    /// frame's width. Native-scrollback commits snap to these boundaries so
    /// a width change can remap the committed prefix exactly.
    pub history_item_offsets: Vec<usize>,
}

impl VisualCanvasFrame {
    /// Two-segment view over every frame row (history followed by tail). All
    /// row indices carried by the frame — `history_rows`, `committable_rows`,
    /// `pinned_rows`, cursor rows, `history_item_offsets` — address this
    /// concatenation; the head/tail split is storage only.
    pub fn lines(&self) -> FrameLines<'_> {
        FrameLines::new(&self.history_lines, &self.tail_lines)
    }

    pub fn line_count(&self) -> usize {
        self.history_lines.len() + self.tail_lines.len()
    }

    /// Materialized copy of every frame row, for test assertions only — the
    /// production path never concatenates history and tail into one buffer.
    #[cfg(test)]
    pub fn active_frame_lines(&self) -> Vec<CanvasLine> {
        self.lines().to_vec()
    }
}

/// Borrowed two-segment view over a frame's rows: the Arc-shared history
/// segment followed by the per-frame tail. Row indices span the
/// concatenation. Ranges that fall inside a single segment borrow it
/// directly; only a range that straddles the seam materializes (rare and
/// bounded — production commit slices always lie inside the history
/// segment, and hand-built test frames keep everything in the tail).
#[derive(Clone, Copy, Debug)]
pub struct FrameLines<'a> {
    head: &'a [CanvasLine],
    tail: &'a [CanvasLine],
}

impl<'a> FrameLines<'a> {
    pub fn new(head: &'a [CanvasLine], tail: &'a [CanvasLine]) -> Self {
        Self { head, tail }
    }

    /// View over a single contiguous buffer (test call sites that assemble
    /// raw line vectors without a frame).
    #[cfg(test)]
    pub fn from_slice(lines: &'a [CanvasLine]) -> Self {
        Self {
            head: &[],
            tail: lines,
        }
    }

    pub fn len(self) -> usize {
        self.head.len() + self.tail.len()
    }

    pub fn iter(self) -> impl Iterator<Item = &'a CanvasLine> {
        self.head.iter().chain(self.tail.iter())
    }

    /// Lines in `[start, end)`, clamped to the view's bounds.
    pub fn range(self, start: usize, end: usize) -> impl Iterator<Item = &'a CanvasLine> {
        let end = end.min(self.len());
        let start = start.min(end);
        let head_len = self.head.len();
        let head = &self.head[start.min(head_len)..end.min(head_len)];
        let tail_start = start.saturating_sub(head_len).min(self.tail.len());
        let tail_end = end.saturating_sub(head_len).min(self.tail.len());
        head.iter().chain(self.tail[tail_start..tail_end].iter())
    }

    /// Lines in `[start, end)` as a slice: borrowed when the range lies
    /// within one segment, materialized only when it straddles the seam.
    pub fn range_cow(self, start: usize, end: usize) -> Cow<'a, [CanvasLine]> {
        let end = end.min(self.len());
        let start = start.min(end);
        let head_len = self.head.len();
        if end <= head_len {
            return Cow::Borrowed(&self.head[start..end]);
        }
        if start >= head_len {
            return Cow::Borrowed(&self.tail[start - head_len..end - head_len]);
        }
        Cow::Owned(self.range(start, end).cloned().collect())
    }

    pub fn to_vec(self) -> Vec<CanvasLine> {
        self.iter().cloned().collect()
    }
}

/// Assemble a frame from the Arc-shared rendered history and the per-frame
/// blocks below it, moving each block's lines into `tail_lines` rather than
/// cloning them. The history segment is shared with the canvas cache and
/// never copied; only History rows are committable, so the committable
/// prefix is exactly the history segment.
fn derive_frame_owned(
    history_lines: Arc<Vec<CanvasLine>>,
    blocks: Vec<VisualBlock>,
    focus: FocusOwner,
) -> VisualCanvasFrame {
    let pinned_rows = pinned_suffix_rows(&blocks);
    let prefer_stable_height = focus == FocusOwner::BottomSurface;
    let history_rows = history_lines.len();
    let committable_rows = history_rows;
    let mut tail_lines: Vec<CanvasLine> = Vec::new();
    let mut cursor = None;

    for block in blocks {
        if cursor.is_none() && focus.allows_cursor(block.role) {
            let rows_so_far = u16::try_from(history_rows + tail_lines.len()).unwrap_or(u16::MAX);
            cursor = block.cursor.map(|block_cursor| CursorTarget {
                row: block_cursor.row.saturating_add(rows_so_far),
                column: block_cursor.column,
            });
        }
        tail_lines.extend(block.lines);
    }

    VisualCanvasFrame {
        required_height: u16::try_from(history_rows + tail_lines.len()).unwrap_or(u16::MAX),
        pinned_rows,
        history_lines,
        tail_lines,
        cursor,
        history_rows,
        committable_rows,
        prefer_stable_height,
        history_item_offsets: Vec::new(),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisualBlock {
    pub role: VisualBlockRole,
    pub lines: Vec<CanvasLine>,
    pub cursor: Option<BlockCursor>,
}

impl VisualBlock {
    pub fn new(role: VisualBlockRole, lines: Vec<CanvasLine>) -> Self {
        Self {
            role,
            lines,
            cursor: None,
        }
    }

    pub fn with_cursor(mut self, cursor: BlockCursor) -> Self {
        self.cursor = Some(cursor);
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisualBlockRole {
    LiveTranscript,
    PermissionAsk,
    Activity,
    BottomSurface,
    Composer,
    Status,
    Notice,
    /// Reserved footer rhythm row, pinned with notice/composer/status chrome.
    Spacer,
}

fn pinned_suffix_rows(blocks: &[VisualBlock]) -> usize {
    blocks
        .iter()
        .rev()
        .take_while(|block| is_pinned_suffix_role(block.role))
        .map(|block| active_row_count(&block.lines))
        .sum()
}

fn is_pinned_suffix_role(role: VisualBlockRole) -> bool {
    matches!(
        role,
        VisualBlockRole::PermissionAsk
            | VisualBlockRole::Activity
            | VisualBlockRole::BottomSurface
            | VisualBlockRole::Composer
            | VisualBlockRole::Notice
            | VisualBlockRole::Spacer
            | VisualBlockRole::Status
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanvasStatusSnapshot {
    pub target: CanvasText,
    pub line: CanvasLine,
}

impl CanvasStatusSnapshot {
    pub fn new(target: impl Into<String>, line: CanvasLine) -> Self {
        Self {
            target: CanvasText::plain_lossy(target),
            line,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanvasComposerSnapshot {
    pub draft: CanvasText,
    pub visible_lines: Vec<CanvasLine>,
    pub cursor: Option<BlockCursor>,
}

impl CanvasComposerSnapshot {
    pub fn new(
        draft: impl Into<String>,
        visible_lines: Vec<CanvasLine>,
        cursor: Option<BlockCursor>,
    ) -> Self {
        Self {
            draft: CanvasText::plain_lossy(draft),
            visible_lines,
            cursor,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FocusOwner {
    Composer,
    BottomSurface,
    Modal,
}

impl FocusOwner {
    fn allows_cursor(self, role: VisualBlockRole) -> bool {
        matches!(
            (self, role),
            (Self::Composer, VisualBlockRole::Composer)
                | (Self::BottomSurface, VisualBlockRole::BottomSurface)
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockCursor {
    pub row: u16,
    pub column: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorTarget {
    pub row: u16,
    pub column: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanvasLine {
    pub spans: Vec<CanvasSpan>,
}

impl CanvasLine {
    #[cfg(test)]
    pub fn plain(text: impl Into<String>) -> Self {
        Self::styled(text, TextRole::Plain)
    }

    pub fn plain_lossy(text: impl Into<String>) -> Self {
        Self::styled_lossy(text, TextRole::Plain)
    }

    #[cfg(test)]
    pub fn styled(text: impl Into<String>, role: TextRole) -> Self {
        Self {
            spans: vec![CanvasSpan::new(text, role)],
        }
    }

    pub fn styled_lossy(text: impl Into<String>, role: TextRole) -> Self {
        Self {
            spans: vec![CanvasSpan::new_lossy(text, role)],
        }
    }

    pub fn from_spans(spans: Vec<CanvasSpan>) -> Self {
        Self { spans }
    }

    #[cfg(test)]
    pub(crate) fn plain_text(&self) -> String {
        self.spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>()
            .join("")
    }

    #[cfg(test)]
    pub fn text(&self) -> String {
        self.plain_text()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanvasSpan {
    pub text: CanvasText,
    pub role: TextRole,
    pub style: Style,
}

impl CanvasSpan {
    #[cfg(test)]
    pub fn new(text: impl Into<String>, role: TextRole) -> Self {
        Self {
            text: CanvasText::plain(text),
            role,
            style: Style::default(),
        }
    }

    pub fn new_lossy(text: impl Into<String>, role: TextRole) -> Self {
        Self {
            text: CanvasText::plain_lossy(text),
            role,
            style: Style::default(),
        }
    }

    pub fn styled_lossy(text: impl Into<String>, role: TextRole, style: Style) -> Self {
        Self {
            text: CanvasText::plain_lossy(text),
            role,
            style,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextRole {
    Plain,
    Prompt,
    Status,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanvasText(String);

impl CanvasText {
    #[cfg(test)]
    pub fn plain(text: impl Into<String>) -> Self {
        Self::try_plain(text).expect("visual canvas text cannot contain escape bytes")
    }

    pub fn plain_lossy(text: impl Into<String>) -> Self {
        Self(text.into().replace(ESC, "\u{fffd}"))
    }

    #[cfg(test)]
    pub fn try_plain(text: impl Into<String>) -> Result<Self, CanvasTextError> {
        let text = text.into();
        if text.contains(ESC) {
            return Err(CanvasTextError::EscapeByte);
        }
        Ok(Self(text))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanvasTextError {
    EscapeByte,
}

#[cfg(test)]
impl std::fmt::Display for CanvasTextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EscapeByte => f.write_str("text contains an escape byte"),
        }
    }
}

#[cfg(test)]
impl std::error::Error for CanvasTextError {}

fn active_row_count(lines: &[CanvasLine]) -> usize {
    lines.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuild_reseeds_clock_so_new_items_continue_the_pre_rebuild_timeline() {
        // Review finding: `new_with_entries` used to build `Self::default()`,
        // giving the fresh clock a blank first/previous. The first item
        // stamped after a resume/rollback rebuild then got since_start≈0 and
        // since_previous=None instead of continuing the session's real
        // timeline. Fixed ts strings only — never wall clock (hermeticity).
        let mut clock = TimingClock::default();
        let first_timing = clock
            .stamp_at("2026-07-05T00:00:00.000Z")
            .expect("valid ts");
        let previous_timing = clock
            .stamp_at("2026-07-05T00:00:10.000Z")
            .expect("valid ts");
        let entries = vec![
            ProjectedEntry {
                item: TranscriptItem::UserMessage("hello".to_owned()),
                timing: Some(first_timing),
            },
            ProjectedEntry {
                item: TranscriptItem::AssistantMessage("hi there".to_owned()),
                timing: Some(previous_timing),
            },
        ];

        // `clock` now holds the seed a real rebuild caller (timed_items)
        // would thread through: first = 00:00:00, previous = 00:00:10.
        let mut state = VisualCanvasState::new_with_entries(entries, clock);

        state.push_finalized_with_ts(
            TranscriptItem::AssistantMessage("continued".to_owned()),
            Some("2026-07-05T00:00:25.000Z"),
        );

        let pushed = state.finalized.last().expect("pushed entry");
        let timing = pushed.timing.as_ref().expect("timed item");
        assert_eq!(
            timing.since_previous_for_test(),
            Some("15s"),
            "since_previous should continue from the pre-rebuild previous stamp (00:00:10), not restart at None"
        );
        assert_eq!(
            timing.since_start_for_test(),
            Some("25s"),
            "since_start should continue from the pre-rebuild first stamp (00:00:00), not restart at ~0"
        );
    }

    #[test]
    fn text_rejects_escape_bytes_instead_of_carrying_raw_terminal_payloads() {
        assert_eq!(
            CanvasText::try_plain("\u{1b}[31mred"),
            Err(CanvasTextError::EscapeByte)
        );
    }

    #[test]
    fn frame_stacks_blocks_in_stream_order() {
        let snapshot = snapshot_with_blocks(vec![VisualBlock::new(
            VisualBlockRole::Activity,
            vec![CanvasLine::plain("active")],
        )]);

        let frame = derive_test_frame(&snapshot, vec![CanvasLine::plain("final")]);

        assert_eq!(
            line_texts(&frame.active_frame_lines()),
            vec!["final", "active"]
        );
        assert_eq!(frame.required_height, 2);
        assert_eq!(frame.committable_rows, 1);
    }

    #[test]
    fn live_transcript_rows_stay_after_native_scrollback_commit_boundary() {
        let snapshot = snapshot_with_blocks(vec![VisualBlock::new(
            VisualBlockRole::LiveTranscript,
            vec![CanvasLine::plain("live")],
        )]);

        let frame = derive_test_frame(&snapshot, vec![CanvasLine::plain("history")]);

        assert_eq!(
            line_texts(&frame.active_frame_lines()),
            vec!["history", "live"]
        );
        assert_eq!(frame.committable_rows, 1);
        assert_eq!(frame.history_rows, 1);
    }

    #[test]
    fn non_prefix_transcript_rows_are_not_committable() {
        let snapshot = snapshot_with_blocks(vec![
            VisualBlock::new(VisualBlockRole::Activity, vec![CanvasLine::plain("tool")]),
            VisualBlock::new(
                VisualBlockRole::LiveTranscript,
                vec![CanvasLine::plain("live")],
            ),
        ]);

        let frame = derive_test_frame(&snapshot, vec![CanvasLine::plain("history")]);

        assert_eq!(
            line_texts(&frame.active_frame_lines()),
            vec!["history", "tool", "live"]
        );
        assert_eq!(frame.history_rows, 1);
        assert_eq!(frame.committable_rows, 1);
        assert!(frame.committable_rows <= frame.history_rows);
        assert!(frame.history_rows <= frame.active_frame_lines().len());
    }

    #[test]
    fn live_transcript_tail_closes_committable_prefix() {
        let snapshot = snapshot_with_blocks(vec![
            VisualBlock::new(
                VisualBlockRole::LiveTranscript,
                vec![CanvasLine::plain("stable")],
            ),
            VisualBlock::new(
                VisualBlockRole::LiveTranscript,
                vec![CanvasLine::plain("mutable")],
            ),
        ]);

        let frame = derive_test_frame(&snapshot, vec![CanvasLine::plain("history")]);

        assert_eq!(
            line_texts(&frame.active_frame_lines()),
            vec!["history", "stable", "mutable"]
        );
        assert_eq!(frame.committable_rows, 1);
        assert_eq!(frame.history_rows, 1);
    }

    #[test]
    fn trailing_live_control_stack_is_pinned_suffix() {
        let snapshot = snapshot_with_blocks(vec![
            VisualBlock::new(
                VisualBlockRole::LiveTranscript,
                vec![CanvasLine::plain("stream")],
            ),
            VisualBlock::new(
                VisualBlockRole::Activity,
                vec![CanvasLine::plain("tool active")],
            ),
            VisualBlock::new(
                VisualBlockRole::Notice,
                vec![CanvasLine::plain("press Ctrl+C again")],
            ),
            VisualBlock::new(
                VisualBlockRole::BottomSurface,
                vec![CanvasLine::plain("/ command menu")],
            ),
            VisualBlock::new(VisualBlockRole::Composer, vec![CanvasLine::plain("draft")]),
            VisualBlock::new(VisualBlockRole::Status, vec![CanvasLine::plain("status")]),
        ]);

        let frame = derive_test_frame(&snapshot, vec![CanvasLine::plain("history")]);

        assert_eq!(frame.pinned_rows, 5);
    }

    #[test]
    fn spacer_rows_are_part_of_the_trailing_pinned_suffix() {
        let snapshot = snapshot_with_blocks(vec![
            VisualBlock::new(
                VisualBlockRole::LiveTranscript,
                vec![CanvasLine::plain("stream")],
            ),
            VisualBlock::new(VisualBlockRole::Notice, vec![CanvasLine::plain("notice")]),
            VisualBlock::new(VisualBlockRole::Spacer, vec![CanvasLine::plain("")]),
            VisualBlock::new(VisualBlockRole::Composer, vec![CanvasLine::plain("draft")]),
            VisualBlock::new(VisualBlockRole::Spacer, vec![CanvasLine::plain("")]),
            VisualBlock::new(VisualBlockRole::Status, vec![CanvasLine::plain("status")]),
        ]);

        let frame = derive_test_frame(&snapshot, vec![CanvasLine::plain("history")]);

        assert_eq!(frame.pinned_rows, 5);
        assert_eq!(
            line_texts(&frame.active_frame_lines()),
            vec!["history", "stream", "notice", "", "draft", "", "status"]
        );
    }

    #[test]
    fn pinned_suffix_stops_at_transcript_content() {
        let snapshot = snapshot_with_blocks(vec![
            VisualBlock::new(
                VisualBlockRole::Activity,
                vec![CanvasLine::plain("old tool")],
            ),
            VisualBlock::new(
                VisualBlockRole::LiveTranscript,
                vec![CanvasLine::plain("stream")],
            ),
            VisualBlock::new(VisualBlockRole::Composer, vec![CanvasLine::plain("draft")]),
            VisualBlock::new(VisualBlockRole::Status, vec![CanvasLine::plain("status")]),
        ]);

        let frame = derive_test_frame(&snapshot, vec![CanvasLine::plain("history")]);

        assert_eq!(frame.pinned_rows, 2);
    }

    #[test]
    fn history_inserted_before_focused_block_offsets_cursor() {
        let snapshot = VisualCanvasSnapshot::new(
            80,
            vec![VisualBlock::new(
                VisualBlockRole::Composer,
                vec![CanvasLine::styled("draft", TextRole::Prompt)],
            )
            .with_cursor(BlockCursor { row: 0, column: 3 })],
            status_snapshot(),
            composer_snapshot("draft"),
            FocusOwner::Composer,
        );

        let frame = derive_test_frame(
            &snapshot,
            vec![
                CanvasLine::plain("history-1"),
                CanvasLine::plain("history-2"),
            ],
        );

        assert_eq!(frame.history_rows, 2);
        assert_eq!(frame.committable_rows, 2);
        assert_eq!(frame.required_height, 3);
        assert_eq!(frame.cursor, Some(CursorTarget { row: 2, column: 3 }));
    }

    #[test]
    fn cursor_target_is_relative_to_active_frame_lines() {
        let snapshot = VisualCanvasSnapshot::new(
            80,
            vec![
                VisualBlock::new(
                    VisualBlockRole::Activity,
                    vec![CanvasLine::plain("working")],
                ),
                VisualBlock::new(
                    VisualBlockRole::Composer,
                    vec![CanvasLine::styled("draft", TextRole::Prompt)],
                )
                .with_cursor(BlockCursor { row: 0, column: 2 }),
            ],
            status_snapshot(),
            composer_snapshot("draft"),
            FocusOwner::Composer,
        );

        let frame = derive_test_frame(&snapshot, Vec::new());

        assert_eq!(frame.cursor, Some(CursorTarget { row: 1, column: 2 }));
    }

    #[test]
    fn snapshot_preserves_status_and_composer_as_structured_ui_state() {
        let snapshot = VisualCanvasSnapshot::new(
            100,
            Vec::new(),
            CanvasStatusSnapshot::new("chatgpt/gpt-5.5", CanvasLine::plain("status line")),
            composer_snapshot("hello"),
            FocusOwner::Composer,
        );

        assert_eq!(snapshot.width, 100);
        assert_eq!(snapshot.status.target.as_str(), "chatgpt/gpt-5.5");
        assert_eq!(snapshot.status.line.text(), "status line");
        assert_eq!(snapshot.composer.draft.as_str(), "hello");
        assert_eq!(snapshot.focus, FocusOwner::Composer);
    }

    #[test]
    fn seam_snapshot_is_plain_ui_data_without_session_objects() {
        fn assert_plain_ui_data<T: Clone + Eq + std::fmt::Debug>() {}

        assert_plain_ui_data::<VisualCanvasSnapshot>();
        assert_plain_ui_data::<VisualCanvasFrame>();
    }

    // ---- Incremental history cache -------------------------------------
    //
    // A deterministic stand-in for the real markdown/highlight renderer. It
    // honours the same two contracts the cache depends on: it renders only
    // `entries[render_from..]` (offsets relative to that segment), and it
    // reproduces the renderer's rhythm rule that a run of consecutive notices
    // stacks with no blank between them. That is enough to exercise the
    // append/splice, seam-blank retraction, and dirty-truncation paths against
    // a full re-render, without pulling markdown into a unit test.

    fn fake_item_text(item: &TranscriptItem) -> String {
        match item {
            TranscriptItem::UserMessage(text) => format!("user:{text}"),
            TranscriptItem::AssistantMessage(text) => format!("assistant:{text}"),
            TranscriptItem::Notice(text) => format!("notice:{text}"),
            TranscriptItem::Exploration { summaries } => {
                format!("explore:{}", summaries.join(","))
            }
            other => format!("{other:?}"),
        }
    }

    fn fake_render(
        entries: &[ProjectedEntry],
        render_from: usize,
        _width: u16,
    ) -> (Vec<CanvasLine>, Vec<usize>) {
        let mut lines = Vec::new();
        let mut offsets = Vec::new();
        for (index, entry) in entries.iter().enumerate().skip(render_from) {
            lines.push(CanvasLine::plain(fake_item_text(&entry.item)));
            let next_is_notice_run = entries.get(index + 1).is_some_and(|next| {
                crate::ui::transcript::consecutive_notices(&entry.item, &next.item)
            });
            if !next_is_notice_run {
                lines.push(CanvasLine::plain(""));
            }
            offsets.push(lines.len());
        }
        (lines, offsets)
    }

    fn bare_snapshot(width: u16) -> VisualCanvasSnapshot {
        VisualCanvasSnapshot::new(
            width,
            Vec::new(),
            status_snapshot(),
            composer_snapshot(""),
            FocusOwner::Modal,
        )
    }

    fn render_incremental(state: &mut VisualCanvasState, width: u16) -> VisualCanvasFrame {
        state.render(bare_snapshot(width), fake_render)
    }

    /// A from-scratch full render of the same push sequence: a fresh state's
    /// empty cache always takes the `render_from == 0` path.
    fn full_render(items: &[TranscriptItem], width: u16) -> VisualCanvasFrame {
        let mut state = VisualCanvasState::default();
        for item in items {
            state.push_finalized(item.clone());
        }
        render_incremental(&mut state, width)
    }

    #[test]
    fn appending_an_item_only_appends_and_leaves_prior_rows_byte_identical() {
        let mut state = VisualCanvasState::default();
        state.push_finalized(TranscriptItem::UserMessage("one".to_owned()));
        let before = render_incremental(&mut state, 80);

        state.push_finalized(TranscriptItem::AssistantMessage("two".to_owned()));
        let after = render_incremental(&mut state, 80);

        // Every previously rendered row is byte-identical, and the second
        // render only extended the buffer — nothing above the seam moved.
        assert_eq!(
            after.active_frame_lines()[..before.active_frame_lines().len()],
            before.active_frame_lines()[..]
        );
        assert!(after.active_frame_lines().len() > before.active_frame_lines().len());
        assert_eq!(
            after.history_item_offsets[..before.history_item_offsets.len()],
            before.history_item_offsets[..]
        );
        // And the incremental result matches a from-scratch full render.
        let full = full_render(
            &[
                TranscriptItem::UserMessage("one".to_owned()),
                TranscriptItem::AssistantMessage("two".to_owned()),
            ],
            80,
        );
        assert_eq!(after.active_frame_lines(), full.active_frame_lines());
        assert_eq!(after.history_item_offsets, full.history_item_offsets);
    }

    #[test]
    fn appending_renders_only_the_new_items_not_the_whole_session() {
        let mut state = VisualCanvasState::default();
        for index in 0..64 {
            state.push_finalized(TranscriptItem::UserMessage(format!("m{index}")));
        }

        let rendered = std::cell::Cell::new(0usize);
        // First render at this width: full render of all 64 items.
        state.render(bare_snapshot(80), |entries, render_from, width| {
            rendered.set(rendered.get() + (entries.len() - render_from));
            fake_render(entries, render_from, width)
        });
        assert_eq!(rendered.get(), 64, "cold render renders the whole session");

        // Steady state: each subsequent append renders exactly one item —
        // O(1) per push instead of the O(n) full re-render the old cache did.
        for index in 64..80 {
            state.push_finalized(TranscriptItem::UserMessage(format!("m{index}")));
            rendered.set(0);
            state.render(bare_snapshot(80), |entries, render_from, width| {
                rendered.set(rendered.get() + (entries.len() - render_from));
                fake_render(entries, render_from, width)
            });
            assert_eq!(rendered.get(), 1, "append re-renders only the new item");
        }
    }

    #[test]
    fn a_render_with_no_new_items_renders_nothing() {
        let mut state = VisualCanvasState::default();
        state.push_finalized(TranscriptItem::UserMessage("only".to_owned()));
        render_incremental(&mut state, 80);

        let rendered = std::cell::Cell::new(0usize);
        state.render(bare_snapshot(80), |entries, render_from, width| {
            rendered.set(rendered.get() + (entries.len() - render_from));
            fake_render(entries, render_from, width)
        });
        assert_eq!(rendered.get(), 0, "an idle frame reuses the cache verbatim");
    }

    #[test]
    fn width_change_forces_a_full_re_render() {
        let mut state = VisualCanvasState::default();
        for index in 0..8 {
            state.push_finalized(TranscriptItem::UserMessage(format!("m{index}")));
        }
        render_incremental(&mut state, 80);

        let rendered = std::cell::Cell::new(0usize);
        let narrow = state.render(bare_snapshot(40), |entries, render_from, width| {
            rendered.set(rendered.get() + (entries.len() - render_from));
            fake_render(entries, render_from, width)
        });
        assert_eq!(
            rendered.get(),
            8,
            "a width change re-renders every finalized item"
        );

        let items: Vec<_> = (0..8)
            .map(|index| TranscriptItem::UserMessage(format!("m{index}")))
            .collect();
        let full = full_render(&items, 40);
        assert_eq!(narrow.active_frame_lines(), full.active_frame_lines());
        assert_eq!(narrow.history_item_offsets, full.history_item_offsets);
    }

    #[test]
    fn incremental_offsets_stay_consistent_with_terminal_commit_accounting() {
        let mut state = VisualCanvasState::default();
        let items = vec![
            TranscriptItem::UserMessage("hello".to_owned()),
            TranscriptItem::AssistantMessage("world".to_owned()),
            TranscriptItem::UserMessage("again".to_owned()),
        ];
        for item in &items {
            state.push_finalized(item.clone());
            render_incremental(&mut state, 80);
        }
        let frame = render_incremental(&mut state, 80);

        // One offset per finalized item, strictly increasing, and the last
        // offset equals the history row count — the invariant terminal.rs
        // relies on for `partition_point` commit-boundary remapping.
        assert_eq!(frame.history_item_offsets.len(), items.len());
        assert!(frame
            .history_item_offsets
            .windows(2)
            .all(|pair| pair[0] < pair[1]));
        assert_eq!(
            frame.history_item_offsets.last().copied(),
            Some(frame.history_rows)
        );
        assert_eq!(frame.history_rows, frame.active_frame_lines().len());

        // partition_point over the offsets recovers a whole-item boundary for
        // any committed row count (terminal.rs `set_committed_active_rows`).
        for committed_rows in 0..=frame.history_rows {
            let items_covered = frame
                .history_item_offsets
                .partition_point(|end| *end <= committed_rows);
            assert!(items_covered <= items.len());
        }
    }

    #[test]
    fn consecutive_notices_stack_identically_under_incremental_append() {
        // The renderer suppresses the blank between consecutive notices. When
        // the second notice arrives as an incremental append, the cache must
        // retract the first notice's already-emitted trailing blank.
        let items = vec![
            TranscriptItem::AssistantMessage("prose".to_owned()),
            TranscriptItem::Notice("first".to_owned()),
            TranscriptItem::Notice("second".to_owned()),
            TranscriptItem::UserMessage("after".to_owned()),
        ];
        let mut state = VisualCanvasState::default();
        for item in &items {
            state.push_finalized(item.clone());
            render_incremental(&mut state, 80);
        }
        let incremental = render_incremental(&mut state, 80);
        let full = full_render(&items, 80);
        assert_eq!(incremental.active_frame_lines(), full.active_frame_lines());
        assert_eq!(incremental.history_item_offsets, full.history_item_offsets);
        // The two notices really are adjacent (no blank between them).
        let texts = line_texts(&incremental.active_frame_lines());
        let first = texts
            .iter()
            .position(|t| t == "notice:first")
            .expect("first");
        assert_eq!(
            texts.get(first + 1).map(String::as_str),
            Some("notice:second")
        );
    }

    #[test]
    fn in_place_merge_re_renders_only_the_dirtied_tail() {
        // Exploration items coalesce into the last one in place. That mutation
        // must invalidate only the merged item's cached rows, and the result
        // must still match a full re-render.
        let mut state = VisualCanvasState::default();
        state.push_finalized(TranscriptItem::UserMessage("lead".to_owned()));
        state.push_finalized(TranscriptItem::Exploration {
            summaries: vec!["read a.rs".to_owned()],
        });
        render_incremental(&mut state, 80);

        // A second exploration merges into the first (no new item appended).
        let rendered = std::cell::Cell::new(0usize);
        state.push_finalized(TranscriptItem::Exploration {
            summaries: vec!["read b.rs".to_owned()],
        });
        state.render(bare_snapshot(80), |entries, render_from, width| {
            rendered.set(rendered.get() + (entries.len() - render_from));
            fake_render(entries, render_from, width)
        });
        assert_eq!(
            rendered.get(),
            1,
            "merge re-renders only the mutated exploration, not the lead message"
        );

        let merged = render_incremental(&mut state, 80);
        let full = full_render(
            &[
                TranscriptItem::UserMessage("lead".to_owned()),
                TranscriptItem::Exploration {
                    summaries: vec!["read a.rs".to_owned()],
                },
                TranscriptItem::Exploration {
                    summaries: vec!["read b.rs".to_owned()],
                },
            ],
            80,
        );
        assert_eq!(merged.active_frame_lines(), full.active_frame_lines());
        assert_eq!(merged.history_item_offsets, full.history_item_offsets);
    }

    #[test]
    fn committed_boundary_prefix_is_untouched_by_later_appends() {
        // Rows already committed to native scrollback must never change. After
        // advancing the committed boundary, appending more items may only add
        // rows below it — the committed prefix stays byte-identical.
        let mut state = VisualCanvasState::default();
        state.push_finalized(TranscriptItem::UserMessage("committed-1".to_owned()));
        state.push_finalized(TranscriptItem::UserMessage("committed-2".to_owned()));
        let committed_frame = render_incremental(&mut state, 80);
        let committed_items = 2;
        state.set_committed_items(committed_items);
        let committed_rows = committed_frame.history_item_offsets[committed_items - 1];

        state.push_finalized(TranscriptItem::AssistantMessage("later".to_owned()));
        let after = render_incremental(&mut state, 80);

        assert_eq!(
            after.active_frame_lines()[..committed_rows],
            committed_frame.active_frame_lines()[..committed_rows],
            "committed prefix rows must be byte-identical after later appends"
        );
        assert!(after.active_frame_lines().len() > committed_rows);
    }

    #[test]
    fn committed_notice_seam_keeps_its_blank_and_commits_the_next_notice_once() {
        // Regression (review items 1-3): Notice A is rendered and committed to
        // native scrollback — its trailing blank included. A later Notice B is
        // appended. A fresh full render stacks consecutive notices with no
        // blank between them, so the naive incremental path retracted Notice
        // A's trailing blank. But A's rows are already physically in scrollback
        // and cannot be rewritten: the committed prefix (lines AND offsets)
        // must stay byte-identical, and Notice B must commit exactly once,
        // starting right where Notice A ended.
        let mut state = VisualCanvasState::default();
        state.push_finalized(TranscriptItem::AssistantMessage("prose".to_owned()));
        state.push_finalized(TranscriptItem::Notice("A".to_owned()));
        let committed_frame = render_incremental(&mut state, 80);

        // Commit both items to native scrollback (prose + Notice A, blank and
        // all), then advance the canvas's committed boundary to match.
        let committed_items = 2;
        state.set_committed_items(committed_items);
        let committed_rows = committed_frame.history_item_offsets[committed_items - 1];
        assert_eq!(
            committed_frame.active_frame_lines()[committed_rows - 1].text(),
            "",
            "Notice A's committed tail ends in its trailing blank"
        );

        // Append Notice B and re-render incrementally.
        state.push_finalized(TranscriptItem::Notice("B".to_owned()));
        let after = render_incremental(&mut state, 80);

        // 1) The committed prefix — lines and offsets — is byte-identical.
        //    Notice A's trailing blank is NOT retracted, even though a fresh
        //    full render would drop it: scrollback consistency wins over the
        //    notice-run rhythm rule above the committed boundary.
        assert_eq!(
            after.active_frame_lines()[..committed_rows],
            committed_frame.active_frame_lines()[..committed_rows],
            "committed prefix rows (incl. Notice A's blank) must be unchanged"
        );
        assert_eq!(
            after.history_item_offsets[..committed_items],
            committed_frame.history_item_offsets[..committed_items],
            "committed item offsets must be unchanged"
        );
        assert_eq!(
            after.active_frame_lines()[committed_rows - 1].text(),
            "",
            "Notice A keeps its committed trailing blank after Notice B lands"
        );

        // 2) Terminal-side commit accounting (terminal.rs
        //    `set_committed_active_rows` / `commit_scrolled_history`): the
        //    already-committed row count is `committed_rows`; the next draw
        //    commits the slice [committed_rows .. history_rows). It must start
        //    exactly at the boundary (no re-emit, no gap) and cover Notice B
        //    exactly once — partition_point advances by one whole item.
        let new_commit_start = committed_rows;
        let new_commit_end = after.history_rows;
        assert_eq!(
            after.history_item_offsets[committed_items - 1],
            new_commit_start,
            "Notice A still ends exactly at the committed boundary row"
        );
        assert_eq!(
            after.history_item_offsets[committed_items], new_commit_end,
            "Notice B is the only item past the boundary; its end is the new tail"
        );
        let items_before = committed_frame
            .history_item_offsets
            .partition_point(|end| *end <= new_commit_start);
        let items_after = after
            .history_item_offsets
            .partition_point(|end| *end <= new_commit_end);
        assert_eq!(items_before, committed_items);
        assert_eq!(
            items_after,
            committed_items + 1,
            "Notice B commits exactly once (partition_point advances by one item)"
        );
        assert!(
            new_commit_end > new_commit_start,
            "Notice B contributes at least one newly committed row"
        );
    }

    #[test]
    fn mixed_sequence_incremental_matches_full_render() {
        // A stronger end-to-end invariant: render after every push and require
        // the final incremental frame to equal a from-scratch full render of
        // the same (post-merge) sequence.
        let pushes = vec![
            TranscriptItem::UserMessage("start the task".to_owned()),
            TranscriptItem::AssistantMessage("on it".to_owned()),
            TranscriptItem::Exploration {
                summaries: vec!["read main.rs".to_owned()],
            },
            TranscriptItem::Exploration {
                summaries: vec!["read lib.rs".to_owned()],
            },
            TranscriptItem::Notice("secret redacted".to_owned()),
            TranscriptItem::Notice("secret redacted again".to_owned()),
            TranscriptItem::AssistantMessage("done".to_owned()),
        ];
        let mut state = VisualCanvasState::default();
        for item in &pushes {
            state.push_finalized(item.clone());
            render_incremental(&mut state, 72);
        }
        let incremental = render_incremental(&mut state, 72);
        let full = full_render(&pushes, 72);
        assert_eq!(incremental.active_frame_lines(), full.active_frame_lines());
        assert_eq!(incremental.history_item_offsets, full.history_item_offsets);
    }

    #[test]
    fn frames_share_the_cached_history_allocation_instead_of_copying_it() {
        let mut state = VisualCanvasState::default();
        for index in 0..64 {
            state.push_finalized(TranscriptItem::UserMessage(format!("m{index}")));
        }
        let first = render_incremental(&mut state, 80);

        // Idle repaint (spinner tick, keystroke) must reuse the allocation.
        let second = render_incremental(&mut state, 80);
        assert!(
            Arc::ptr_eq(&second.history_lines, &first.history_lines),
            "an idle frame must reuse the cached allocation verbatim"
        );
        let allocation_before_append = Arc::as_ptr(&second.history_lines);

        // Production drops every prior frame before the next render. With the
        // cache then uniquely owned, Arc::make_mut must retain this exact Arc
        // allocation across an incremental append. Holding an Arc clone here
        // would force copy-on-write and make this assertion meaningless.
        drop(first);
        drop(second);
        state.push_finalized(TranscriptItem::UserMessage("appended".to_owned()));
        let after = render_incremental(&mut state, 80);
        assert_eq!(
            Arc::as_ptr(&after.history_lines),
            allocation_before_append,
            "an incremental append must retain the cached Arc allocation"
        );
        assert_eq!(after.history_rows, after.history_lines.len());
        assert!(
            after.tail_lines.is_empty(),
            "history rows must never be rematerialized into the frame tail"
        );
    }

    fn derive_test_frame(
        snapshot: &VisualCanvasSnapshot,
        history_lines: Vec<CanvasLine>,
    ) -> VisualCanvasFrame {
        derive_frame_owned(
            Arc::new(history_lines),
            snapshot.blocks.clone(),
            snapshot.focus,
        )
    }

    fn snapshot_with_blocks(blocks: Vec<VisualBlock>) -> VisualCanvasSnapshot {
        VisualCanvasSnapshot::new(
            80,
            blocks,
            status_snapshot(),
            composer_snapshot(""),
            FocusOwner::Modal,
        )
    }

    fn status_snapshot() -> CanvasStatusSnapshot {
        CanvasStatusSnapshot::new("fixture/model", CanvasLine::plain("status"))
    }

    fn composer_snapshot(draft: &str) -> CanvasComposerSnapshot {
        CanvasComposerSnapshot::new(draft, vec![CanvasLine::plain(draft)], None)
    }

    fn line_texts(lines: &[CanvasLine]) -> Vec<String> {
        lines.iter().map(CanvasLine::text).collect()
    }
}
