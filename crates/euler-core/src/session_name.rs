use euler_event::{object, EventEnvelope, EventKind};

pub(crate) fn validate_session_name_for_write(name: &str) -> Option<String> {
    if name.chars().any(char::is_control) {
        return None;
    }
    let normalized = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    Some(normalized.chars().take(120).collect())
}

pub(crate) fn session_name_for_display(name: &str) -> Option<String> {
    let projected = name
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>();
    (!projected.trim().is_empty()).then_some(projected)
}

pub(crate) fn session_renamed_event(
    session_id: impl Into<String>,
    agent_id: impl Into<String>,
    parent: Option<String>,
    name: String,
) -> EventEnvelope {
    EventEnvelope::new(
        session_id,
        agent_id,
        parent,
        EventKind::SESSION_RENAMED,
        object([("name", name.into())]),
    )
}
