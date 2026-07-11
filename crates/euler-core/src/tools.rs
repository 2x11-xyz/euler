use crate::{
    apply_patch_update_chunks, capture_workspace_snapshot, parse_single_file_apply_patch,
    ApplyPatchDocument, ApplyPatchError, ObservedFileChange,
};
use euler_event::{EventEnvelope, EventKind};
use euler_provider::ToolDefinition;
use euler_sdk::Capability;
use serde_json::json;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

const DEFAULT_MAX_BYTES: usize = 16 * 1024;
const DEFAULT_MAX_LINES: usize = 400;
const DEFAULT_SHELL_TIMEOUT_MS: u64 = 120_000;
const MAX_SHELL_TIMEOUT_MS: u64 = 600_000;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ToolError {
    #[error("missing field `{0}`")]
    MissingField(&'static str),
    #[error("invalid field `{0}`")]
    InvalidField(&'static str),
    #[error("path `{path}` is outside the workspace root ({reason}); paths must be relative and stay inside the workspace root")]
    PathOutsideWorkspace { path: String, reason: &'static str },
    #[error("invalid patch: {0}")]
    InvalidPatch(&'static str),
    #[error("file already exists")]
    FileAlreadyExists,
    #[error("parent directory does not exist")]
    ParentDirectoryMissing,
    #[error("unsupported tool `{0}`")]
    Unsupported(String),
    #[error("replacement text matched {0} times; expected exactly one")]
    ReplacementMatchCount(usize),
    #[error("update hunk {hunk} matched {count} times; expected exactly one")]
    UpdateHunkMatchCount { hunk: usize, count: usize },
    #[error("update hunk {hunk} overlaps earlier update hunk {previous_hunk}")]
    UpdateHunkOverlap { hunk: usize, previous_hunk: usize },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolExecution {
    pub name: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub patch: Option<PatchEvents>,
    pub file_changes: Vec<ObservedFileChange>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatchEvents {
    pub path: String,
    pub before: String,
    pub after: String,
    pub(crate) origin: &'static str,
    pub(crate) action: &'static str,
    pub(crate) before_sha256: Option<String>,
    pub(crate) after_sha256: String,
    pub(crate) before_byte_len: usize,
    pub(crate) after_byte_len: usize,
    write_path: PathBuf,
    write_content: String,
}

#[derive(Debug)]
pub struct ToolRegistry {
    root: PathBuf,
}

impl ToolRegistry {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn required_capability(&self, name: &str) -> Option<Capability> {
        match name {
            "read_file" | "git_status" | "git_diff" | "tool_result_get" => Some(Capability::FsRead),
            "edit_file" | "apply_patch" => Some(Capability::FsWrite),
            "run_shell" => Some(Capability::ShellExec),
            _ => None,
        }
    }

    pub fn required_capability_for_input(&self, name: &str, input: &Value) -> Option<Capability> {
        if is_shell_apply_patch_request(name, input) {
            Some(Capability::FsWrite)
        } else {
            self.required_capability(name)
        }
    }

    pub fn permission_reason(&self, name: &str, input: &Value) -> String {
        if is_shell_apply_patch_request(name, input) {
            "tool apply_patch".to_owned()
        } else {
            format!("tool {name}")
        }
    }

    pub fn execute(&self, name: &str, input: &Value) -> Result<ToolExecution, ToolError> {
        match name {
            "read_file" => self.read_file(input),
            "edit_file" => self.edit_file(input),
            "apply_patch" => self.apply_patch_tool(input),
            "run_shell" => self.run_shell(input),
            "git_status" => self.git(&["status", "--short"], "git_status"),
            "git_diff" => self.git(&["diff", "--"], "git_diff"),
            "tool_result_get" => Err(ToolError::InvalidField(
                "tool_result_get requires session events",
            )),
            other => Err(ToolError::Unsupported(other.to_owned())),
        }
    }

    pub fn execute_with_events(
        &self,
        name: &str,
        input: &Value,
        events: &[EventEnvelope],
    ) -> Result<ToolExecution, ToolError> {
        if name == "tool_result_get" {
            return tool_result_get(events, input);
        }
        self.execute(name, input)
    }

    pub fn model_tools(&self) -> Vec<ToolDefinition> {
        let mut tools = coding_tool_definitions();
        tools.push(tool_result_get_definition());
        tools
    }

    fn read_file(&self, input: &Value) -> Result<ToolExecution, ToolError> {
        let path = self.resolve_path(required_str(input, "path")?)?;
        let offset = optional_positive_usize(input, "offset")?.unwrap_or(1);
        let max_bytes = optional_positive_usize(input, "max_bytes")?.unwrap_or(DEFAULT_MAX_BYTES);
        let max_lines = optional_positive_usize(input, "max_lines")?.unwrap_or(DEFAULT_MAX_LINES);
        let content = fs::read_to_string(path)?;
        let output = bound_read_file_window(&content, offset, max_bytes, max_lines);
        Ok(ToolExecution {
            name: "read_file".to_owned(),
            output,
            exit_code: None,
            patch: None,
            file_changes: Vec::new(),
        })
    }

    fn edit_file(&self, input: &Value) -> Result<ToolExecution, ToolError> {
        let relative = required_str(input, "path")?;
        let old = required_str(input, "old")?;
        let new = required_str(input, "new")?;
        if old.is_empty() {
            let path = self.resolve_create_path(relative)?;
            if path.exists() {
                return Err(ToolError::FileAlreadyExists);
            }
            return Ok(ToolExecution {
                name: "edit_file".to_owned(),
                output: format!("created {relative}"),
                exit_code: None,
                patch: Some(PatchEvents {
                    path: relative.to_owned(),
                    before: String::new(),
                    after: new.to_owned(),
                    origin: "edit_file",
                    action: "add",
                    before_sha256: None,
                    after_sha256: hash_bytes(new.as_bytes()),
                    before_byte_len: 0,
                    after_byte_len: new.len(),
                    write_path: path,
                    write_content: new.to_owned(),
                }),
                file_changes: Vec::new(),
            });
        }
        let path = self.resolve_path(relative)?;
        let content = fs::read_to_string(&path)?;
        let count = overlapping_match_count(&content, old);
        if count != 1 {
            return Err(ToolError::ReplacementMatchCount(count));
        }
        let updated = content.replacen(old, new, 1);
        let before_bytes_len = content.len();
        let before_sha = hash_bytes(content.as_bytes());
        let after_sha = hash_bytes(updated.as_bytes());
        Ok(ToolExecution {
            name: "edit_file".to_owned(),
            output: format!("edited {relative}"),
            exit_code: None,
            patch: Some(PatchEvents {
                path: relative.to_owned(),
                // Full file contents, not the matched snippets: downstream
                // diff projections derive line numbers from these.
                before: content,
                after: updated.clone(),
                origin: "edit_file",
                action: "modify",
                before_sha256: Some(before_sha),
                after_sha256: after_sha,
                before_byte_len: before_bytes_len,
                after_byte_len: updated.len(),
                write_path: path,
                write_content: updated,
            }),
            file_changes: Vec::new(),
        })
    }

    fn apply_patch_tool(&self, input: &Value) -> Result<ToolExecution, ToolError> {
        self.apply_patch_text(required_str(input, "patch")?, "apply_patch", "apply_patch")
    }

    fn apply_patch_text(
        &self,
        patch: &str,
        origin: &'static str,
        name: &str,
    ) -> Result<ToolExecution, ToolError> {
        let label = if origin == "run_shell:apply_patch" {
            "intercepted apply_patch"
        } else {
            origin
        };
        match parse_single_file_apply_patch(patch).map_err(tool_error_from_apply_patch)? {
            ApplyPatchDocument::Add { path, content } => {
                let write_path = self.resolve_create_path(&path)?;
                if write_path.exists() {
                    return Err(ToolError::FileAlreadyExists);
                }
                Ok(ToolExecution {
                    name: name.to_owned(),
                    output: format!("{label} prepared add {path}"),
                    exit_code: None,
                    patch: Some(PatchEvents {
                        path,
                        before: String::new(),
                        after_sha256: hash_bytes(content.as_bytes()),
                        after_byte_len: content.len(),
                        after: content.clone(),
                        origin,
                        action: "add",
                        before_sha256: None,
                        before_byte_len: 0,
                        write_path,
                        write_content: content,
                    }),
                    file_changes: Vec::new(),
                })
            }
            ApplyPatchDocument::Update { path, chunks } => {
                let write_path = self.resolve_path(&path)?;
                let content = fs::read_to_string(&write_path)?;
                let updated = apply_patch_update_chunks(&content, &chunks)
                    .map_err(tool_error_from_apply_patch)?;
                Ok(ToolExecution {
                    name: name.to_owned(),
                    output: format!("{label} prepared update {path}"),
                    exit_code: None,
                    patch: Some(PatchEvents {
                        path,
                        before_sha256: Some(hash_bytes(content.as_bytes())),
                        after_sha256: hash_bytes(updated.as_bytes()),
                        before_byte_len: content.len(),
                        after_byte_len: updated.len(),
                        // Full file contents, not concatenated hunk excerpts:
                        // downstream diff projections derive line numbers
                        // from these.
                        before: content,
                        after: updated.clone(),
                        origin,
                        action: "modify",
                        write_path,
                        write_content: updated,
                    }),
                    file_changes: Vec::new(),
                })
            }
        }
    }

    pub fn apply_patch(&self, patch: &PatchEvents) -> Result<(), ToolError> {
        fs::write(&patch.write_path, &patch.write_content)?;
        Ok(())
    }

    /// Write UTF-8 content to a workspace-relative path (used by `/rollback`).
    pub fn write_workspace_file(&self, relative: &str, content: &str) -> Result<(), ToolError> {
        let path = self.resolve_path(relative)?;
        fs::write(path, content)?;
        Ok(())
    }

    fn run_shell(&self, input: &Value) -> Result<ToolExecution, ToolError> {
        let command = required_str(input, "command")?;
        let max_bytes = optional_positive_usize(input, "max_bytes")?.unwrap_or(DEFAULT_MAX_BYTES);
        if command_begins_apply_patch(command) {
            // Strict apply_patch interception must return before spawning a shell.
            return self.apply_patch_text(
                &strict_apply_patch_heredoc(command)?,
                "run_shell:apply_patch",
                "run_shell",
            );
        }
        let timeout_ms = match optional_positive_usize(input, "timeout_ms")? {
            None => DEFAULT_SHELL_TIMEOUT_MS,
            Some(value) => {
                let value = value as u64;
                if value > MAX_SHELL_TIMEOUT_MS {
                    return Err(ToolError::InvalidField("timeout_ms"));
                }
                value
            }
        };
        let before = capture_workspace_snapshot(&self.root).ok();
        let mut child = Command::new("sh");
        child.arg("-c").arg(command).current_dir(&self.root);
        scrub_secret_env(&mut child);
        let outcome = run_with_timeout(child, timeout_ms)?;
        let after = capture_workspace_snapshot(&self.root).ok();
        let file_changes = before
            .zip(after)
            .map_or_else(Vec::new, |(before, after)| before.changes_to(&after));
        let bounded = bound_text(&outcome.text, max_bytes, DEFAULT_MAX_LINES);
        let (status, header) = match outcome.status {
            Some(status) => (status, format!("exit {status}")),
            None => (
                -1,
                format!(
                    "exit -1 (command timed out after {timeout_ms} ms and was killed; \
pass timeout_ms up to {MAX_SHELL_TIMEOUT_MS} for longer runs)"
                ),
            ),
        };
        Ok(ToolExecution {
            name: "run_shell".to_owned(),
            output: format!("{header}\n{bounded}"),
            exit_code: Some(status),
            patch: None,
            file_changes,
        })
    }

    fn git(&self, args: &[&str], name: &str) -> Result<ToolExecution, ToolError> {
        let mut child = Command::new("git");
        child.args(args).current_dir(&self.root);
        scrub_secret_env(&mut child);
        let output = child.output()?;
        let mut text = String::new();
        text.push_str(&String::from_utf8_lossy(&output.stdout));
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        let status = output.status.code().unwrap_or(-1);
        Ok(ToolExecution {
            name: name.to_owned(),
            output: bound_text(&text, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES),
            exit_code: Some(status),
            patch: None,
            file_changes: Vec::new(),
        })
    }

    fn resolve_path(&self, relative: &str) -> Result<PathBuf, ToolError> {
        self.resolve_path_inner(relative, false)
    }

    /// Canonicalized workspace-relative form of a model-supplied path, for
    /// scope matching: `..` and symlinks resolved exactly as the write path
    /// resolves them. `None` when the path cannot be resolved inside the
    /// workspace - scoped grant matching then fails closed to the ask path.
    pub fn workspace_relative_path(&self, relative: &str) -> Option<PathBuf> {
        let canonical = self.resolve_path_inner(relative, false).ok()?;
        let root = self.root.canonicalize().ok()?;
        canonical.strip_prefix(&root).ok().map(Path::to_path_buf)
    }

    fn resolve_create_path(&self, relative: &str) -> Result<PathBuf, ToolError> {
        self.resolve_path_inner(relative, true)
    }

    fn resolve_path_inner(
        &self,
        relative: &str,
        parent_must_be_directory: bool,
    ) -> Result<PathBuf, ToolError> {
        if relative.is_empty() {
            return Err(ToolError::InvalidField("path"));
        }
        let path = Path::new(relative);
        if path.is_absolute() {
            return Err(ToolError::PathOutsideWorkspace {
                path: display_path(relative),
                reason: "absolute paths are not allowed",
            });
        }
        let root = self.root.canonicalize()?;
        let full = root.join(path);
        let canonical = if full.exists() {
            full.canonicalize()?
        } else {
            if full.symlink_metadata().is_ok() {
                return Err(ToolError::PathOutsideWorkspace {
                    path: display_path(relative),
                    reason:
                        "path is a symlink whose target cannot be verified inside the workspace",
                });
            }
            let parent = full.parent().ok_or(ToolError::InvalidField("path"))?;
            if parent_must_be_directory && !parent.is_dir() {
                return Err(ToolError::ParentDirectoryMissing);
            }
            let parent = parent.canonicalize()?;
            let file_name = full.file_name().ok_or(ToolError::InvalidField("path"))?;
            parent.join(file_name)
        };
        if !canonical.starts_with(&root) {
            return Err(ToolError::PathOutsideWorkspace {
                path: display_path(relative),
                reason: "path escapes the workspace root",
            });
        }
        Ok(canonical)
    }
}

const DISPLAY_PATH_MAX_CHARS: usize = 256;

/// Sanitize a model-supplied path for inclusion in an error message:
/// replace control characters and cap the length so hostile or degenerate
/// input cannot inject terminal escapes, split log lines, or bloat events.
fn display_path(path: &str) -> String {
    let mut sanitized: String = path
        .chars()
        .take(DISPLAY_PATH_MAX_CHARS)
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect();
    if path.chars().count() > DISPLAY_PATH_MAX_CHARS {
        sanitized.push('\u{2026}');
    }
    sanitized
}

fn scrub_secret_env(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        if is_secret_env_name(&name) {
            command.env_remove(name);
        }
    }
}

fn is_secret_env_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "ANTHROPIC_API_KEY"
            | "OPENAI_API_KEY"
            | "OPENROUTER_API_KEY"
            | "XAI_API_KEY"
            | "EULER_AUTH_FILE"
    ) || upper.ends_with("_API_KEY")
        || upper.ends_with("_ACCESS_KEY")
        || upper.split('_').any(|segment| {
            matches!(
                segment,
                "KEY" | "TOKEN" | "SECRET" | "CREDENTIAL" | "CREDENTIALS" | "PASSWORD"
            )
        })
}

fn empty_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

fn required_str<'a>(input: &'a Value, key: &'static str) -> Result<&'a str, ToolError> {
    input
        .get(key)
        .ok_or(ToolError::MissingField(key))?
        .as_str()
        .ok_or(ToolError::InvalidField(key))
}

fn optional_usize(input: &Value, key: &'static str) -> Result<Option<usize>, ToolError> {
    let Some(value) = input.get(key) else {
        return Ok(None);
    };
    let value = value.as_u64().ok_or(ToolError::InvalidField(key))?;
    usize::try_from(value)
        .map(Some)
        .map_err(|_| ToolError::InvalidField(key))
}

struct ShellOutcome {
    /// `None` means the command was killed at the timeout deadline.
    status: Option<i32>,
    text: String,
}

/// Runs the child in its own process group, polling for completion and
/// killing the whole group at the deadline so grandchildren (e.g. a python
/// computation spawned by `sh -c`) cannot outlive the tool call. Partial
/// stdout/stderr captured before the kill is preserved for the model.
fn run_with_timeout(mut child: Command, timeout_ms: u64) -> Result<ShellOutcome, ToolError> {
    use std::io::Read;
    use std::os::unix::process::CommandExt as _;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    child
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut handle = child.spawn()?;
    let pid = handle.id() as i32;
    let reader = |stream: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            if let Some(mut stream) = stream {
                let _ = stream.read_to_end(&mut buffer);
            }
            buffer
        })
    };
    let stdout = reader(
        handle
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let stderr = reader(
        handle
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let status = loop {
        match handle.try_wait()? {
            Some(status) => break Some(status.code().unwrap_or(-1)),
            None if Instant::now() >= deadline => {
                // SAFETY: plain libc kill on the process group we created.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
                let _ = handle.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    };
    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&stdout.join().unwrap_or_default()));
    text.push_str(&String::from_utf8_lossy(&stderr.join().unwrap_or_default()));
    Ok(ShellOutcome { status, text })
}

fn optional_positive_usize(input: &Value, key: &'static str) -> Result<Option<usize>, ToolError> {
    let Some(value) = optional_usize(input, key)? else {
        return Ok(None);
    };
    if value == 0 {
        return Err(ToolError::InvalidField(key));
    }
    Ok(Some(value))
}

fn tool_error_from_apply_patch(error: ApplyPatchError) -> ToolError {
    match error {
        ApplyPatchError::Invalid(message) => ToolError::InvalidPatch(message),
        ApplyPatchError::UpdateHunkMatchCount { hunk, count } => {
            ToolError::UpdateHunkMatchCount { hunk, count }
        }
        ApplyPatchError::UpdateHunkOverlap {
            hunk,
            previous_hunk,
        } => ToolError::UpdateHunkOverlap {
            hunk,
            previous_hunk,
        },
    }
}

fn strict_apply_patch_heredoc(command: &str) -> Result<String, ToolError> {
    let (first, rest) = command
        .split_once('\n')
        .ok_or(ToolError::InvalidPatch("malformed heredoc"))?;
    let tag = first
        .strip_prefix("apply_patch <<'")
        .or_else(|| first.strip_prefix("apply_patch<<'"))
        .and_then(|rest| rest.strip_suffix('\''))
        .ok_or(ToolError::InvalidPatch("malformed heredoc"))?;
    if tag.is_empty()
        || !tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(ToolError::InvalidPatch("invalid heredoc tag"));
    }
    let mut body = String::new();
    let mut remaining = rest;
    loop {
        let Some((line, after)) = remaining.split_once('\n') else {
            return if remaining == tag {
                Ok(body)
            } else {
                Err(ToolError::InvalidPatch("unterminated heredoc"))
            };
        };
        if line == tag {
            return if after.is_empty() {
                Ok(body)
            } else {
                Err(ToolError::InvalidPatch("trailing heredoc content"))
            };
        }
        body.push_str(line);
        body.push('\n');
        remaining = after;
    }
}

fn is_shell_apply_patch_request(name: &str, input: &Value) -> bool {
    name == "run_shell" && required_str(input, "command").is_ok_and(command_begins_apply_patch)
}

fn command_begins_apply_patch(command: &str) -> bool {
    command == "apply_patch"
        || command.starts_with("apply_patch ")
        || command.starts_with("apply_patch\t")
        || command.starts_with("apply_patch<<")
}

fn bound_read_file_window(text: &str, offset: usize, max_bytes: usize, max_lines: usize) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let total_lines = lines.len();
    if offset > total_lines && !(offset == 1 && total_lines == 0) {
        return format!("[offset beyond EOF: total lines {total_lines}]");
    }

    let start_index = offset.saturating_sub(1);
    let mut output = String::new();
    let mut full_lines_shown = 0usize;
    let mut partial_line = None;
    let mut truncated = false;

    for (index, line) in lines.iter().enumerate().skip(start_index) {
        if full_lines_shown == max_lines {
            truncated = true;
            break;
        }
        if output.len() + line.len() > max_bytes {
            let remaining = max_bytes.saturating_sub(output.len());
            if remaining > 0 {
                let split = floor_char_boundary(line, remaining);
                if split > 0 {
                    output.push_str(&line[..split]);
                    partial_line = Some(index + 1);
                }
            }
            truncated = true;
            break;
        }
        output.push_str(line);
        full_lines_shown += 1;
    }

    if truncated {
        let last_full_line = offset + full_lines_shown - 1;
        append_read_file_truncation_marker(
            &mut output,
            offset,
            last_full_line,
            partial_line,
            total_lines,
        );
    }

    output
}

fn append_read_file_truncation_marker(
    output: &mut String,
    start_line: usize,
    last_full_line: usize,
    partial_line: Option<usize>,
    total_lines: usize,
) {
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }

    let continuation_offset = last_full_line + 1;
    let marker = if let Some(partial_line) = partial_line {
        if last_full_line >= start_line {
            format!(
                "[truncated: showing full lines {start_line}-{last_full_line} of {total_lines}, plus partial line {partial_line}; line {partial_line} is partial; call read_file with offset={partial_line} and a larger max_bytes for the rest]"
            )
        } else {
            format!(
                "[truncated: showing no full lines of {total_lines}, plus partial line {partial_line}; line {partial_line} is partial; call read_file with offset={partial_line} and a larger max_bytes for the rest]"
            )
        }
    } else if last_full_line >= start_line {
        format!(
            "[truncated: showing lines {start_line}-{last_full_line} of {total_lines}; call read_file with offset={continuation_offset} for more]"
        )
    } else {
        format!(
            "[truncated: showing no lines of {total_lines}; call read_file with offset={continuation_offset} for more]"
        )
    };
    output.push_str(&marker);
}

fn bound_text(text: &str, max_bytes: usize, max_lines: usize) -> String {
    let mut output = String::new();
    let mut truncated = false;
    for line in text.split_inclusive('\n').take(max_lines) {
        if output.len() + line.len() > max_bytes {
            let remaining = max_bytes.saturating_sub(output.len());
            output.push_str(&line[..floor_char_boundary(line, remaining)]);
            truncated = true;
            break;
        }
        output.push_str(line);
    }
    if text.split_inclusive('\n').count() > max_lines || text.len() > output.len() {
        truncated = true;
    }
    if truncated {
        output.push_str("\n[truncated]");
    }
    output
}

fn overlapping_match_count(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return usize::MAX;
    }
    haystack
        .char_indices()
        .filter(|(index, _)| haystack[*index..].starts_with(needle))
        .count()
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn coding_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "read_file".to_owned(),
            description: "Read a UTF-8 file under the workspace root, optionally starting at a 1-indexed line offset for windowed reads. The path must be relative to the workspace root; absolute and parent-traversal paths are rejected.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "minimum": 1},
                    "max_bytes": {"type": "integer", "minimum": 1},
                    "max_lines": {"type": "integer", "minimum": 1}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "edit_file".to_owned(),
            description: "Replace exactly one text occurrence in a UTF-8 file under the workspace root. The path must be relative to the workspace root; absolute and parent-traversal paths are rejected.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old": {"type": "string"},
                    "new": {"type": "string"}
                },
                "required": ["path", "old", "new"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "apply_patch".to_owned(),
            description: "Apply one structured patch envelope to add or update one UTF-8 file under the workspace root. Prefer this over shell commands for code and text edits. File paths inside the patch must be relative to the workspace root; absolute paths (for example /tmp/name.py) and parent-traversal paths are rejected. Updates may contain multiple hunks for the same file. V0 rejects delete, rename, and multi-file patches.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patch": {"type": "string"}
                },
                "required": ["patch"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "run_shell".to_owned(),
            description: "Run a shell command in the workspace root. Commands time out \
after 120000 ms by default; pass timeout_ms (up to 600000) for longer runs."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "max_bytes": {"type": "integer", "minimum": 1},
                    "timeout_ms": {"type": "integer", "minimum": 1, "maximum": MAX_SHELL_TIMEOUT_MS}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "git_status".to_owned(),
            description: "Return short git status for the workspace.".to_owned(),
            parameters: empty_parameters(),
        },
        ToolDefinition {
            name: "git_diff".to_owned(),
            description: "Return git diff for the workspace.".to_owned(),
            parameters: empty_parameters(),
        },
    ]
}

fn tool_result_get_definition() -> ToolDefinition {
    ToolDefinition {
        name: "tool_result_get".to_owned(),
        description: "Rehydrate a demoted or compacted tool result from the current session by event_id (required). Use the event id printed in canvas stubs (`event <id>`) instead of re-running the original tool.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "event_id": {"type": "string"},
                "max_bytes": {"type": "integer", "minimum": 1}
            },
            "required": ["event_id"],
            "additionalProperties": false
        }),
    }
}

fn tool_result_get(events: &[EventEnvelope], input: &Value) -> Result<ToolExecution, ToolError> {
    let max_bytes = input
        .get("max_bytes")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(64 * 1024)
        .max(1);
    let event = find_tool_result_event(events, input)?;
    let name = event
        .payload
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let ok = event
        .payload
        .get("ok")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let content = tool_result_content(event, ok);
    let body = truncate_rehydrate_body(content, max_bytes);
    let status = if ok { "ok" } else { "failed" };
    Ok(ToolExecution {
        name: "tool_result_get".to_owned(),
        output: format!("[rehydrated {name} event {} ({status})]\n{body}", event.id),
        exit_code: None,
        patch: None,
        file_changes: Vec::new(),
    })
}

fn find_tool_result_event<'a>(
    events: &'a [EventEnvelope],
    input: &Value,
) -> Result<&'a EventEnvelope, ToolError> {
    // Live bus and resume rehydration keep tool output inline with empty
    // `blobs`; only event_id is a reliable session-local handle.
    let event_id = input
        .get("event_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(ToolError::MissingField("event_id"))?;
    let event = events
        .iter()
        .find(|event| event.id == event_id)
        .ok_or(ToolError::InvalidField("event_id"))?;
    if event.kind.as_str() != EventKind::TOOL_RESULT {
        return Err(ToolError::InvalidField("event_id"));
    }
    Ok(event)
}

fn tool_result_content(event: &EventEnvelope, ok: bool) -> &str {
    if ok {
        event
            .payload
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("")
    } else {
        event
            .payload
            .get("error")
            .and_then(Value::as_str)
            .or_else(|| event.payload.get("output").and_then(Value::as_str))
            .unwrap_or("")
    }
}

fn truncate_rehydrate_body(content: &str, max_bytes: usize) -> String {
    if content.len() <= max_bytes {
        return content.to_owned();
    }
    let cut = floor_char_boundary(content, max_bytes);
    format!(
        "{}\n[truncated: showing {} of {} bytes; call tool_result_get with a larger max_bytes for more]",
        &content[..cut],
        cut,
        content.len()
    )
}

#[cfg(test)]
#[path = "tools_test.rs"]
mod tools_test;
