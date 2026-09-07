use crate::git_neutralization::{
    cached_fsmonitor_daemon_support, fsmonitor_override, parse_executable_config, GitNeutralization,
};
use crate::sandbox::WorkspaceSandbox;
use crate::structured_file;
use crate::{
    apply_patch_update_chunks, capture_workspace_snapshot, parse_single_file_apply_patch,
    ApplyPatchDocument, ApplyPatchError, IncompleteObservation, ObservedFileChange,
    SandboxAvailability, SandboxFailureCause, SandboxStatus, SandboxUnavailableReason,
    SubprocessSandbox, WorkspaceSnapshot, MAX_WORKSPACE_SNAPSHOT_FILES,
};
use euler_event::{tool_result_succeeded, EventEnvelope, EventKind};
use euler_provider::ToolDefinition;
use euler_sdk::{CancellationToken, Capability};
use serde_json::json;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Read as _;
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
    #[error("path `{path}` is not a regular file; structured file tools read and write regular files only")]
    UnsupportedFileType { path: String },
    #[error(
        "file `{path}` changed after this write was prepared; read it again and prepare a new edit"
    )]
    StalePreparedWrite { path: String },
    #[error("cannot write `{path}`: {subject} is read-only")]
    ReadOnlyTarget { path: String, subject: &'static str },
    #[error("unsupported tool `{0}`")]
    Unsupported(String),
    #[error("replacement text matched {0} times; expected exactly one")]
    ReplacementMatchCount(usize),
    #[error("update hunk {hunk} matched {count} times; expected exactly one")]
    UpdateHunkMatchCount { hunk: usize, count: usize },
    #[error("update hunk {hunk} overlaps earlier update hunk {previous_hunk}")]
    UpdateHunkOverlap { hunk: usize, previous_hunk: usize },
    #[error("{reason} ({cause}); run `euler --check-sandbox` for the full diagnostic")]
    SandboxUnavailable {
        reason: SandboxUnavailableReason,
        /// The likely host cause, so an in-session failure carries the same
        /// attribution the session-start diagnostic did.
        cause: SandboxFailureCause,
    },
    #[error("{0}, so Euler will not run git with repository-selected helpers live")]
    GitProbeFailed(&'static str),
    #[error("tool cancelled")]
    Cancelled,
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
    /// Snapshot digest when this output contains project-context-classified
    /// bytes. The session persists it on `tool.result`; canvas filtering and
    /// rehydration must preserve it so child policy cannot be bypassed.
    pub project_context_snapshot_digest: Option<String>,
    pub exit_code: Option<i32>,
    pub patch: Option<PatchEvents>,
    pub file_changes: Vec<ObservedFileChange>,
    /// Set when the workspace walk around this tool could not observe
    /// everything. `file_changes` is then "not observed", never "no changes"
    /// (ADR 0021 row E).
    pub observation: Option<IncompleteObservation>,
}

/// Result of a tool invocation that was admitted before cancellation.
///
/// A cancelled subprocess may already have produced output and changed the
/// workspace before its process group was stopped. Keep that evidence as a
/// normal [`ToolExecution`] while distinguishing it from completion so the
/// session can emit an honest failed `tool.result`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ToolExecutionOutcome {
    Completed(ToolExecution),
    Cancelled(ToolExecution),
}

#[derive(Clone, Copy)]
enum ToolResultAccess<'a> {
    All,
    Child {
        allowed_project_context_snapshot_digest: Option<&'a str>,
    },
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
    target: ResolvedWorkspacePath,
    write_content: String,
}

/// A model-supplied path already resolved against the workspace root, kept as
/// the root plus the normalized components beneath it. The structured tools
/// re-open the target from `root` hop by hop at apply time; the joined
/// `absolute` form exists only for diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedWorkspacePath {
    root: PathBuf,
    relative: PathBuf,
}

impl ResolvedWorkspacePath {
    fn absolute(&self) -> PathBuf {
        self.root.join(&self.relative)
    }

    /// The model-visible form of this path: workspace-relative, control
    /// characters scrubbed and length capped by [`display_path`]. The host
    /// root never appears in a tool error or a durability warning.
    fn display(&self) -> String {
        display_path(&self.relative.to_string_lossy())
    }

    /// Resolve this target to a descriptor on its confined parent directory.
    fn confine(&self) -> Result<structured_file::ConfinedTarget, ToolError> {
        structured_file::confine(&self.root, &self.relative).map_err(ToolError::Io)
    }
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
    skills: BTreeMap<String, FrozenSkill>,
    observation_bound: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrozenSkill {
    /// Candidate digest of the immutable snapshot that owns this body.
    pub snapshot_digest: String,
    pub name: String,
    pub description: String,
    pub scope: String,
    /// Source identity (workspace-relative or `user/` path) echoed in the
    /// `skill_read` result header; never re-read from disk.
    pub path: String,
    pub body_digest: String,
    pub body: String,
}

/// Discoverable metadata for one skill frozen into the current session.
///
/// This view deliberately omits the body. Interactive surfaces may use it for
/// command discovery, but admission always resolves against the registry that
/// produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillCatalogEntry {
    pub name: String,
    pub description: String,
}

pub(crate) struct ResolvedSkillActivation {
    pub model_content: String,
    pub snapshot_digest: String,
    pub scope: String,
    pub source: String,
    pub body_digest: String,
}

impl ToolRegistry {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_subprocess_sandbox(root, SubprocessSandbox::Host)
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
            SubprocessSandbox::Host => None,
            SubprocessSandbox::Enforce(profile) => Some(WorkspaceSandbox::new(&root, profile)),
        };
        Self {
            root,
            workspace_sandbox,
            agent_euler_home: OnceLock::new(),
            skills: BTreeMap::new(),
            observation_bound: MAX_WORKSPACE_SNAPSHOT_FILES,
        }
    }

    /// Override the workspace-observation entry bound (ADR 0021 row E). The
    /// bound exists so a pathological tree cannot stall a command; a caller
    /// working in a large repository can raise it, and tests lower it.
    #[must_use]
    pub fn with_observation_bound(mut self, bound: usize) -> Self {
        self.observation_bound = bound.max(1);
        self
    }

    /// The execution boundary that agent subprocesses actually get, as
    /// recorded on `session.start`.
    pub fn sandbox_status(&self) -> SandboxStatus {
        self.workspace_sandbox
            .as_ref()
            .map_or(SandboxStatus::Host, WorkspaceSandbox::status)
    }

    /// Things the profile can see but cannot fix, worth saying once. Empty
    /// unless a sandbox was requested and something is worth reporting.
    pub fn sandbox_advisories(&self) -> Vec<String> {
        self.workspace_sandbox
            .as_ref()
            .map(WorkspaceSandbox::advisories)
            .unwrap_or_default()
    }

    pub fn set_frozen_skills(&mut self, skills: impl IntoIterator<Item = FrozenSkill>) {
        self.skills = skills
            .into_iter()
            .map(|skill| (skill.name.clone(), skill))
            .collect();
    }

    /// Return the frozen catalog in canonical name order.
    pub fn skill_catalog(&self) -> Vec<SkillCatalogEntry> {
        self.skills
            .values()
            .map(|skill| SkillCatalogEntry {
                name: skill.name.clone(),
                description: skill.description.clone(),
            })
            .collect()
    }

    pub(crate) fn resolve_skill_activation(
        &self,
        name: &str,
        arguments: Option<&str>,
    ) -> Option<ResolvedSkillActivation> {
        let skill = self.skills.get(name)?;
        Some(ResolvedSkillActivation {
            model_content: crate::project_context::render_skill_activation(
                &skill.name,
                &skill.scope,
                &skill.path,
                &skill.body_digest,
                &skill.body,
                arguments,
            ),
            snapshot_digest: skill.snapshot_digest.clone(),
            scope: skill.scope.clone(),
            source: skill.path.clone(),
            body_digest: skill.body_digest.clone(),
        })
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
        match self.execute_cancellable(name, input, &CancellationToken::new())? {
            ToolExecutionOutcome::Completed(execution) => Ok(execution),
            ToolExecutionOutcome::Cancelled(_) => {
                unreachable!("a private never-cancelled invocation cannot be cancelled")
            }
        }
    }

    pub(crate) fn execute_cancellable(
        &self,
        name: &str,
        input: &Value,
        cancellation: &CancellationToken,
    ) -> Result<ToolExecutionOutcome, ToolError> {
        if cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let execution = match name {
            "read_file" => self.read_file(input),
            "edit_file" => self.edit_file(input),
            "write_file" => self.write_file(input),
            "apply_patch" => self.apply_patch_tool(input),
            "run_shell" => return self.run_shell(input, cancellation),
            "git_status" => {
                return self.git(
                    &["status", "--short", "--ignore-submodules=dirty"],
                    "git_status",
                    cancellation,
                )
            }
            "git_diff" => {
                return self.git(
                    &[
                        "diff",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--ignore-submodules=dirty",
                        "--",
                    ],
                    "git_diff",
                    cancellation,
                )
            }
            "tool_result_get" => Err(ToolError::InvalidField(
                "tool_result_get requires session events",
            )),
            "skill_read" => self.skill_read(input),
            other => Err(ToolError::Unsupported(other.to_owned())),
        }?;
        Ok(ToolExecutionOutcome::Completed(execution))
    }

    pub fn execute_with_events(
        &self,
        name: &str,
        input: &Value,
        events: &[EventEnvelope],
    ) -> Result<ToolExecution, ToolError> {
        match self.execute_with_events_cancellable(
            name,
            input,
            events,
            &CancellationToken::new(),
        )? {
            ToolExecutionOutcome::Completed(execution) => Ok(execution),
            ToolExecutionOutcome::Cancelled(_) => {
                unreachable!("a private never-cancelled invocation cannot be cancelled")
            }
        }
    }

    pub(crate) fn execute_with_events_cancellable(
        &self,
        name: &str,
        input: &Value,
        events: &[EventEnvelope],
        cancellation: &CancellationToken,
    ) -> Result<ToolExecutionOutcome, ToolError> {
        self.execute_with_events_cancellable_access(
            name,
            input,
            events,
            cancellation,
            ToolResultAccess::All,
        )
    }

    /// Child execution preserves the ordinary coding tool surface while
    /// enforcing the request's project-context data-flow policy on result
    /// rehydration. `None` denies every classified result; an inherited digest
    /// admits only bytes from that exact immutable snapshot.
    pub(crate) fn execute_with_events_cancellable_for_child(
        &self,
        name: &str,
        input: &Value,
        events: &[EventEnvelope],
        cancellation: &CancellationToken,
        allowed_project_context_snapshot_digest: Option<&str>,
    ) -> Result<ToolExecutionOutcome, ToolError> {
        self.execute_with_events_cancellable_access(
            name,
            input,
            events,
            cancellation,
            ToolResultAccess::Child {
                allowed_project_context_snapshot_digest,
            },
        )
    }

    fn execute_with_events_cancellable_access(
        &self,
        name: &str,
        input: &Value,
        events: &[EventEnvelope],
        cancellation: &CancellationToken,
        result_access: ToolResultAccess<'_>,
    ) -> Result<ToolExecutionOutcome, ToolError> {
        if cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if name == "tool_result_get" {
            return tool_result_get(events, input, result_access)
                .map(ToolExecutionOutcome::Completed);
        }
        self.execute_cancellable(name, input, cancellation)
    }

    pub fn model_tools(&self) -> Vec<ToolDefinition> {
        let mut tools = coding_tool_definitions();
        tools.push(tool_result_get_definition());
        if !self.skills.is_empty() {
            tools.push(skill_read_definition());
        }
        tools
    }

    /// Tools advertised to companion/spawned agents. Children default to
    /// project-context `none`, so the skill catalog and `skill_read` never
    /// reach them; only the root driver (or an `inherit` child, once that
    /// lands) is eligible (docs/contracts/project-context.md).
    pub fn child_model_tools(&self) -> Vec<ToolDefinition> {
        let mut tools = coding_tool_definitions();
        tools.push(tool_result_get_definition());
        tools
    }

    fn skill_read(&self, input: &Value) -> Result<ToolExecution, ToolError> {
        let name = required_str(input, "name")?;
        let skill = self
            .skills
            .get(name)
            .ok_or(ToolError::InvalidField("name"))?;
        Ok(ToolExecution {
            name: "skill_read".to_owned(),
            output: crate::project_context::render_skill_result(
                &skill.name,
                &skill.scope,
                &skill.path,
                &skill.body_digest,
                &skill.body,
            ),
            output_preview_budget: None,
            project_context_snapshot_digest: Some(skill.snapshot_digest.clone()),
            exit_code: None,
            patch: None,
            file_changes: Vec::new(),
            observation: None,
        })
    }

    fn read_file(&self, input: &Value) -> Result<ToolExecution, ToolError> {
        let path = self.resolve_path(required_str(input, "path")?)?;
        let offset = optional_positive_usize(input, "offset")?.unwrap_or(1);
        let max_bytes = optional_positive_usize(input, "max_bytes")?.unwrap_or(DEFAULT_MAX_BYTES);
        let max_lines = optional_positive_usize(input, "max_lines")?.unwrap_or(DEFAULT_MAX_LINES);
        let content = self.read_resolved_file(&path)?;
        let output = bound_read_file_window(&content, offset, max_bytes, max_lines);
        Ok(ToolExecution {
            name: "read_file".to_owned(),
            output,
            output_preview_budget: None,
            project_context_snapshot_digest: None,
            exit_code: None,
            patch: None,
            file_changes: Vec::new(),
            observation: None,
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
        let content = self.read_resolved_file(&path)?;
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
            project_context_snapshot_digest: None,
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
                target: path,
                write_content: updated,
            }),
            file_changes: Vec::new(),
            observation: None,
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
        if path.absolute().exists() {
            return Err(ToolError::FileAlreadyExists);
        }
        Ok(ToolExecution {
            name: origin.to_owned(),
            output: format!("created {relative}"),
            output_preview_budget: None,
            project_context_snapshot_digest: None,
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
                target: path,
                write_content: content.to_owned(),
            }),
            file_changes: Vec::new(),
            observation: None,
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
                let target = self.resolve_create_path(&path)?;
                if target.absolute().exists() {
                    return Err(ToolError::FileAlreadyExists);
                }
                Ok(ToolExecution {
                    name: name.to_owned(),
                    output: format!("{label} prepared add {path}"),
                    output_preview_budget: None,
                    project_context_snapshot_digest: None,
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
                        target,
                        write_content: content,
                    }),
                    file_changes: Vec::new(),
                    observation: None,
                })
            }
            ApplyPatchDocument::Update { path, chunks } => {
                let target = self.resolve_path(&path)?;
                let content = self.read_resolved_file(&target)?;
                let updated = apply_patch_update_chunks(&content, &chunks)
                    .map_err(tool_error_from_apply_patch)?;
                Ok(ToolExecution {
                    name: name.to_owned(),
                    output: format!("{label} prepared update {path}"),
                    output_preview_budget: None,
                    project_context_snapshot_digest: None,
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
                        target,
                        write_content: updated,
                    }),
                    file_changes: Vec::new(),
                    observation: None,
                })
            }
        }
    }

    pub fn apply_patch(&self, patch: &PatchEvents) -> Result<(), ToolError> {
        self.apply_patch_cancellable(patch, &CancellationToken::new())
            .map(|_| ())
    }

    /// `Ok(Some(warning))` means the write is applied but its directory entry
    /// could not be made durable. The write happened; the caller reports the
    /// caveat rather than a failure.
    pub(crate) fn apply_patch_cancellable(
        &self,
        patch: &PatchEvents,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>, ToolError> {
        // This is the final check before the filesystem mutation. Patch
        // parsing, permission review, and `patch.proposed` emission may all
        // have taken time during which the user pressed Esc.
        if cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let expected = if patch.action == "add" {
            ExpectedTarget::Absent
        } else {
            ExpectedTarget::Exactly(&patch.before)
        };
        write_confined(&patch.target, &patch.write_content, expected)
    }

    /// Replace a workspace-relative file whose current content is already
    /// known, without going through a prepared patch. Used by `/rollback`.
    pub(crate) fn write_verified_workspace_file(
        &self,
        relative: &str,
        content: &str,
        expected: ExpectedTarget<'_>,
    ) -> Result<Option<String>, ToolError> {
        let path = match expected {
            ExpectedTarget::Absent => self.resolve_create_path(relative)?,
            ExpectedTarget::Exactly(_) => self.resolve_path(relative)?,
        };
        write_confined(&path, content, expected)
    }

    /// Refuse a prepared patch whose target cannot be written, before any
    /// checkpoint is stored or recorded for it.
    pub(crate) fn ensure_patch_writable(&self, patch: &PatchEvents) -> Result<(), ToolError> {
        if patch.action == "add" {
            return Ok(());
        }
        ensure_writable(&patch.target.confine()?, &patch.target)
    }

    /// Read a workspace-relative path through the confined structured open
    /// (used by `/rollback` to see what it is about to replace).
    pub(crate) fn read_workspace_file(&self, relative: &str) -> Result<String, ToolError> {
        self.read_resolved_file(&self.resolve_path(relative)?)
    }

    fn run_shell(
        &self,
        input: &Value,
        cancellation: &CancellationToken,
    ) -> Result<ToolExecutionOutcome, ToolError> {
        let command = required_str(input, "command")?;
        let max_bytes = optional_positive_usize(input, "max_bytes")?.unwrap_or(DEFAULT_MAX_BYTES);
        if command_begins_apply_patch(command) {
            // Strict apply_patch interception must return before spawning a shell.
            return self
                .apply_patch_text(
                    &strict_apply_patch_heredoc(command)?,
                    "run_shell:apply_patch",
                    "run_shell",
                )
                .map(ToolExecutionOutcome::Completed);
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
        let before = self.observe_workspace();
        let child = self.agent_subprocess("sh", &["-c", command], &[])?;
        let sandboxed = child.sandboxed;
        let outcome = run_process(child.command, Some(timeout_ms), cancellation)
            .map_err(|error| normalize_sandbox_subprocess_error(sandboxed, error))?;
        let text = collected_agent_output(
            outcome.stdout,
            outcome.stderr,
            sandboxed,
            matches!(
                outcome.termination,
                ProcessTermination::TimedOut | ProcessTermination::Cancelled
            ),
        )?;
        let after = self.observe_workspace();
        let observation =
            incomplete_observation(before.as_ref(), after.as_ref(), self.observation_bound);
        let file_changes = before
            .zip(after)
            .map_or_else(Vec::new, |(before, after)| before.changes_to(&after));
        let (status, header, cancelled) = shell_termination(outcome.termination, timeout_ms);
        // The incompleteness leads the agent-visible text: a model that sees
        // only "exit 0" and an empty change list would otherwise read an
        // unobserved workspace as an unchanged one (ADR 0021 row E).
        let output = match observation {
            Some(observation) => format!(
                "file observation incomplete: {}; changes may be unreported\n{header}\n{text}",
                observation.describe()
            ),
            None => format!("{header}\n{text}"),
        };
        let execution = ToolExecution {
            name: "run_shell".to_owned(),
            output,
            output_preview_budget: Some(OutputPreviewBudget {
                max_bytes,
                max_lines: DEFAULT_MAX_LINES,
            }),
            project_context_snapshot_digest: None,
            exit_code: Some(status),
            patch: None,
            file_changes,
            observation,
        };
        Ok(if cancelled {
            ToolExecutionOutcome::Cancelled(execution)
        } else {
            ToolExecutionOutcome::Completed(execution)
        })
    }

    /// Run one of Euler's own git tools under ADR 0021 row G neutralization.
    ///
    /// The two probes ahead of the command discover what this repository has
    /// configured; the neutralization then overrides exactly that, because a
    /// blanket override cannot know which filter drivers exist and blanket
    /// `core.fsmonitor=false` costs a full worktree scan.
    fn git(
        &self,
        command: &[&str],
        name: &str,
        cancellation: &CancellationToken,
    ) -> Result<ToolExecutionOutcome, ToolError> {
        let neutralization = self.git_neutralization(cancellation)?;
        let args = neutralization.args(command);
        let child = self.agent_subprocess("git", &args, neutralization.env())?;
        let sandboxed = child.sandboxed;
        let mut command = child.command;
        neutralization.strip_inherited_redirects(&mut command);
        let outcome = run_process(command, None, cancellation)
            .map_err(|error| normalize_sandbox_subprocess_error(sandboxed, error))?;
        let cancelled = outcome.termination == ProcessTermination::Cancelled;
        let text = collected_agent_output(outcome.stdout, outcome.stderr, sandboxed, cancelled)?;
        let status = match outcome.termination {
            ProcessTermination::Exited(status) => status,
            ProcessTermination::Cancelled => -1,
            ProcessTermination::TimedOut => unreachable!("git has no timeout"),
        };
        // Only on a run that succeeded: a note about what a successful listing
        // omits has nothing to say about why a command failed or was killed.
        let text = if status == 0 {
            format!("{text}{}", self.submodule_notice())
        } else {
            text
        };
        let execution = ToolExecution {
            name: name.to_owned(),
            output: text,
            output_preview_budget: Some(OutputPreviewBudget {
                max_bytes: DEFAULT_MAX_BYTES,
                max_lines: DEFAULT_MAX_LINES,
            }),
            project_context_snapshot_digest: None,
            exit_code: Some(status),
            patch: None,
            file_changes: Vec::new(),
            observation: None,
        };
        Ok(if cancelled {
            ToolExecutionOutcome::Cancelled(execution)
        } else {
            ToolExecutionOutcome::Completed(execution)
        })
    }

    /// Say what the submodule neutralization costs, where it costs anything.
    ///
    /// `diff.ignoreSubmodules=dirty` is what stops the recursive spawn that
    /// would run a driver configured in a submodule's own config, so it
    /// stays. But it also hides worktree edits inside a submodule, and an
    /// agent told its changes do not exist is the silent loss ADR 0021 row E
    /// exists to prevent.
    ///
    /// `modules/` under the git directory is the cheap test, and the right
    /// one: `.gitmodules` alone is a declaration, so a fresh clone before
    /// `submodule update` would get the note on every call while nothing is
    /// being hidden. A directory check keeps this free of an extra git spawn.
    fn submodule_notice(&self) -> &'static str {
        if git_directory(&self.root).join("modules").is_dir() {
            "\nnote: changes inside submodule worktrees are not shown here, because Euler does \
not let git recurse into submodules; run `git status` or `git diff` inside the submodule to \
see them.\n"
        } else {
            ""
        }
    }

    /// Probe this repository for the two pieces of configuration Git turns
    /// into executable code, then build the overrides that neutralize them.
    /// A probe that cannot run yields the strictest answer.
    fn git_neutralization(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<GitNeutralization, ToolError> {
        let probe = GitNeutralization::probe();
        let config =
            parse_executable_config(&self.git_probe(&probe, &probe.probe_args(), cancellation)?);
        // Only a repository that configures fsmonitor pays for the second
        // launch, and only the first such repository in this process.
        let has_daemon = config.fsmonitor.is_some()
            && cached_fsmonitor_daemon_support(|| {
                self.git_probe(
                    &probe,
                    &probe.args(&["version", "--build-options"]),
                    cancellation,
                )
                .ok()
            });
        let fsmonitor = fsmonitor_override(config.fsmonitor.as_deref(), has_daemon);
        Ok(GitNeutralization::new(fsmonitor, &config.filter_drivers))
    }

    /// Run one bounded git probe and return its stdout.
    ///
    /// Exit 1 from `git config --get-regexp` is the legitimate answer
    /// "nothing configured". Every other outcome — a timeout, a spawn failure,
    /// any other status — means Euler does not know what this repository has
    /// configured, and the command must not then run with repository-selected
    /// helpers live.
    fn git_probe(
        &self,
        neutralization: &GitNeutralization,
        args: &[&str],
        cancellation: &CancellationToken,
    ) -> Result<String, ToolError> {
        let child = self.agent_subprocess("git", args, neutralization.env())?;
        let sandboxed = child.sandboxed;
        let mut command = child.command;
        neutralization.strip_inherited_redirects(&mut command);
        let outcome = run_process(command, Some(GIT_PROBE_TIMEOUT_MS), cancellation)
            .map_err(|error| normalize_sandbox_subprocess_error(sandboxed, error))?;
        match outcome.termination {
            ProcessTermination::Exited(0) => {}
            ProcessTermination::Exited(1) => return Ok(String::new()),
            ProcessTermination::Cancelled => return Err(ToolError::Cancelled),
            // A timeout is worth separating: the wait is long enough to be
            // felt, and the cause is almost never the repository's contents.
            ProcessTermination::TimedOut => {
                return Err(ToolError::GitProbeFailed(GIT_PROBE_TIMEOUT_MESSAGE))
            }
            ProcessTermination::Exited(_) => {
                return Err(ToolError::GitProbeFailed(
                    "could not read this repository's git configuration",
                ))
            }
        }
        if !sandboxed {
            return Ok(outcome.stdout);
        }
        crate::sandbox::strip_sandbox_ready_marker(&outcome.stdout)
            .map(str::to_owned)
            .map_err(sandbox_unavailable)
    }

    /// Construct the child process for an agent-controlled command. The
    /// sandbox branch deliberately receives no host `current_dir`: Bubblewrap
    /// establishes `/workspace` inside its private mount namespace.
    fn agent_subprocess(
        &self,
        program: &str,
        args: &[&str],
        env: &[(OsString, OsString)],
    ) -> Result<AgentSubprocess, ToolError> {
        let sandboxed = self.workspace_sandbox.is_some();
        let mut child = match &self.workspace_sandbox {
            Some(sandbox) => sandbox
                .command(program, args, env)
                .map_err(sandbox_unavailable)?,
            None => {
                let mut command = Command::new(program);
                command
                    .args(args)
                    .current_dir(&self.root)
                    .envs(env.to_vec());
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

    /// Walk the workspace under the configured observation bound.
    fn observe_workspace(&self) -> Option<WorkspaceSnapshot> {
        capture_workspace_snapshot(&self.root, self.observation_bound).ok()
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

    fn resolve_path(&self, relative: &str) -> Result<ResolvedWorkspacePath, ToolError> {
        self.resolve_path_inner(relative, false)
    }

    /// Canonicalized workspace-relative form of a model-supplied path, for
    /// scope matching: `..` and symlinks resolved exactly as the write path
    /// resolves them. `None` when the path cannot be resolved inside the
    /// workspace - scoped grant matching then fails closed to the ask path.
    pub fn workspace_relative_path(&self, relative: &str) -> Option<PathBuf> {
        Some(self.resolve_path_inner(relative, false).ok()?.relative)
    }

    fn resolve_create_path(&self, relative: &str) -> Result<ResolvedWorkspacePath, ToolError> {
        self.resolve_path_inner(relative, true)
    }

    fn resolve_path_inner(
        &self,
        relative: &str,
        parent_must_be_directory: bool,
    ) -> Result<ResolvedWorkspacePath, ToolError> {
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
        let Ok(relative_path) = canonical.strip_prefix(&root) else {
            return Err(ToolError::PathOutsideWorkspace {
                path: display_path(relative),
                reason: "path escapes the workspace root",
            });
        };
        let relative_path = relative_path.to_path_buf();
        Ok(ResolvedWorkspacePath {
            root,
            relative: relative_path,
        })
    }

    /// Read a structured-tool target through its confined descriptor. The
    /// regular-file check runs on the same descriptor the bytes come from.
    fn read_resolved_file(&self, path: &ResolvedWorkspacePath) -> Result<String, ToolError> {
        read_confined(&path.confine()?, path)
    }
}

/// Read an already-confined target, refusing anything that is not a regular
/// file at the moment the descriptor was opened.
fn read_confined(
    target: &structured_file::ConfinedTarget,
    path: &ResolvedWorkspacePath,
) -> Result<String, ToolError> {
    let mut file = target.open_read()?;
    if !file.metadata()?.is_file() {
        return Err(ToolError::UnsupportedFileType {
            path: path.display(),
        });
    }
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    Ok(content)
}

/// What the caller believes is at the target right now.
#[derive(Clone, Copy)]
pub(crate) enum ExpectedTarget<'a> {
    /// Nothing: the write is a create and must fail if any name exists.
    Absent,
    /// Exactly these bytes, as read when the write was prepared.
    Exactly(&'a str),
}

/// Refuse a write the filesystem would not have allowed in place.
fn ensure_writable(
    target: &structured_file::ConfinedTarget,
    path: &ResolvedWorkspacePath,
) -> Result<(), ToolError> {
    let subject = match target.writability()? {
        structured_file::Writability::Writable => return Ok(()),
        structured_file::Writability::ReadOnlyFile => "the file",
        structured_file::Writability::ReadOnlyDirectory => "its directory",
    };
    Err(ToolError::ReadOnlyTarget {
        path: path.display(),
        subject,
    })
}

/// Perform a structured write on a confined target.
///
/// A create opens the final name with `O_CREAT | O_EXCL`, so a file that
/// appeared after the call was prepared is refused rather than clobbered.
/// A replace reads the current content through one descriptor, compares it to
/// the exact prepared pre-image, and then publishes the new bytes by renaming
/// a sibling temporary file over the target. The target is therefore only
/// ever its complete old content or its complete new content, and a
/// concurrent edit is refused instead of overwritten.
fn write_confined(
    path: &ResolvedWorkspacePath,
    content: &str,
    expected: ExpectedTarget<'_>,
) -> Result<Option<String>, ToolError> {
    let target = path.confine()?;
    let durability = match expected {
        ExpectedTarget::Absent => target.create_new(content.as_bytes()).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                ToolError::FileAlreadyExists
            } else {
                ToolError::Io(error)
            }
        })?,
        ExpectedTarget::Exactly(expected) => {
            ensure_writable(&target, path)?;
            let current = target.open_read()?;
            let metadata = current.metadata()?;
            if !metadata.is_file() {
                return Err(ToolError::UnsupportedFileType {
                    path: path.display(),
                });
            }
            if read_to_string(current)? != expected {
                return Err(ToolError::StalePreparedWrite {
                    path: path.display(),
                });
            }
            target.replace(content.as_bytes(), &metadata)?
        }
    };
    Ok(match durability {
        structured_file::Durability::Synced => None,
        structured_file::Durability::DirectoryUnsynced(reason) => Some(format!(
            "`{}` was written, but its directory entry could not be made durable ({reason}); \
the change is present and may not survive an immediate power loss",
            path.display()
        )),
    })
}

fn read_to_string(mut file: fs::File) -> Result<String, ToolError> {
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    Ok(content)
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
    interrupted: bool,
) -> Result<String, ToolError> {
    let stdout = if sandboxed {
        match crate::sandbox::strip_sandbox_ready_marker(&stdout) {
            Ok(stdout) => stdout,
            // Timeout or cancellation can kill the launcher before it is
            // ready. Raw stdout/stderr must remain hidden because neither
            // came from the agent command.
            Err(_) if interrupted => return Ok(String::new()),
            Err(reason) => return Err(sandbox_unavailable(reason)),
        }
    } else {
        &stdout
    };
    Ok(format!("{stdout}{stderr}"))
}

/// Attach the likely host cause to a sandbox failure, so a tool result names
/// what the user has to change instead of only that something failed.
fn sandbox_unavailable(reason: SandboxUnavailableReason) -> ToolError {
    ToolError::SandboxUnavailable {
        reason,
        cause: SandboxFailureCause::for_reason(reason),
    }
}

/// A selected profile must never fall back to host execution or disclose raw
/// launcher details. An I/O failure while launching or supervising it is
/// therefore reported as the same concise enforcement failure as a missing
/// readiness marker.
fn normalize_sandbox_subprocess_error(sandboxed: bool, error: ToolError) -> ToolError {
    if sandboxed && matches!(&error, ToolError::Io(_)) {
        sandbox_unavailable(SandboxUnavailableReason::CannotEnforce)
    } else {
        error
    }
}

const DISPLAY_PATH_MAX_CHARS: usize = 256;

/// Sanitize a model-supplied path for inclusion in an error message or a
/// permission-prompt reason: replace control characters and cap the length so
/// hostile or degenerate input cannot inject terminal escapes, split log
/// lines, or bloat events.
pub(crate) fn display_path(path: &str) -> String {
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

/// Remove inherited variables that would configure the child behind the
/// caller's back.
///
/// The Git redirect family is deliberately not here: stripping it from every
/// agent-controlled shell would change what `run_shell` sees on the host
/// backend, and that is ADR 0021 row C's decision to make. Euler's own git
/// invocations strip it themselves.
fn scrub_agent_subprocess_env(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        if crate::redaction::is_secret_env_name(&name) || is_parent_control_env_name(&name) {
            command.env_remove(name);
        }
    }
}

/// Ambient controls for the owning Euler process must not silently configure
/// programs launched by the agent. A command can still set any of these
/// explicitly in its own shell text when that is part of the requested work.
/// `EULER_AUTH_FILE` lives here rather than in the secret-name classifier:
/// its value is a path (the credentials live in the file), so the subprocess
/// must not see it, but the path itself is not a redaction known-value.
fn is_parent_control_env_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    matches!(
        name,
        "EULER_HOME"
            | "EULER_PROVIDER"
            | "EULER_MODEL"
            | "EULER_NO_TTY"
            | "EULER_TUI_METRICS"
            | "EULER_AUTH_FILE"
    )
}

/// The exit code, the header line, and whether the shell result is a
/// cancellation, for one process outcome.
fn shell_termination(termination: ProcessTermination, timeout_ms: u64) -> (i32, String, bool) {
    match termination {
        ProcessTermination::Exited(status) => (status, format!("exit {status}"), false),
        ProcessTermination::TimedOut => (
            -1,
            format!(
                "exit -1 (command timed out after {timeout_ms} ms and was killed; \
pass timeout_ms up to {MAX_SHELL_TIMEOUT_MS} for longer runs)"
            ),
            false,
        ),
        ProcessTermination::Cancelled => (
            -1,
            "exit -1 (command cancelled and process group killed)".to_owned(),
            true,
        ),
    }
}

/// The git directory for a workspace root.
///
/// `.git` is a directory in a plain checkout, but a *file* naming another
/// gitdir in a linked worktree or a submodule checkout — which is exactly
/// where submodule edits are most likely to be hidden, so resolving it is the
/// difference between the notice appearing and being silently absent.
fn git_directory(root: &Path) -> PathBuf {
    let git = root.join(".git");
    let Ok(pointer) = fs::read_to_string(&git) else {
        return git;
    };
    let Some(target) = pointer
        .lines()
        .find_map(|line| line.trim().strip_prefix("gitdir:"))
    else {
        return git;
    };
    let target = Path::new(target.trim());
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        root.join(target)
    }
}

/// The bound on Euler's own git probes.
///
/// A probe is a `git config` read, so this is not a budget for slow work: it
/// is the point past which the repository is hung rather than busy, and the
/// probe fails the tool closed. It is deliberately generous, because a probe
/// that expires under ordinary load would refuse the tool for no reason.
const GIT_PROBE_TIMEOUT_MS: u64 = 30_000;

/// Said instead of a generic probe failure when the bound above expires, so a
/// user who waited that long is told what to look at.
const GIT_PROBE_TIMEOUT_MESSAGE: &str =
    "reading this repository's git configuration timed out after 30 seconds, which usually means \
a slow or unresponsive filesystem rather than anything about the repository";

/// The incompleteness to report for a pair of captures. A missing capture is
/// itself an unreadable workspace: the command still ran, so the caller must
/// say the changes were not observed rather than that there were none.
fn incomplete_observation(
    before: Option<&WorkspaceSnapshot>,
    after: Option<&WorkspaceSnapshot>,
    bound: usize,
) -> Option<IncompleteObservation> {
    match (before, after) {
        (Some(before), Some(after)) => before.incomplete().or_else(|| after.incomplete()),
        _ => Some(IncompleteObservation {
            reason: crate::ObservationLimit::Unreadable,
            bound,
        }),
    }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessTermination {
    Exited(i32),
    TimedOut,
    Cancelled,
}

struct ShellOutcome {
    termination: ProcessTermination,
    stdout: String,
    stderr: String,
}

struct SupervisedProcess {
    handle: std::process::Child,
    pid: i32,
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
}

fn spawn_supervised_process(mut child: Command) -> Result<SupervisedProcess, ToolError> {
    use std::os::unix::process::CommandExt as _;
    use std::process::Stdio;

    child
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut handle = child.spawn()?;
    let pid = handle.id() as i32;
    let stdout = handle
        .stdout
        .take()
        .expect("stdout was configured as a pipe");
    let stderr = handle
        .stderr
        .take()
        .expect("stderr was configured as a pipe");
    if let Err(error) = set_nonblocking(&stdout).and_then(|()| set_nonblocking(&stderr)) {
        kill_process_group(pid);
        let _ = handle.wait();
        return Err(error.into());
    }
    Ok(SupervisedProcess {
        handle,
        pid,
        stdout,
        stderr,
    })
}

/// Runs the child in its own process group, polling for completion,
/// cancellation, and an optional deadline. Cancellation and timeout kill and
/// reap the still-owned process group before reaping its leader. That covers
/// the leader and ordinary descendants that remain in the group; a descendant
/// that deliberately moves itself into another process group is outside this
/// ownership guarantee.
fn run_process(
    child: Command,
    timeout_ms: Option<u64>,
    cancellation: &CancellationToken,
) -> Result<ShellOutcome, ToolError> {
    use std::time::{Duration, Instant};

    if cancellation.is_cancelled() {
        return Err(ToolError::Cancelled);
    }
    let SupervisedProcess {
        mut handle,
        pid,
        mut stdout,
        mut stderr,
    } = spawn_supervised_process(child)?;
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();

    let deadline = timeout_ms.map(|timeout_ms| Instant::now() + Duration::from_millis(timeout_ms));
    let termination = loop {
        if let Err(error) = drain_process_pipe(
            &mut stdout,
            &mut stdout_open,
            &mut stdout_bytes,
            PROCESS_PIPE_DRAIN_BUDGET,
        )
        .and_then(|()| {
            drain_process_pipe(
                &mut stderr,
                &mut stderr_open,
                &mut stderr_bytes,
                PROCESS_PIPE_DRAIN_BUDGET,
            )
        }) {
            kill_process_group(pid);
            let _ = handle.wait();
            return Err(error.into());
        }

        if cancellation.is_cancelled() {
            kill_process_group(pid);
            let _ = handle.wait();
            drain_immediately_available(
                &mut stdout,
                &mut stdout_open,
                &mut stdout_bytes,
                &mut stderr,
                &mut stderr_open,
                &mut stderr_bytes,
            );
            break ProcessTermination::Cancelled;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            kill_process_group(pid);
            let _ = handle.wait();
            drain_immediately_available(
                &mut stdout,
                &mut stdout_open,
                &mut stdout_bytes,
                &mut stderr,
                &mut stderr_open,
                &mut stderr_bytes,
            );
            break ProcessTermination::TimedOut;
        }

        // Do not reap (or even `try_wait`) while a descendant can still own a
        // pipe. Keeping the leader unreaped pins its pid/process-group id, so
        // a later cancellation cannot signal an unrelated reused group.
        if !stdout_open && !stderr_open {
            match handle.try_wait() {
                Ok(Some(status)) => {
                    break ProcessTermination::Exited(status.code().unwrap_or(-1));
                }
                Ok(None) => {}
                Err(error) => {
                    kill_process_group(pid);
                    let _ = handle.wait();
                    return Err(error.into());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    Ok(ShellOutcome {
        termination,
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
    })
}

const PROCESS_PIPE_DRAIN_BUDGET: usize = 256 * 1024;

fn set_nonblocking(stream: &impl std::os::fd::AsRawFd) -> std::io::Result<()> {
    let fd = stream.as_raw_fd();
    // SAFETY: `fd` is borrowed from a live child pipe for the duration of
    // both calls. `F_GETFL` returns the current descriptor status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: preserves every existing status flag and adds O_NONBLOCK to the
    // same live descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn drain_process_pipe(
    stream: &mut impl std::io::Read,
    open: &mut bool,
    output: &mut Vec<u8>,
    budget: usize,
) -> std::io::Result<()> {
    if !*open {
        return Ok(());
    }
    let mut remaining = budget;
    let mut chunk = [0_u8; 8192];
    while remaining > 0 {
        let read_len = remaining.min(chunk.len());
        match stream.read(&mut chunk[..read_len]) {
            Ok(0) => {
                *open = false;
                return Ok(());
            }
            Ok(read) => {
                output.extend_from_slice(&chunk[..read]);
                remaining -= read;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn drain_immediately_available(
    stdout: &mut impl std::io::Read,
    stdout_open: &mut bool,
    stdout_bytes: &mut Vec<u8>,
    stderr: &mut impl std::io::Read,
    stderr_open: &mut bool,
    stderr_bytes: &mut Vec<u8>,
) {
    // Once the owned group is dead, retain only bytes already waiting in the
    // two kernel pipes. Each drain is capped so a deliberately escaped writer
    // cannot keep cancellation stuck by continuously refilling a pipe.
    let _ = drain_process_pipe(stdout, stdout_open, stdout_bytes, PROCESS_PIPE_DRAIN_BUDGET);
    let _ = drain_process_pipe(stderr, stderr_open, stderr_bytes, PROCESS_PIPE_DRAIN_BUDGET);
}

fn kill_process_group(pid: i32) {
    // SAFETY: plain libc kill on the process group created for this child.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
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
    if text.len() <= max_bytes
        && text
            .split_inclusive('\n')
            .take(max_lines.saturating_add(1))
            .count()
            <= max_lines
    {
        return text.to_owned();
    }

    // Preserve both the command's beginning and its terminal/error region.
    // Byte and line budgets apply to retained content; the honest omission
    // marker is additional projection metadata. Restrict line discovery to
    // the byte windows first so preview work never scans the omitted middle.
    let head_line_count = max_lines.div_ceil(2);
    let tail_line_count = max_lines / 2;
    let head_byte_budget = max_bytes.div_ceil(2);
    let tail_byte_budget = max_bytes / 2;
    let head_byte_end = floor_char_boundary(text, text.len().min(head_byte_budget));
    let head_end = text[..head_byte_end]
        .split_inclusive('\n')
        .take(head_line_count)
        .map(|line| line.len())
        .sum::<usize>();
    let tail_byte_start = ceil_char_boundary(text, text.len().saturating_sub(tail_byte_budget));
    let tail_window = &text[tail_byte_start..];
    let tail_line_bytes = tail_window
        .split_inclusive('\n')
        .rev()
        .take(tail_line_count)
        .map(|line| line.len())
        .sum::<usize>();
    let tail_line_start = text.len().saturating_sub(tail_line_bytes);
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

pub(crate) fn hash_bytes(bytes: &[u8]) -> String {
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

fn skill_read_definition() -> ToolDefinition {
    ToolDefinition {
        name: "skill_read".to_owned(),
        description: "Read one accepted skill body from the current session's immutable skill snapshot. Pass the exact name shown in the skill catalog. This reads no live files and grants no permissions.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Exact accepted skill name from the catalog."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        }),
    }
}

fn tool_result_get(
    events: &[EventEnvelope],
    input: &Value,
    access: ToolResultAccess<'_>,
) -> Result<ToolExecution, ToolError> {
    let offset_bytes = optional_usize(input, "offset_bytes")?.unwrap_or(0);
    let max_bytes = optional_positive_usize(input, "max_bytes")?.unwrap_or(64 * 1024);
    let event = find_tool_result_event(events, input)?;
    let project_context_snapshot_digest = tool_result_project_context_snapshot_digest(event)?;
    if let ToolResultAccess::Child {
        allowed_project_context_snapshot_digest,
    } = access
    {
        if project_context_snapshot_digest.is_some()
            && project_context_snapshot_digest != allowed_project_context_snapshot_digest
        {
            return Err(ToolError::InvalidField("event_id"));
        }
    }
    let name = event
        .payload
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let ok = tool_result_succeeded(&event.payload);
    let content = tool_result_content(event);
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
        project_context_snapshot_digest: project_context_snapshot_digest.map(str::to_owned),
        exit_code: None,
        patch: None,
        file_changes: Vec::new(),
        observation: None,
    })
}

fn tool_result_project_context_snapshot_digest(
    event: &EventEnvelope,
) -> Result<Option<&str>, ToolError> {
    match event.payload.get("project_context_snapshot_digest") {
        None => Ok(None),
        Some(Value::String(digest))
            if digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) =>
        {
            Ok(Some(digest))
        }
        Some(_) => Err(ToolError::InvalidField("event_id")),
    }
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

fn tool_result_content(event: &EventEnvelope) -> &str {
    event
        .payload
        .get("output")
        .and_then(Value::as_str)
        .filter(|output| !output.is_empty())
        .or_else(|| event.payload.get("error").and_then(Value::as_str))
        .unwrap_or("")
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
