use anyhow::{anyhow, bail, Result};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute, queue,
    style::{
        Attribute, Color as CrosstermColor, Print, ResetColor, SetAttribute, SetBackgroundColor,
        SetForegroundColor,
    },
    terminal::{disable_raw_mode, enable_raw_mode, size, Clear, ClearType},
};
use ratatui::{
    backend::Backend,
    layout::{Position, Rect, Size},
    style::{Color as RatatuiColor, Modifier, Style},
    Terminal, TerminalOptions, Viewport,
};
#[cfg(test)]
use ratatui::{
    text::{Line, Span},
    Frame,
};
use signal_hook::{consts::signal, flag, low_level, SigId};
use std::{
    fmt::Write as _,
    io::{self, Stdout, Write},
    panic::{self, PanicHookInfo},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LazyLock, Mutex, Once,
    },
};

use super::history_insert::{emit_history_lines, plan_history_insert};
use super::metrics;
use super::text::display_width;
use super::theme::USER_RAIL_COLOR;
use super::visual_canvas::{CanvasLine, CanvasSpan, CursorTarget, TextRole, VisualCanvasFrame};

static TERMINAL_OWNER: AtomicBool = AtomicBool::new(false);
static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK: Once = Once::new();
static SIGNAL_BRIDGE: Mutex<Option<SignalBridgeIds>> = Mutex::new(None);
static SIGINT_PENDING: LazyLock<Arc<AtomicBool>> =
    LazyLock::new(|| Arc::new(AtomicBool::new(false)));
static SIGTERM_PENDING: LazyLock<Arc<AtomicBool>> =
    LazyLock::new(|| Arc::new(AtomicBool::new(false)));

const SYNC_UPDATE_START: &[u8] = b"\x1b[?2026h";
const SYNC_UPDATE_END: &[u8] = b"\x1b[?2026l";

pub struct TerminalSession {
    _signals: SignalBridgeGuard,
    _owner: TerminalOwnerGuard,
}

impl TerminalSession {
    pub fn enter() -> Result<Self> {
        let owner = acquire_terminal_owner()?;
        install_panic_restore_hook();
        // NO_COLOR (no-color.org) targets plain text output with decorative
        // color; in the full-screen TUI color carries interaction semantics
        // (selection, status, diffs), and crossterm's global NO_COLOR honor
        // would render the canvas illegible. Force color for the interactive
        // surface only; headless/exec output paths are unaffected.
        crossterm::style::force_color_output(true);
        let signals = match install_signal_bridge() {
            Ok(signals) => signals,
            Err(error) => {
                drop(owner);
                return Err(error);
            }
        };
        if let Err(error) = enable_raw_mode() {
            drop(signals);
            drop(owner);
            return Err(error.into());
        }
        if let Err(error) = enable_terminal_session_modes(&mut io::stdout()) {
            let _ = disable_raw_mode();
            drop(signals);
            drop(owner);
            return Err(error.into());
        }
        TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
        Ok(Self {
            _signals: signals,
            _owner: owner,
        })
    }

    pub fn ratatui_terminal(
        &self,
    ) -> Result<InlineTerminal<ratatui::backend::CrosstermBackend<FrameBufferedStdout>>> {
        let (_, rows) = size()?;
        let height = rows.max(1);
        Ok(InlineTerminal::new(
            ratatui::backend::CrosstermBackend::new(FrameBufferedStdout::new()),
            height,
        )?)
    }
}

/// Frame-granular stdout writer: buffers until an explicit flush so one
/// repaint reaches the terminal as one write, never auto-flushing mid-frame
/// the way a fixed-capacity `BufWriter` or line-buffered stdout would. On
/// drop it discards instead of flushing when the terminal session is already
/// restored: buffered paint bytes can contain an open DEC 2026 guard, and
/// writing them after restore closed that guard would re-freeze the terminal.
pub struct FrameBufferedStdout {
    buffer: Vec<u8>,
    out: Stdout,
}

impl FrameBufferedStdout {
    fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(64 * 1024),
            out: io::stdout(),
        }
    }
}

impl Write for FrameBufferedStdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            self.out.write_all(&self.buffer)?;
            self.buffer.clear();
        }
        self.out.flush()
    }
}

impl Drop for FrameBufferedStdout {
    fn drop(&mut self) {
        if TERMINAL_ACTIVE.load(Ordering::SeqCst) {
            let _ = self.flush();
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        restore_terminal();
    }
}

#[derive(Debug)]
struct TerminalOwnerGuard;

impl Drop for TerminalOwnerGuard {
    fn drop(&mut self) {
        TERMINAL_OWNER.store(false, Ordering::Release);
    }
}

fn acquire_terminal_owner() -> Result<TerminalOwnerGuard> {
    match TERMINAL_OWNER.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed) {
        Ok(false) => Ok(TerminalOwnerGuard),
        _ => bail!("terminal session is already active"),
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct SignalBridgeIds {
    sigint: SigId,
    sigterm: SigId,
}

#[cfg(not(unix))]
#[derive(Debug)]
struct SignalBridgeIds;

#[derive(Debug)]
struct SignalBridgeGuard;

impl Drop for SignalBridgeGuard {
    fn drop(&mut self) {
        unregister_signal_bridge();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingSignal {
    Interrupt,
    Terminate,
}

pub fn take_pending_signal() -> Option<PendingSignal> {
    if SIGTERM_PENDING.swap(false, Ordering::SeqCst) {
        Some(PendingSignal::Terminate)
    } else if SIGINT_PENDING.swap(false, Ordering::SeqCst) {
        Some(PendingSignal::Interrupt)
    } else {
        None
    }
}

pub fn restore_terminal() {
    if TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = restore_terminal_session_modes(&mut io::stdout());
        let _ = disable_raw_mode();
    }
}

pub fn suspend_for_external_command<T>(run: impl FnOnce() -> T) -> Result<T> {
    let was_active = TERMINAL_ACTIVE.swap(false, Ordering::SeqCst);
    if was_active {
        let mode_restore = restore_terminal_session_modes(&mut io::stdout());
        let raw_restore = disable_raw_mode();
        mode_restore?;
        raw_restore?;
    }
    let result = run();
    if was_active {
        enable_raw_mode()?;
        if let Err(error) = enable_terminal_session_modes(&mut io::stdout()) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
    }
    Ok(result)
}

fn enable_terminal_session_modes(output: &mut impl Write) -> io::Result<()> {
    execute!(
        output,
        EnableBracketedPaste,
        EnableFocusChange,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        Hide
    )
}

fn restore_terminal_session_modes(output: &mut impl Write) -> io::Result<()> {
    execute!(
        output,
        PopKeyboardEnhancementFlags,
        DisableFocusChange,
        DisableBracketedPaste,
        Show
    )?;
    // Close any dangling synchronized-update guard (panic mid-replay) before
    // resetting the cursor color; a held guard would freeze the terminal.
    output.write_all(SYNC_UPDATE_END)?;
    output.write_all(b"\x1b]112\x07")?;
    output.flush()
}

pub(crate) struct InlineTerminal<B>
where
    // Euler owns an inline, native-scrollback terminal surface. Keep this
    // wrapper intentionally limited to crossterm-style I/O backends so terminal
    // mode restoration, direct writes, and Ratatui backend errors share one
    // error type.
    B: Backend<Error = io::Error> + Write,
{
    inner: Terminal<B>,
    viewport_area: Rect,
    last_known_screen_size: Size,
    last_reported_resize_size: Size,
    last_known_cursor_pos: Position,
    max_active_height: u16,
    committed_active_rows: usize,
    committed_history_items: usize,
    committed_active_lines: Vec<CanvasLine>,
    committed_active_width: u16,
    pending_stale_rows: Vec<u16>,
    last_background_fill: Option<(Size, Rect, RatatuiColor)>,
    last_drawn_area: Option<Rect>,
    last_drawn_lines: Vec<CanvasLine>,
    review_scroll_offset: usize,
    linefeed_history_insert_enabled: bool,
    linefeed_history_insert_suspended_after_resize: bool,
    cursor_position_authoritative: bool,
    foreground: RatatuiColor,
    background: RatatuiColor,
    cursor: RatatuiColor,
}

impl<B> InlineTerminal<B>
where
    B: Backend<Error = io::Error> + Write,
{
    pub(crate) fn new(mut backend: B, max_active_height: u16) -> io::Result<Self> {
        let screen_size = backend.size()?;
        let viewport_area = claim_visible_startup_area(&mut backend, screen_size)?;
        let inner = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(viewport_area),
            },
        )?;
        Ok(Self {
            inner,
            viewport_area,
            last_known_screen_size: screen_size,
            last_reported_resize_size: screen_size,
            last_known_cursor_pos: Position::ORIGIN,
            max_active_height: max_active_height.max(1),
            committed_active_rows: 0,
            committed_history_items: 0,
            committed_active_lines: Vec::new(),
            committed_active_width: screen_size.width,
            pending_stale_rows: Vec::new(),
            last_background_fill: None,
            last_drawn_area: None,
            last_drawn_lines: Vec::new(),
            review_scroll_offset: 0,
            linefeed_history_insert_enabled: false,
            linefeed_history_insert_suspended_after_resize: false,
            cursor_position_authoritative: false,
            foreground: RatatuiColor::Reset,
            background: RatatuiColor::Reset,
            cursor: RatatuiColor::Reset,
        })
    }

    #[cfg(test)]
    pub(crate) fn draw<F>(&mut self, render_callback: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame<'_>),
    {
        self.autoresize()?;
        self.inner.resize(self.viewport_area)?;
        self.inner.draw(render_callback)?;
        self.invalidate_draw_cache();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn write_finalized_lines(&mut self, lines: &[CanvasLine]) -> io::Result<()> {
        self.write_finalized_lines_preserving_bottom_band(lines, None)
    }

    #[cfg(test)]
    fn write_finalized_lines_preserving_bottom_band(
        &mut self,
        lines: &[CanvasLine],
        bottom_band_rows: Option<u16>,
    ) -> io::Result<()> {
        self.write_finalized_lines_with_bridge_policy(lines, bottom_band_rows, true)
    }

    fn write_finalized_lines_with_bridge_policy(
        &mut self,
        lines: &[CanvasLine],
        bottom_band_rows: Option<u16>,
        allow_bridge: bool,
    ) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        self.autoresize()?;
        // Only the live viewport is mutable. Finalized output is written once
        // after clearing that owned active frame, never by rewriting scrollback.
        let width = usize::from(self.viewport_area.width).max(1);
        let wrapped_lines = wrap_canvas_lines(lines, width);
        if allow_bridge
            && self.try_write_finalized_lines_with_bridge(&wrapped_lines, bottom_band_rows)?
        {
            return Ok(());
        }
        let area = self.viewport_area;
        let writer = self.inner.backend_mut();
        queue_clear_area(writer, area, self.background)?;
        queue!(writer, MoveTo(0, self.viewport_area.top()))?;
        for line in &wrapped_lines {
            write_canvas_row(writer, Some(line), width, self.foreground, self.background)?;
            queue!(writer, Print("\r\n"))?;
        }
        queue!(writer, SetAttribute(Attribute::Reset), ResetColor)?;
        flush_terminal_writer(writer)?;

        let screen_size = self.inner.size()?;
        let cursor_pos = self
            .queried_cursor_position()
            .unwrap_or(Position::new(0, self.viewport_area.top()));
        self.last_known_screen_size = screen_size;
        self.last_reported_resize_size = screen_size;
        self.last_known_cursor_pos = cursor_pos;
        let height = self
            .viewport_area
            .height
            .max(1)
            .min(screen_size.height.max(1));
        self.invalidate_draw_cache();
        self.set_viewport_area(Rect::new(0, cursor_pos.y, screen_size.width, height))
    }

    fn try_write_finalized_lines_with_bridge(
        &mut self,
        wrapped_lines: &[CanvasLine],
        bottom_band_rows: Option<u16>,
    ) -> io::Result<bool> {
        if !self.linefeed_history_insert_enabled {
            return Ok(false);
        }
        let screen_size = self.inner.size()?;
        let commit_rows = u16::try_from(wrapped_lines.len()).ok();
        let bottom_band_rows = bottom_band_rows
            .unwrap_or_else(|| screen_size.height.saturating_sub(self.viewport_area.top()));
        let Some(plan) = commit_rows
            .and_then(|rows| plan_history_insert(screen_size.height, bottom_band_rows, rows))
        else {
            return Ok(false);
        };
        let cursor_pos = if std::mem::take(&mut self.cursor_position_authoritative) {
            // A history replay just parked the cursor at a known position
            // with the clear bytes still buffered. Querying now would flush
            // the bare clear and block mid-guard on the DSR round-trip,
            // exposing a blank screen on terminals without DEC 2026.
            self.last_known_cursor_pos
        } else {
            self.queried_cursor_position()
                .unwrap_or(self.last_known_cursor_pos)
        };
        let writer = self.inner.backend_mut();
        let width = usize::from(screen_size.width).max(1);
        emit_history_lines(writer, plan, wrapped_lines, |writer, line| {
            write_canvas_row(writer, Some(line), width, self.foreground, self.background)
        })?;
        queue!(
            writer,
            SetAttribute(Attribute::Reset),
            ResetColor,
            MoveTo(cursor_pos.x, cursor_pos.y)
        )?;
        flush_terminal_writer(writer)?;
        self.last_known_screen_size = screen_size;
        self.last_reported_resize_size = screen_size;
        self.last_known_cursor_pos = cursor_pos;
        self.invalidate_draw_cache();
        Ok(true)
    }

    pub(crate) fn draw_visual_frame(&mut self, frame: &VisualCanvasFrame) -> io::Result<()> {
        let desired_height = self.frame_desired_height(frame)?;
        let mut visible_height = self.resize_active_height(desired_height)?;
        if !frame.prefer_stable_height && self.commit_scrolled_history(frame, visible_height)? {
            visible_height = self.resize_active_height(frame.required_height)?;
        }
        let visible = visible_active_lines(
            &frame.active_frame_lines,
            usize::from(visible_height),
            self.review_scroll_offset,
            frame.pinned_rows,
        );
        let cursor = visible_cursor(frame.cursor, &visible);
        self.draw_canvas_lines(&visible.lines, cursor)
    }

    fn commit_scrolled_history(
        &mut self,
        frame: &VisualCanvasFrame,
        visible_height: u16,
    ) -> io::Result<bool> {
        if frame.history_rows == 0 {
            self.linefeed_history_insert_suspended_after_resize = false;
        }
        if self.review_scroll_offset > 0 {
            return Ok(false);
        }
        let tail_visible = visible_active_lines(
            &frame.active_frame_lines,
            usize::from(visible_height),
            0,
            frame.pinned_rows,
        );
        let commit_until = tail_visible.prefix_start.min(frame.committable_rows);
        let width = self.viewport_area.width;
        if self.committed_active_width != width {
            if self.committed_active_rows > 0 && frame.history_rows > 0 {
                // Native scrollback cannot be reflowed after a terminal
                // resize; the rows already emitted stay as they were. Remap
                // our accounting to the same *items* re-rendered at the new
                // width: rows for still-uncommitted items are re-derived and
                // will commit later exactly once. (Previously this branch
                // declared the whole rendered history "represented", which
                // silently dropped never-emitted rows — or, in reflowing
                // terminals, re-emitted rows that were already visible: the
                // duplicate-line audit finding, P1.)
                if frame.history_item_offsets.is_empty() {
                    // No item accounting available (history not derived from
                    // finalized items). Fall back to treating the rendered
                    // prefix as represented — accepts losing never-emitted
                    // rows rather than re-emitting everything.
                    let shared_prefix = shared_committed_prefix_len(
                        &self.committed_active_lines,
                        &frame.active_frame_lines,
                    );
                    let represented_rows = frame.history_rows.max(shared_prefix);
                    self.set_committed_active_rows(frame, represented_rows);
                } else {
                    // Remap the committed boundary by item identity, rendered
                    // at the new width. Rounding down to the item boundary
                    // can re-emit the head rows of one partially-committed
                    // item — bounded, and preferable to losing rows (or, as
                    // before this fix, re-committing the entire history).
                    let remapped_rows = if self.committed_history_items == 0 {
                        0
                    } else {
                        frame
                            .history_item_offsets
                            .get(self.committed_history_items - 1)
                            .copied()
                            .unwrap_or(0)
                            .min(frame.history_rows)
                    };
                    self.set_committed_active_rows(frame, remapped_rows);
                }
                self.linefeed_history_insert_suspended_after_resize = true;
            }
            self.committed_active_width = width;
        }
        if commit_until <= self.committed_active_rows {
            return Ok(false);
        }
        let start = self
            .committed_active_rows
            .min(frame.active_frame_lines.len());
        let end = commit_until.min(frame.active_frame_lines.len());
        let was_suspended_after_resize = self.linefeed_history_insert_suspended_after_resize;
        if start < end
            && was_suspended_after_resize
            && self.linefeed_history_insert_enabled
            && canvas_lines_are_blank(&frame.active_frame_lines[start..end])
        {
            return Ok(false);
        }
        if start < end {
            let bottom_band_rows = tail_visible.visible_pinned_bottom_band_rows(
                &frame.active_frame_lines,
                usize::from(width).max(1),
            );
            self.write_finalized_lines_with_bridge_policy(
                &frame.active_frame_lines[start..end],
                bottom_band_rows,
                !was_suspended_after_resize,
            )?;
        }
        self.set_committed_active_rows(frame, commit_until);
        if was_suspended_after_resize && start < end {
            self.linefeed_history_insert_suspended_after_resize = false;
        }
        Ok(start < end)
    }

    /// Whole finalized history items whose rows are committed to native
    /// scrollback. Fed back to the visual canvas after each draw.
    pub(crate) fn committed_history_items(&self) -> usize {
        self.committed_history_items
    }

    fn set_committed_active_rows(&mut self, frame: &VisualCanvasFrame, rows: usize) {
        self.committed_active_rows = rows.min(frame.active_frame_lines.len());
        // Track how many whole finalized items the committed rows cover; this
        // is the width-independent identity used for resize remapping and for
        // the canvas's mutate-above-the-boundary guard.
        self.committed_history_items = frame
            .history_item_offsets
            .partition_point(|end| *end <= self.committed_active_rows);
        self.committed_active_lines = frame
            .active_frame_lines
            .iter()
            .take(self.committed_active_rows)
            .cloned()
            .collect();
    }

    pub(crate) fn set_review_scroll_offset(&mut self, offset: usize) {
        self.review_scroll_offset = offset;
    }

    pub(crate) fn set_theme_colors(
        &mut self,
        foreground: RatatuiColor,
        background: RatatuiColor,
        cursor: RatatuiColor,
    ) -> io::Result<()> {
        if self.foreground != foreground || self.background != background {
            self.foreground = foreground;
            self.background = background;
            self.last_background_fill = None;
            self.invalidate_draw_cache();
        }
        if self.cursor != cursor {
            self.cursor = cursor;
            if let Some(sequence) = terminal_cursor_color_sequence(cursor) {
                self.write_terminal_sequence(&sequence)?;
            }
        }
        Ok(())
    }

    pub(crate) fn set_linefeed_history_insert_enabled(&mut self, enabled: bool) {
        self.linefeed_history_insert_enabled = enabled;
    }

    pub(crate) fn write_terminal_sequence(&mut self, sequence: &str) -> io::Result<()> {
        let writer = self.inner.backend_mut();
        writer.write_all(sequence.as_bytes())?;
        flush_terminal_writer(writer)
    }

    pub(crate) fn suspend_linefeed_history_insert_after_resize(&mut self) {
        self.linefeed_history_insert_suspended_after_resize = true;
    }

    /// Open a DEC 2026 synchronized-update region. Callers must pair this
    /// with `end_synchronized_update`, including on error paths; the
    /// terminal-restore path also closes a dangling guard defensively.
    pub(crate) fn begin_synchronized_update(&mut self) -> io::Result<()> {
        self.inner.backend_mut().write_all(SYNC_UPDATE_START)
    }

    pub(crate) fn end_synchronized_update(&mut self) -> io::Result<()> {
        let writer = self.inner.backend_mut();
        writer.write_all(SYNC_UPDATE_END)?;
        flush_terminal_writer(writer)
    }

    /// A failed replay leaves the cursor in an unknown state; drop the
    /// replay-parked position so the next bridge commit queries instead.
    pub(crate) fn invalidate_cursor_position_authority(&mut self) {
        self.cursor_position_authoritative = false;
    }

    pub(crate) fn reset_for_history_replay(&mut self, purge_scrollback: bool) -> io::Result<()> {
        metrics::record(metrics::Metric::HistoryReplay);
        if purge_scrollback {
            metrics::record(metrics::Metric::ScrollbackPurge);
        }
        let screen_size = self.inner.size()?;
        let writer = self.inner.backend_mut();
        queue_span_style(writer, Style::default(), self.foreground, self.background)?;
        if purge_scrollback {
            queue!(
                writer,
                Hide,
                MoveTo(0, 0),
                Clear(ClearType::All),
                Clear(ClearType::Purge)
            )?;
        } else {
            queue!(writer, Hide, MoveTo(0, 0), Clear(ClearType::FromCursorDown))?;
        }

        self.viewport_area = Rect::new(0, 0, screen_size.width, screen_size.height.min(1));
        self.last_known_screen_size = screen_size;
        self.last_reported_resize_size = screen_size;
        self.last_known_cursor_pos = Position::ORIGIN;
        self.cursor_position_authoritative = true;
        self.committed_active_rows = 0;
        self.committed_history_items = 0;
        self.committed_active_lines.clear();
        self.committed_active_width = screen_size.width;
        self.pending_stale_rows.clear();
        self.last_background_fill = None;
        self.review_scroll_offset = 0;
        self.linefeed_history_insert_suspended_after_resize = false;
        self.invalidate_draw_cache();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    pub(crate) fn active_width(&mut self) -> io::Result<u16> {
        self.autoresize()?;
        Ok(self.viewport_area.width.max(1))
    }

    pub(crate) fn note_resize_event(&mut self, width: u16, height: u16) {
        self.last_reported_resize_size = Size::new(width, height);
    }

    pub(crate) fn observed_size_change(&mut self) -> io::Result<Option<Size>> {
        let screen_size = self.inner.size()?;
        if screen_size == self.last_known_screen_size {
            self.last_reported_resize_size = screen_size;
            return Ok(None);
        }
        if screen_size == self.last_reported_resize_size {
            return Ok(None);
        }
        self.last_reported_resize_size = screen_size;
        Ok(Some(screen_size))
    }

    #[cfg(test)]
    pub(crate) fn backend(&self) -> &B {
        self.inner.backend()
    }

    #[cfg(test)]
    pub(crate) fn backend_mut(&mut self) -> &mut B {
        self.inner.backend_mut()
    }

    #[cfg(test)]
    pub(crate) fn viewport_area(&self) -> Rect {
        self.viewport_area
    }

    fn queried_cursor_position(&mut self) -> io::Result<Position> {
        // The DSR round-trip bypasses the buffered writer, so queued bytes
        // must reach the terminal before the query for the response to
        // reflect them.
        let writer = self.inner.backend_mut();
        Write::flush(writer)?;
        writer.get_cursor_position()
    }

    fn set_viewport_area(&mut self, area: Rect) -> io::Result<()> {
        self.viewport_area = area;
        Ok(())
    }

    fn frame_desired_height(&mut self, frame: &VisualCanvasFrame) -> io::Result<u16> {
        if !frame.prefer_stable_height {
            return Ok(frame.required_height);
        }
        let screen_size = self.inner.size()?;
        if self
            .viewport_area
            .top()
            .saturating_add(frame.required_height)
            <= screen_size.height
        {
            return Ok(frame.required_height);
        }
        Ok(self
            .viewport_area
            .height
            .max(1)
            .min(screen_size.height.max(1)))
    }

    pub(crate) fn resize_active_height(&mut self, desired_height: u16) -> io::Result<u16> {
        let screen_size = self.inner.size()?;
        if screen_size.height == 0 || screen_size.width == 0 {
            return Ok(0);
        }

        let height = desired_height
            .max(1)
            .min(self.max_active_height)
            .min(screen_size.height);

        let old_area = self.viewport_area;
        let mut top = old_area.top().min(screen_size.height.saturating_sub(1));
        let mut scrolled = false;
        if top.saturating_add(height) > screen_size.height {
            let scroll_rows = top
                .saturating_add(height)
                .saturating_sub(screen_size.height);
            self.scroll_terminal_for_live_area(scroll_rows)?;
            top = top.saturating_sub(scroll_rows);
            scrolled = true;
        }
        let area = Rect::new(0, top, screen_size.width, height);
        self.last_known_screen_size = screen_size;
        self.last_reported_resize_size = screen_size;
        if area == old_area {
            return Ok(height);
        }

        // Native scrolling moves physical rows underneath identical logical
        // spacer rows, so stale rows must be repainted with current theme colors.
        self.invalidate_draw_cache();
        if !scrolled {
            self.pending_stale_rows.extend(stale_rows_after_resize(
                old_area,
                area,
                screen_size.height,
            ));
        }
        self.set_viewport_area(area)?;
        Ok(height)
    }

    fn autoresize(&mut self) -> io::Result<()> {
        let screen_size = self.inner.size()?;
        if screen_size == self.last_known_screen_size {
            return Ok(());
        }
        self.resize_active_height(self.viewport_area.height)?;
        self.last_known_screen_size = screen_size;
        Ok(())
    }

    fn invalidate_draw_cache(&mut self) {
        self.last_drawn_area = None;
        self.last_drawn_lines.clear();
    }

    fn draw_canvas_lines(
        &mut self,
        lines: &[CanvasLine],
        cursor: Option<CursorTarget>,
    ) -> io::Result<()> {
        let area = self.viewport_area;
        let screen_size = self.inner.size()?;
        let background_fill_key = (screen_size, area, self.background);
        let repaint_screen_background = self.last_background_fill != Some(background_fill_key);
        let last_area = self.last_drawn_area;
        let last_lines = self.last_drawn_lines.clone();
        let writer = self.inner.backend_mut();
        queue!(writer, Hide)?;
        for row in 0..area.height {
            let index = usize::from(row);
            let current = lines.get(index);
            let previous = last_lines.get(index);
            if last_area == Some(area) && current == previous {
                continue;
            }
            queue!(writer, MoveTo(area.x, area.y.saturating_add(row)))?;
            write_canvas_row(
                writer,
                current,
                usize::from(area.width),
                self.foreground,
                self.background,
            )?;
        }
        if repaint_screen_background && self.committed_active_rows == 0 {
            queue_clear_inactive_tail(writer, screen_size, area, self.background)?;
            self.last_background_fill = Some(background_fill_key);
        }
        queue!(writer, SetAttribute(Attribute::Reset), ResetColor)?;
        for row in self.pending_stale_rows.drain(..) {
            if row >= area.top() && row < area.bottom() {
                continue;
            }
            queue!(writer, MoveTo(0, row))?;
            queue_clear_until_new_line(writer, self.background)?;
        }
        if let Some(cursor) = cursor.filter(|cursor| {
            cursor.row < area.height
                && cursor.column < area.width
                && area.y.saturating_add(cursor.row) < area.bottom()
        }) {
            queue!(
                writer,
                MoveTo(
                    area.x.saturating_add(cursor.column),
                    area.y.saturating_add(cursor.row)
                ),
                Show
            )?;
        } else {
            queue!(writer, Hide)?;
        }
        flush_terminal_writer(writer)?;
        self.last_drawn_area = Some(area);
        self.last_drawn_lines = lines
            .iter()
            .take(usize::from(area.height))
            .cloned()
            .collect();
        // The draw moved the physical cursor; a later bridge commit must
        // query instead of trusting a replay-parked position.
        self.cursor_position_authoritative = false;
        Ok(())
    }

    fn scroll_terminal_for_live_area(&mut self, rows: u16) -> io::Result<()> {
        if rows == 0 {
            return Ok(());
        }
        self.cursor_position_authoritative = false;
        let screen_size = self.inner.size()?;
        let writer = self.inner.backend_mut();
        queue!(writer, MoveTo(0, screen_size.height.saturating_sub(1)))?;
        writer.append_lines(rows)
    }
}

mod render;

use render::*;

#[cfg(test)]
mod tests;
