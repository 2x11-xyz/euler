//! Startup banner: brand wordmark with status rail, identity caption, version.
//!
//! Implements the terminal brand rules: a half-block pixel "Euler" with a
//! segmented status rail on the
//! left. The rail reads top→bottom as time — open → dead end → promising →
//! verified — and uses ANSI color slots (yellow 3, red 1, cyan 6, blue 4),
//! never truecolor, so the user's terminal theme restyles the brand.
//!
//! One layout contract, two renderings: the line-oriented CLI path emits SGR
//! escapes directly (`render_string`, color-gated), and the Ratatui path emits
//! per-span styled lines (`styled_lines`). Both are assembled from the same
//! row source so they cannot drift.

use super::theme::Theme;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

/// Letterform rows, excluding the rail column. Each row is rendered as
/// `<rail glyph><body>`; the leading space separating rail from letters is
/// part of the body.
const WORDMARK_BODY: &[&str] = &[
    " █████       ██",
    " ██▄▄  ██ ██ ██ ██▀██ ██▀▀",
    " ██▀▀  ██ ██ ██ ██▀▀▀ ██",
    " █████ █████ ██ ▀▀▀▀▀ ██",
];

const RAIL_GLYPH: &str = "█";

/// Left margin before the rail: one column, matching the gap between the
/// rail and the letterforms so the mark doesn't sit flush against the
/// terminal edge.
const LEFT_PAD: &str = " ";

/// Rail segment colors, top→bottom: open, dead end, promising, verified.
/// ANSI slots only (3, 1, 6, 4); the logo inherits the theme.
const RAIL_COLORS: [Color; 4] = [Color::Yellow, Color::Red, Color::Cyan, Color::Blue];
const RAIL_SGR: [&str; 4] = ["\x1b[33m", "\x1b[31m", "\x1b[36m", "\x1b[34m"];
const SGR_RESET: &str = "\x1b[0m";
const SGR_DIM: &str = "\x1b[2m";

const EQUATION: &str = "e^(iπ) + 1 = 0";
const CAPTION_MARGIN: usize = 2;

#[cfg(test)]
const FIRST_WORDMARK_LINE: usize = 1;
#[cfg(test)]
const CAPTION_LINE: usize = FIRST_WORDMARK_LINE + WORDMARK_BODY.len() + 1;
/// blank + wordmark rows + blank + caption + blank.
const HEIGHT: u16 = WORDMARK_BODY.len() as u16 + 4;

#[allow(dead_code)]
pub fn height() -> u16 {
    HEIGHT
}

#[allow(dead_code)]
pub fn banner_widget(theme: &Theme) -> BannerWidget<'_> {
    BannerWidget { theme }
}

#[allow(dead_code)]
pub struct BannerWidget<'a> {
    theme: &'a Theme,
}

#[allow(dead_code)]
impl Widget for BannerWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        Paragraph::new(styled_lines(self.theme)).render(area, buf);
    }
}

/// Display width of the wordmark: rail column plus the widest body row.
fn wordmark_width() -> usize {
    1 + WORDMARK_BODY
        .iter()
        .map(|row| row.chars().count())
        .max()
        .unwrap_or(0)
}

/// Caption content: equation left, version right-aligned to the wordmark's
/// right edge, with a 2-space left margin.
fn caption() -> String {
    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    let width = LEFT_PAD.chars().count() + wordmark_width();
    let used = CAPTION_MARGIN + EQUATION.chars().count() + version.chars().count();
    let gap = width.saturating_sub(used).max(1);
    format!(
        "{}{EQUATION}{}{version}",
        " ".repeat(CAPTION_MARGIN),
        " ".repeat(gap)
    )
}

/// Plain (uncolored) banner lines, flush-left. This is the no-color rendering
/// and the shared layout skeleton for both display paths.
pub fn render(_width: usize) -> Vec<String> {
    let mut lines = Vec::with_capacity(usize::from(HEIGHT));
    lines.push(String::new());
    for body in WORDMARK_BODY {
        lines.push(format!("{LEFT_PAD}{RAIL_GLYPH}{body}"));
    }
    lines.push(String::new());
    lines.push(caption());
    lines.push(String::new());
    lines
}

/// Render the banner for the line-oriented CLI path as one newline-joined
/// string, emitting SGR escapes when `color` allows (degradation:
/// no color support → the mark prints in one tone).
pub fn ansi_string(color: bool) -> String {
    if !color {
        let mut out = String::new();
        for line in render(0) {
            out.push_str(&line);
            out.push('\n');
        }
        return out;
    }
    let mut out = String::new();
    out.push('\n');
    for (row, body) in WORDMARK_BODY.iter().enumerate() {
        out.push_str(LEFT_PAD);
        out.push_str(RAIL_SGR[row]);
        out.push_str(RAIL_GLYPH);
        out.push_str(SGR_RESET);
        out.push_str(body);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(SGR_DIM);
    out.push_str(&caption());
    out.push_str(SGR_RESET);
    out.push('\n');
    out.push('\n');
    out
}

/// Line-oriented entrypoint used by the CLI. Honors `NO_COLOR`
/// (no-color.org) and `TERM=dumb`.
pub fn render_string(_width: usize) -> String {
    ansi_string(color_allowed())
}

fn color_allowed() -> bool {
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    let dumb = std::env::var("TERM").is_ok_and(|term| term == "dumb");
    !no_color && !dumb
}

/// Ratatui rendering: rail spans carry the brand slot colors; letterforms and
/// caption take their tones from the theme.
pub fn styled_lines(theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(usize::from(HEIGHT));
    lines.push(Line::from(""));
    for (row, body) in WORDMARK_BODY.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::raw(LEFT_PAD),
            Span::styled(RAIL_GLYPH, Style::default().fg(RAIL_COLORS[row])),
            Span::styled((*body).to_owned(), theme.banner.wordmark),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(caption(), theme.banner.identity)));
    lines.push(Line::from(""));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_backend::VT100Backend;
    use ratatui::{layout::Rect, Terminal};

    fn plain() -> Vec<String> {
        render(80)
    }

    #[test]
    fn banner_contains_wordmark_caption_and_version() {
        let joined = plain().join("\n");
        for body in WORDMARK_BODY {
            let row = format!("{LEFT_PAD}{RAIL_GLYPH}{body}");
            assert!(joined.contains(&row), "missing wordmark row: {row}");
        }
        assert!(joined.contains(EQUATION));
        assert!(joined.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn banner_has_one_column_left_margin() {
        let lines = plain();
        for (index, body) in WORDMARK_BODY.iter().enumerate() {
            assert_eq!(
                lines[FIRST_WORDMARK_LINE + index],
                format!("{LEFT_PAD}{RAIL_GLYPH}{body}"),
                "wordmark rows carry exactly the one-column left margin"
            );
        }
    }

    #[test]
    fn version_is_right_aligned_to_wordmark_edge() {
        let caption = &plain()[CAPTION_LINE];
        assert!(
            caption.starts_with("  e^(iπ)"),
            "caption keeps 2-space margin"
        );
        assert_eq!(
            caption.chars().count(),
            LEFT_PAD.chars().count() + wordmark_width(),
            "version must end at the wordmark's right edge"
        );
        let version = format!("v{}", env!("CARGO_PKG_VERSION"));
        assert!(caption.ends_with(&version));
    }

    #[test]
    fn ansi_rendering_uses_rail_slots_and_reset() {
        let out = ansi_string(true);
        for sgr in RAIL_SGR {
            assert!(out.contains(sgr), "rail SGR {sgr:?} present");
        }
        assert!(out.contains(SGR_DIM), "caption is dim");
        // Every escape is closed: count resets >= colored segments.
        let opens = RAIL_SGR.len() + 1;
        assert_eq!(out.matches(SGR_RESET).count(), opens);
    }

    #[test]
    fn no_color_rendering_has_zero_escapes() {
        let out = ansi_string(false);
        assert!(!out.contains('\x1b'), "no SGR escapes without color");
        assert!(out.contains(EQUATION));
        for body in WORDMARK_BODY {
            assert!(out.contains(body));
        }
    }

    #[test]
    fn narrow_width_does_not_panic_and_still_renders() {
        let lines = render(1);
        assert_eq!(lines.len(), usize::from(HEIGHT));
        assert!(lines.join("\n").contains(EQUATION));
    }

    #[test]
    fn height_matches_render_line_count() {
        assert_eq!(usize::from(height()), render(72).len());
    }

    #[test]
    fn styled_lines_color_the_rail_with_ansi_slots() {
        let theme = Theme::default_dark();
        let lines = styled_lines(&theme);

        for (row, color) in RAIL_COLORS.iter().enumerate() {
            let line = &lines[FIRST_WORDMARK_LINE + row];
            assert_eq!(line.spans[0].content, LEFT_PAD);
            assert_eq!(line.spans[1].content, RAIL_GLYPH);
            assert_eq!(line.spans[1].style.fg, Some(*color), "rail row {row}");
            assert_eq!(
                line.spans[2].style, theme.banner.wordmark,
                "letterforms take the theme tone"
            );
        }
        assert_eq!(lines[CAPTION_LINE].spans[0].style, theme.banner.identity);
    }

    #[test]
    fn ratatui_widget_renders_banner_into_vt100_backend() {
        let mut terminal =
            Terminal::new(VT100Backend::new(72, height())).expect("terminal should initialize");
        let theme = Theme::default_dark();

        terminal
            .draw(|frame| {
                frame.render_widget(banner_widget(&theme), Rect::new(0, 0, 72, height()));
            })
            .expect("draw should succeed");

        let contents = terminal.backend().screen_contents();
        assert!(contents.contains("██▄▄"));
        assert!(contents.contains("e^(iπ) + 1 = 0"));
        assert!(contents.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))));
    }
}
