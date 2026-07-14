use euler_sdk::Capability;
use serde_json::{json, Map, Value};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const DEFAULT_MAX_FILE_BYTES: usize = 100_000;
pub const DEFAULT_MAX_TOTAL_BYTES: usize = 256 * 1024;
pub const MAX_TOTAL_BYTES: usize = 256 * 1024;
pub const CONTEXT_OVERHEAD_BYTES: usize = 8_000;
/// Largest review prompt that still fits [`CONTEXT_OVERHEAD_BYTES`] once the
/// `\nReview prompt:\n` framing is added. Callers bound `focus` by this so a
/// prompt is accepted or rejected exactly once, at one limit.
pub const MAX_PROMPT_BYTES: usize = CONTEXT_OVERHEAD_BYTES - 64;
const MAX_COMMAND_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_FILES: usize = 64;
const MAX_SKIPPED: usize = 64;
/// Wall-clock bound for one `git`/`gh` assembly call. Assembly runs inline on
/// the session thread, so an unbounded child would hang the whole session.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ERROR_DETAIL_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewMode {
    Plan,
    Code,
    Diff,
    PullRequest,
}

impl ReviewMode {
    pub(super) fn parse(value: Option<&Value>) -> Result<Self, String> {
        let Some(value) = value else {
            return Ok(Self::Plan);
        };
        match value.as_str() {
            Some("plan") => Ok(Self::Plan),
            Some("review-code") => Ok(Self::Code),
            Some("review-diff") => Ok(Self::Diff),
            Some("review-pr") => Ok(Self::PullRequest),
            Some(_) => Err("mode must be plan, review-code, review-diff, or review-pr".to_owned()),
            None => Err("mode must be a string".to_owned()),
        }
    }

    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Code => "review-code",
            Self::Diff => "review-diff",
            Self::PullRequest => "review-pr",
        }
    }

    /// Authority this mode actually exercises while assembling context.
    ///
    /// `code_swarm_review` itself is gated on `AgentSpawn`, but assembly reads
    /// the workspace (`review-code`), runs `git` (`review-diff`), and runs
    /// `gh` against the network (`review-pr`). Each mode declares what it uses
    /// so the ordinary permission machinery can approve or deny it (ADR 0010:
    /// the contract update lands with the authority, not after it).
    pub(super) const fn required_capabilities(self) -> &'static [Capability] {
        match self {
            Self::Plan => &[],
            Self::Code => &[Capability::FsRead],
            Self::Diff => &[Capability::ShellExec],
            Self::PullRequest => &[Capability::ShellExec, Capability::Network],
        }
    }
}

/// Parse a `code-swarm review` input object into a [`ContextRequest`].
///
/// Shared by the `code_swarm_review` tool and the extension-run bridge so a
/// malformed selector is rejected identically on both seams.
pub(super) fn request_from_object(object: &Map<String, Value>) -> Result<ContextRequest, String> {
    let max_total_bytes = object_usize(object, "max_total_bytes", DEFAULT_MAX_TOTAL_BYTES)?;
    Ok(ContextRequest {
        mode: ReviewMode::parse(object.get("mode"))?,
        prompt: object_string(object, "prompt")?.unwrap_or_default(),
        context: object_string(object, "context")?,
        files: object_string_list(object, "files")?,
        base: object_string(object, "base")?,
        staged: object_bool(object, "staged")?,
        pr: object_string(object, "pr")?,
        current: object_bool(object, "current")?,
        include_full_files: object_bool(object, "include_full_files")?,
        include_comments: object_bool(object, "include_comments")?,
        max_file_bytes: object_usize(object, "max_file_bytes", DEFAULT_MAX_FILE_BYTES)?,
        max_total_bytes,
        max_diff_bytes: object_usize(
            object,
            "max_diff_bytes",
            max_total_bytes.saturating_sub(CONTEXT_OVERHEAD_BYTES),
        )?,
    })
}

#[derive(Debug)]
pub struct ContextRequest {
    pub mode: ReviewMode,
    pub prompt: String,
    pub context: Option<String>,
    pub files: Vec<String>,
    pub base: Option<String>,
    pub staged: bool,
    pub pr: Option<String>,
    pub current: bool,
    pub include_full_files: bool,
    pub include_comments: bool,
    pub max_file_bytes: usize,
    pub max_total_bytes: usize,
    pub max_diff_bytes: usize,
}

#[derive(Debug)]
pub struct AssembledContext {
    pub body: String,
    pub manifest: Value,
}

impl AssembledContext {
    pub fn replace_body(&mut self, body: String) -> Result<(), String> {
        if body.len() > MAX_TOTAL_BYTES {
            return Err(format!(
                "redacted review context exceeds the {MAX_TOTAL_BYTES}-byte limit"
            ));
        }
        self.body = body;
        if let Some(object) = self.manifest.as_object_mut() {
            object.insert("context_bytes".to_owned(), self.body.len().into());
            object.insert("redacted".to_owned(), true.into());
        }
        Ok(())
    }
}

pub fn assemble(root: &Path, request: &ContextRequest) -> Result<AssembledContext, String> {
    validate_budget(request)?;
    let prompt_bytes = request.prompt.trim().len();
    if prompt_bytes == 0 {
        return Err("prompt must contain a review question or focus".to_owned());
    }
    if prompt_bytes > MAX_PROMPT_BYTES {
        return Err(format!(
            "prompt is {prompt_bytes} bytes; the limit is {MAX_PROMPT_BYTES}"
        ));
    }
    let body_budget = request.max_total_bytes - CONTEXT_OVERHEAD_BYTES;
    let mut assembler = Assembler::new(body_budget, request.max_file_bytes);
    match request.mode {
        ReviewMode::Plan => assemble_plan(request, &mut assembler),
        ReviewMode::Code => assemble_code(root, request, &mut assembler)?,
        ReviewMode::Diff => assemble_diff(root, request, &mut assembler)?,
        ReviewMode::PullRequest => assemble_pr(root, request, &mut assembler)?,
    }
    if let Some(context) = request.context.as_deref() {
        assembler.push_text("Additional caller context", context, body_budget);
    }
    assembler.limit = request.max_total_bytes;
    assembler.push_required_prompt(&request.prompt)?;
    let manifest = assembler.manifest(request.mode);
    Ok(AssembledContext {
        body: assembler.body,
        manifest,
    })
}

fn validate_budget(request: &ContextRequest) -> Result<(), String> {
    if request.max_file_bytes == 0 {
        return Err("max_file_bytes must be positive".to_owned());
    }
    if request.max_total_bytes <= CONTEXT_OVERHEAD_BYTES
        || request.max_total_bytes > MAX_TOTAL_BYTES
    {
        return Err(format!(
            "max_total_bytes must be greater than {CONTEXT_OVERHEAD_BYTES} and at most {MAX_TOTAL_BYTES}"
        ));
    }
    if request.max_diff_bytes == 0 {
        return Err("max_diff_bytes must be positive".to_owned());
    }
    Ok(())
}

fn assemble_plan(request: &ContextRequest, assembler: &mut Assembler) {
    let _ = (request, assembler);
}

fn assemble_code(
    root: &Path,
    request: &ContextRequest,
    assembler: &mut Assembler,
) -> Result<(), String> {
    if request.files.is_empty() {
        return Err("review-code requires at least one file".to_owned());
    }
    if request.files.len() > MAX_FILES {
        return Err(format!("review-code accepts at most {MAX_FILES} files"));
    }
    for raw in &request.files {
        let path = resolve_repo_file(root, raw)?;
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                assembler.skip(format!("{raw} (missing)"));
                continue;
            }
        };
        if !metadata.is_file() {
            assembler.skip(format!("{raw} (not a regular file)"));
            continue;
        }
        let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        if size > request.max_file_bytes {
            assembler.skip(format!(
                "{raw} ({size} bytes > {} per-file limit)",
                request.max_file_bytes
            ));
            continue;
        }
        let data = fs::read(&path).map_err(|error| format!("could not read {raw}: {error}"))?;
        if data.iter().take(512).any(|byte| *byte == 0) {
            assembler.skip(format!("{raw} (binary)"));
            continue;
        }
        let content = String::from_utf8_lossy(&data);
        assembler.push_text(&format!("File: {raw}"), &content, request.max_file_bytes);
    }
    Ok(())
}

fn assemble_diff(
    root: &Path,
    request: &ContextRequest,
    assembler: &mut Assembler,
) -> Result<(), String> {
    if request.staged && request.base.is_some() {
        return Err("review-diff accepts either staged=true or base, not both".to_owned());
    }
    validate_ref(request.base.as_deref())?;
    let (label, args) = if request.staged {
        (
            "Staged diff".to_owned(),
            vec![
                "diff".to_owned(),
                "--cached".to_owned(),
                "--patch".to_owned(),
            ],
        )
    } else if let Some(base) = request.base.as_deref() {
        (
            format!("Diff against {base}"),
            vec![
                "diff".to_owned(),
                "--patch".to_owned(),
                format!("{base}...HEAD"),
            ],
        )
    } else {
        (
            "Working tree diff".to_owned(),
            vec!["diff".to_owned(), "--patch".to_owned()],
        )
    };
    let diff = run_git(root, &args)?;
    assembler.push_text(&label, &diff, request.max_diff_bytes);
    Ok(())
}

fn assemble_pr(
    root: &Path,
    request: &ContextRequest,
    assembler: &mut Assembler,
) -> Result<(), String> {
    if request.current == request.pr.is_some() {
        return Err("review-pr requires exactly one of pr or current=true".to_owned());
    }
    let mut view_args = vec!["pr".to_owned(), "view".to_owned()];
    if let Some(pr) = request.pr.as_deref() {
        validate_pr_target(pr)?;
        view_args.push(pr.to_owned());
    }
    view_args.extend([
        "--json".to_owned(),
        "number,title,body,author,baseRefName,headRefName,url,files,commits,reviews,comments"
            .to_owned(),
    ]);
    let metadata_text = run_gh(root, &view_args)?;
    let metadata: Value = serde_json::from_str(&metadata_text)
        .map_err(|error| format!("gh pr view returned invalid JSON: {error}"))?;
    let summary = json!({
        "number": metadata.get("number"),
        "title": metadata.get("title"),
        "url": metadata.get("url"),
        "author": metadata.get("author").and_then(|value| value.get("login")),
        "base": metadata.get("baseRefName"),
        "head": metadata.get("headRefName"),
        "files": metadata.get("files"),
        "commits": metadata.get("commits"),
        "body": metadata.get("body"),
    });
    let pretty = serde_json::to_string_pretty(&summary)
        .map_err(|error| format!("could not encode PR metadata: {error}"))?;
    assembler.push_text("Pull request metadata", &pretty, assembler.remaining());

    let mut diff_args = vec!["pr".to_owned(), "diff".to_owned()];
    if let Some(pr) = request.pr.as_deref() {
        diff_args.push(pr.to_owned());
    }
    diff_args.push("--patch".to_owned());
    let diff = run_gh(root, &diff_args)?;
    assembler.push_text("Pull request patch", &diff, request.max_diff_bytes);

    if request.include_comments {
        let comments = json!({
            "reviews": metadata.get("reviews"),
            "comments": metadata.get("comments"),
        });
        let pretty = serde_json::to_string_pretty(&comments)
            .map_err(|error| format!("could not encode PR comments: {error}"))?;
        assembler.push_text(
            "Existing reviews and comments",
            &pretty,
            assembler.remaining() / 3,
        );
    }
    if request.include_full_files {
        push_pr_local_files(root, request, assembler, &metadata)?;
    }
    Ok(())
}

/// Append the current local contents of each PR-touched file.
///
/// These bytes come from the local checkout, not the PR head, so the manifest
/// says so: labelling working-tree content as PR content would make every
/// finding about it unfalsifiable.
fn push_pr_local_files(
    root: &Path,
    request: &ContextRequest,
    assembler: &mut Assembler,
    metadata: &Value,
) -> Result<(), String> {
    assembler.skip(
        "full-file context was read from the local checkout, not the PR head; it may differ \
         from the patch above"
            .to_owned(),
    );
    let files = metadata
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| "gh pr view omitted the files array".to_owned())?;
    for file in files {
        let Some(path) = file.get("path").and_then(Value::as_str) else {
            continue;
        };
        let resolved = match resolve_repo_file(root, path) {
            Ok(resolved) => resolved,
            Err(error) => {
                assembler.skip(format!("{path} ({error})"));
                continue;
            }
        };
        let Ok(data) = fs::read(&resolved) else {
            assembler.skip(format!("{path} (missing from local checkout)"));
            continue;
        };
        if data.len() > request.max_file_bytes {
            assembler.skip(format!(
                "{path} ({} bytes > {} per-file limit)",
                data.len(),
                request.max_file_bytes
            ));
            continue;
        }
        if data.iter().take(512).any(|byte| *byte == 0) {
            assembler.skip(format!("{path} (binary)"));
            continue;
        }
        assembler.push_text(
            &format!("Current local file: {path}"),
            &String::from_utf8_lossy(&data),
            request.max_file_bytes,
        );
    }
    Ok(())
}

struct Assembler {
    body: String,
    limit: usize,
    max_file_bytes: usize,
    included: Vec<Value>,
    skipped: Vec<String>,
    truncated: bool,
}

impl Assembler {
    fn new(limit: usize, max_file_bytes: usize) -> Self {
        Self {
            body: String::new(),
            limit,
            max_file_bytes,
            included: Vec::new(),
            skipped: Vec::new(),
            truncated: false,
        }
    }

    fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.body.len())
    }

    fn skip(&mut self, reason: String) {
        if self.skipped.len() < MAX_SKIPPED {
            self.skipped.push(reason);
        } else if self.skipped.len() == MAX_SKIPPED {
            self.skipped
                .push("additional skipped entries omitted".to_owned());
            self.truncated = true;
        }
    }

    fn push_text(&mut self, label: &str, text: &str, item_limit: usize) {
        let header = format!("\n--- {label} ---\n");
        let available = self.remaining().saturating_sub(header.len() + 1);
        if available == 0 {
            self.skip(format!("{label} (total context limit)"));
            return;
        }
        let allowed = available.min(item_limit);
        let (bounded, was_truncated) = truncate_utf8(text, allowed);
        self.body.push_str(&header);
        self.body.push_str(bounded);
        self.body.push('\n');
        self.included.push(json!({
            "label": label,
            "bytes": bounded.len(),
            "source_bytes": text.len(),
            "truncated": was_truncated,
        }));
        if was_truncated {
            self.truncated = true;
            self.skip(format!(
                "{label} truncated from {} bytes to {} bytes",
                text.len(),
                bounded.len()
            ));
        }
    }

    fn push_required_prompt(&mut self, prompt: &str) -> Result<(), String> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err("prompt must contain a review question or focus".to_owned());
        }
        let tail = format!("\nReview prompt:\n{prompt}\n");
        if tail.len() > CONTEXT_OVERHEAD_BYTES || self.body.len() + tail.len() > self.limit {
            return Err(format!(
                "review prompt does not fit the reserved {CONTEXT_OVERHEAD_BYTES}-byte overhead"
            ));
        }
        self.body.push_str(&tail);
        Ok(())
    }

    fn manifest(&self, mode: ReviewMode) -> Value {
        json!({
            "mode": mode.as_str(),
            "context_bytes": self.body.len(),
            "max_total_bytes": self.limit,
            "max_file_bytes": self.max_file_bytes,
            "included": self.included,
            "skipped": self.skipped,
            "truncated": self.truncated,
        })
    }
}

fn truncate_utf8(text: &str, limit: usize) -> (&str, bool) {
    if text.len() <= limit {
        return (text, false);
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

fn resolve_repo_file(root: &Path, raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() || Path::new(raw).is_absolute() {
        return Err(format!("file path `{raw}` must be repository-relative"));
    }
    if Path::new(raw)
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "file path `{raw}` must not contain parent traversal"
        ));
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("could not resolve repository root: {error}"))?;
    let candidate = root.join(raw);
    let canonical = candidate
        .canonicalize()
        .map_err(|error| format!("could not resolve `{raw}`: {error}"))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(format!("file path `{raw}` resolves outside the repository"));
    }
    Ok(canonical)
}

fn validate_ref(value: Option<&str>) -> Result<(), String> {
    if let Some(value) = value {
        if value.is_empty()
            || value.starts_with('-')
            || value.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err("base must be a non-option Git revision".to_owned());
        }
    }
    Ok(())
}

fn validate_pr_target(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.starts_with('-')
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err("pr must be a non-option PR number, URL, or branch".to_owned());
    }
    Ok(())
}

fn run_git(root: &Path, args: &[String]) -> Result<String, String> {
    run_command(root, "git", args)
}

fn run_gh(root: &Path, args: &[String]) -> Result<String, String> {
    run_command(root, "gh", args)
}

fn run_command(root: &Path, program: &str, args: &[String]) -> Result<String, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in ["OPENROUTER_API_KEY", "ANTHROPIC_API_KEY", "OPENAI_API_KEY"] {
        command.env_remove(key);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not run {program}: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "missing stdout pipe".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "missing stderr pipe".to_owned())?;
    let (overflow_tx, overflow_rx) = std::sync::mpsc::channel();
    let out_tx = overflow_tx.clone();
    let out_thread = thread::spawn(move || {
        let result = read_bounded(stdout, MAX_COMMAND_OUTPUT_BYTES);
        if result.is_err() {
            let _ = out_tx.send(());
        }
        result
    });
    let err_thread = thread::spawn(move || {
        let result = read_bounded(stderr, 64 * 1024);
        if result.is_err() {
            let _ = overflow_tx.send(());
        }
        result
    });
    let (status, timed_out) = wait_bounded(&mut child, program, &overflow_rx)?;
    let stdout = out_thread
        .join()
        .map_err(|_| format!("{program} stdout reader panicked"))??;
    let stderr = err_thread
        .join()
        .map_err(|_| format!("{program} stderr reader panicked"))??;
    if timed_out {
        return Err(format!(
            "{program} {} timed out after {}s",
            args.join(" "),
            COMMAND_TIMEOUT.as_secs()
        ));
    }
    if !status.success() {
        // Callers redact this before it reaches a model or an artifact.
        let detail = String::from_utf8_lossy(&stderr);
        let detail = detail.trim();
        let (detail, _) = truncate_utf8(detail, MAX_ERROR_DETAIL_BYTES);
        return Err(if detail.is_empty() {
            format!("{program} {} failed with status {status}", args.join(" "))
        } else {
            format!(
                "{program} {} failed with status {status}: {detail}",
                args.join(" ")
            )
        });
    }
    String::from_utf8(stdout).map_err(|_| format!("{program} output was not UTF-8"))
}

/// Wait for `child`, killing it if it outruns [`COMMAND_TIMEOUT`] or if a
/// reader reports its output overflowed. Assembly runs inline on the session
/// thread, so an unbounded wait here would hang the whole session.
fn wait_bounded(
    child: &mut std::process::Child,
    program: &str,
    overflow_rx: &std::sync::mpsc::Receiver<()>,
) -> Result<(std::process::ExitStatus, bool), String> {
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        if overflow_rx.try_recv().is_ok() {
            let _ = child.kill();
            let status = child
                .wait()
                .map_err(|error| format!("could not stop {program}: {error}"))?;
            return Ok((status, false));
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("could not poll {program}: {error}"))?
        {
            return Ok((status, false));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let status = child
                .wait()
                .map_err(|error| format!("could not stop {program}: {error}"))?;
            return Ok((status, true));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn read_bounded(mut reader: impl Read, limit: usize) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let read = reader
            .read(&mut chunk)
            .map_err(|error| format!("could not read command output: {error}"))?;
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > limit {
            return Err(format!(
                "command output exceeds the {limit}-byte assembly limit"
            ));
        }
        output
            .write_all(&chunk[..read])
            .map_err(|error| format!("could not buffer command output: {error}"))?;
    }
}

pub(super) fn object_usize(
    object: &Map<String, Value>,
    field: &str,
    default: usize,
) -> Result<usize, String> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| format!("{field} must be a positive integer")),
    }
}

pub(super) fn object_bool(object: &Map<String, Value>, field: &str) -> Result<bool, String> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| format!("{field} must be boolean")),
    }
}

pub(super) fn object_string(
    object: &Map<String, Value>,
    field: &str,
) -> Result<Option<String>, String> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(|value| Some(value.to_owned()))
            .ok_or_else(|| format!("{field} must be a string")),
    }
}

pub(super) fn object_string_list(
    object: &Map<String, Value>,
    field: &str,
) -> Result<Vec<String>, String> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| format!("{field} must contain only strings"))
            })
            .collect(),
        Some(_) => Err(format!("{field} must be an array of strings")),
    }
}

#[cfg(test)]
#[path = "swarm_context_test.rs"]
mod tests;
