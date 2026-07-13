const ESC: char = '\u{1b}';
use super::transcript::{item_wants_timestamp, parse_event_time, EventTiming, ProjectedEntry};
use super::transcript::{TimingClock, TranscriptItem};
use chrono::Local;
use ratatui::style::Style;
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
                    self.history_cache = None;
                    return;
                }
            }
            self.finalized.push(ProjectedEntry {
                item: TranscriptItem::Exploration { summaries },
                timing,
            });
            self.history_cache = None;
            return;
        }
        if let TranscriptItem::Companion { spawn_event_id, .. } = &item {
            if let Some(entry) = self.finalized[boundary..].iter_mut().rev().find(|entry| {
                entry.item.companion_spawn_event_id() == Some(spawn_event_id.as_str())
            }) {
                let _ = super::transcript::merge_companion_item(&mut entry.item, item);
                if timing.is_some() {
                    entry.timing = timing;
                }
                self.history_cache = None;
                return;
            }
        }
        if let TranscriptItem::FileDiff { path, .. } = &item {
            if let Some(index) = self.finalized[boundary..].iter().rposition(|entry| {
                matches!(&entry.item, TranscriptItem::PatchApplied { path: p, .. } if p == path)
            }) {
                self.finalized.remove(boundary + index);
            }
            if let Some(index) = self.finalized[boundary..].iter().rposition(|entry| {
                matches!(&entry.item, TranscriptItem::FileChange { path: p, .. } if p == path)
            }) {
                self.finalized.remove(boundary + index);
            }
        }
        self.finalized.push(ProjectedEntry { item, timing });
        self.history_cache = None;
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
        mut snapshot: VisualCanvasSnapshot,
        render_finalized: R,
    ) -> VisualCanvasFrame
    where
        R: FnOnce(&[ProjectedEntry], u16) -> (Vec<CanvasLine>, Vec<usize>),
    {
        let (history, offsets) = self.render_history(snapshot.width, render_finalized);
        if !history.is_empty() {
            snapshot
                .blocks
                .insert(0, VisualBlock::new(VisualBlockRole::History, history));
        }
        let mut frame = derive_frame(&snapshot);
        frame.history_item_offsets = offsets;
        frame
    }

    fn render_history<R>(
        &mut self,
        width: u16,
        render_finalized: R,
    ) -> (Vec<CanvasLine>, Vec<usize>)
    where
        R: FnOnce(&[ProjectedEntry], u16) -> (Vec<CanvasLine>, Vec<usize>),
    {
        if let Some(cache) = &self.history_cache {
            if cache.width == width {
                return (cache.lines.clone(), cache.item_end_offsets.clone());
            }
        }
        let (lines, item_end_offsets) = render_finalized(&self.finalized, width);
        self.history_cache = Some(RenderedHistoryCache {
            width,
            lines: lines.clone(),
            item_end_offsets: item_end_offsets.clone(),
        });
        (lines, item_end_offsets)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RenderedHistoryCache {
    width: u16,
    lines: Vec<CanvasLine>,
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
    pub active_frame_lines: Vec<CanvasLine>,
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

pub fn derive_frame(snapshot: &VisualCanvasSnapshot) -> VisualCanvasFrame {
    let mut active_frame_lines = Vec::new();
    let mut cursor = None;
    let mut history_rows = 0;
    let mut committable_rows = 0;
    let mut committable_prefix_open = true;

    for block in &snapshot.blocks {
        if cursor.is_none() && snapshot.focus.allows_cursor(block.role) {
            cursor = block.cursor.map(|block_cursor| CursorTarget {
                row: block_cursor
                    .row
                    .saturating_add(active_row_count_u16(&active_frame_lines)),
                column: block_cursor.column,
            });
        }
        active_frame_lines.extend(block.lines.clone());
        if block.role == VisualBlockRole::History {
            history_rows = active_row_count(&active_frame_lines);
        }
        if committable_prefix_open && is_committable_prefix_role(block.role) {
            committable_rows = active_row_count(&active_frame_lines);
        } else if active_row_count(&block.lines) > 0 {
            committable_prefix_open = false;
        }
    }

    VisualCanvasFrame {
        required_height: line_count_u16(&active_frame_lines),
        pinned_rows: pinned_suffix_rows(&snapshot.blocks),
        active_frame_lines,
        cursor,
        history_rows,
        committable_rows,
        prefer_stable_height: snapshot.focus == FocusOwner::BottomSurface,
        history_item_offsets: Vec::new(),
    }
}

fn is_committable_prefix_role(role: VisualBlockRole) -> bool {
    role == VisualBlockRole::History
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
    History,
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

fn active_row_count_u16(lines: &[CanvasLine]) -> u16 {
    line_count_u16(lines)
}

fn line_count_u16(lines: &[CanvasLine]) -> u16 {
    u16::try_from(lines.len()).unwrap_or(u16::MAX)
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
        let snapshot = snapshot_with_blocks(vec![
            VisualBlock::new(VisualBlockRole::History, vec![CanvasLine::plain("final")]),
            VisualBlock::new(VisualBlockRole::Activity, vec![CanvasLine::plain("active")]),
        ]);

        let frame = derive_frame(&snapshot);

        assert_eq!(
            line_texts(&frame.active_frame_lines),
            vec!["final", "active"]
        );
        assert_eq!(frame.required_height, 2);
        assert_eq!(frame.committable_rows, 1);
    }

    #[test]
    fn live_transcript_rows_stay_after_native_scrollback_commit_boundary() {
        let snapshot = snapshot_with_blocks(vec![
            VisualBlock::new(VisualBlockRole::History, vec![CanvasLine::plain("history")]),
            VisualBlock::new(
                VisualBlockRole::LiveTranscript,
                vec![CanvasLine::plain("live")],
            ),
        ]);

        let frame = derive_frame(&snapshot);

        assert_eq!(
            line_texts(&frame.active_frame_lines),
            vec!["history", "live"]
        );
        assert_eq!(frame.committable_rows, 1);
        assert_eq!(frame.history_rows, 1);
    }

    #[test]
    fn non_prefix_transcript_rows_are_not_committable() {
        let snapshot = snapshot_with_blocks(vec![
            VisualBlock::new(VisualBlockRole::History, vec![CanvasLine::plain("history")]),
            VisualBlock::new(VisualBlockRole::Activity, vec![CanvasLine::plain("tool")]),
            VisualBlock::new(
                VisualBlockRole::LiveTranscript,
                vec![CanvasLine::plain("live")],
            ),
        ]);

        let frame = derive_frame(&snapshot);

        assert_eq!(
            line_texts(&frame.active_frame_lines),
            vec!["history", "tool", "live"]
        );
        assert_eq!(frame.history_rows, 1);
        assert_eq!(frame.committable_rows, 1);
        assert!(frame.committable_rows <= frame.history_rows);
        assert!(frame.history_rows <= frame.active_frame_lines.len());
    }

    #[test]
    fn live_transcript_tail_closes_committable_prefix() {
        let snapshot = snapshot_with_blocks(vec![
            VisualBlock::new(VisualBlockRole::History, vec![CanvasLine::plain("history")]),
            VisualBlock::new(
                VisualBlockRole::LiveTranscript,
                vec![CanvasLine::plain("stable")],
            ),
            VisualBlock::new(
                VisualBlockRole::LiveTranscript,
                vec![CanvasLine::plain("mutable")],
            ),
        ]);

        let frame = derive_frame(&snapshot);

        assert_eq!(
            line_texts(&frame.active_frame_lines),
            vec!["history", "stable", "mutable"]
        );
        assert_eq!(frame.committable_rows, 1);
        assert_eq!(frame.history_rows, 1);
    }

    #[test]
    fn trailing_live_control_stack_is_pinned_suffix() {
        let snapshot = snapshot_with_blocks(vec![
            VisualBlock::new(VisualBlockRole::History, vec![CanvasLine::plain("history")]),
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

        let frame = derive_frame(&snapshot);

        assert_eq!(frame.pinned_rows, 5);
    }

    #[test]
    fn spacer_rows_are_part_of_the_trailing_pinned_suffix() {
        let snapshot = snapshot_with_blocks(vec![
            VisualBlock::new(VisualBlockRole::History, vec![CanvasLine::plain("history")]),
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

        let frame = derive_frame(&snapshot);

        assert_eq!(frame.pinned_rows, 5);
        assert_eq!(
            line_texts(&frame.active_frame_lines),
            vec!["history", "stream", "notice", "", "draft", "", "status"]
        );
    }

    #[test]
    fn pinned_suffix_stops_at_transcript_content() {
        let snapshot = snapshot_with_blocks(vec![
            VisualBlock::new(VisualBlockRole::History, vec![CanvasLine::plain("history")]),
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

        let frame = derive_frame(&snapshot);

        assert_eq!(frame.pinned_rows, 2);
    }

    #[test]
    fn history_inserted_before_focused_block_offsets_cursor() {
        let snapshot = VisualCanvasSnapshot::new(
            80,
            vec![
                VisualBlock::new(
                    VisualBlockRole::History,
                    vec![
                        CanvasLine::plain("history-1"),
                        CanvasLine::plain("history-2"),
                    ],
                ),
                VisualBlock::new(
                    VisualBlockRole::Composer,
                    vec![CanvasLine::styled("draft", TextRole::Prompt)],
                )
                .with_cursor(BlockCursor { row: 0, column: 3 }),
            ],
            status_snapshot(),
            composer_snapshot("draft"),
            FocusOwner::Composer,
        );

        let frame = derive_frame(&snapshot);

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

        let frame = derive_frame(&snapshot);

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
