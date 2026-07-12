use super::*;

impl ReplacementPicker {
    pub(in crate::ui::bottom_surface) fn render_causal_dag_canvas_lines(
        &self,
        theme: &Theme,
        width: u16,
    ) -> Vec<CanvasLine> {
        self.causal_dag_lines(width, |text, selected| {
            if selected {
                select_bar_canvas_line(&text, width, theme)
            } else {
                CanvasLine::plain_lossy(text)
            }
        })
    }

    pub(super) fn render_causal_dag_lines(&self, width: u16) -> Vec<String> {
        self.causal_dag_lines(width, |text, _| text)
    }

    fn causal_dag_lines<T>(&self, width: u16, row: impl Fn(String, bool) -> T) -> Vec<T> {
        let width = usize::from(width);
        let count = self.items.len();
        let position = if count == 0 {
            "0/0".to_owned()
        } else {
            format!("{}/{count}", self.selected + 1)
        };
        let mut lines = vec![row(
            truncate_display(&format!("{}  {position}", self.title), width),
            false,
        )];
        lines.push(row("─".repeat(width), false));
        for (index, item) in self.items.iter().enumerate() {
            let selected = index == self.selected;
            let marker = if selected { "›" } else { " " };
            let command = format!("{:<10}", item.label);
            let suffix = item.status.as_deref().unwrap_or_default();
            let detail = item.detail.as_deref().unwrap_or_default();
            let suffix_width = display_width(suffix);
            let body_width =
                width.saturating_sub(2 + suffix_width + usize::from(!suffix.is_empty()));
            let body = truncate_display(&format!("{marker} {command}{detail}"), body_width);
            let text = if suffix.is_empty() {
                body
            } else {
                let padding = width
                    .saturating_sub(display_width(&body))
                    .saturating_sub(suffix_width);
                format!("{body}{}{suffix}", " ".repeat(padding.max(1)))
            };
            lines.push(row(truncate_display(&text, width), selected));
        }
        lines.push(row("─".repeat(width), false));
        let footer = if self.kind == PickerKind::CausalDagFormats {
            "↑↓ move · ⏎ export · ⌫ back · esc cancel"
        } else {
            "↑↓ move · ⏎ select · esc cancel"
        };
        lines.push(row(truncate_display(footer, width), false));
        lines
    }
}

pub(super) fn action_items(stats: CausalDagStats) -> Vec<PickerItem> {
    vec![
        item(
            "view",
            "Show current graph — dead ends · active path · open",
            None,
            extension_action("view", serde_json::json!({})),
        ),
        item(
            "export",
            "Save graph — html · json · svg · dot · md …",
            None,
            CommandAction::OpenCausalDagExport { stats },
        ),
        item(
            "refresh",
            "Re-observe recent activity and update graph",
            None,
            extension_action("refresh", serde_json::json!({})),
        ),
    ]
}

pub(super) fn format_items() -> Vec<PickerItem> {
    [
        (
            "html",
            "Interactive viewer — Tufte 2D, day/night, hover",
            ".html",
        ),
        (
            "json",
            "Raw DAG object — euler.causal_dag.v2 artifact",
            ".json",
        ),
        ("svg", "Static vector render of graph", ".svg"),
        ("dot", "Graphviz source (external graph tooling)", ".dot"),
        (
            "markdown",
            "Readable outline — backbone · dead ends · open",
            ".md",
        ),
        ("summary", "Compact GRAPH: slot text", ".txt"),
    ]
    .into_iter()
    .map(|(format, detail, suffix)| {
        item(
            format,
            detail,
            Some(suffix),
            extension_action("export", serde_json::json!({"format": format})),
        )
    })
    .collect()
}

pub(super) fn short_session_id(session_id: &str) -> String {
    if session_id.chars().count() <= 8 {
        session_id.to_owned()
    } else {
        format!("{}…", session_id.chars().take(6).collect::<String>())
    }
}

fn extension_action(command: &str, input: serde_json::Value) -> CommandAction {
    CommandAction::ExtensionRun {
        id: "causal-dag".to_owned(),
        command: command.to_owned(),
        input,
        raw_args: None,
    }
}

fn item(label: &str, detail: &str, status: Option<&str>, action: CommandAction) -> PickerItem {
    PickerItem {
        label: label.to_owned(),
        detail: Some(detail.to_owned()),
        status: status.map(str::to_owned),
        group: None,
        provider_tag: None,
        current: false,
        action,
    }
}
