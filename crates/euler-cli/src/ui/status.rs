use super::text::{display_width, truncate_display, truncate_display_left};
use super::theme::Theme;
#[cfg(test)]
use ratatui::{buffer::Buffer, layout::Rect, text::Line, widgets::Widget};
use ratatui::{style::Style, text::Span};
use std::path::{Path, PathBuf};

const SEGMENT_GAP: &str = " · ";

/// Short display form of a session id, for UI surfaces that only need a
/// glanceable handle (banner, footer identity cluster, exit recap headline).
/// A full ULID (26 chars) becomes `e` + its last 4 characters lowercased —
/// the tail of a ULID varies fastest, so it's the most distinguishing sliver
/// at a glance. Ids already at or under 5 chars (e.g. the `e????`/`e0000`
/// no-session fallbacks) are returned unchanged. The full id always belongs
/// in `/status` output and in copy-ready resume commands.
pub fn short_session_id(id: &str) -> String {
    if id.chars().count() <= 5 {
        return id.to_owned();
    }
    // char-based tail: a malformed (non-ASCII) id must not panic the TUI.
    let tail: String = id
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("e{}", tail.to_lowercase())
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TokenUsageSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub context_window_tokens: Option<u64>,
    pub canvas_retained_bytes: Option<u64>,
    pub canvas_budget_bytes: Option<u64>,
    /// Cumulative USD in trillionths (pico-dollars), computed from the model
    /// metadata attached to each persisted model result.
    pub session_cost_picos: u128,
    pub priced_calls: u64,
    pub unpriced_calls: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusSnapshot {
    pub session_id: Option<String>,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub cwd: PathBuf,
    pub git_branch: Option<String>,
    /// Footer §13.5 / #46: the user-set `/name`, never the session id.
    /// `None` until named — unnamed sessions show nothing in the footer.
    pub session_name: Option<String>,
    pub extension_slots: StatusSlots,
    /// Non-default permission reviewer label (ADR 0011): `Some("guardian")`
    /// when the guardian reviews asks; `None` for the default user reviewer.
    /// Surfaced by `/status`, not the footer.
    pub permission_reviewer: Option<String>,
    /// The active posture and its envelope (§5.1), cached here because
    /// `/status` is answerable mid-turn and `AppState::TurnInFlight` does not
    /// carry the session. Refreshed whenever the modes behind it change.
    pub permission_envelope: Option<String>,
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
            session_name: None,
            extension_slots: StatusSlots::default(),
            permission_reviewer: None,
            permission_envelope: None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StatusSlots {
    labels: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnStatus {
    Idle,
    Running(String),
}

/// Footer v2 (Review v2 §15): two hard-edged clusters with empty space
/// between — left flush-left (contextual hints, then `cwd (branch)`),
/// right flush-right (`model(effort) · ctx N% · $N.NNN` [+ session name once
/// named]).
/// Test-only: exercises the same span builder as the production
/// `status_line_canvas` but flattens to plain text for easy assertions.
#[cfg(test)]
pub fn status_line_text(
    snapshot: &StatusSnapshot,
    tokens: &TokenUsageSnapshot,
    turn: TurnStatus,
    has_foldable: bool,
    width: u16,
) -> String {
    status_line_spans(
        snapshot,
        tokens,
        turn,
        has_foldable,
        &Theme::default(),
        width,
    )
    .iter()
    .map(|span| span.content.as_ref())
    .collect()
}

/// Production entry point: builds the fully-styled footer line (faint
/// footer token throughout, branch parens one step brighter, ctx% keeping
/// its threshold colors) for rendering onto the visual canvas.
pub fn status_line_canvas(
    snapshot: &StatusSnapshot,
    tokens: &TokenUsageSnapshot,
    turn: TurnStatus,
    has_foldable: bool,
    theme: &Theme,
    width: u16,
) -> super::visual_canvas::CanvasLine {
    use super::visual_canvas::{CanvasLine, CanvasSpan, TextRole};
    CanvasLine::from_spans(
        status_line_spans(snapshot, tokens, turn, has_foldable, theme, width)
            .into_iter()
            .map(|span| {
                CanvasSpan::styled_lossy(span.content.into_owned(), TextRole::Plain, span.style)
            })
            .collect(),
    )
}

#[cfg(test)]
pub fn status_widget<'a>(snapshot: &'a StatusSnapshot, theme: &'a Theme) -> StatusWidget<'a> {
    StatusWidget {
        snapshot,
        tokens: None,
        turn: TurnStatus::Idle,
        theme,
    }
}

#[cfg(test)]
pub struct StatusWidget<'a> {
    snapshot: &'a StatusSnapshot,
    tokens: Option<&'a TokenUsageSnapshot>,
    turn: TurnStatus,
    theme: &'a Theme,
}

#[cfg(test)]
impl<'a> StatusWidget<'a> {
    pub fn runtime(mut self, tokens: &'a TokenUsageSnapshot, turn: TurnStatus) -> Self {
        self.tokens = Some(tokens);
        self.turn = turn;
        self
    }
}

#[cfg(test)]
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
        Line::from(status_line_spans(
            self.snapshot,
            tokens,
            self.turn,
            false,
            self.theme,
            area.width,
        ))
        .render(area, buf);
    }
}

#[cfg(test)]
pub fn context_segment(snapshot: &TokenUsageSnapshot) -> String {
    match snapshot.context_window_tokens.filter(|window| *window > 0) {
        Some(window) => {
            let percent = rounded_context_percent(snapshot.input_tokens, window).min(99);
            format!("Context {percent}% used")
        }
        None => match (snapshot.canvas_retained_bytes, snapshot.canvas_budget_bytes) {
            (Some(retained), Some(budget)) if budget > 0 => {
                format!(
                    "Canvas {}KB/{}KB",
                    retained.div_ceil(1024),
                    budget.div_ceil(1024)
                )
            }
            _ => "Context ?% used".to_owned(),
        },
    }
}

fn rounded_context_percent(input_tokens: u64, window: u64) -> u64 {
    let numerator = u128::from(input_tokens)
        .saturating_mul(100)
        .saturating_add(u128::from(window / 2));
    let percent = numerator / u128::from(window);
    u64::try_from(percent).unwrap_or(u64::MAX)
}

fn compact_model_label<'a>(provider: &str, model: &'a str) -> &'a str {
    if provider == "anthropic" {
        model.strip_prefix("claude-").unwrap_or(model)
    } else {
        model
    }
}

fn status_hints(turn: &TurnStatus, has_foldable: bool) -> String {
    match turn {
        TurnStatus::Idle if has_foldable => "/ commands · ctrl+o expand".to_owned(),
        TurnStatus::Idle => "/ commands".to_owned(),
        TurnStatus::Running(_) => "⏎ steer · esc interrupt now".to_owned(),
    }
}

/// Compact prompt convention: paths with an owner/repository pair below
/// `~/code` replace only that source-root prefix (`~/code/2x11/euler` becomes
/// `../2x11/euler`). Any path below the repository remains visible. Other
/// home-relative paths retain the familiar `~` prefix. `home` is injected for
/// hermetic testing — the production caller resolves it from `$HOME`.
fn home_relative_path(cwd: &Path, home: Option<&Path>) -> String {
    if cwd.as_os_str().is_empty() {
        return "?".to_owned();
    }
    if let Some(home) = home.filter(|home| !home.as_os_str().is_empty() && *home != Path::new("/"))
    {
        if cwd == home {
            return "~".to_owned();
        }
        if let Ok(rest) = cwd.strip_prefix(home) {
            if rest.as_os_str().is_empty() {
                return "~".to_owned();
            }
            if let Ok(project_path) = rest.strip_prefix("code") {
                let has_owner_and_repository = project_path.components().count() >= 2;
                let has_only_normal_components = project_path
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_)));
                if has_owner_and_repository && has_only_normal_components {
                    return PathBuf::from("..").join(project_path).display().to_string();
                }
            }
            return format!("~/{}", rest.display());
        }
    }
    cwd.display().to_string()
}

fn cwd_display(cwd: &Path) -> String {
    home_relative_path(cwd, std::env::var_os("HOME").map(PathBuf::from).as_deref())
}

fn identity_context_label(tokens: &TokenUsageSnapshot) -> String {
    match identity_context_percent(tokens) {
        Some(percent) => format!("ctx {percent}%"),
        None => match (tokens.canvas_retained_bytes, tokens.canvas_budget_bytes) {
            (Some(retained), Some(budget)) if budget > 0 => {
                format!(
                    "canvas {}KB/{}KB",
                    retained.div_ceil(1024),
                    budget.div_ceil(1024)
                )
            }
            _ => "ctx ?%".to_owned(),
        },
    }
}

fn identity_context_percent(tokens: &TokenUsageSnapshot) -> Option<u64> {
    tokens
        .context_window_tokens
        .filter(|window| *window > 0)
        .map(|window| rounded_context_percent(tokens.input_tokens, window).min(99))
}

fn identity_context_style(tokens: &TokenUsageSnapshot, theme: &Theme) -> Style {
    match identity_context_percent(tokens) {
        Some(percent) if percent >= 85 => Style::default().fg(theme.palette.error),
        Some(percent) if percent >= 70 => theme.status.cost,
        _ => theme.status.faint,
    }
}

/// The footer cost chip follows the "absence over punctuation" rule: it is
/// present only when the priced cost subtotal is greater than zero, rendered
/// as the plain `$N.NNN`. A zero subtotal (genuinely free, no calls made, or
/// only unpriced calls) yields no chip at all: no `$0`, no `$?`, no marker.
/// The mixed case (nonzero priced plus some unpriced) shows the plain subtotal
/// unmarked; the complete honesty (`$?` / `$N.NNN+ (N unpriced calls)`) lives
/// in `/usage`, not the footer.
fn identity_cost_label(tokens: &TokenUsageSnapshot) -> Option<String> {
    if tokens.session_cost_picos == 0 {
        None
    } else {
        Some(format_cost_picos(tokens.session_cost_picos, 3))
    }
}

pub(super) fn format_cost_picos(picos: u128, decimals: u32) -> String {
    debug_assert!(decimals <= 12);
    let divisor = 10_u128.pow(12 - decimals);
    let rounded = picos / divisor + u128::from(picos % divisor >= divisor.div_ceil(2));
    if decimals == 0 {
        return format!("${rounded}");
    }
    let scale = 10_u128.pow(decimals);
    let whole = rounded / scale;
    let fraction = rounded % scale;
    format!("${whole}.{fraction:0width$}", width = decimals as usize)
}

/// Right cluster, flush-right: `model(effort) · ctx N%` [· `$N.NNN`] [· session
/// name once named]. The cost chip appears only for a nonzero priced subtotal
/// (see `identity_cost_label`). Branch no longer lives here (#48) — see the
/// left cluster's `cwd (branch)` instead.
fn identity_segment_spans(
    snapshot: &StatusSnapshot,
    tokens: &TokenUsageSnapshot,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let model = compact_model_label(&snapshot.provider, &snapshot.model);
    let model = if model.is_empty() { "?" } else { model };
    let model = match snapshot
        .reasoning_effort
        .as_deref()
        .filter(|effort| !effort.is_empty())
    {
        Some(effort) => format!("{model}({effort})"),
        None => model.to_owned(),
    };
    let ctx = identity_context_label(tokens);
    let mut spans = vec![
        Span::styled(format!("{model} · "), theme.status.faint),
        Span::styled(ctx, identity_context_style(tokens, theme)),
    ];
    if let Some(cost) = identity_cost_label(tokens) {
        spans.push(Span::styled(
            format!("{SEGMENT_GAP}{cost}"),
            theme.status.cost,
        ));
    }
    if let Some(name) = snapshot
        .session_name
        .as_deref()
        .filter(|name| !name.is_empty())
    {
        spans.push(Span::styled(format!(" · {name}"), theme.status.faint));
    }
    spans
}

/// Left cluster, flush-left: contextual hints, then `cwd (branch)` —
/// zsh/fish prompt convention. Non-git directories render no parens. The
/// directory is the first thing squeezed at narrow widths (§4): its budget
/// is whatever's left after hints + branch + the right cluster + a 1-cell
/// minimum gap, truncated from the left (`…/2x11/euler`).
fn status_left_spans(
    snapshot: &StatusSnapshot,
    turn: &TurnStatus,
    has_foldable: bool,
    theme: &Theme,
    right_width: usize,
    body_width: usize,
) -> Vec<Span<'static>> {
    let hints = status_hints(turn, has_foldable);
    let hints_prefix = format!("{hints}{SEGMENT_GAP}");
    let path_full = cwd_display(&snapshot.cwd);
    let branch_suffix = snapshot
        .git_branch
        .as_deref()
        .filter(|branch| !branch.is_empty())
        .map(|branch| format!(" ({branch})"));

    let reserved = display_width(&hints_prefix)
        + branch_suffix.as_deref().map(display_width).unwrap_or(0)
        + 1 // minimum gap between clusters
        + right_width;
    let path_budget = body_width.saturating_sub(reserved);
    let path = truncate_display_left(&path_full, path_budget);

    let mut spans = vec![
        Span::styled(hints_prefix, theme.status.faint),
        Span::styled(path, theme.status.faint),
    ];
    if let Some(branch) = branch_suffix {
        spans.push(Span::styled(branch, theme.status.branch));
    }
    spans
}

fn join_footer_span_clusters(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
) -> Vec<Span<'static>> {
    let right_width = spans_width(&right);
    if right_width >= width {
        // The right cluster (model(effort) · ctx N% · $N.NNN [· session name]) never yields;
        // if even it alone can't fit, it gets the whole line.
        return truncate_spans(&right, width);
    }

    // Always reserve at least one column of separation before the right
    // cluster (v4 dogfood, #round4): the path is already squeezed to its
    // minimum inside `status_left_spans`, but once the right cluster grows
    // (adding session name this round) an exact-fit left cluster used to
    // render with zero gap — `(inte` glued directly against
    // `z-ai/glm-5.2(medium) · ctx …`. Truncate the left cluster (cwd/branch)
    // harder instead of ever letting the gap collapse to nothing.
    let available_for_left = width - right_width;
    let left_budget = available_for_left - 1;
    let mut spans = truncate_spans(&left, left_budget);
    let gap = available_for_left - spans_width(&spans);
    if gap > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
    }
    spans.extend(right);
    spans
}

fn status_line_spans(
    snapshot: &StatusSnapshot,
    tokens: &TokenUsageSnapshot,
    turn: TurnStatus,
    has_foldable: bool,
    theme: &Theme,
    width: u16,
) -> Vec<Span<'static>> {
    let indent = status_indent(width);
    let body_width = status_body_width(width, display_width(indent));
    let right = identity_segment_spans(snapshot, tokens, theme);
    let right_width = spans_width(&right);
    let left = status_left_spans(
        snapshot,
        &turn,
        has_foldable,
        theme,
        right_width,
        body_width,
    );

    let indent_span = Span::styled(indent, theme.status.faint);
    let mut spans = vec![indent_span];
    spans.extend(join_footer_span_clusters(left, right, body_width));
    spans
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

        let full = status_line_text(&snapshot, &tokens, TurnStatus::Idle, false, 120);
        assert!(full.contains("/ commands"));
        assert!(!full.contains("ctrl+o expand"));
        assert!(full.contains("/tmp/repo (main)"));
        assert!(full.contains("z-ai/glm-5.2(extra-high) · ctx ?%"));
        // Zero priced subtotal: the cost chip is absent, no orphaned separator.
        assert!(!full.contains("ctx ?% · $"));
        // Branch v2 (#48): lives beside the directory, never on the right.
        assert!(!full.contains("ctx ?% · main"));
        assert!(!full.contains("e???? ·"));
        assert!(!full.contains("openrouter/z-ai/glm-5.2"));
        assert!(!full.contains("Context ?% used"));
        assert!(!full.contains("idle"));
        assert!(!full.contains("run"));
        assert!(!full.contains("--"));
        // No `euler ·` prefix and no lingering middle-dot join between clusters.
        assert!(!full.contains("euler ·"));

        let narrow = status_line_text(&snapshot, &tokens, TurnStatus::Idle, false, 18);
        assert!(display_width(&narrow) <= 18);
    }

    /// #46: unnamed sessions show nothing extra; naming appends the name
    /// to the right cluster; ids never appear in the footer.
    #[test]
    fn session_name_appears_only_once_named() {
        let mut snapshot = StatusSnapshot::new("fixture", "echo", PathBuf::from("/tmp/repo"));
        snapshot.session_id = Some("01KW3Q6NN5A9R6E2EWZ7M3QW9T".to_owned());
        let tokens = TokenUsageSnapshot::default();

        let unnamed = status_line_text(&snapshot, &tokens, TurnStatus::Idle, false, 120);
        assert!(unnamed.ends_with("echo · ctx ?%"));
        assert!(!unnamed.contains("01KW3Q6NN5A9R6E2EWZ7M3QW9T"));

        snapshot.session_name = Some("research-branch".to_owned());
        let named = status_line_text(&snapshot, &tokens, TurnStatus::Idle, false, 120);
        assert!(named.ends_with("echo · ctx ?% · research-branch"));
        assert!(!named.contains("01KW3Q6NN5A9R6E2EWZ7M3QW9T"));
    }

    #[test]
    fn statusline_uses_honest_placeholders_and_omits_parens_without_a_branch() {
        let snapshot = StatusSnapshot::new("fixture", "echo", PathBuf::from("/tmp/repo"));
        let tokens = TokenUsageSnapshot::default();

        let rendered = status_line_text(&snapshot, &tokens, TurnStatus::Idle, false, 80);
        assert!(rendered.starts_with("  / commands · /tmp/repo"));
        assert!(!rendered.contains('('));
        assert!(rendered.ends_with("echo · ctx ?%"));
    }

    #[test]
    fn statusline_shows_ctrl_o_hint_only_when_foldable_artifact_exists() {
        let snapshot = StatusSnapshot::new("fixture", "echo", PathBuf::from("/tmp/repo"));
        let tokens = TokenUsageSnapshot::default();

        let rendered = status_line_text(&snapshot, &tokens, TurnStatus::Idle, true, 80);
        assert!(rendered.starts_with("  / commands · ctrl+o expand · /tmp/repo"));
        assert!(rendered.ends_with("echo · ctx ?%"));
    }

    #[test]
    fn statusline_compacts_anthropic_model_and_joins_branch_beside_cwd() {
        let mut snapshot = StatusSnapshot::new(
            "anthropic",
            "claude-fable-5",
            PathBuf::from("/home/user/projects/euler"),
        );
        snapshot.reasoning_effort = Some("medium".to_owned());
        snapshot.git_branch = Some("fix/warm-spine-anchor".to_owned());
        let tokens = TokenUsageSnapshot {
            input_tokens: 120_000,
            output_tokens: 50_000,
            reasoning_tokens: Some(25_000),
            context_window_tokens: Some(1_000_000),
            canvas_retained_bytes: None,
            canvas_budget_bytes: None,
            ..TokenUsageSnapshot::default()
        };

        let rendered = status_line_text(&snapshot, &tokens, TurnStatus::Idle, false, 120);

        assert!(rendered.contains("/home/user/projects/euler (fix/warm-spine-anchor)"));
        assert!(rendered.ends_with("fable-5(medium) · ctx 12%"));
        assert_eq!(snapshot.model, "claude-fable-5");
    }

    #[test]
    fn statusline_renders_root_cwd_as_root() {
        let snapshot = StatusSnapshot::new("fixture", "echo", PathBuf::from("/"));
        let tokens = TokenUsageSnapshot::default();

        let rendered = status_line_text(&snapshot, &tokens, TurnStatus::Idle, false, 80);

        assert!(rendered.starts_with("  / commands · /"));
        assert!(rendered.ends_with("echo · ctx ?%"));
    }

    #[test]
    fn home_relative_path_contracts_the_home_prefix_to_a_tilde() {
        let home = PathBuf::from("/Users/eli");

        assert_eq!(
            home_relative_path(&PathBuf::from("/Users/eli/code/euler"), Some(&home)),
            "~/code/euler"
        );
        assert_eq!(home_relative_path(&home, Some(&home)), "~");
        assert_eq!(
            home_relative_path(&PathBuf::from("/var/tmp/euler"), Some(&home)),
            "/var/tmp/euler"
        );
        assert_eq!(home_relative_path(&PathBuf::from(""), Some(&home)), "?");
        // A root home dir is never treated as a meaningful prefix.
        assert_eq!(
            home_relative_path(&PathBuf::from("/etc"), Some(Path::new("/"))),
            "/etc"
        );
    }

    #[test]
    fn deep_home_path_keeps_project_and_repository_tail() {
        let home = PathBuf::from("/Users/eli");

        assert_eq!(
            home_relative_path(&PathBuf::from("/Users/eli/code/2x11/euler"), Some(&home)),
            "../2x11/euler"
        );
        assert_eq!(
            home_relative_path(
                &PathBuf::from("/Users/eli/code/2x11/euler/crates/euler-cli"),
                Some(&home)
            ),
            "../2x11/euler/crates/euler-cli"
        );
        assert_eq!(
            home_relative_path(&PathBuf::from("/Users/eli/work/acme/euler"), Some(&home)),
            "~/work/acme/euler"
        );
    }

    #[test]
    fn statusline_shows_running_queue_and_interrupt_hints() {
        let snapshot = StatusSnapshot::new("fixture", "echo", PathBuf::from("/tmp/repo"));
        let tokens = TokenUsageSnapshot::default();

        let rendered = status_line_text(
            &snapshot,
            &tokens,
            TurnStatus::Running("extension session-export.export".to_owned()),
            false,
            120,
        );

        assert!(rendered.contains("⏎ steer · esc interrupt now"));
        assert!(!rendered.contains("running extension session-export.export"));
    }

    #[test]
    fn statusline_spans_use_faint_footer_token_and_brighter_branch() {
        let mut snapshot = StatusSnapshot::new("fixture", "echo", PathBuf::from("/repo"));
        snapshot.git_branch = Some("main".to_owned());
        let tokens = TokenUsageSnapshot::default();
        let theme = Theme::default();

        let spans = status_line_spans(&snapshot, &tokens, TurnStatus::Idle, false, &theme, 80);

        assert!(spans
            .iter()
            .any(|span| span.content == "echo · " && span.style == theme.status.faint));
        assert!(spans
            .iter()
            .any(|span| span.content == "ctx ?%" && span.style == theme.status.faint));
        assert!(spans
            .iter()
            .any(|span| span.content.contains("/repo") && span.style == theme.status.faint));
        assert!(spans
            .iter()
            .any(|span| span.content == " (main)" && span.style == theme.status.branch));
        assert_eq!(theme.status.faint.fg, Some(theme.palette.gutter));
        assert_eq!(theme.status.branch.fg, Some(theme.palette.muted));
        assert_eq!(theme.status.cost.fg, Some(theme.palette.st_cost));
        assert_eq!(theme.status.ctx.fg, Some(theme.palette.st_ctx));
    }

    #[test]
    fn statusline_ctx_percent_uses_attention_and_failure_thresholds() {
        let snapshot = StatusSnapshot::new("fixture", "echo", PathBuf::from("/repo"));
        let theme = Theme::default();

        assert_eq!(ctx_span_style(&snapshot, &theme, 69), theme.status.faint);
        assert_eq!(ctx_span_style(&snapshot, &theme, 70), theme.status.cost);
        assert_eq!(
            ctx_span_style(&snapshot, &theme, 85),
            Style::default().fg(theme.palette.error)
        );
    }

    #[test]
    fn statusline_shows_pi_style_cumulative_dollar_cost() {
        let snapshot = StatusSnapshot::new("fixture", "echo", PathBuf::from("/repo"));
        let tokens = TokenUsageSnapshot {
            session_cost_picos: 12_345_600_000_000,
            priced_calls: 3,
            ..TokenUsageSnapshot::default()
        };

        let rendered = status_line_text(&snapshot, &tokens, TurnStatus::Idle, false, 120);

        assert!(rendered.ends_with("echo · ctx ?% · $12.346"));
    }

    /// Absence over punctuation: a zero priced subtotal shows no cost chip at
    /// all (even when unpriced calls were made) and never orphans the ` · `
    /// separator. A nonzero subtotal that also has unpriced calls shows the
    /// plain numeric subtotal, unmarked (no `+`); the `$?` / `$N.NNN+` honesty
    /// lives in `/usage`, not the footer.
    #[test]
    fn statusline_cost_chip_absent_at_zero_subtotal_and_unmarked_when_partial() {
        let snapshot = StatusSnapshot::new("fixture", "echo", PathBuf::from("/repo"));
        let unknown = TokenUsageSnapshot {
            unpriced_calls: 1,
            ..TokenUsageSnapshot::default()
        };
        let rendered = status_line_text(&snapshot, &unknown, TurnStatus::Idle, false, 120);
        assert!(rendered.ends_with("echo · ctx ?%"));
        assert!(!rendered.contains('$'));

        let partial = TokenUsageSnapshot {
            session_cost_picos: 1_250_000_000_000,
            priced_calls: 1,
            unpriced_calls: 2,
            ..TokenUsageSnapshot::default()
        };
        let partial_rendered = status_line_text(&snapshot, &partial, TurnStatus::Idle, false, 120);
        assert!(partial_rendered.ends_with("echo · ctx ?% · $1.250"));
        assert!(!partial_rendered.contains('+'));
    }

    #[test]
    fn fixed_point_cost_format_rounds_without_floating_point() {
        assert_eq!(format_cost_picos(0, 3), "$0.000");
        assert_eq!(format_cost_picos(499_999_999, 3), "$0.000");
        assert_eq!(format_cost_picos(500_000_000, 3), "$0.001");
        assert_eq!(format_cost_picos(12_345_678_499_999, 6), "$12.345678");
        assert_eq!(format_cost_picos(12_345_678_500_000, 6), "$12.345679");
    }

    fn ctx_span_style(snapshot: &StatusSnapshot, theme: &Theme, percent: u64) -> Style {
        let tokens = TokenUsageSnapshot {
            input_tokens: percent,
            output_tokens: 0,
            reasoning_tokens: None,
            context_window_tokens: Some(100),
            canvas_retained_bytes: None,
            canvas_budget_bytes: None,
            ..TokenUsageSnapshot::default()
        };
        let label = format!("ctx {percent}%");
        status_line_spans(snapshot, &tokens, TurnStatus::Idle, false, theme, 120)
            .into_iter()
            .find(|span| span.content == label)
            .unwrap_or_else(|| panic!("missing ctx span {label}"))
            .style
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

        let rendered = status_line_text(&snapshot, &tokens, TurnStatus::Idle, false, 72);

        assert!(display_width(&rendered) < 72);
        assert!(rendered.contains("ctx ?%"));

        for width in 1..=10 {
            let rendered = status_line_text(&snapshot, &tokens, TurnStatus::Idle, false, width);
            assert!(
                display_width(&rendered) < usize::from(width),
                "width {width} rendered {rendered:?}"
            );

            let spans = status_line_spans(
                &snapshot,
                &tokens,
                TurnStatus::Idle,
                false,
                &Theme::default(),
                width,
            );
            assert!(
                spans_width(&spans) < usize::from(width),
                "width {width} rendered spans {spans:?}"
            );
        }
    }

    /// Footer §4: at narrow widths the directory truncates from the left
    /// (`…/2x11/euler`) before the hints, branch, or right cluster give up
    /// any of their content.
    #[test]
    fn narrow_width_truncates_directory_before_anything_else() {
        let mut snapshot =
            StatusSnapshot::new("fixture", "echo", PathBuf::from("/Users/x/code/2x11/euler"));
        snapshot.git_branch = Some("main".to_owned());
        let tokens = TokenUsageSnapshot::default();

        // Sized so the directory's budget is exactly 12 cells — enough for
        // `…/2x11/euler` (the ellipsis plus the last 11 characters) and not
        // one cell more. At zero cost the right cluster is `echo · ctx ?%`
        // (no cost chip), so the width that leaves a 12-cell path budget is 49.
        let rendered = status_line_text(&snapshot, &tokens, TurnStatus::Idle, false, 49);

        assert!(rendered.contains("/ commands · …/2x11/euler (main)"));
        assert!(rendered.ends_with("echo · ctx ?%"));
        assert!(display_width(&rendered) < 49);
    }

    /// Issue #59: a narrower budget that no longer fits the `2x11` component
    /// must drop that whole component and land on the next `/` boundary
    /// (`…/euler`), never bisect it (`…11/euler`).
    #[test]
    fn narrow_width_truncates_directory_at_component_boundary_not_mid_component() {
        let mut snapshot =
            StatusSnapshot::new("fixture", "echo", PathBuf::from("/Users/x/code/2x11/euler"));
        snapshot.git_branch = Some("main".to_owned());
        let tokens = TokenUsageSnapshot::default();

        // 4 cells narrower than the `…/2x11/euler` case above (49 → 45),
        // exactly enough budget for the pre-fix raw cut to land inside "2x11"
        // ("11/euler"); the fix must sacrifice the whole component instead.
        let rendered = status_line_text(&snapshot, &tokens, TurnStatus::Idle, false, 45);

        assert!(rendered.contains("/ commands · …/euler (main)"));
        assert!(!rendered.contains("11/euler"));
        assert!(display_width(&rendered) < 45);
    }

    /// Footer §4 (v4 dogfood): once the right cluster grows (model(effort) · ctx ·
    /// session name), a width can exist where the clusters still don't fit
    /// even after `status_left_spans` squeezes the path to nothing — the
    /// fallback safety net used to truncate the left cluster to exactly
    /// `width - right_width` with no gap reserved, so `(inte` butted
    /// directly against `z-ai/glm-5.2(medium) · ctx …` with zero separation. At
    /// every width where the left cluster still has content left to show,
    /// the clusters must keep at least one column of gap, and the right
    /// cluster's full text must survive intact — the left cluster
    /// (cwd/branch) truncates harder instead.
    #[test]
    fn narrow_width_keeps_a_gap_between_clusters_and_truncates_left_harder() {
        let mut snapshot = StatusSnapshot::new(
            "openrouter",
            "z-ai/glm-5.2",
            PathBuf::from("/Users/x/code/2x11/euler"),
        );
        snapshot.git_branch = Some("integration".to_owned());
        snapshot.session_name = Some("apple-terminal-v4-test".to_owned());
        snapshot.reasoning_effort = Some("medium".to_owned());
        let tokens = TokenUsageSnapshot::default();
        // Zero cost: no chip, so the right cluster carries no `$0` segment.
        let right_cluster = "z-ai/glm-5.2(medium) · ctx ?% · apple-terminal-v4-test";

        // 66 is the exact width from the owner's dogfood screenshot where
        // the fallback collided (`(int` glued to `z-ai/glm-5.2`). Sweep a
        // wide band of widths — enough above `right_cluster`'s own width
        // (plus the 2-cell indent and 1-cell body-width margin) that the
        // right cluster always fits, where the gap invariant must hold.
        let min_width = display_width(right_cluster) as u16 + 4;
        for width in min_width..120u16 {
            let rendered = status_line_text(&snapshot, &tokens, TurnStatus::Idle, false, width);
            assert!(
                display_width(&rendered) <= usize::from(width),
                "width {width} rendered {rendered:?} overflows"
            );
            assert!(
                rendered.ends_with(right_cluster),
                "width {width} rendered {rendered:?} must keep the right cluster intact"
            );
            let left_of_right = rendered
                .strip_suffix(right_cluster)
                .expect("right cluster suffix");
            assert!(
                left_of_right.ends_with(' '),
                "width {width} rendered {rendered:?} must keep >=1 column of gap before the right cluster"
            );
        }
    }

    #[test]
    fn absent_context_window_tokens_render_unknown_fill() {
        let snapshot = TokenUsageSnapshot {
            input_tokens: 100,
            output_tokens: 25,
            ..TokenUsageSnapshot::default()
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
            canvas_retained_bytes: None,
            canvas_budget_bytes: None,
            ..TokenUsageSnapshot::default()
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
            canvas_retained_bytes: None,
            canvas_budget_bytes: None,
            ..TokenUsageSnapshot::default()
        };

        assert_eq!(context_segment(&snapshot), "Context 13% used");

        let clamped = TokenUsageSnapshot {
            input_tokens: 1_000,
            output_tokens: 0,
            reasoning_tokens: None,
            context_window_tokens: Some(1_000),
            canvas_retained_bytes: None,
            canvas_budget_bytes: None,
            ..TokenUsageSnapshot::default()
        };

        assert_eq!(context_segment(&clamped), "Context 99% used");
    }
}
