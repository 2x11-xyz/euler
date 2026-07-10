use super::*;

pub(in crate::ui::transcript) struct PermissionDecisionView<'a> {
    pub(in crate::ui::transcript) capability: &'a str,
    pub(in crate::ui::transcript) decision: &'a str,
    pub(in crate::ui::transcript) allowed: Option<bool>,
    pub(in crate::ui::transcript) grant_scope: Option<&'a str>,
    pub(in crate::ui::transcript) instruction: Option<&'a str>,
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
        (Some(true), _) => "allowed once",
        _ => "",
    };
    let inst = view
        .instruction
        .filter(|instruction| !instruction.is_empty());
    let capability = view.capability;
    let decision = view.decision;
    let text = if view.allowed == Some(true) && !scope_label.is_empty() && !capability.is_empty() {
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
}

pub(in crate::ui::transcript) struct PermissionAskView<'a> {
    pub(in crate::ui::transcript) capability: &'a str,
    pub(in crate::ui::transcript) reason: &'a str,
    pub(in crate::ui::transcript) command: Option<&'a str>,
    pub(in crate::ui::transcript) scope_prefix: Option<&'a str>,
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
    Hint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PermissionPanelRow {
    text: String,
    style: PermissionPanelRowStyle,
}

impl PermissionPanelRow {
    fn title(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PermissionPanelRowStyle::Title,
        }
    }

    fn metadata(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PermissionPanelRowStyle::Metadata,
        }
    }

    fn body(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PermissionPanelRowStyle::Body,
        }
    }

    fn selected(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PermissionPanelRowStyle::Selected,
        }
    }

    fn hint(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PermissionPanelRowStyle::Hint,
        }
    }
}

pub(in crate::ui::transcript) fn render_permission_ask(
    lines: &mut Vec<Line<'static>>,
    ask: PermissionAskView<'_>,
    theme: &Theme,
    width: u16,
) {
    let preview = ask
        .command
        .filter(|command| !command.is_empty())
        .map(|command| format!("command: $ {command}"))
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
        PermissionPanelRow::title(title),
        PermissionPanelRow::metadata(format!(
            "Approval required · {} · cwd {}",
            ask.capability,
            current_cwd_label()
        )),
        PermissionPanelRow::body(preview),
        PermissionPanelRow::metadata(consequences_row(
            ask.capability,
            ask.scope_prefix,
            ask.prior_count,
        )),
    ];
    rows.extend(
        crate::ui::patch_approval::approval_option_lines(
            ask.capability,
            ask.scope_prefix,
            ask.selected_option,
        )
        .into_iter()
        .map(|line| {
            if line.selected {
                PermissionPanelRow::selected(line.text)
            } else if line.hint {
                PermissionPanelRow::hint(line.text)
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
        for segment in wrap_text(&row.text, inner_width) {
            push_permission_panel_row(lines, &segment, inner_width, row.style, theme);
        }
    }
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(panel_width.saturating_sub(2))),
        theme.transcript.permission,
    )));
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
        PermissionPanelRowStyle::Metadata | PermissionPanelRowStyle::Hint => theme.transcript.muted,
        PermissionPanelRowStyle::Body => theme.transcript.body,
        PermissionPanelRowStyle::Selected => theme.surfaces.transcript.selection,
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

fn consequences_row(capability: &str, scope_prefix: Option<&str>, prior_count: usize) -> String {
    let write_scope = if capability == "fs-write" {
        scope_prefix
            .filter(|prefix| !prefix.trim().is_empty())
            .unwrap_or("unknown")
    } else {
        "unknown"
    };
    let network = if capability == "network" {
        "requested"
    } else {
        "unknown"
    };
    format!(
        "consequences: write scope {write_scope} · network {network} · duration unknown · ran-before {prior_count}×"
    )
}
