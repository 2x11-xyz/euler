use crate::sandbox::WorkspaceSandbox;
use crate::{
    apply_patch_update_chunks, capture_workspace_snapshot, parse_single_file_apply_patch,
    ApplyPatchDocument, ApplyPatchError, ObservedFileChange, SandboxAvailability,
    SandboxUnavailableReason, SubprocessSandbox,
};
use euler_event::{EventEnvelope, EventKind};
use euler_provider::ToolDefinition;
use euler_sdk::Capability;
use serde_json::json;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use thiserror::Error;

/// Rung-2 escalation threshold (issue #94): the first failure of a
/// formatted tool gets the rung-1 teaching one-liner; from the second
/// consecutive failure of the SAME tool on, the tool's full-format
/// re-teach payload is appended to the error the model reads next.
const RETEACH_AFTER_CONSECUTIVE_FAILURES: u32 = 2;

/// Full apply_patch grammar plus worked examples, appended to repeated
/// parse failures. Every example here must parse: `reteach_examples_parse`
/// in tools_test.rs runs each `*** Begin Patch` block through the real
/// parser so this text can never drift into syntax the parser rejects.
const APPLY_PATCH_RETEACH: &str = r#"apply_patch full format specification:
A patch is one envelope that adds or updates exactly one file. Paths are relative to the workspace root; delete and rename are not supported. Send one patch per file.

Add a new file (every content line starts with `+`):
*** Begin Patch
*** Add File: src/example.rs
+fn main() {
+    println!("hello");
+}
*** End Patch

Update an existing file (one or more `@@` hunks; hunk lines start with a space for context, `-` for removed, `+` for added; each hunk's context and removed lines must match the file exactly once):
*** Begin Patch
*** Update File: src/example.rs
@@
 fn main() {
-    println!("hello");
+    println!("hello, world");
 }
*** End Patch"#;

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
    #[error("{0}")]
    SandboxUnavailable(SandboxUnavailableReason),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolExecution {
    pub name: String,
    /// Complete tool output retained in the canonical event and provenance.
    pub output: String,
    /// Optional display budget applied only after session redaction. The
    /// complete `output` remains recoverable by event id.
    pub output_preview_budget: Option<OutputPreviewBudget>,
    pub exit_code: Option<i32>,
    pub patch: Option<PatchEvents>,
    pub file_changes: Vec<ObservedFileChange>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputPreviewBudget {
    pub max_bytes: usize,
    pub max_lines: usize,
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

/// Per-tool consecutive-failure streaks driving rung-2 format
/// re-teaching (issue #94). One tracker per model context — the driver
/// session and each companion own their own — because context rot is a
/// property of a single model context, not of the process. A tool's
/// success clears only that tool's streak; other tools' outcomes never
/// touch it.
#[derive(Debug, Default)]
pub(crate) struct ReteachTracker {
    consecutive_failures: BTreeMap<String, u32>,
}

impl ReteachTracker {
    pub(crate) fn record_success(&mut self, identity: &str) {
        self.consecutive_failures.remove(identity);
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.consecutive_failures.is_empty()
    }

    fn record_failure(&mut self, identity: &str) -> u32 {
        let streak = self
            .consecutive_failures
            .entry(identity.to_owned())
            .or_insert(0);
        *streak += 1;
        *streak
    }
}

#[derive(Debug)]
pub struct ToolRegistry {
    root: PathBuf,
    workspace_sandbox: Option<WorkspaceSandbox>,
    agent_euler_home: OnceLock<tempfile::TempDir>,
}

impl ToolRegistry {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_subprocess_sandbox(root, SubprocessSandbox::Disabled)
    }

    /// Build a registry whose agent-controlled subprocesses either execute
    /// normally or must use the supplied workspace profile. An unavailable
    /// selected profile is retained so execution can fail closed with a
    /// concise diagnostic rather than silently falling back to the host.
    pub fn with_subprocess_sandbox(
        root: impl Into<PathBuf>,
        subprocess_sandbox: SubprocessSandbox,
    ) -> Self {
        let root = root.into();
        let workspace_sandbox = match subprocess_sandbox {
            SubprocessSandbox::Disabled => None,
            SubprocessSandbox::Enforce(profile) => Some(WorkspaceSandbox::new(&root, profile)),
        };
        Self {
            root,
            workspace_sandbox,
            agent_euler_home: OnceLock::new(),
        }
    }

    /// The workspace root every tool executes in (`run_shell` is
    /// `sh -c <command>` with this as its cwd). Permission requests carry it
    /// so path-confinement checks reason about the real execution cwd.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The enforcement result for the selected profile, if subprocess
    /// sandboxing was requested. `None` means ordinary host execution is the
    /// configured posture.
    pub fn sandbox_availability(&self) -> Option<SandboxAvailability> {
        self.workspace_sandbox
            .as_ref()
            .map(WorkspaceSandbox::availability)
    }

    pub fn required_capability(&self, name: &str) -> Option<Capability> {
        match name {
            "read_file" | "git_status" | "git_diff" | "tool_result_get" => Some(Capability::FsRead),
            "edit_file" | "write_file" | "apply_patch" => Some(Capability::FsWrite),
            "run_shell" => Some(Capability::ShellExec),
            // Session-level review gate (tools contract): executed by the
            // session, not this registry, but gated here like every tool.
            "code_swarm_review" => Some(Capability::AgentSpawn),
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

    /// Identity a tool call teaches (and counts failures) under: an
    /// intercepted `apply_patch` heredoc sent through `run_shell` counts
    /// against `apply_patch`, mirroring `permission_reason`. Everything
    /// else teaches under its own tool name.
    pub(crate) fn reteach_identity<'a>(&self, name: &'a str, input: &Value) -> &'a str {
        if is_shell_apply_patch_request(name, input) {
            "apply_patch"
        } else {
            name
        }
    }

    /// Rung-2 re-teach payload registry (issue #94): a tool with a strict
    /// input format registers its full grammar plus a worked example here.
    /// Registration is the only per-tool step — the escalation machinery
    /// in the session loops is tool-agnostic.
    fn reteach_payload(identity: &str) -> Option<&'static str> {
        match identity {
            "apply_patch" => Some(APPLY_PATCH_RETEACH),
            _ => None,
        }
    }

    /// Record a failed call in `tracker` and escalate the error text with
    /// the tool's full-format payload once that tool's consecutive-failure
    /// streak reaches [`RETEACH_AFTER_CONSECUTIVE_FAILURES`]. Deterministic:
    /// the same failure sequence always yields the same strings, so
    /// fixture and resume replays stay stable.
    pub(crate) fn teach_on_failure(
        &self,
        tracker: &mut ReteachTracker,
        name: &str,
        input: &Value,
        error: String,
    ) -> String {
        let identity = self.reteach_identity(name, input);
        let streak = tracker.record_failure(identity);
        match Self::reteach_payload(identity) {
            Some(payload) if streak >= RETEACH_AFTER_CONSECUTIVE_FAILURES => {
                format!("{error}\n\n{payload}")
            }
            _ => error,
        }
    }

    pub fn execute(&self, name: &str, input: &Value) -> Result<ToolExecution, ToolError> {
        match name {
            "read_file" => self.read_file(input),
            "edit_file" => self.edit_file(input),
            "write_file" => self.write_file(input),
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
            output_preview_budget: None,
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
            return self.prepare_create(relative, new, "edit_file");
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
            output_preview_budget: None,
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

    fn write_file(&self, input: &Value) -> Result<ToolExecution, ToolError> {
        let relative = required_str(input, "path")?;
        let content = required_str(input, "content")?;
        self.prepare_create(relative, content, "write_file")
    }

    /// Shared create path for `write_file` and `edit_file` with an empty
    /// `old`: create-only (never clobbers an existing file), same workspace
    /// confinement as every write, and the same `PatchEvents` add-action
    /// provenance apply_patch's `Add File` emits.
    fn prepare_create(
        &self,
        relative: &str,
        content: &str,
        origin: &'static str,
    ) -> Result<ToolExecution, ToolError> {
        let path = self.resolve_create_path(relative)?;
        if path.exists() {
            return Err(ToolError::FileAlreadyExists);
        }
        Ok(ToolExecution {
            name: origin.to_owned(),
            output: format!("created {relative}"),
            output_preview_budget: None,
            exit_code: None,
            patch: Some(PatchEvents {
                path: relative.to_owned(),
                before: String::new(),
                after: content.to_owned(),
                origin,
                action: "add",
                before_sha256: None,
                after_sha256: hash_bytes(content.as_bytes()),
                before_byte_len: 0,
                after_byte_len: content.len(),
                write_path: path,
                write_content: content.to_owned(),
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
                    output_preview_budget: None,
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
                    output_preview_budget: None,
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
        let child = self.agent_subprocess("sh", &["-c", command])?;
        let sandboxed = child.sandboxed;
        let outcome = run_with_timeout(child.command, timeout_ms)
            .map_err(|error| normalize_sandbox_subprocess_error(sandboxed, error))?;
        let text = collected_agent_output(
            outcome.stdout,
            outcome.stderr,
            sandboxed,
            outcome.status.is_none(),
        )?;
        let after = capture_workspace_snapshot(&self.root).ok();
        let file_changes = before
            .zip(after)
            .map_or_else(Vec::new, |(before, after)| before.changes_to(&after));
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
        let output = format!("{header}\n{text}");
        Ok(ToolExecution {
            name: "run_shell".to_owned(),
            output,
            output_preview_budget: Some(OutputPreviewBudget {
                max_bytes,
                max_lines: DEFAULT_MAX_LINES,
            }),
            exit_code: Some(status),
            patch: None,
            file_changes,
        })
    }

    fn git(&self, args: &[&str], name: &str) -> Result<ToolExecution, ToolError> {
        let mut child = self.agent_subprocess("git", args)?;
        let sandboxed = child.sandboxed;
        let output = child
            .command
            .output()
            .map_err(ToolError::Io)
            .map_err(|error| normalize_sandbox_subprocess_error(sandboxed, error))?;
        let text = collected_agent_output(
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
            sandboxed,
            false,
        )?;
        let status = output.status.code().unwrap_or(-1);
        Ok(ToolExecution {
            name: name.to_owned(),
            output: text,
            output_preview_budget: Some(OutputPreviewBudget {
                max_bytes: DEFAULT_MAX_BYTES,
                max_lines: DEFAULT_MAX_LINES,
            }),
            exit_code: Some(status),
            patch: None,
            file_changes: Vec::new(),
        })
    }

    /// Construct the child process for an agent-controlled command. The
    /// sandbox branch deliberately receives no host `current_dir`: Bubblewrap
    /// establishes `/workspace` inside its private mount namespace.
    fn agent_subprocess(&self, program: &str, args: &[&str]) -> Result<AgentSubprocess, ToolError> {
        let sandboxed = self.workspace_sandbox.is_some();
        let mut child = match &self.workspace_sandbox {
            Some(sandbox) => sandbox
                .command(program, args)
                .map_err(ToolError::SandboxUnavailable)?,
            None => {
                let mut command = Command::new(program);
                command.args(args).current_dir(&self.root);
                command
            }
        };
        // Defense in depth: Bubblewrap clears this environment too, while
        // ordinary host execution needs an explicit child-process boundary.
        scrub_agent_subprocess_env(&mut child);
        if !sandboxed {
            child.env("EULER_HOME", self.agent_euler_home()?);
        }
        Ok(AgentSubprocess {
            command: child,
            sandboxed,
        })
    }

    fn agent_euler_home(&self) -> Result<&Path, ToolError> {
        if let Some(home) = self.agent_euler_home.get() {
            return Ok(home.path());
        }
        let candidate = tempfile::Builder::new()
            .prefix("euler-agent-home-")
            .tempdir()?;
        let _ = self.agent_euler_home.set(candidate);
        Ok(self
            .agent_euler_home
            .get()
            .expect("an initialized agent Euler home cannot disappear")
            .path())
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

/// One prepared agent-controlled process, with enough provenance to remove
/// the sandbox launch prelude from captured output.
struct AgentSubprocess {
    command: Command,
    sandboxed: bool,
}

/// Preserve program output, but do not expose Bubblewrap diagnostics when the
/// launcher did not reach the inner command. The readiness marker is emitted
/// by the private sandbox wrapper only after its mount namespace exists.
fn collected_agent_output(
    stdout: String,
    stderr: String,
    sandboxed: bool,
    timed_out: bool,
) -> Result<String, ToolError> {
    let stdout = if sandboxed {
        match crate::sandbox::strip_sandbox_ready_marker(&stdout) {
            Ok(stdout) => stdout,
            // A requested timeout can kill the launcher before it is ready.
            // It is still a timeout, but its raw stdout/stderr must remain
            // hidden because neither came from the agent command.
            Err(_) if timed_out => return Ok(String::new()),
            Err(reason) => return Err(ToolError::SandboxUnavailable(reason)),
        }
    } else {
        &stdout
    };
    Ok(format!("{stdout}{stderr}"))
}

/// A selected profile must never fall back to host execution or disclose raw
/// launcher details. An I/O failure while launching or supervising it is
/// therefore reported as the same concise enforcement failure as a missing
/// readiness marker.
fn normalize_sandbox_subprocess_error(sandboxed: bool, error: ToolError) -> ToolError {
    if sandboxed && matches!(&error, ToolError::Io(_)) {
        ToolError::SandboxUnavailable(SandboxUnavailableReason::CannotEnforce)
    } else {
        error
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

fn scrub_agent_subprocess_env(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        if is_secret_env_name(&name) || is_parent_control_env_name(&name) {
            command.env_remove(name);
        }
    }
}

/// Ambient controls for the owning Euler process must not silently configure
/// programs launched by the agent. A command can still set any of these
/// explicitly in its own shell text when that is part of the requested work.
fn is_parent_control_env_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    matches!(
        name,
        "EULER_HOME" | "EULER_PROVIDER" | "EULER_MODEL" | "EULER_NO_TTY" | "EULER_TUI_METRICS"
    )
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
    stdout: String,
    stderr: String,
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
    Ok(ShellOutcome {
        status,
        stdout: String::from_utf8_lossy(&stdout.join().unwrap_or_default()).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.join().unwrap_or_default()).into_owned(),
    })
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

pub(crate) fn bound_text(text: &str, max_bytes: usize, max_lines: usize) -> String {
    let line_count = text.split_inclusive('\n').count();
    if text.len() <= max_bytes && line_count <= max_lines {
        return text.to_owned();
    }

    // Preserve both the command's beginning and its terminal/error region.
    // Byte and line budgets apply to retained content; the honest omission
    // marker is additional projection metadata.
    let head_line_count = max_lines.div_ceil(2);
    let tail_line_count = max_lines / 2;
    let head_line_end = text
        .split_inclusive('\n')
        .take(head_line_count)
        .map(|line| line.len())
        .sum::<usize>();
    let tail_line_bytes = text
        .split_inclusive('\n')
        .rev()
        .take(tail_line_count)
        .map(|line| line.len())
        .sum::<usize>();
    let tail_line_start = text.len().saturating_sub(tail_line_bytes);

    let head_byte_budget = max_bytes.div_ceil(2);
    let tail_byte_budget = max_bytes / 2;
    let head_end = floor_char_boundary(text, head_line_end.min(head_byte_budget));
    let tail_byte_start = ceil_char_boundary(text, text.len().saturating_sub(tail_byte_budget));
    let tail_start = tail_line_start.max(tail_byte_start).max(head_end);

    let mut output = text[..head_end].to_owned();
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str("[... middle output omitted ...]");
    if tail_start < text.len() {
        output.push('\n');
        output.push_str(&text[tail_start..]);
    }
    if output.len() < text.len() {
        output
    } else {
        text.to_owned()
    }
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
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
        write_file_definition(),
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
                    "max_bytes": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum command-output bytes retained across the active head/tail preview. Complete output remains recoverable by result event id."
                    },
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

fn write_file_definition() -> ToolDefinition {
    ToolDefinition {
        name: "write_file".to_owned(),
        description: "Create a new UTF-8 file at `path` (relative to the workspace root) with exactly `content`. Fails if the file already exists — use edit_file or apply_patch to modify an existing file — and if the parent directory is missing. Absolute and parent-traversal paths are rejected. For creating whole files this is more direct than apply_patch: plain JSON fields, no patch syntax.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative path of the file to create; the parent directory must already exist."
                },
                "content": {
                    "type": "string",
                    "description": "Complete file contents, written verbatim."
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }),
    }
}

fn tool_result_get_definition() -> ToolDefinition {
    ToolDefinition {
        name: "tool_result_get".to_owned(),
        description: "Rehydrate a demoted, compacted, or previewed tool result from the current session by event_id (required). Use optional offset_bytes and max_bytes to read a bounded byte window instead of re-running the original tool.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "event_id": {
                    "type": "string",
                    "description": "Tool-result event id printed by a canvas preview or stub."
                },
                "offset_bytes": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "UTF-8 byte offset into the canonical redacted result; defaults to 0."
                },
                "max_bytes": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Target result-body byte budget; defaults to 65536 and may expand just enough to return one complete UTF-8 code point."
                }
            },
            "required": ["event_id"],
            "additionalProperties": false
        }),
    }
}

fn tool_result_get(events: &[EventEnvelope], input: &Value) -> Result<ToolExecution, ToolError> {
    let offset_bytes = optional_usize(input, "offset_bytes")?.unwrap_or(0);
    let max_bytes = optional_positive_usize(input, "max_bytes")?.unwrap_or(64 * 1024);
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
    let window = rehydrate_window(content, offset_bytes, max_bytes);
    let status = if ok { "ok" } else { "failed" };
    let mut output = format!(
        "[rehydrated {name} event {} ({status}); bytes {}..{} of {}]\n{}",
        event.id, window.start, window.end, window.total, window.body
    );
    if window.end < window.total {
        output.push_str(&format!(
            "\n[truncated: call tool_result_get with event_id={} and offset_bytes={} for more]",
            event.id, window.end
        ));
    }
    Ok(ToolExecution {
        name: "tool_result_get".to_owned(),
        output,
        output_preview_budget: None,
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

struct RehydrateWindow {
    body: String,
    start: usize,
    end: usize,
    total: usize,
}

fn rehydrate_window(content: &str, offset_bytes: usize, max_bytes: usize) -> RehydrateWindow {
    let total = content.len();
    let start = ceil_char_boundary(content, offset_bytes.min(total));
    let requested_end = start.saturating_add(max_bytes).min(total);
    let mut end = floor_char_boundary(content, requested_end);
    if end == start && start < total {
        end = start + content[start..].chars().next().map_or(0, char::len_utf8);
    }
    RehydrateWindow {
        body: content[start..end].to_owned(),
        start,
        end,
        total,
    }
}

#[cfg(test)]
#[path = "tools_test.rs"]
mod tools_test;
