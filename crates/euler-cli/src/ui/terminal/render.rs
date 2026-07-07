use super::*;
use crate::ui::visual_canvas::CanvasText;

pub(super) fn flush_terminal_writer(writer: &mut impl Write) -> io::Result<()> {
    metrics::record(metrics::Metric::TerminalFlush);
    writer.flush()
}

pub(super) fn claim_visible_startup_area<B>(backend: &mut B, screen_size: Size) -> io::Result<Rect>
where
    B: Backend<Error = io::Error> + Write,
{
    // Start Euler as an app-owned surface, matching Codex's clean launch
    // behavior rather than leaving shell history above the banner.
    queue!(
        backend,
        Clear(ClearType::All),
        Clear(ClearType::Purge),
        MoveTo(0, 0)
    )?;
    flush_terminal_writer(backend)?;
    let height = screen_size.height.min(1);
    Ok(Rect::new(0, 0, screen_size.width, height))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct VisibleActiveLines {
    pub(super) lines: Vec<CanvasLine>,
    pub(super) prefix_start: usize,
    pub(super) prefix_end: usize,
    pub(super) pinned_start: usize,
    pub(super) pinned_visible_start: usize,
}

impl VisibleActiveLines {
    pub(super) fn visible_pinned_bottom_band_rows(
        &self,
        source_lines: &[CanvasLine],
        width: usize,
    ) -> Option<u16> {
        let visible_pinned = source_lines.get(self.pinned_visible_start..)?;
        u16::try_from(wrap_canvas_lines(visible_pinned, width).len()).ok()
    }
}

pub(super) fn visible_active_lines(
    lines: &[CanvasLine],
    height: usize,
    scroll_offset: usize,
    pinned_rows: usize,
) -> VisibleActiveLines {
    let pinned_start = lines.len().saturating_sub(pinned_rows.min(lines.len()));
    if height == 0 {
        return VisibleActiveLines {
            lines: Vec::new(),
            prefix_start: 0,
            prefix_end: 0,
            pinned_start,
            pinned_visible_start: lines.len(),
        };
    }
    if lines.len() <= height {
        return VisibleActiveLines {
            lines: lines.to_vec(),
            prefix_start: 0,
            prefix_end: pinned_start,
            pinned_start,
            pinned_visible_start: pinned_start,
        };
    }

    let visible_pinned_rows = lines.len().saturating_sub(pinned_start).min(height);
    let prefix_height = height.saturating_sub(visible_pinned_rows);
    let max_prefix_start = pinned_start.saturating_sub(prefix_height);
    let prefix_start = max_prefix_start.saturating_sub(scroll_offset.min(max_prefix_start));
    let prefix_end = prefix_start.saturating_add(prefix_height).min(pinned_start);
    let pinned_visible_start = lines.len().saturating_sub(visible_pinned_rows);
    let mut visible =
        Vec::with_capacity(prefix_end.saturating_sub(prefix_start) + visible_pinned_rows);
    visible.extend_from_slice(&lines[prefix_start..prefix_end]);
    visible.extend_from_slice(&lines[pinned_visible_start..]);

    VisibleActiveLines {
        lines: visible,
        prefix_start,
        prefix_end,
        pinned_start,
        pinned_visible_start,
    }
}

pub(super) fn visible_cursor(
    cursor: Option<CursorTarget>,
    visible: &VisibleActiveLines,
) -> Option<CursorTarget> {
    let cursor = cursor?;
    let source_row = usize::from(cursor.row);
    if source_row >= visible.prefix_start && source_row < visible.prefix_end {
        return Some(CursorTarget {
            row: u16::try_from(source_row - visible.prefix_start).ok()?,
            column: cursor.column,
        });
    }
    if source_row >= visible.pinned_visible_start {
        let prefix_rows = visible.prefix_end.saturating_sub(visible.prefix_start);
        return Some(CursorTarget {
            row: u16::try_from(prefix_rows + source_row - visible.pinned_visible_start).ok()?,
            column: cursor.column,
        });
    }
    None
}

pub(super) fn canvas_lines_are_blank(lines: &[CanvasLine]) -> bool {
    lines.iter().all(|line| {
        line.spans
            .iter()
            .all(|span| span.text.as_str().trim().is_empty())
    })
}

pub(super) fn shared_committed_prefix_len(
    committed: &[CanvasLine],
    current: &[CanvasLine],
) -> usize {
    committed
        .iter()
        .zip(current)
        .take_while(|(committed, current)| committed == current)
        .count()
}

pub(super) fn queue_clear_area<W>(
    writer: &mut W,
    area: Rect,
    background: RatatuiColor,
) -> io::Result<()>
where
    W: Write,
{
    for row in area.top()..area.bottom() {
        queue!(writer, MoveTo(area.x, row))?;
        queue_clear_until_new_line(writer, background)?;
    }
    queue!(writer, SetAttribute(Attribute::Reset), ResetColor)
}

pub(super) fn queue_clear_inactive_tail<W>(
    writer: &mut W,
    screen_size: Size,
    active_area: Rect,
    background: RatatuiColor,
) -> io::Result<()>
where
    W: Write,
{
    let start = active_area.bottom().min(screen_size.height);
    let area = Rect::new(
        0,
        start,
        screen_size.width,
        screen_size.height.saturating_sub(start),
    );
    queue_clear_area(writer, area, background)
}

pub(super) fn write_canvas_row<W>(
    writer: &mut W,
    line: Option<&CanvasLine>,
    width: usize,
    foreground: RatatuiColor,
    background: RatatuiColor,
) -> io::Result<()>
where
    W: Write,
{
    // Always materialize every cell in the row. Otherwise xterm-compatible
    // terminals can expose default/stale backgrounds in blank row segments.
    queue_clear_until_new_line(writer, background)?;
    let used_width = match line {
        // Clip the line to the row width before printing: bytes past the
        // last column would auto-wrap, and on the bottom screen row —
        // routine after a terminal resize narrows the screen under existing
        // content — auto-wrap physically scrolls the terminal, pushing rows
        // (banner, transcript) off-screen and corrupting the repaint.
        Some(line) => match wrap_canvas_line(line, width).first() {
            Some(clipped) => write_canvas_line(writer, clipped, foreground, background)?,
            None => 0,
        },
        None => 0,
    };
    queue_fill_row_remainder(writer, width, used_width, foreground, background)
}

pub(super) fn queue_clear_until_new_line<W>(
    writer: &mut W,
    background: RatatuiColor,
) -> io::Result<()>
where
    W: Write,
{
    // Reset before Clear so cleared cells pick up the selected theme background.
    queue!(writer, SetAttribute(Attribute::Reset), ResetColor)?;
    if let Some(bg) = crossterm_color(background) {
        queue!(writer, SetBackgroundColor(bg))?;
    }
    queue!(writer, Clear(ClearType::UntilNewLine))
}

pub(super) fn stale_rows_after_resize(
    old_area: Rect,
    new_area: Rect,
    screen_height: u16,
) -> Vec<u16> {
    (old_area.top().min(screen_height)..old_area.bottom().min(screen_height))
        .filter(|row| *row < new_area.top() || *row >= new_area.bottom())
        .collect()
}

pub(super) fn canvas_lines_to_ratatui(lines: &[CanvasLine]) -> Vec<Line<'static>> {
    lines
        .iter()
        .map(|line| {
            Line::from(
                line.spans
                    .iter()
                    .map(|span| {
                        Span::styled(span.text.as_str().to_owned(), canvas_span_style(span))
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

pub(super) fn wrap_canvas_lines(lines: &[CanvasLine], width: usize) -> Vec<CanvasLine> {
    lines
        .iter()
        .flat_map(|line| wrap_canvas_line(line, width))
        .collect()
}

pub(super) fn wrap_canvas_line(line: &CanvasLine, width: usize) -> Vec<CanvasLine> {
    if line.spans.is_empty() {
        return vec![CanvasLine::plain_lossy("")];
    }

    let mut rows = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0;
    let width = width.max(1);

    for span in &line.spans {
        for ch in span.text.as_str().chars().filter(|ch| *ch != '\r') {
            if ch == '\n' {
                rows.push(CanvasLine::from_spans(std::mem::take(&mut current)));
                current_width = 0;
                continue;
            }

            let char_width = display_width(&ch.to_string());
            if current_width + char_width > width && !current.is_empty() {
                rows.push(CanvasLine::from_spans(std::mem::take(&mut current)));
                current_width = 0;
            }

            push_wrapped_char(&mut current, span, ch);
            current_width += char_width;
        }
    }

    rows.push(CanvasLine::from_spans(current));
    rows
}

fn push_wrapped_char(spans: &mut Vec<CanvasSpan>, source: &CanvasSpan, ch: char) {
    let text = ch.to_string();
    if let Some(last) = spans
        .last_mut()
        .filter(|last| last.role == source.role && last.style == source.style)
    {
        last.text = CanvasText::plain_lossy(format!("{}{}", last.text.as_str(), text));
    } else {
        spans.push(CanvasSpan::styled_lossy(text, source.role, source.style));
    }
}

pub(super) fn write_canvas_line<W>(
    writer: &mut W,
    line: &CanvasLine,
    foreground: RatatuiColor,
    background: RatatuiColor,
) -> io::Result<usize>
where
    W: Write,
{
    let mut width = 0;
    for span in &line.spans {
        queue_span_style(writer, canvas_span_style(span), foreground, background)?;
        queue!(writer, Print(span.text.as_str()))?;
        width += display_width(span.text.as_str());
    }
    queue!(writer, SetAttribute(Attribute::Reset), ResetColor)?;
    Ok(width)
}

pub(super) fn queue_fill_row_remainder<W>(
    writer: &mut W,
    width: usize,
    used_width: usize,
    foreground: RatatuiColor,
    background: RatatuiColor,
) -> io::Result<()>
where
    W: Write,
{
    let fill_width = width.saturating_sub(used_width);
    if fill_width == 0 {
        return Ok(());
    }
    queue_span_style(writer, Style::default(), foreground, background)?;
    queue!(writer, Print(" ".repeat(fill_width)))
}

pub(super) fn canvas_span_style(span: &CanvasSpan) -> Style {
    role_style(span.role).patch(span.style)
}

pub(super) fn role_style(role: TextRole) -> Style {
    match role {
        TextRole::Plain => Style::default(),
        TextRole::Prompt => {
            let mut style = Style::default().add_modifier(Modifier::BOLD);
            style.fg = Some(USER_RAIL_COLOR);
            style
        }
        TextRole::Status => Style::default().add_modifier(Modifier::DIM),
    }
}

pub(super) fn queue_span_style<W>(
    writer: &mut W,
    style: Style,
    foreground: RatatuiColor,
    background: RatatuiColor,
) -> io::Result<()>
where
    W: Write,
{
    queue!(writer, SetAttribute(Attribute::Reset), ResetColor)?;
    if let Some(fg) = style.fg.or(Some(foreground)).and_then(crossterm_color) {
        queue!(writer, SetForegroundColor(fg))?;
    }
    if let Some(bg) = style.bg.or(Some(background)).and_then(crossterm_color) {
        queue!(writer, SetBackgroundColor(bg))?;
    }
    for attribute in crossterm_attributes(style.add_modifier) {
        queue!(writer, SetAttribute(attribute))?;
    }
    Ok(())
}

pub(super) fn crossterm_color(color: RatatuiColor) -> Option<CrosstermColor> {
    Some(match color {
        RatatuiColor::Reset => return None,
        RatatuiColor::Black => CrosstermColor::Black,
        RatatuiColor::Red => CrosstermColor::DarkRed,
        RatatuiColor::Green => CrosstermColor::DarkGreen,
        RatatuiColor::Yellow => CrosstermColor::DarkYellow,
        RatatuiColor::Blue => CrosstermColor::DarkBlue,
        RatatuiColor::Magenta => CrosstermColor::DarkMagenta,
        RatatuiColor::Cyan => CrosstermColor::DarkCyan,
        RatatuiColor::Gray => CrosstermColor::Grey,
        RatatuiColor::DarkGray => CrosstermColor::DarkGrey,
        RatatuiColor::LightRed => CrosstermColor::Red,
        RatatuiColor::LightGreen => CrosstermColor::Green,
        RatatuiColor::LightYellow => CrosstermColor::Yellow,
        RatatuiColor::LightBlue => CrosstermColor::Blue,
        RatatuiColor::LightMagenta => CrosstermColor::Magenta,
        RatatuiColor::LightCyan => CrosstermColor::Cyan,
        RatatuiColor::White => CrosstermColor::White,
        RatatuiColor::Rgb(red, green, blue) => CrosstermColor::Rgb {
            r: red,
            g: green,
            b: blue,
        },
        RatatuiColor::Indexed(index) => CrosstermColor::AnsiValue(index),
    })
}

pub(super) fn terminal_cursor_color_sequence(color: RatatuiColor) -> Option<String> {
    let (red, green, blue) = terminal_rgb(color)?;
    let mut sequence = String::from("\x1b]12;#");
    write!(&mut sequence, "{red:02x}{green:02x}{blue:02x}\x07").expect("write cursor color");
    Some(sequence)
}

pub(super) fn terminal_rgb(color: RatatuiColor) -> Option<(u8, u8, u8)> {
    match color {
        RatatuiColor::Reset => None,
        RatatuiColor::Black => Some((0x00, 0x00, 0x00)),
        RatatuiColor::Red => Some((0x80, 0x00, 0x00)),
        RatatuiColor::Green => Some((0x00, 0x80, 0x00)),
        RatatuiColor::Yellow => Some((0x80, 0x80, 0x00)),
        RatatuiColor::Blue => Some((0x00, 0x00, 0x80)),
        RatatuiColor::Magenta => Some((0x80, 0x00, 0x80)),
        RatatuiColor::Cyan => Some((0x00, 0x80, 0x80)),
        RatatuiColor::Gray => Some((0xc0, 0xc0, 0xc0)),
        RatatuiColor::DarkGray => Some((0x80, 0x80, 0x80)),
        RatatuiColor::LightRed => Some((0xff, 0x00, 0x00)),
        RatatuiColor::LightGreen => Some((0x00, 0xff, 0x00)),
        RatatuiColor::LightYellow => Some((0xff, 0xff, 0x00)),
        RatatuiColor::LightBlue => Some((0x00, 0x00, 0xff)),
        RatatuiColor::LightMagenta => Some((0xff, 0x00, 0xff)),
        RatatuiColor::LightCyan => Some((0x00, 0xff, 0xff)),
        RatatuiColor::White => Some((0xff, 0xff, 0xff)),
        RatatuiColor::Rgb(red, green, blue) => Some((red, green, blue)),
        RatatuiColor::Indexed(index) => Some(xterm_rgb(index)),
    }
}

pub(super) fn xterm_rgb(index: u8) -> (u8, u8, u8) {
    if index < 16 {
        return terminal_rgb(indexed_basic_color(index)).expect("basic color");
    }
    if index < 232 {
        return color_cube_rgb(index);
    }
    let shade = 8 + ((index - 232) * 10);
    (shade, shade, shade)
}

pub(super) fn indexed_basic_color(index: u8) -> RatatuiColor {
    match index {
        0 => RatatuiColor::Black,
        1 => RatatuiColor::Red,
        2 => RatatuiColor::Green,
        3 => RatatuiColor::Yellow,
        4 => RatatuiColor::Blue,
        5 => RatatuiColor::Magenta,
        6 => RatatuiColor::Cyan,
        7 => RatatuiColor::Gray,
        8 => RatatuiColor::DarkGray,
        9 => RatatuiColor::LightRed,
        10 => RatatuiColor::LightGreen,
        11 => RatatuiColor::LightYellow,
        12 => RatatuiColor::LightBlue,
        13 => RatatuiColor::LightMagenta,
        14 => RatatuiColor::LightCyan,
        _ => RatatuiColor::White,
    }
}

pub(super) fn color_cube_rgb(index: u8) -> (u8, u8, u8) {
    let cube_index = index - 16;
    let red = cube_index / 36;
    let green = (cube_index % 36) / 6;
    let blue = cube_index % 6;
    (cube_channel(red), cube_channel(green), cube_channel(blue))
}

pub(super) fn cube_channel(value: u8) -> u8 {
    if value == 0 {
        0
    } else {
        55 + (value * 40)
    }
}

pub(super) fn crossterm_attributes(modifier: Modifier) -> Vec<Attribute> {
    let mut attributes = Vec::new();
    if modifier.contains(Modifier::BOLD) {
        attributes.push(Attribute::Bold);
    }
    if modifier.contains(Modifier::DIM) {
        attributes.push(Attribute::Dim);
    }
    if modifier.contains(Modifier::ITALIC) {
        attributes.push(Attribute::Italic);
    }
    if modifier.contains(Modifier::UNDERLINED) {
        attributes.push(Attribute::Underlined);
    }
    if modifier.contains(Modifier::SLOW_BLINK) {
        attributes.push(Attribute::SlowBlink);
    }
    if modifier.contains(Modifier::RAPID_BLINK) {
        attributes.push(Attribute::RapidBlink);
    }
    if modifier.contains(Modifier::REVERSED) {
        attributes.push(Attribute::Reverse);
    }
    if modifier.contains(Modifier::HIDDEN) {
        attributes.push(Attribute::Hidden);
    }
    if modifier.contains(Modifier::CROSSED_OUT) {
        attributes.push(Attribute::CrossedOut);
    }
    attributes
}

pub(super) fn install_panic_restore_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
            restore_terminal();
            previous(info);
        }));
    });
}

#[cfg(unix)]
pub(super) fn install_signal_bridge() -> Result<SignalBridgeGuard> {
    // Process-level TUI bridge: signal-hook chains with existing handlers and
    // only records pending shutdown intent for the event loop to consume.
    let mut bridge = SIGNAL_BRIDGE
        .lock()
        .map_err(|_| anyhow!("terminal signal bridge lock poisoned"))?;
    if bridge.is_some() {
        bail!("terminal signal bridge is already active");
    }
    let sigint = flag::register(signal::SIGINT, Arc::clone(&SIGINT_PENDING))?;
    let sigterm = match flag::register(signal::SIGTERM, Arc::clone(&SIGTERM_PENDING)) {
        Ok(sigterm) => sigterm,
        Err(error) => {
            low_level::unregister(sigint);
            return Err(error.into());
        }
    };
    *bridge = Some(SignalBridgeIds { sigint, sigterm });
    Ok(SignalBridgeGuard)
}

#[cfg(not(unix))]
pub(super) fn install_signal_bridge() -> Result<SignalBridgeGuard> {
    let mut bridge = SIGNAL_BRIDGE
        .lock()
        .map_err(|_| anyhow!("terminal signal bridge lock poisoned"))?;
    if bridge.is_some() {
        bail!("terminal signal bridge is already active");
    }
    *bridge = Some(SignalBridgeIds);
    Ok(SignalBridgeGuard)
}

#[cfg(unix)]
pub(super) fn unregister_signal_bridge() {
    if let Ok(mut bridge) = SIGNAL_BRIDGE.lock() {
        if let Some(ids) = bridge.take() {
            low_level::unregister(ids.sigint);
            low_level::unregister(ids.sigterm);
            SIGINT_PENDING.store(false, Ordering::SeqCst);
            SIGTERM_PENDING.store(false, Ordering::SeqCst);
        }
    }
}

#[cfg(not(unix))]
pub(super) fn unregister_signal_bridge() {
    if let Ok(mut bridge) = SIGNAL_BRIDGE.lock() {
        if bridge.take().is_some() {
            SIGINT_PENDING.store(false, Ordering::SeqCst);
            SIGTERM_PENDING.store(false, Ordering::SeqCst);
        }
    }
}
