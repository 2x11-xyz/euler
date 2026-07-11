use super::*;

pub(in crate::ui::transcript) struct PermissionDecisionView<'a> {
    pub(in crate::ui::transcript) capability: &'a str,
    pub(in crate::ui::transcript) decision: &'a str,
    pub(in crate::ui::transcript) allowed: Option<bool>,
    pub(in crate::ui::transcript) grant_scope: Option<&'a str>,
    pub(in crate::ui::transcript) instruction: Option<&'a str>,
    /// `Some("guardian")` when an automated reviewer decided (ADR 0011).
    pub(in crate::ui::transcript) decision_source: Option<&'a str>,
    /// Guardian rationale; rendered as a dim follow-up line.
    pub(in crate::ui::transcript) rationale: Option<&'a str>,
}

pub(in crate::ui::transcript) fn render_permission_decision(
    lines: &mut Vec<Line<'static>>,
    view: PermissionDecisionView<'_>,
    theme: &Theme,
    width: u16,
) {
    // v2 (§0): the ✓/✗ lives in the spine anchor (green/red); the record
    // text is dim — gold means pending attention, never a settled decision
    // (§3). No redundant "({decision})" suffix (audit S3).
    let scope_label = match (view.allowed, view.grant_scope) {
        (Some(true), Some("session")) => "allowed for session",
        (Some(true), Some("project")) => "allowed for project",
        (Some(true), Some("user")) => "allowed always (user rule)",
        (Some(true), _) => "allowed once",
        _ => "",
    };
    let inst = view
        .instruction
        .filter(|instruction| !instruction.is_empty());
    let capability = view.capability;
    let decision = view.decision;
    let mut text =
        if view.allowed == Some(true) && !scope_label.is_empty() && !capability.is_empty() {
            format!("{scope_label} · {capability}")
        } else if view.allowed == Some(false) && inst.is_some() && !capability.is_empty() {
            format!("denied · {capability} — \"{}\"", inst.unwrap_or_default())
        } else if view.allowed == Some(false) && decision.contains("cancel") {
            format!("permission canceled · {capability}")
        } else if view.allowed == Some(false) && !capability.is_empty() {
            format!("denied · {capability}")
        } else if capability.is_empty() {
            format!("permission decided · {decision}")
        } else {
            format!("permission decided · {capability}")
        };
    // ADR 0011: automated decisions are always distinguishable from user
    // decisions — a quiet `· guardian` tag on the record line.
    let source = view.decision_source.filter(|source| !source.is_empty());
    if let Some(source) = source {
        text.push_str(&format!(" · {source}"));
    }
    push_wrapped_with_prefix(
        lines,
        CellPrefixes {
            first: blank_gutter(),
            next: blank_gutter(),
        },
        &text,
        theme.transcript.muted,
        theme,
        width,
    );
    // Guardian rationale as a dim follow-up line, only when a reviewer is
    // tagged: user decisions carry no rationale payload.
    if let Some(rationale) = view
        .rationale
        .filter(|rationale| !rationale.is_empty() && source.is_some())
    {
        push_wrapped_with_prefix(
            lines,
            CellPrefixes {
                first: blank_gutter(),
                next: blank_gutter(),
            },
            &format!("\"{rationale}\""),
            theme.transcript.muted,
            theme,
            width,
        );
    }
}

pub(in crate::ui::transcript) struct PermissionAskView<'a> {
    pub(in crate::ui::transcript) capability: &'a str,
    pub(in crate::ui::transcript) reason: &'a str,
    pub(in crate::ui::transcript) command: Option<&'a str>,
    pub(in crate::ui::transcript) scope_prefix: Option<&'a str>,
    /// `Some` only when the durable `u  Allow <prefix> * always` option is
    /// offerable (simple shell command + loaded user store).
    pub(in crate::ui::transcript) user_rule_prefix: Option<&'a str>,
    pub(in crate::ui::transcript) prior_count: usize,
    pub(in crate::ui::transcript) selected_option: crate::ui::patch_approval::ApprovalOption,
    pub(in crate::ui::transcript) companion_name: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PermissionPanelRowStyle {
    Title,
    Metadata,
    Body,
    Selected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PermissionPanelRow {
    text: String,
    style: PermissionPanelRowStyle,
    /// Faint text placed in the title row's right corner (capability · cwd),
    /// replacing the old "Approval required · " label row (review v2 §7).
    corner: Option<String>,
}

impl PermissionPanelRow {
    fn title_with_corner(text: impl Into<String>, corner: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PermissionPanelRowStyle::Title,
            corner: Some(corner.into()),
        }
    }

    fn metadata(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PermissionPanelRowStyle::Metadata,
            corner: None,
        }
    }

    fn body(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PermissionPanelRowStyle::Body,
            corner: None,
        }
    }

    fn selected(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PermissionPanelRowStyle::Selected,
            corner: None,
        }
    }
}

pub(in crate::ui::transcript) fn render_permission_ask(
    lines: &mut Vec<Line<'static>>,
    ask: PermissionAskView<'_>,
    theme: &Theme,
    width: u16,
) {
    // v2.1 (§7b): bare `$ command`, no "command:" label.
    let preview = ask
        .command
        .filter(|command| !command.is_empty())
        .map(|command| format!("$ {command}"))
        .unwrap_or_else(|| format!("request: {}", ask.reason));
    let title = match ask.companion_name.filter(|name| !name.is_empty()) {
        Some(name) => format!(
            "{} · {} {name}",
            crate::ui::patch_approval::approval_title(ask.capability),
            crate::ui::glyphs::companion_glyph()
        ),
        None => crate::ui::patch_approval::approval_title(ask.capability).to_owned(),
    };
    let mut rows = vec![
        PermissionPanelRow::title_with_corner(
            title,
            format!("{} · cwd {}", ask.capability, current_cwd_label()),
        ),
        PermissionPanelRow::body(preview),
    ];
    if let Some(consequences) = consequences_row(ask.capability, ask.scope_prefix, ask.prior_count)
    {
        rows.push(PermissionPanelRow::metadata(consequences));
    }
    // v2.1 (§7b): a blank line separates the command block from the options.
    rows.push(PermissionPanelRow::body(String::new()));
    rows.extend(
        crate::ui::patch_approval::approval_option_lines(
            ask.capability,
            ask.scope_prefix,
            ask.user_rule_prefix,
            ask.selected_option,
        )
        .into_iter()
        .map(|line| {
            if line.selected {
                PermissionPanelRow::selected(line.text)
            } else {
                PermissionPanelRow::body(line.text)
            }
        }),
    );
    push_bordered_permission_panel(lines, &rows, theme, width);
}

fn push_bordered_permission_panel(
    lines: &mut Vec<Line<'static>>,
    rows: &[PermissionPanelRow],
    theme: &Theme,
    width: u16,
) {
    let panel_width = usize::from(width.clamp(8, 96));
    let inner_width = panel_width.saturating_sub(4).max(1);
    lines.push(Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(panel_width.saturating_sub(2))),
        theme.transcript.permission,
    )));
    for row in rows {
        if let Some(corner) = &row.corner {
            lines.push(bordered_title_corner_line(
                &row.text,
                corner,
                inner_width,
                theme,
            ));
            continue;
        }
        for segment in wrap_text(&row.text, inner_width) {
            push_permission_panel_row(lines, &segment, inner_width, row.style, theme);
        }
    }
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(panel_width.saturating_sub(2))),
        theme.transcript.permission,
    )));
}

/// Title row with capability · cwd faint in the right corner (review v2 §7):
/// replaces the old "Approval required · " label row. Degrades gracefully —
/// the corner is dropped first, then truncated, when the panel is too
/// narrow to hold both.
fn bordered_title_corner_line(
    title: &str,
    corner: &str,
    inner_width: usize,
    theme: &Theme,
) -> Line<'static> {
    let title_width = display_width(title);
    let gap = 2usize;
    let corner_budget = inner_width.saturating_sub(title_width + gap);
    let corner_text = if corner_budget == 0 {
        String::new()
    } else if display_width(corner) <= corner_budget {
        corner.to_owned()
    } else {
        crate::ui::text::truncate_display(corner, corner_budget)
    };
    let used = title_width + display_width(&corner_text) + usize::from(!corner_text.is_empty());
    let padding = inner_width.saturating_sub(used);
    let title_style = permission_panel_row_style(PermissionPanelRowStyle::Title, theme);
    let mut spans = vec![
        Span::styled("│ ", theme.transcript.permission),
        Span::styled(title.to_owned(), title_style),
        Span::styled(" ".repeat(padding), title_style),
    ];
    if !corner_text.is_empty() {
        spans.push(Span::styled(" ", title_style));
        spans.push(Span::styled(corner_text, theme.transcript.muted));
    }
    spans.push(Span::styled(" │", theme.transcript.permission));
    Line::from(spans)
}

fn push_permission_panel_row(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    inner_width: usize,
    style: PermissionPanelRowStyle,
    theme: &Theme,
) {
    let padding = inner_width.saturating_sub(display_width(text));
    let content_style = permission_panel_row_style(style, theme);
    lines.push(Line::from(vec![
        Span::styled("│ ", theme.transcript.permission),
        Span::styled(text.to_owned(), content_style),
        Span::styled(" ".repeat(padding), content_style),
        Span::styled(" │", theme.transcript.permission),
    ]));
}

fn permission_panel_row_style(
    style: PermissionPanelRowStyle,
    theme: &Theme,
) -> ratatui::style::Style {
    match style {
        PermissionPanelRowStyle::Title => theme.transcript.permission,
        PermissionPanelRowStyle::Metadata => theme.transcript.muted,
        PermissionPanelRowStyle::Body => theme.transcript.body,
        // v2.1 (§7b): select-bg + gold text for the selected option.
        PermissionPanelRowStyle::Selected => ratatui::style::Style::default()
            .fg(theme.palette.warning)
            .bg(theme.palette.selection),
    }
}

fn current_cwd_label() -> String {
    std::env::current_dir()
        .map(|path| compact_cwd_label(&path.display().to_string()))
        .unwrap_or_else(|_| "unknown".to_owned())
}

/// Bounded cwd for the approval panel corner: keep the path tail so the row
/// wraps identically regardless of how deep the workspace lives (§9 keeps
/// the consequences row to a predictable width; also test hermeticity —
/// panels must render the same row count from any checkout location).
fn compact_cwd_label(path: &str) -> String {
    const MAX_CWD_CHARS: usize = 24;
    let chars = path.chars().count();
    if chars <= MAX_CWD_CHARS {
        return path.to_owned();
    }
    let tail: String = path
        .chars()
        .rev()
        .take(MAX_CWD_CHARS - 1)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

/// Only known fields render (review v2 §7b): omit the row entirely while
/// every field is unknown, rather than pad it with "network unknown ·
/// duration unknown" filler.
fn consequences_row(
    capability: &str,
    scope_prefix: Option<&str>,
    prior_count: usize,
) -> Option<String> {
    let mut parts = Vec::new();
    if capability == "fs-write" {
        if let Some(scope) = scope_prefix.filter(|prefix| !prefix.trim().is_empty()) {
            parts.push(format!("write scope {scope}"));
        }
    }
    if capability == "network" {
        parts.push("network requested".to_owned());
    }
    if prior_count > 0 {
        parts.push(format!("ran-before {prior_count}×"));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("consequences: {}", parts.join(" · ")))
}
