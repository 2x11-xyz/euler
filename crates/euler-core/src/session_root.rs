use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn session_root_for_event(root: &Path) -> String {
    normalize_session_root(root).to_string_lossy().into_owned()
}

pub(crate) fn normalize_session_root(root: &Path) -> PathBuf {
    let absolute = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(root))
            .unwrap_or_else(|_| root.to_path_buf())
    };
    fs::canonicalize(&absolute).unwrap_or(absolute)
}

pub(crate) fn session_root_from_str(root: &str) -> Option<PathBuf> {
    (!root.is_empty()).then(|| PathBuf::from(root))
}
