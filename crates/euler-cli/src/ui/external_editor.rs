use super::terminal;
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) trait ExternalEditorRunner {
    fn edit(&self, draft: &str) -> EditorResult;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EditorResult {
    Updated(String),
    Unset,
    Failed(String),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SystemExternalEditor;

impl ExternalEditorRunner for SystemExternalEditor {
    fn edit(&self, draft: &str) -> EditorResult {
        let editor = match std::env::var("EDITOR") {
            Ok(value) if !value.trim().is_empty() => value,
            _ => return EditorResult::Unset,
        };
        match terminal::suspend_for_external_command(|| edit_with_command(&editor, draft)) {
            Ok(result) => result,
            Err(error) => EditorResult::Failed(format!("terminal restore failed: {error}")),
        }
    }
}

fn edit_with_command(editor: &str, draft: &str) -> EditorResult {
    let path = match write_temp_draft(draft) {
        Ok(path) => path,
        Err(error) => {
            return EditorResult::Failed(format!("could not write editor draft: {error}"))
        }
    };
    let status = run_editor(editor, &path);
    let result = match status {
        Ok(true) => read_updated_draft(&path),
        Ok(false) => EditorResult::Failed("editor exited with a non-zero status".to_owned()),
        Err(error) => EditorResult::Failed(format!("could not launch editor: {error}")),
    };
    let _ = fs::remove_file(path);
    result
}

fn write_temp_draft(draft: &str) -> std::io::Result<PathBuf> {
    let path = temp_draft_path();
    let mut file = private_temp_file_options().open(&path)?;
    file.write_all(draft.as_bytes())?;
    Ok(path)
}

fn private_temp_file_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
}

fn read_updated_draft(path: &PathBuf) -> EditorResult {
    match fs::read_to_string(path) {
        Ok(contents) => EditorResult::Updated(contents),
        Err(error) => EditorResult::Failed(format!("could not read editor draft: {error}")),
    }
}

fn run_editor(editor: &str, path: &PathBuf) -> std::io::Result<bool> {
    let mut parts = editor.split_whitespace();
    let Some(program) = parts.next() else {
        return Ok(false);
    };
    let status = Command::new(program).args(parts).arg(path).status()?;
    Ok(status.success())
}

fn temp_draft_path() -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    std::env::temp_dir().join(format!("euler-draft-{}-{now}.md", std::process::id()))
}
