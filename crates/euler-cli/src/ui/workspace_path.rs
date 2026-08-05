use euler_event::{EventEnvelope, EventKind};

pub(crate) type WorkspacePathKey = (String, String);

pub(crate) fn session_primary_root(events: &[EventEnvelope]) -> Option<&str> {
    events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::SESSION_START)
        .and_then(|event| payload_str(event, "root"))
        .filter(|root| !root.is_empty())
}

/// Stable file identity for event projections. New events carry their exact
/// writable root; legacy events inherit the first session root. Root and path
/// remain separate until display so malformed paths cannot replace or escape
/// the recorded root through `Path::join` semantics.
pub(crate) fn workspace_path_key(
    event: &EventEnvelope,
    legacy_primary_root: Option<&str>,
) -> Option<WorkspacePathKey> {
    let path = payload_str(event, "path").filter(|path| !path.is_empty())?;
    let root = payload_str(event, "workspace_root")
        .filter(|root| !root.is_empty())
        .or(legacy_primary_root)
        .unwrap_or_default();
    Some((root.to_owned(), path.to_owned()))
}

pub(crate) fn workspace_path_display(path: &WorkspacePathKey) -> String {
    let (root, relative) = path;
    if root.is_empty() {
        return relative.clone();
    }
    if !is_guarded_relative_path(relative) {
        return format!("{root}::{relative}");
    }
    std::path::Path::new(root)
        .join(relative)
        .to_string_lossy()
        .into_owned()
}

fn is_guarded_relative_path(path: &str) -> bool {
    !path.is_empty()
        && std::path::Path::new(path).components().all(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::Normal(_)
            )
        })
}

fn payload_str<'a>(event: &'a EventEnvelope, key: &str) -> Option<&'a str> {
    event.payload.get(key).and_then(serde_json::Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_absolute_or_parent_paths_cannot_discard_the_root() {
        assert_eq!(
            workspace_path_display(&("/work/root".to_owned(), "/outside/file".to_owned())),
            "/work/root::/outside/file"
        );
        assert_eq!(
            workspace_path_display(&("/work/root".to_owned(), "../outside".to_owned())),
            "/work/root::../outside"
        );
    }
}
