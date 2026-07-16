use super::*;

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
            "Raw DAG object — euler.causal_dag.v3 artifact",
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
