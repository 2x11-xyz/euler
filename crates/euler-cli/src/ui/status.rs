use super::text::{display_width, truncate_display};
use super::theme::Theme;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Widget,
};
use std::path::PathBuf;

const SEGMENT_GAP: &str = " · ";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TokenUsageSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub context_window_tokens: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusSnapshot {
    pub session_id: Option<String>,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub cwd: PathBuf,
    pub git_branch: Option<String>,
    pub extension_slots: StatusSlots,
}

impl StatusSnapshot {
    pub fn new(provider: impl Into<String>, model: impl Into<String>, cwd: PathBuf) -> Self {
        Self {
            session_id: None,
            provider: provider.into(),
            model: model.into(),
            reasoning_effort: None,
            cwd,
            git_branch: None,
            extension_slots: StatusSlots::default(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StatusSlots {
    labels: Vec<String>,
}

impl StatusSlots {
    pub fn push_label(&mut self, label: impl Into<String>) {
        self.labels.push(label.into());
    }

    fn has_renderable_labels(&self) -> bool {
        self.labels.iter().any(|label| !label.is_empty())
    }

    fn renderable_labels(&self) -> impl Iterator<Item = &str> {
        self.labels
            .iter()
            .map(String::as_str)
            .filter(|label| !label.is_empty())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnStatus {
    Idle,
    Running(String),
}

pub fn status_line_text(
    snapshot: &StatusSnapshot,
    tokens: &TokenUsageSnapshot,
    turn: TurnStatus,
    width: u16,
) -> String {
    let left = status_left_segment(snapshot, &turn);
    let indent = status_indent(width);
    let body_width = status_body_width(width, display_width(indent));
    format!(
        "{indent}{}",
        join_status_halves(left, context_segment(tokens), body_width)
    )
}

pub fn status_widget<'a>(snapshot: &'a StatusSnapshot, theme: &'a Theme) -> StatusWidget<'a> {
    StatusWidget {
        snapshot,
        tokens: None,
        turn: TurnStatus::Idle,
        theme,
    }
}

pub struct StatusWidget<'a> {
    snapshot: &'a StatusSnapshot,
    tokens: Option<&'a TokenUsageSnapshot>,
    turn: TurnStatus,
    theme: &'a Theme,
}

impl<'a> StatusWidget<'a> {
    pub fn runtime(mut self, tokens: &'a TokenUsageSnapshot, turn: TurnStatus) -> Self {
        self.tokens = Some(tokens);
        self.turn = turn;
        self
    }
}

impl Widget for StatusWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let default_tokens;
        let tokens = match self.tokens {
            Some(tokens) => tokens,
            None => {
                default_tokens = TokenUsageSnapshot::default();
                &default_tokens
            }
        };
        status_line(self.snapshot, tokens, self.turn, self.theme, area.width).render(area, buf);
    }
}

pub fn context_segment(snapshot: &TokenUsageSnapshot) -> String {
    let Some(window) = snapshot.context_window_tokens.filter(|window| *window > 0) else {
        return "Context ?% used".to_owned();
    };
    let percent = rounded_context_percent(snapshot.input_tokens, window).min(99);
    format!("Context {percent}% used")
}

fn rounded_context_percent(input_tokens: u64, window: u64) -> u64 {
    let numerator = u128::from(input_tokens)
        .saturating_mul(100)
        .saturating_add(u128::from(window / 2));
    let percent = numerator / u128::from(window);
    u64::try_from(percent).unwrap_or(u64::MAX)
}

fn model_effort_segment(snapshot: &StatusSnapshot) -> String {
    let model = compact_model_label(&snapshot.provider, &snapshot.model);
    let target = if snapshot.provider.is_empty() && snapshot.model.is_empty() {
        "?/?".to_owned()
    } else if snapshot.provider.is_empty() {
        model.to_owned()
    } else {
        format!("{}/{}", snapshot.provider, model)
    };
    let target = if display_width(&target) > 32 {
        let mut truncated = truncate_display(&target, 31);
        truncated.push('…');
        truncated
    } else {
        target
    };
    let effort = snapshot
        .reasoning_effort
        .as_deref()
        .filter(|effort| !effort.is_empty())
        .unwrap_or("?");
    format!("{target} {effort}")
}

fn compact_model_label<'a>(provider: &str, model: &'a str) -> &'a str {
    if provider == "anthropic" {
        model.strip_prefix("claude-").unwrap_or(model)
    } else {
        model
    }
}

fn status_left_segment(snapshot: &StatusSnapshot, turn: &TurnStatus) -> String {
    let base = format!(
        "{}{}{}",
        model_effort_segment(snapshot),
        SEGMENT_GAP,
        cwd_segment(snapshot)
    );
    match turn {
        TurnStatus::Idle => base,
        TurnStatus::Running(label) => format!("{base}{SEGMENT_GAP}running {label}"),
    }
}

fn cwd_segment(snapshot: &StatusSnapshot) -> String {
    if snapshot.cwd.as_os_str().is_empty() {
        return "?".to_owned();
    }
    snapshot
        .cwd
        .file_name()
        .map(|name| format!("/{}", name.to_string_lossy()))
        .unwrap_or_else(|| "/".to_owned())
}

fn join_status_halves(left: String, right: String, width: usize) -> String {
    let left_width = display_width(&left);
    let right_width = display_width(&right);
    let gap_width = display_width(SEGMENT_GAP);
    if left_width + right_width + gap_width <= width {
        return format!("{left}{SEGMENT_GAP}{right}");
    }
    let right_room = gap_width + right_width;
    if width > right_room {
        let left = truncate_display(&left, width - right_room);
        return format!("{left}{SEGMENT_GAP}{right}");
    }
    truncate_display(&right, width)
}

fn status_line(
    snapshot: &StatusSnapshot,
    tokens: &TokenUsageSnapshot,
    turn: TurnStatus,
    theme: &Theme,
    width: u16,
) -> Line<'static> {
    Line::from(status_line_spans(snapshot, tokens, turn, theme, width))
}

fn status_line_spans(
    snapshot: &StatusSnapshot,
    tokens: &TokenUsageSnapshot,
    turn: TurnStatus,
    theme: &Theme,
    width: u16,
) -> Vec<Span<'static>> {
    let left = vec![
        Span::styled(model_effort_segment(snapshot), theme.status.model),
        Span::styled(SEGMENT_GAP, theme.status.base),
        Span::styled(cwd_segment(snapshot), theme.status.state),
    ];
    let left = status_spans_with_turn(left, turn, theme);
    let right = vec![Span::styled(context_segment(tokens), theme.status.ctx)];
    let indent = Span::styled(status_indent(width), theme.status.base);
    let body_width = status_body_width(width, display_width(indent.content.as_ref()));
    let mut spans = vec![indent];
    spans.extend(join_status_span_halves(
        left,
        right,
        body_width,
        theme.status.base,
    ));
    spans
}

fn status_spans_with_turn(
    mut left: Vec<Span<'static>>,
    turn: TurnStatus,
    theme: &Theme,
) -> Vec<Span<'static>> {
    if let TurnStatus::Running(label) = turn {
        left.push(Span::styled(SEGMENT_GAP, theme.status.base));
        left.push(Span::styled(format!("running {label}"), theme.status.state));
    }
    left
}

fn join_status_span_halves(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
    base: Style,
) -> Vec<Span<'static>> {
    let left_width = spans_width(&left);
    let right_width = spans_width(&right);
    let gap_width = display_width(SEGMENT_GAP);
    if left_width + right_width + gap_width <= width {
        let mut spans = left;
        spans.push(Span::styled(SEGMENT_GAP, base));
        spans.extend(right);
        return spans;
    }

    let right_room = gap_width + right_width;
    if width > right_room {
        let mut spans = truncate_spans(&left, width - right_room);
        spans.push(Span::styled(SEGMENT_GAP, base));
        spans.extend(right);
        return spans;
    }

    truncate_spans(&right, width)
}

fn status_indent(width: u16) -> &'static str {
    if width > 3 {
        "  "
    } else {
        ""
    }
}

fn status_body_width(width: u16, indent_width: usize) -> usize {
    usize::from(width)
        .saturating_sub(indent_width)
        .saturating_sub(1)
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

fn truncate_spans(spans: &[Span<'static>], width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut remaining = width;
    for span in spans {
        if remaining == 0 {
            break;
        }
        let text = span.content.as_ref();
        let truncated = truncate_display(text, remaining);
        if truncated.is_empty() {
            continue;
        }
        remaining = remaining.saturating_sub(display_width(&truncated));
        out.push(Span::styled(truncated, span.style));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{test_backend::VT100Backend, theme::Theme};
    use ratatui::{layout::Rect, Terminal};
    use std::path::PathBuf;

    #[test]
    fn status_line_base_rendering_includes_core_segments_and_truncates() {
        let mut snapshot =
            StatusSnapshot::new("openrouter", "z-ai/glm-5.2", PathBuf::from("/tmp/repo"));
        snapshot.reasoning_effort = Some("extra-high".to_owned());
        snapshot.git_branch = Some("main".to_owned());
        let tokens = TokenUsageSnapshot::default();

        let full = status_line_text(&snapshot, &tokens, TurnStatus::Idle, 120);
        assert!(full.contains("openrouter/z-ai/glm-5.2"));
        assert!(full.contains("extra-high"));
        assert!(full.contains("/repo"));
        assert!(full.contains("Context ?% used"));
        assert!(!full.contains("idle"));
        assert!(!full.contains("run"));
        assert!(!full.contains("--"));

        let narrow = status_line_text(&snapshot, &tokens, TurnStatus::Idle, 18);
        assert!(display_width(&narrow) <= 18);
    }

    #[test]
    fn statusline_uses_honest_placeholders() {
        let snapshot = StatusSnapshot::new("fixture", "echo", PathBuf::from("/tmp/repo"));
        let tokens = TokenUsageSnapshot::default();

        let rendered = status_line_text(&snapshot, &tokens, TurnStatus::Idle, 80);
        assert_eq!(rendered, "  fixture/echo ? · /repo · Context ?% used");
    }

    #[test]
    fn statusline_compacts_anthropic_model_and_cwd_for_footer_only() {
        let mut snapshot = StatusSnapshot::new(
            "anthropic",
            "claude-fable-5",
            PathBuf::from("/home/user/projects/euler"),
        );
        snapshot.reasoning_effort = Some("medium".to_owned());
        let tokens = TokenUsageSnapshot {
            input_tokens: 120_000,
            output_tokens: 50_000,
            reasoning_tokens: Some(25_000),
            context_window_tokens: Some(1_000_000),
        };

        let rendered = status_line_text(&snapshot, &tokens, TurnStatus::Idle, 120);

        assert_eq!(
            rendered,
            "  anthropic/fable-5 medium · /euler · Context 12% used"
        );
        assert_eq!(snapshot.model, "claude-fable-5");
    }

    #[test]
    fn statusline_renders_root_cwd_as_root() {
        let snapshot = StatusSnapshot::new("fixture", "echo", PathBuf::from("/"));
        let tokens = TokenUsageSnapshot::default();

        let rendered = status_line_text(&snapshot, &tokens, TurnStatus::Idle, 80);

        assert_eq!(rendered, "  fixture/echo ? · / · Context ?% used");
    }

    #[test]
    fn statusline_names_running_extension_command() {
        let snapshot = StatusSnapshot::new("fixture", "echo", PathBuf::from("/tmp/repo"));
        let tokens = TokenUsageSnapshot::default();

        let rendered = status_line_text(
            &snapshot,
            &tokens,
            TurnStatus::Running("extension causal-dag.catch-up".to_owned()),
            120,
        );

        assert!(rendered.contains("running extension causal-dag.catch-up"));
    }

    #[test]
    fn statusline_spans_use_named_slot_styles_and_base_gaps() {
        let snapshot = StatusSnapshot::new("fixture", "echo", PathBuf::from("/repo"));
        let tokens = TokenUsageSnapshot::default();
        let theme = Theme::default();

        let spans = status_line_spans(&snapshot, &tokens, TurnStatus::Idle, &theme, 80);

        assert!(spans
            .iter()
            .any(|span| span.content.contains("fixture/echo") && span.style == theme.status.model));
        assert!(spans
            .iter()
            .any(|span| span.content.contains("/repo") && span.style == theme.status.state));
        assert!(spans
            .iter()
            .any(|span| span.content == "Context ?% used" && span.style == theme.status.ctx));
        assert!(spans
            .iter()
            .any(|span| span.content == " · " && span.style == theme.status.base));
        assert_eq!(theme.status.state.fg, Some(theme.palette.st_state));
        assert_eq!(theme.status.model.fg, Some(theme.palette.st_model));
        assert_eq!(theme.status.cost.fg, Some(theme.palette.st_cost));
        assert_eq!(theme.status.ctx.fg, Some(theme.palette.st_ctx));
    }

    #[test]
    fn status_widget_renders_without_panicking_at_narrow_width() {
        let snapshot = StatusSnapshot::new("fixture", "echo", PathBuf::from("/very/long/path"));
        let tokens = TokenUsageSnapshot::default();
        let theme = Theme::default();
        let backend = VT100Backend::new(12, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| {
                frame.render_widget(
                    status_widget(&snapshot, &theme).runtime(&tokens, TurnStatus::Idle),
                    Rect::new(0, 0, 12, 1),
                )
            })
            .expect("draw");

        assert!(!terminal.backend().screen_contents().is_empty());
    }

    #[test]
    fn statusline_reserves_trailing_cell_to_avoid_terminal_autowrap() {
        let snapshot = StatusSnapshot::new(
            "fixture",
            "echo",
            PathBuf::from("/tmp/euler-tui-live-resize-hardening"),
        );
        let tokens = TokenUsageSnapshot::default();

        let rendered = status_line_text(&snapshot, &tokens, TurnStatus::Idle, 72);

        assert!(display_width(&rendered) < 72);
        assert!(rendered.contains("Context ?% used"));

        for width in 1..=10 {
            let rendered = status_line_text(&snapshot, &tokens, TurnStatus::Idle, width);
            assert!(
                display_width(&rendered) < usize::from(width),
                "width {width} rendered {rendered:?}"
            );

            let spans = status_line_spans(
                &snapshot,
                &tokens,
                TurnStatus::Idle,
                &Theme::default(),
                width,
            );
            assert!(
                spans_width(&spans) < usize::from(width),
                "width {width} rendered spans {spans:?}"
            );
        }
    }

    #[test]
    fn absent_context_window_tokens_render_unknown_fill() {
        let snapshot = TokenUsageSnapshot {
            input_tokens: 100,
            output_tokens: 25,
            reasoning_tokens: None,
            context_window_tokens: None,
        };

        assert_eq!(context_segment(&snapshot), "Context ?% used");
    }

    #[test]
    fn token_bar_uses_saturating_usage_math() {
        let snapshot = TokenUsageSnapshot {
            input_tokens: u64::MAX,
            output_tokens: u64::MAX,
            reasoning_tokens: Some(u64::MAX),
            context_window_tokens: Some(1),
        };

        assert!(context_segment(&snapshot).contains("Context "));
        assert!(context_segment(&snapshot).contains("% used"));
    }

    #[test]
    fn context_percent_rounds_to_nearest_integer_and_clamps_display() {
        let snapshot = TokenUsageSnapshot {
            input_tokens: 125,
            output_tokens: 999,
            reasoning_tokens: None,
            context_window_tokens: Some(1_000),
        };

        assert_eq!(context_segment(&snapshot), "Context 13% used");

        let clamped = TokenUsageSnapshot {
            input_tokens: 1_000,
            output_tokens: 0,
            reasoning_tokens: None,
            context_window_tokens: Some(1_000),
        };

        assert_eq!(context_segment(&clamped), "Context 99% used");
    }
}
