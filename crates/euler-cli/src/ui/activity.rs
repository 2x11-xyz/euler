use super::text::{content_width, display_width, wrap_text, GUTTER_WIDTH};
use super::theme::Theme;
use super::transcript::normalized_shell_command;
use euler_event::{EventEnvelope, EventKind};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Span},
    widgets::Widget,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub struct ToolFlavor {
    pub flavor: &'static str,
    pub label: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub enum ActivityItem {
    Status(String),
    ToolGroup {
        flavor: &'static str,
        label: &'static str,
        details: Vec<String>,
    },
}

#[allow(dead_code)]
pub fn tool_flavor(name: &str) -> ToolFlavor {
    match name {
        "read_file" | "git_status" | "git_diff" => ToolFlavor {
            flavor: "explore",
            label: "explore",
        },
        "edit_file" => ToolFlavor {
            flavor: "edit",
            label: "edit",
        },
        "run_shell" => ToolFlavor {
            flavor: "bash",
            label: "bash",
        },
        _ => ToolFlavor {
            flavor: "tool",
            label: "tool",
        },
    }
}

#[allow(dead_code)]
pub fn project_activity(events: &[EventEnvelope]) -> Vec<ActivityItem> {
    project_activity_with_live_status(events, false)
}

#[allow(dead_code)]
pub fn project_activity_with_live_status(
    events: &[EventEnvelope],
    show_live_status: bool,
) -> Vec<ActivityItem> {
    let mut entries = Vec::new();
    let mut tools = Vec::new();

    for event in events {
        match event.kind.as_str() {
            EventKind::ASSISTANT_ACTIVITY => {
                if let Some(text) = activity_text(event) {
                    entries.push(ActivityEntry::Status(text));
                }
            }
            EventKind::TOOL_CALL => {
                if let Some(activity) = tool_activity(event) {
                    let index = tools.len();
                    tools.push(activity);
                    entries.push(ActivityEntry::Tool(index));
                }
            }
            EventKind::TOOL_RESULT => {
                project_tool_result(event, &mut entries, &mut tools);
            }
            _ => {}
        }
    }

    let mut items = activity_items_from_entries(&entries, &tools);
    if show_live_status {
        if let Some(status) = live_status(events) {
            items.push(ActivityItem::Status(status));
        }
    }
    items
}

#[allow(dead_code)]
pub fn activity_widget<'a>(events: &'a [EventEnvelope], theme: &'a Theme) -> ActivityWidget<'a> {
    ActivityWidget::new(events, theme)
}

#[allow(dead_code)]
pub struct ActivityWidget<'a> {
    events: &'a [EventEnvelope],
    theme: &'a Theme,
    show_live_status: bool,
}

#[allow(dead_code)]
impl<'a> ActivityWidget<'a> {
    pub fn new(events: &'a [EventEnvelope], theme: &'a Theme) -> Self {
        Self {
            events,
            theme,
            show_live_status: false,
        }
    }

    pub fn live_status(mut self, show_live_status: bool) -> Self {
        self.show_live_status = show_live_status;
        self
    }
}

impl Widget for ActivityWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let items = project_activity_with_live_status(self.events, self.show_live_status);
        let lines = render_activity_items(&items, self.theme, area.width);
        let paragraph = ratatui::widgets::Paragraph::new(lines);
        paragraph.render(area, buf);
    }
}

fn render_activity_items(items: &[ActivityItem], theme: &Theme, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            lines.push(Line::from(""));
        }

        match item {
            ActivityItem::Status(text) => {
                let (text, style) = if text.starts_with('✱') {
                    (text.clone(), theme.transcript.reasoning)
                } else {
                    (format!("• {text}"), theme.activity.status)
                };
                push_wrapped(&mut lines, "    ", &text, style, theme, width);
            }
            ActivityItem::ToolGroup { label, details, .. } => {
                push_tool_group(&mut lines, label, details, theme, width);
            }
        }
    }

    lines
}

fn push_tool_group(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    details: &[String],
    theme: &Theme,
    width: u16,
) {
    if label == "bash" {
        for detail in details {
            push_wrapped(
                lines,
                "    ",
                &format!("bash $ {detail}"),
                theme.activity.header,
                theme,
                width,
            );
        }
        return;
    }

    let steps = details.len();
    let header = if steps == 0 {
        label.to_owned()
    } else {
        format!("{label} · {steps} steps")
    };
    push_wrapped(lines, "    ", &header, theme.activity.header, theme, width);
    for (index, detail) in details.iter().enumerate() {
        let gutter = if index + 1 == details.len() {
            "  └ "
        } else {
            "  ├ "
        };
        push_wrapped(lines, gutter, detail, theme.activity.detail, theme, width);
    }
}

fn activity_text(event: &EventEnvelope) -> Option<String> {
    payload_string(event, "message")
        .or_else(|| payload_string(event, "summary"))
        .or_else(|| payload_string(event, "content"))
        .filter(|text| !text.is_empty())
}

fn payload_string(event: &EventEnvelope, key: &str) -> Option<String> {
    event.payload.get(key)?.as_str().map(str::to_owned)
}

fn tool_name(event: &EventEnvelope) -> Option<String> {
    payload_string(event, "name").filter(|name| !name.is_empty())
}

fn tool_input_string(event: &EventEnvelope, key: &str) -> Option<String> {
    event
        .payload
        .get("input")?
        .get(key)?
        .as_str()
        .map(str::to_owned)
}

fn live_status(events: &[EventEnvelope]) -> Option<String> {
    let event = events
        .iter()
        .rev()
        .find(|event| event.kind.as_str() != EventKind::MODEL_RESULT)?;
    match event.kind.as_str() {
        EventKind::MODEL_CALL => Some("Contacting model".to_owned()),
        EventKind::MODEL_DELTA => match payload_string(event, "kind").as_deref() {
            Some("reasoning") => Some("✱ thinking · 0s · esc interrupt".to_owned()),
            Some("text") => Some("Streaming response".to_owned()),
            _ => None,
        },
        _ => None,
    }
}

fn activity_items_from_entries(
    entries: &[ActivityEntry],
    tools: &[ToolActivity],
) -> Vec<ActivityItem> {
    let mut items = Vec::new();
    let mut groups: Vec<ToolGroupBuilder> = Vec::new();

    for entry in entries {
        match entry {
            ActivityEntry::Status(text) => {
                flush_tool_groups(&mut items, &mut groups);
                items.push(ActivityItem::Status(text.clone()));
            }
            ActivityEntry::Tool(index) => {
                if let Some(tool) = tools.get(*index).filter(|tool| tool.visible) {
                    push_tool_detail(&mut groups, &tool.name, tool.detail.clone());
                }
            }
        }
    }

    flush_tool_groups(&mut items, &mut groups);
    items
}

fn flush_tool_groups(items: &mut Vec<ActivityItem>, groups: &mut Vec<ToolGroupBuilder>) {
    items.extend(groups.drain(..).map(ToolGroupBuilder::finish));
}

fn push_tool_detail(groups: &mut Vec<ToolGroupBuilder>, name: &str, detail: String) {
    let flavor = tool_flavor(name);
    if let Some(group) = groups
        .iter_mut()
        .find(|group| group.flavor == flavor.flavor)
    {
        group.details.push(detail);
    } else {
        groups.push(ToolGroupBuilder {
            flavor: flavor.flavor,
            label: flavor.label,
            details: vec![detail],
        });
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ActivityEntry {
    Status(String),
    Tool(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ToolActivity {
    event_id: String,
    call_id: Option<String>,
    name: String,
    detail: String,
    visible: bool,
}

fn tool_activity(event: &EventEnvelope) -> Option<ToolActivity> {
    let name = tool_name(event)?;
    Some(ToolActivity {
        event_id: event.id.clone(),
        call_id: payload_string(event, "id"),
        detail: tool_detail(event, &name),
        name,
        visible: true,
    })
}

fn tool_detail(event: &EventEnvelope, name: &str) -> String {
    match name {
        "read_file" => tool_input_string(event, "path")
            .map(|path| format!("Read {path}"))
            .unwrap_or_else(|| "Read file".to_owned()),
        "git_status" => "Git status".to_owned(),
        "git_diff" => "Git diff".to_owned(),
        "edit_file" => tool_input_string(event, "path")
            .map(|path| format!("Edit {path}"))
            .unwrap_or_else(|| "Edit file".to_owned()),
        "run_shell" => tool_input_string(event, "command")
            .map(|command| normalized_shell_command(&command))
            .filter(|command| !command.is_empty())
            .unwrap_or_else(|| "Run command".to_owned()),
        _ => format!("Use {name}"),
    }
}

fn failed_tool_activity(event: &EventEnvelope) -> Option<ToolActivity> {
    let name = tool_name(event)?;
    Some(ToolActivity {
        event_id: event.id.clone(),
        call_id: payload_string(event, "id"),
        detail: format!("{name} failed"),
        name,
        visible: true,
    })
}

fn project_tool_result(
    result: &EventEnvelope,
    entries: &mut Vec<ActivityEntry>,
    tools: &mut Vec<ToolActivity>,
) {
    let ok = result
        .payload
        .get("ok")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    if ok {
        hide_matching_tool(tools, result);
    } else if let Some(activity) = failed_tool_activity(result) {
        hide_matching_tool(tools, result);
        let index = tools.len();
        tools.push(activity);
        entries.push(ActivityEntry::Tool(index));
    }
}

fn hide_matching_tool(active_tools: &mut [ToolActivity], result: &EventEnvelope) {
    let result_id = payload_string(result, "id");
    let result_parent = result.parent.as_deref();
    let result_name = tool_name(result);
    if let Some(index) = active_tools.iter().position(|tool| {
        tool.visible
            && (result_id
                .as_deref()
                .is_some_and(|id| tool.call_id.as_deref() == Some(id) || tool.event_id == id)
                || result_parent.is_some_and(|parent| tool.event_id == parent))
    }) {
        active_tools[index].visible = false;
        return;
    }

    if let Some(name) = result_name {
        let mut matches = active_tools
            .iter()
            .enumerate()
            .filter_map(|(index, tool)| (tool.visible && tool.name == name).then_some(index));
        if let (Some(index), None) = (matches.next(), matches.next()) {
            active_tools[index].visible = false;
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ToolGroupBuilder {
    flavor: &'static str,
    label: &'static str,
    details: Vec<String>,
}

impl ToolGroupBuilder {
    fn finish(self) -> ActivityItem {
        ActivityItem::ToolGroup {
            flavor: self.flavor,
            label: self.label,
            details: self.details,
        }
    }
}

fn push_wrapped(
    lines: &mut Vec<Line<'static>>,
    gutter: &'static str,
    text: &str,
    style: ratatui::style::Style,
    theme: &Theme,
    width: u16,
) {
    debug_assert_eq!(display_width(gutter), GUTTER_WIDTH);

    for segment in wrap_text(text, content_width(width)) {
        lines.push(Line::from(vec![
            Span::styled(gutter.to_owned(), theme.activity.gutter),
            Span::styled(segment, style),
        ]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{test_backend::VT100Backend, theme::Theme};
    use euler_event::object;
    use ratatui::{layout::Rect, Terminal};

    #[test]
    fn tool_flavor_dispatch_table_is_exact() {
        assert_eq!(
            tool_flavor("read_file"),
            ToolFlavor {
                flavor: "explore",
                label: "explore"
            }
        );
        assert_eq!(
            tool_flavor("git_status"),
            ToolFlavor {
                flavor: "explore",
                label: "explore"
            }
        );
        assert_eq!(
            tool_flavor("git_diff"),
            ToolFlavor {
                flavor: "explore",
                label: "explore"
            }
        );
        assert_eq!(
            tool_flavor("edit_file"),
            ToolFlavor {
                flavor: "edit",
                label: "edit"
            }
        );
        assert_eq!(
            tool_flavor("run_shell"),
            ToolFlavor {
                flavor: "bash",
                label: "bash"
            }
        );
        assert_eq!(
            tool_flavor("something_else"),
            ToolFlavor {
                flavor: "tool",
                label: "tool"
            }
        );
    }

    #[test]
    fn assistant_activity_uses_message_summary_content_order() {
        let events = vec![
            event(
                EventKind::ASSISTANT_ACTIVITY,
                object([
                    ("message", "from message".into()),
                    ("summary", "from summary".into()),
                    ("content", "from content".into()),
                ]),
            ),
            event(
                EventKind::ASSISTANT_ACTIVITY,
                object([
                    ("summary", "summary fallback".into()),
                    ("content", "content fallback".into()),
                ]),
            ),
            event(
                EventKind::ASSISTANT_ACTIVITY,
                object([("content", "content fallback".into())]),
            ),
        ];

        assert_eq!(
            project_activity(&events),
            vec![
                ActivityItem::Status("from message".to_owned()),
                ActivityItem::Status("summary fallback".to_owned()),
                ActivityItem::Status("content fallback".to_owned()),
            ]
        );
    }

    #[test]
    fn groups_tools_by_first_seen_flavor_order() {
        let events = vec![
            event(
                EventKind::TOOL_CALL,
                object([("name", "git_status".into())]),
            ),
            event(EventKind::TOOL_CALL, object([("name", "edit_file".into())])),
            event(EventKind::TOOL_CALL, object([("name", "read_file".into())])),
            event(EventKind::TOOL_CALL, object([("name", "run_shell".into())])),
            event(
                EventKind::TOOL_CALL,
                object([("name", "custom_tool".into())]),
            ),
        ];

        assert_eq!(
            project_activity(&events),
            vec![
                ActivityItem::ToolGroup {
                    flavor: "explore",
                    label: "explore",
                    details: vec!["Git status".to_owned(), "Read file".to_owned()],
                },
                ActivityItem::ToolGroup {
                    flavor: "edit",
                    label: "edit",
                    details: vec!["Edit file".to_owned()],
                },
                ActivityItem::ToolGroup {
                    flavor: "bash",
                    label: "bash",
                    details: vec!["Run command".to_owned()],
                },
                ActivityItem::ToolGroup {
                    flavor: "tool",
                    label: "tool",
                    details: vec!["Use custom_tool".to_owned()],
                },
            ]
        );
    }

    #[test]
    fn active_tool_keeps_chronology_before_later_status() {
        let events = vec![
            event(
                EventKind::TOOL_CALL,
                object([
                    ("id", "call-read".into()),
                    ("name", "read_file".into()),
                    ("input", serde_json::json!({"path": "README.md"})),
                ]),
            ),
            event(
                EventKind::ASSISTANT_ACTIVITY,
                object([("message", "Checking the result".into())]),
            ),
        ];

        assert_eq!(
            project_activity(&events),
            vec![
                ActivityItem::ToolGroup {
                    flavor: "explore",
                    label: "explore",
                    details: vec!["Read README.md".to_owned()],
                },
                ActivityItem::Status("Checking the result".to_owned()),
            ]
        );
    }

    #[test]
    fn successful_tool_activity_is_removed_without_lifecycle_detail() {
        let events = vec![
            event(
                EventKind::TOOL_CALL,
                object([
                    ("id", "call-read".into()),
                    ("name", "read_file".into()),
                    ("input", serde_json::json!({"path": "README.md"})),
                ]),
            ),
            event(
                EventKind::ASSISTANT_ACTIVITY,
                object([("message", "Checking the result".into())]),
            ),
            event(
                EventKind::TOOL_RESULT,
                object([
                    ("id", "call-read".into()),
                    ("name", "read_file".into()),
                    ("ok", true.into()),
                ]),
            ),
        ];

        assert_eq!(
            project_activity(&events),
            vec![ActivityItem::Status("Checking the result".to_owned())]
        );
    }

    #[test]
    fn ambiguous_same_name_result_does_not_hide_unrelated_visible_tool() {
        let events = vec![
            event(
                EventKind::TOOL_CALL,
                object([
                    ("id", "call-read-one".into()),
                    ("name", "read_file".into()),
                    ("input", serde_json::json!({"path": "one.md"})),
                ]),
            ),
            event(
                EventKind::TOOL_CALL,
                object([
                    ("id", "call-read-two".into()),
                    ("name", "read_file".into()),
                    ("input", serde_json::json!({"path": "two.md"})),
                ]),
            ),
            event(
                EventKind::TOOL_RESULT,
                object([("name", "read_file".into()), ("ok", true.into())]),
            ),
        ];

        assert_eq!(
            project_activity(&events),
            vec![ActivityItem::ToolGroup {
                flavor: "explore",
                label: "explore",
                details: vec!["Read one.md".to_owned(), "Read two.md".to_owned()],
            }]
        );
    }

    #[test]
    fn failed_tool_result_stays_visible_at_result_boundary() {
        let events = vec![
            event(
                EventKind::TOOL_CALL,
                object([
                    ("id", "call-shell".into()),
                    ("name", "run_shell".into()),
                    ("input", serde_json::json!({"command": "cargo test"})),
                ]),
            ),
            event(
                EventKind::ASSISTANT_ACTIVITY,
                object([("message", "Checking the result".into())]),
            ),
            event(
                EventKind::TOOL_RESULT,
                object([
                    ("id", "call-shell".into()),
                    ("name", "run_shell".into()),
                    ("ok", false.into()),
                ]),
            ),
        ];

        assert_eq!(
            project_activity(&events),
            vec![
                ActivityItem::Status("Checking the result".to_owned()),
                ActivityItem::ToolGroup {
                    flavor: "bash",
                    label: "bash",
                    details: vec!["run_shell failed".to_owned()],
                },
            ]
        );
    }

    #[test]
    fn skips_tool_events_with_empty_names() {
        let events = vec![
            event(EventKind::TOOL_CALL, object([])),
            event(EventKind::TOOL_CALL, object([("name", "".into())])),
            event(
                EventKind::TOOL_RESULT,
                object([("name", "".into()), ("ok", false.into())]),
            ),
            event(EventKind::TOOL_CALL, object([("name", "read_file".into())])),
        ];

        assert_eq!(
            project_activity(&events),
            vec![ActivityItem::ToolGroup {
                flavor: "explore",
                label: "explore",
                details: vec!["Read file".to_owned()],
            }]
        );
    }

    #[test]
    fn renders_failed_tool_results_inside_their_flavor_group() {
        let events = vec![event(
            EventKind::TOOL_RESULT,
            object([("name", "run_shell".into()), ("ok", false.into())]),
        )];

        assert_eq!(
            project_activity(&events),
            vec![ActivityItem::ToolGroup {
                flavor: "bash",
                label: "bash",
                details: vec!["run_shell failed".to_owned()],
            }]
        );
    }

    #[test]
    fn live_model_status_is_opt_in() {
        let events = vec![event(
            EventKind::MODEL_DELTA,
            object([("kind", "text".into()), ("delta", "partial".into())]),
        )];

        assert_eq!(project_activity(&events), Vec::new());
        assert_eq!(
            project_activity_with_live_status(&events, true),
            vec![ActivityItem::Status("Streaming response".to_owned())]
        );
    }

    #[test]
    fn terminal_events_clear_live_model_status() {
        for kind in [EventKind::ERROR, EventKind::ASSISTANT_MESSAGE] {
            let events = vec![
                event(
                    EventKind::MODEL_DELTA,
                    object([("kind", "text".into()), ("delta", "partial".into())]),
                ),
                event(kind, object([])),
            ];

            assert_eq!(project_activity_with_live_status(&events, true), Vec::new());
        }
    }

    #[test]
    fn vt100_renders_aggregate_headers_with_one_level_detail_gutters() {
        let events = vec![
            event(EventKind::TOOL_CALL, object([("name", "read_file".into())])),
            event(EventKind::TOOL_CALL, object([("name", "git_diff".into())])),
            event(EventKind::TOOL_CALL, object([("name", "edit_file".into())])),
            event(EventKind::TOOL_CALL, object([("name", "run_shell".into())])),
            event(
                EventKind::TOOL_RESULT,
                object([
                    ("name", "run_shell".into()),
                    ("ok", true.into()),
                    ("output", "ok".into()),
                ]),
            ),
        ];
        let theme = Theme::default();

        let contents = rendered_screen(&events, &theme, 48, 12);

        assert!(contents.contains("explore"));
        assert!(contents.contains("edit"));
        assert!(!contents.contains("• Ran"));
        assert!(contents.contains("  ├ Read file"));
        assert!(contents.contains("  └ Git diff"));
        assert!(contents.contains("  └ Edit file"));
        assert!(!contents.contains("read_file call"));
        assert!(!contents.contains("run_shell completed"));

        for line in contents.lines().filter(|line| !line.trim().is_empty()) {
            assert!(
                line.starts_with("    ") || line.starts_with("  ├ ") || line.starts_with("  └ "),
                "unstable activity gutter: {line:?}"
            );
            assert!(
                !line.starts_with("  └   └ "),
                "nested activity gutter: {line:?}"
            );
        }
    }

    #[test]
    fn vt100_activity_renders_status_but_not_model_reasoning() {
        let events = vec![
            event(
                EventKind::ASSISTANT_ACTIVITY,
                object([("message", "Inspecting repository contracts".into())]),
            ),
            event(
                EventKind::MODEL_REASONING,
                object([
                    ("fidelity", "raw".into()),
                    ("content", "private reasoning must not appear".into()),
                ]),
            ),
        ];
        let theme = Theme::default();

        let contents = rendered_screen(&events, &theme, 48, 6);

        assert!(contents.contains("Inspecting repository contracts"));
        assert!(!contents.contains("private reasoning must not appear"));
        assert!(!contents.contains("Reasoning"));
    }

    fn rendered_screen(events: &[EventEnvelope], theme: &Theme, width: u16, height: u16) -> String {
        let backend = VT100Backend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| {
                frame.render_widget(
                    activity_widget(events, theme),
                    Rect::new(0, 0, width, height),
                );
            })
            .expect("draw");

        terminal.backend().screen_contents()
    }

    fn event(kind: &'static str, payload: euler_event::JsonObject) -> EventEnvelope {
        EventEnvelope::new("session", "agent", None, kind, payload)
    }
}
