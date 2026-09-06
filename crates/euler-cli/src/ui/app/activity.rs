//! Deterministic high-level activity projection for the pinned TUI HUD.
//!
//! The reducer consumes only canonical event shape and provenance timestamps.
//! Rendering injects the live clock separately, so replay and tests never
//! depend on a hidden wall-clock read. Model content is deliberately absent
//! from this module: a delta's kind is enough to establish response activity.

use super::turn_recap::{shell_exit_code, TurnRecapAccumulator};
use chrono::{DateTime, Utc};
use euler_core::ProviderRuntimeEvent;
use euler_event::{tool_result_succeeded, EventEnvelope, EventKind};
use euler_provider::{ProviderAttemptEvent, ProviderAttemptOutcome, ProviderTimeoutStage};
use std::collections::BTreeMap;
use std::time::Duration;

pub(super) const ACTIVITY_STALL_THRESHOLD: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) enum ActivityPhase {
    #[default]
    Idle,
    Starting,
    PreparingContext,
    WaitingForModel,
    WaitingForResponseHeaders,
    WaitingForFirstByte,
    WaitingForSemanticOutput,
    RetryingModel {
        retry_ordinal: u64,
        backoff_ms: u64,
    },
    ProviderTimedOut(ProviderTimeoutStage),
    ProviderCancelled,
    ReceivingResponse,
    Inspecting(usize),
    Editing(usize),
    RunningChecks {
        count: usize,
        kind: CheckKind,
    },
    Publishing(usize),
    RunningTools(usize),
    WaitingForApproval,
    PreparingNextStep,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl ActivityPhase {
    pub(super) fn label(&self) -> String {
        match self {
            Self::Idle => "Idle".to_owned(),
            Self::Starting => "Starting run".to_owned(),
            Self::PreparingContext => "Preparing model context".to_owned(),
            Self::WaitingForModel => "Waiting for model".to_owned(),
            Self::WaitingForResponseHeaders => "Waiting for response headers".to_owned(),
            Self::WaitingForFirstByte => "Waiting for first response byte".to_owned(),
            Self::WaitingForSemanticOutput => "Waiting for meaningful model output".to_owned(),
            Self::RetryingModel {
                retry_ordinal,
                backoff_ms,
            } => retry_label(*retry_ordinal, *backoff_ms),
            Self::ProviderTimedOut(stage) => provider_timeout_label(*stage).to_owned(),
            Self::ProviderCancelled => "Model request cancelled".to_owned(),
            Self::ReceivingResponse => "Receiving model response".to_owned(),
            Self::Inspecting(count) => {
                counted("Inspecting workspace", "Inspecting", *count, "files")
            }
            Self::Editing(count) => counted("Editing files", "Editing", *count, "files"),
            Self::RunningChecks { count, kind } => check_label(*count, *kind),
            Self::Publishing(count) => {
                counted("Publishing changes", "Publishing", *count, "operations")
            }
            Self::RunningTools(count) => counted("Running tool", "Running", *count, "tools"),
            Self::WaitingForApproval => "Waiting for approval".to_owned(),
            Self::PreparingNextStep => "Preparing next step".to_owned(),
            Self::Completed => "Completed".to_owned(),
            Self::Failed => "Failed".to_owned(),
            Self::Cancelled => "Cancelled".to_owned(),
            Self::Interrupted => "Interrupted".to_owned(),
        }
    }

    fn can_stall(&self) -> bool {
        !matches!(
            self,
            Self::Idle
                | Self::WaitingForApproval
                | Self::Completed
                | Self::Failed
                | Self::Cancelled
                | Self::Interrupted
                | Self::ProviderTimedOut(_)
                | Self::ProviderCancelled
        )
    }
}

fn retry_label(retry_ordinal: u64, backoff_ms: u64) -> String {
    if backoff_ms == 0 {
        return format!("Retrying model request (retry {retry_ordinal})");
    }
    let delay = if backoff_ms.is_multiple_of(1_000) {
        format!("{}s", backoff_ms / 1_000)
    } else {
        format!("{backoff_ms}ms")
    };
    format!("Retrying model request (retry {retry_ordinal} in {delay})")
}

fn provider_timeout_label(stage: ProviderTimeoutStage) -> &'static str {
    match stage {
        ProviderTimeoutStage::ResponseHeaders => "Model request timed out waiting for headers",
        ProviderTimeoutStage::FirstByte => "Model request timed out waiting for first byte",
        ProviderTimeoutStage::SemanticIdle => "Model response timed out without meaningful output",
    }
}

fn counted(singular: &str, plural: &str, count: usize, noun: &str) -> String {
    if count <= 1 {
        singular.to_owned()
    } else {
        format!("{plural} {count} {noun}")
    }
}

fn check_label(count: usize, kind: CheckKind) -> String {
    if count > 1 {
        return format!("Running {count} checks");
    }
    match kind {
        CheckKind::Tests => "Running tests".to_owned(),
        CheckKind::CargoCheck => "Running cargo check".to_owned(),
        CheckKind::Checks => "Running checks".to_owned(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CheckKind {
    Tests,
    CargoCheck,
    Checks,
}

impl CheckKind {
    fn milestone(self) -> &'static str {
        match self {
            Self::Tests => "tests",
            Self::CargoCheck => "cargo check",
            Self::Checks => "checks",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolKind {
    Inspection,
    Edit,
    Check(CheckKind),
    Publish,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActiveTool {
    kind: ToolKind,
    name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ToolBatch {
    kind: Option<ToolKind>,
    count: usize,
}

impl ToolBatch {
    fn add(&mut self, kind: ToolKind) {
        self.kind = match (self.kind, kind) {
            (None, incoming) if self.count == 0 => Some(incoming),
            (Some(existing), incoming) if existing == incoming => Some(existing),
            (Some(ToolKind::Check(_)), ToolKind::Check(_)) => {
                Some(ToolKind::Check(CheckKind::Checks))
            }
            _ => None,
        };
        self.count += 1;
    }

    fn phase(&self) -> ActivityPhase {
        match self.kind {
            Some(ToolKind::Inspection) => ActivityPhase::Inspecting(self.count),
            Some(ToolKind::Edit) => ActivityPhase::Editing(self.count),
            Some(ToolKind::Check(kind)) => ActivityPhase::RunningChecks {
                count: self.count,
                kind,
            },
            Some(ToolKind::Publish) => ActivityPhase::Publishing(self.count),
            Some(ToolKind::Other) | None => ActivityPhase::RunningTools(self.count),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RunActivityProjection {
    phase: ActivityPhase,
    phase_started_at: Option<DateTime<Utc>>,
    last_event_at: Option<DateTime<Utc>>,
    last_progress_at: Option<DateTime<Utc>>,
    active_tools: BTreeMap<String, ActiveTool>,
    tool_batch: Option<ToolBatch>,
    latest_milestone: Option<String>,
    recap: TurnRecapAccumulator,
}

impl Default for RunActivityProjection {
    fn default() -> Self {
        Self {
            phase: ActivityPhase::Idle,
            phase_started_at: None,
            last_event_at: None,
            last_progress_at: None,
            active_tools: BTreeMap::new(),
            tool_batch: None,
            latest_milestone: None,
            recap: TurnRecapAccumulator::default(),
        }
    }
}

impl RunActivityProjection {
    pub(super) fn begin_at(&mut self, at: DateTime<Utc>) {
        *self = Self::default();
        self.phase = ActivityPhase::Starting;
        self.phase_started_at = Some(at);
        self.last_event_at = Some(at);
        self.last_progress_at = Some(at);
    }

    #[cfg(test)]
    pub(super) fn from_events(events: &[EventEnvelope]) -> Self {
        let mut projection = Self::default();
        for event in events {
            projection.observe(event);
        }
        projection
    }

    /// Fold one event and report whether it established meaningful progress.
    /// Callers use the same answer for stall notification edge-resetting.
    pub(super) fn observe(&mut self, event: &EventEnvelope) -> bool {
        let at = self.event_time(event);
        if self.phase == ActivityPhase::Idle {
            if let Some(at) = at {
                self.begin_at(at);
            }
        }
        if let Some(at) = at {
            self.last_event_at = Some(at);
        }
        self.recap.observe(event);
        let meaningful = self.observe_kind(event, at);
        if meaningful {
            if let Some(at) = at {
                self.last_progress_at = Some(at);
            }
        }
        meaningful
    }

    /// Apply one live provider control transition without turning it into a
    /// canonical event. Only the root scope may refine the foreground HUD;
    /// every transition updates liveness at most and never meaningful
    /// progress, recap state, or the latest completed milestone.
    pub(super) fn observe_provider_runtime(
        &mut self,
        event: &ProviderRuntimeEvent,
        at: DateTime<Utc>,
    ) {
        let phase = match event {
            ProviderRuntimeEvent::Attempt { target, .. }
            | ProviderRuntimeEvent::RetryScheduled { target, .. }
                if !target.scope.is_foreground() =>
            {
                return;
            }
            ProviderRuntimeEvent::Attempt { event, .. } => match event {
                ProviderAttemptEvent::Started { .. } => {
                    Some(ActivityPhase::WaitingForResponseHeaders)
                }
                ProviderAttemptEvent::ResponseHeaders { .. } => {
                    Some(ActivityPhase::WaitingForFirstByte)
                }
                ProviderAttemptEvent::FirstByte { .. } => {
                    Some(ActivityPhase::WaitingForSemanticOutput)
                }
                ProviderAttemptEvent::FirstSemantic { .. } => {
                    Some(ActivityPhase::ReceivingResponse)
                }
                ProviderAttemptEvent::Ended(summary) => match summary.outcome {
                    ProviderAttemptOutcome::TimedOut(stage) => {
                        Some(ActivityPhase::ProviderTimedOut(stage))
                    }
                    ProviderAttemptOutcome::Cancelled => Some(ActivityPhase::ProviderCancelled),
                    ProviderAttemptOutcome::Completed
                    | ProviderAttemptOutcome::Failed
                    | ProviderAttemptOutcome::StreamEnded
                    | ProviderAttemptOutcome::Abandoned => None,
                },
            },
            ProviderRuntimeEvent::RetryScheduled {
                retry_ordinal,
                backoff_ms,
                ..
            } => Some(ActivityPhase::RetryingModel {
                retry_ordinal: *retry_ordinal,
                backoff_ms: *backoff_ms,
            }),
        };
        let at = self.monotonic_time(at);
        self.last_event_at = Some(at);
        if let Some(phase) = phase.filter(|_| self.active_tools.is_empty()) {
            self.set_phase(phase, Some(at), true);
        }
    }

    fn observe_kind(&mut self, event: &EventEnvelope, at: Option<DateTime<Utc>>) -> bool {
        if event
            .payload
            .get("purpose")
            .and_then(serde_json::Value::as_str)
            == Some("compaction")
        {
            return false;
        }
        match event.kind.as_str() {
            EventKind::USER_MESSAGE => self.observe_user_message(at),
            EventKind::CANVAS_SNAPSHOT => self.observe_canvas_snapshot(at),
            EventKind::MODEL_CALL => self.observe_model_call(at),
            EventKind::MODEL_DELTA => self.observe_model_delta(event, at),
            EventKind::MODEL_REASONING => self.observe_model_reasoning(event, at),
            EventKind::MODEL_RESULT => self.observe_model_result(at),
            EventKind::ASSISTANT_MESSAGE => self.observe_assistant_message(at),
            EventKind::ASSISTANT_ACTIVITY => self.observe_assistant_activity(event, at),
            EventKind::TOOL_CALL => self.observe_tool_call(event, at),
            EventKind::TOOL_RESULT => self.observe_tool_result(event, at),
            EventKind::FILE_CHANGE | EventKind::FILE_DIFF | EventKind::PATCH_APPLIED => {
                self.observe_file_progress(event, at)
            }
            EventKind::CHECK_STARTED => self.observe_check_started(event, at),
            EventKind::CHECK_RESULT => self.observe_check_result(event, at),
            EventKind::PERMISSION_PROMPT => self.observe_permission_prompt(at),
            EventKind::PERMISSION_DECISION => self.observe_permission_decision(event, at),
            EventKind::ERROR => self.observe_error(event, at),
            _ => false,
        }
    }

    fn observe_user_message(&mut self, at: Option<DateTime<Utc>>) -> bool {
        self.set_phase(ActivityPhase::Starting, at, true);
        true
    }

    fn observe_canvas_snapshot(&mut self, at: Option<DateTime<Utc>>) -> bool {
        if self.active_tools.is_empty() {
            self.set_phase(ActivityPhase::PreparingContext, at, false);
        }
        false
    }

    fn observe_model_call(&mut self, at: Option<DateTime<Utc>>) -> bool {
        if self.active_tools.is_empty() {
            self.set_phase(ActivityPhase::WaitingForModel, at, true);
        }
        false
    }

    fn observe_model_delta(&mut self, event: &EventEnvelope, at: Option<DateTime<Utc>>) -> bool {
        let semantic = matches!(payload_str(event, "kind"), Some("text" | "reasoning"));
        if semantic && self.active_tools.is_empty() {
            self.set_phase(ActivityPhase::ReceivingResponse, at, false);
        }
        semantic
    }

    fn observe_model_reasoning(
        &mut self,
        event: &EventEnvelope,
        at: Option<DateTime<Utc>>,
    ) -> bool {
        // Opaque artifacts belong exclusively to their provider adapter. Their
        // arrival proves transport liveness, not user-observable semantic
        // progress, and the activity projector must not interpret them.
        if payload_str(event, "fidelity") == Some("opaque") {
            return false;
        }
        if self.active_tools.is_empty() {
            self.set_phase(ActivityPhase::ReceivingResponse, at, false);
        }
        true
    }

    fn observe_model_result(&mut self, at: Option<DateTime<Utc>>) -> bool {
        if self.active_tools.is_empty() {
            self.set_phase(ActivityPhase::PreparingNextStep, at, true);
        }
        self.latest_milestone = Some("model response received".to_owned());
        true
    }

    fn observe_assistant_message(&mut self, at: Option<DateTime<Utc>>) -> bool {
        if self.active_tools.is_empty() {
            self.set_phase(ActivityPhase::PreparingNextStep, at, true);
        }
        self.latest_milestone = Some("response completed".to_owned());
        true
    }

    fn observe_assistant_activity(
        &mut self,
        event: &EventEnvelope,
        at: Option<DateTime<Utc>>,
    ) -> bool {
        if self.active_tools.is_empty() {
            self.set_phase(ActivityPhase::PreparingNextStep, at, true);
        }
        self.latest_milestone = ["message", "summary", "content"]
            .into_iter()
            .find_map(|key| payload_str(event, key))
            .filter(|message| !message.is_empty())
            .map(bounded_label)
            .filter(|message| !message.is_empty())
            .or_else(|| Some("activity milestone recorded".to_owned()));
        true
    }

    fn observe_tool_call(&mut self, event: &EventEnvelope, at: Option<DateTime<Utc>>) -> bool {
        let tool = classify_tool_call(event);
        let key = payload_str(event, "id")
            .filter(|id| !id.is_empty())
            .unwrap_or(event.id.as_str())
            .to_owned();
        if self.active_tools.is_empty() {
            self.tool_batch = Some(ToolBatch {
                kind: None,
                count: 0,
            });
        }
        let batch = self.tool_batch.get_or_insert(ToolBatch {
            kind: None,
            count: 0,
        });
        batch.add(tool.kind);
        let phase = batch.phase();
        self.active_tools.insert(key, tool);
        self.set_phase(phase, at, self.active_tools.len() == 1);
        false
    }

    fn observe_tool_result(&mut self, event: &EventEnvelope, at: Option<DateTime<Utc>>) -> bool {
        let key = payload_str(event, "id").unwrap_or_default();
        let tool = self.active_tools.remove(key);
        if let Some(tool) = tool.as_ref() {
            self.latest_milestone = Some(tool_milestone(tool, event));
        }
        if self.active_tools.is_empty() {
            self.tool_batch = None;
            self.set_phase(ActivityPhase::PreparingNextStep, at, true);
        }
        true
    }

    fn observe_file_progress(&mut self, event: &EventEnvelope, at: Option<DateTime<Utc>>) -> bool {
        if let Some(path) = payload_str(event, "path").filter(|path| !path.is_empty()) {
            self.latest_milestone = Some(format!("updated {}", bounded_label(path)));
        } else {
            self.latest_milestone = Some("workspace updated".to_owned());
        }
        if self.active_tools.is_empty() {
            self.set_phase(ActivityPhase::PreparingNextStep, at, false);
        }
        true
    }

    fn observe_check_started(&mut self, event: &EventEnvelope, at: Option<DateTime<Utc>>) -> bool {
        let kind = classify_check(payload_str(event, "name").unwrap_or_default());
        self.set_phase(ActivityPhase::RunningChecks { count: 1, kind }, at, true);
        false
    }

    fn observe_check_result(&mut self, event: &EventEnvelope, at: Option<DateTime<Utc>>) -> bool {
        let kind = classify_check(payload_str(event, "name").unwrap_or_default());
        let ok = tool_result_succeeded(&event.payload);
        self.latest_milestone = Some(outcome_milestone(
            kind.milestone(),
            ok,
            shell_exit_code(event),
        ));
        self.set_phase(ActivityPhase::PreparingNextStep, at, true);
        true
    }

    fn observe_permission_prompt(&mut self, at: Option<DateTime<Utc>>) -> bool {
        self.set_phase(ActivityPhase::WaitingForApproval, at, true);
        false
    }

    fn observe_permission_decision(
        &mut self,
        event: &EventEnvelope,
        at: Option<DateTime<Utc>>,
    ) -> bool {
        let allowed = event
            .payload
            .get("allowed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        self.latest_milestone = Some(if allowed {
            "approval granted".to_owned()
        } else {
            "approval denied".to_owned()
        });
        let phase = self
            .tool_batch
            .as_ref()
            .map_or(ActivityPhase::PreparingNextStep, ToolBatch::phase);
        self.set_phase(phase, at, true);
        true
    }

    fn observe_error(&mut self, event: &EventEnvelope, at: Option<DateTime<Utc>>) -> bool {
        let terminal = if event
            .payload
            .get("cancelled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            Some(ActivityPhase::Cancelled)
        } else if event
            .payload
            .get("recovery_closure")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            Some(ActivityPhase::Interrupted)
        } else {
            None
        };
        if let Some(phase) = terminal {
            self.active_tools.clear();
            self.tool_batch = None;
            self.set_phase(phase, at, true);
            self.latest_milestone = Some("run interrupted".to_owned());
        } else {
            // An ordinary error is not terminal authority. Extension,
            // guardian, and auxiliary errors can occur inside a continuing
            // run; the worker's explicit outcome owns run terminal state.
            if self.active_tools.is_empty() {
                self.set_phase(ActivityPhase::PreparingNextStep, at, true);
            }
            self.latest_milestone = Some("operation failed".to_owned());
        }
        true
    }

    pub(super) fn finish_at(&mut self, terminal: ActivityTerminal, at: DateTime<Utc>) {
        self.last_event_at = Some(self.monotonic_time(at));
        self.last_progress_at = self.last_event_at;
        self.active_tools.clear();
        self.tool_batch = None;
        self.set_phase(terminal.phase(), self.last_event_at, true);
    }

    #[cfg(test)]
    pub(super) fn phase(&self) -> &ActivityPhase {
        &self.phase
    }

    pub(super) fn is_stalled_at(&self, now: DateTime<Utc>) -> bool {
        self.phase.can_stall()
            && self
                .last_progress_at
                .is_some_and(|last| age(now, last) >= ACTIVITY_STALL_THRESHOLD)
    }

    pub(super) fn snapshot_at(&self, now: DateTime<Utc>) -> ActivitySnapshot {
        ActivitySnapshot {
            phase: self.phase.clone(),
            phase_age: self
                .phase_started_at
                .map(|at| age(now, at))
                .unwrap_or_default(),
            progress_age: self.last_progress_at.map(|at| age(now, at)),
            last_event_age: self.last_event_at.map(|at| age(now, at)),
            stalled: self.is_stalled_at(now),
            latest_milestone: self.latest_milestone.clone(),
            recap: self.recap.recap(),
        }
    }

    fn set_phase(&mut self, phase: ActivityPhase, at: Option<DateTime<Utc>>, restart: bool) {
        if restart || std::mem::discriminant(&phase) != std::mem::discriminant(&self.phase) {
            if let Some(at) = at {
                self.phase_started_at = Some(at);
            }
        }
        self.phase = phase;
    }

    fn event_time(&self, event: &EventEnvelope) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(&event.ts)
            .ok()
            .map(|at| self.monotonic_time(at.with_timezone(&Utc)))
    }

    fn monotonic_time(&self, at: DateTime<Utc>) -> DateTime<Utc> {
        self.last_event_at.map_or(at, |last| last.max(at))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ActivityTerminal {
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl ActivityTerminal {
    fn phase(self) -> ActivityPhase {
        match self {
            Self::Completed => ActivityPhase::Completed,
            Self::Failed => ActivityPhase::Failed,
            Self::Cancelled => ActivityPhase::Cancelled,
            Self::Interrupted => ActivityPhase::Interrupted,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ActivitySnapshot {
    pub phase: ActivityPhase,
    pub phase_age: Duration,
    pub progress_age: Option<Duration>,
    pub last_event_age: Option<Duration>,
    pub stalled: bool,
    pub latest_milestone: Option<String>,
    pub recap: super::turn_recap::TurnRecap,
}

impl ActivitySnapshot {
    pub(super) fn verb(&self) -> String {
        if self.stalled {
            format!("Stalled: {}", self.phase.label())
        } else {
            self.phase.label()
        }
    }

    pub(super) fn detail(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.recap.file_count > 0 {
            parts.push(format!(
                "{} {} changed",
                self.recap.file_count,
                if self.recap.file_count == 1 {
                    "file"
                } else {
                    "files"
                }
            ));
        }
        if let Some(milestone) = &self.latest_milestone {
            parts.push(format!("Last completed: {milestone}"));
        }
        if let Some(progress_age) = self.progress_age {
            let clocks_differ = progress_age.as_secs() != self.phase_age.as_secs();
            if clocks_differ || !parts.is_empty() {
                parts.push(format!("last progress {} ago", format_age(progress_age)));
            }
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

fn classify_tool_call(event: &EventEnvelope) -> ActiveTool {
    let name = payload_str(event, "name").unwrap_or("tool");
    let input = event.payload.get("input");
    let (kind, milestone_name) = match name {
        "read_file" | "git_status" | "git_diff" | "list_files" | "search" | "tool_result_get"
        | "skill_read" => (ToolKind::Inspection, "inspection".to_owned()),
        "edit_file" | "apply_patch" | "apply-patch" | "write_file" => {
            (ToolKind::Edit, "edit".to_owned())
        }
        "run_shell" => classify_shell_tool(input),
        _ => (ToolKind::Other, bounded_label(name)),
    };
    ActiveTool {
        kind,
        name: milestone_name,
    }
}

fn classify_shell_tool(input: Option<&serde_json::Value>) -> (ToolKind, String) {
    let command = input
        .and_then(|input| input.get("command"))
        .and_then(serde_json::Value::as_str)
        .map(super::super::transcript::normalized_shell_command)
        .unwrap_or_default();
    let lower = command.to_ascii_lowercase();
    if is_publish_command(&lower) {
        return (ToolKind::Publish, "publication".to_owned());
    }
    let check = classify_check(&lower);
    if is_check_command(&lower) {
        return (ToolKind::Check(check), check.milestone().to_owned());
    }
    (ToolKind::Other, "shell command".to_owned())
}

fn classify_check(command: &str) -> CheckKind {
    let lower = command.to_ascii_lowercase();
    if lower.contains("cargo check") {
        CheckKind::CargoCheck
    } else if is_test_command(&lower) {
        CheckKind::Tests
    } else {
        CheckKind::Checks
    }
}

fn is_check_command(command: &str) -> bool {
    is_test_command(command)
        || [
            "cargo check",
            "cargo clippy",
            "cargo fmt",
            "npm run lint",
            "yarn lint",
            "pnpm lint",
            "make check",
        ]
        .iter()
        .any(|needle| command.contains(needle))
}

fn is_test_command(command: &str) -> bool {
    command
        .split_whitespace()
        .any(|token| token == "test" || token == "tests")
        || ["nextest", "pytest", "jest", "vitest", "go test", "ctest"]
            .iter()
            .any(|needle| command.contains(needle))
}

fn is_publish_command(command: &str) -> bool {
    ["git commit", "git push", "gh pr create", "gh pr merge"]
        .iter()
        .any(|needle| command.contains(needle))
}

fn tool_milestone(tool: &ActiveTool, event: &EventEnvelope) -> String {
    let ok = tool_result_succeeded(&event.payload);
    match tool.kind {
        ToolKind::Check(kind) => outcome_milestone(kind.milestone(), ok, shell_exit_code(event)),
        ToolKind::Inspection if ok => "inspection completed".to_owned(),
        ToolKind::Edit if ok => "edit completed".to_owned(),
        ToolKind::Publish if ok => "publication completed".to_owned(),
        _ if ok => format!("{} completed", tool.name),
        _ => outcome_milestone(&tool.name, false, shell_exit_code(event)),
    }
}

fn outcome_milestone(name: &str, ok: bool, exit_code: Option<i64>) -> String {
    if ok {
        let verb = if name == "tests" {
            "passed"
        } else {
            "completed"
        };
        return format!("{name} {verb}");
    }
    exit_code.map_or_else(
        || format!("{name} failed"),
        |code| format!("{name} failed (exit {code})"),
    )
}

fn bounded_label(value: &str) -> String {
    const MAX_CHARS: usize = 64;
    let mut label = String::new();
    let mut whitespace = false;
    for ch in value.chars().filter(|ch| !ch.is_control()) {
        if label.chars().count() >= MAX_CHARS {
            label.push('…');
            break;
        }
        if ch.is_whitespace() {
            if !whitespace && !label.is_empty() {
                label.push(' ');
            }
            whitespace = true;
        } else {
            label.push(ch);
            whitespace = false;
        }
    }
    label.trim().to_owned()
}

fn payload_str<'a>(event: &'a EventEnvelope, key: &str) -> Option<&'a str> {
    event.payload.get(key).and_then(serde_json::Value::as_str)
}

fn age(now: DateTime<Utc>, then: DateTime<Utc>) -> Duration {
    let millis = now.signed_duration_since(then).num_milliseconds().max(0) as u64;
    Duration::from_millis(millis)
}

pub(super) fn format_age(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_support::event_at;
    use euler_core::{ProviderRuntimeScope, ProviderRuntimeTarget};
    use euler_event::object;
    use euler_provider::{ProviderAttemptSummary, ProviderErrorCategory};
    use serde_json::json;

    const T0: &str = "2026-07-31T12:00:00Z";

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(T0)
            .expect("fixture time")
            .with_timezone(&Utc)
            + chrono::Duration::seconds(seconds)
    }

    fn observed(
        projection: &mut RunActivityProjection,
        kind: &'static str,
        payload: euler_event::JsonObject,
        seconds: i64,
    ) {
        let ts = at(seconds).to_rfc3339();
        let mut event = event_at(kind, payload, T0);
        event.ts = ts;
        projection.observe(&event);
    }

    fn rendered_state(projection: &RunActivityProjection, seconds: i64) -> String {
        let snapshot = projection.snapshot_at(at(seconds));
        let marker = if snapshot.stalled { "■" } else { "⠋" };
        let mut lines = vec![format!(
            "{marker} {} · {} · esc to interrupt",
            snapshot.verb(),
            format_age(snapshot.phase_age)
        )];
        if let Some(detail) = snapshot.detail() {
            lines.push(format!("  {detail}"));
        }
        lines.join("\n")
    }

    fn provider_attempt(
        scope: ProviderRuntimeScope,
        event: ProviderAttemptEvent,
    ) -> ProviderRuntimeEvent {
        ProviderRuntimeEvent::Attempt {
            target: ProviderRuntimeTarget {
                scope,
                provider: "fixture".to_owned(),
                model: "echo".to_owned(),
            },
            event,
        }
    }

    fn provider_retry(scope: ProviderRuntimeScope) -> ProviderRuntimeEvent {
        ProviderRuntimeEvent::RetryScheduled {
            target: ProviderRuntimeTarget {
                scope,
                provider: "fixture".to_owned(),
                model: "echo".to_owned(),
            },
            failed_attempt_id: Some("attempt-1".to_owned()),
            category: ProviderErrorCategory::Transport,
            retry_ordinal: 1,
            backoff_ms: 250,
        }
    }

    fn provider_ended(outcome: ProviderAttemptOutcome) -> ProviderAttemptEvent {
        ProviderAttemptEvent::Ended(ProviderAttemptSummary {
            attempt_id: "attempt-1".to_owned(),
            outcome,
            elapsed_ms: 1,
            response_headers_ms: Some(0),
            first_byte_ms: Some(0),
            first_semantic_ms: None,
            last_transport_activity_ms: Some(0),
            last_semantic_activity_ms: None,
        })
    }

    #[test]
    fn observable_phase_surface_snapshot() {
        let cases = [
            (
                "starting",
                EventKind::USER_MESSAGE,
                object([("content", "secret prompt".into())]),
            ),
            (
                "waiting",
                EventKind::MODEL_CALL,
                object([("provider", "fixture".into())]),
            ),
            (
                "streaming",
                EventKind::MODEL_DELTA,
                object([
                    ("kind", "reasoning".into()),
                    ("delta", "private reasoning".into()),
                ]),
            ),
            (
                "inspection",
                EventKind::TOOL_CALL,
                object([
                    ("id", "read".into()),
                    ("name", "read_file".into()),
                    ("input", json!({"path":"src/lib.rs"})),
                ]),
            ),
            (
                "editing",
                EventKind::TOOL_CALL,
                object([
                    ("id", "edit".into()),
                    ("name", "edit_file".into()),
                    ("input", json!({"path":"src/lib.rs"})),
                ]),
            ),
            (
                "testing",
                EventKind::TOOL_CALL,
                object([
                    ("id", "test".into()),
                    ("name", "run_shell".into()),
                    ("input", json!({"command":"cargo test"})),
                ]),
            ),
            (
                "permission",
                EventKind::PERMISSION_PROMPT,
                object([
                    ("capability", "shell-exec".into()),
                    ("reason", "private detail".into()),
                ]),
            ),
        ];
        let mut rendered = Vec::new();
        for (index, (name, kind, payload)) in cases.into_iter().enumerate() {
            let mut projection = RunActivityProjection::default();
            projection.begin_at(at(0));
            observed(&mut projection, kind, payload, index as i64 + 1);
            rendered.push(format!(
                "{name}:\n{}",
                rendered_state(&projection, index as i64 + 4)
            ));
        }
        insta::assert_snapshot!(rendered.join("\n\n"), @r###"
        starting:
        ⠋ Starting run · 3s · esc to interrupt

        waiting:
        ⠋ Waiting for model · 3s · esc to interrupt
          last progress 5s ago

        streaming:
        ⠋ Receiving model response · 3s · esc to interrupt

        inspection:
        ⠋ Inspecting workspace · 3s · esc to interrupt
          last progress 7s ago

        editing:
        ⠋ Editing files · 3s · esc to interrupt
          last progress 8s ago

        testing:
        ⠋ Running tests · 3s · esc to interrupt
          last progress 9s ago

        permission:
        ⠋ Waiting for approval · 3s · esc to interrupt
          last progress 10s ago
        "###);
        let text = rendered.join("\n");
        assert!(!text.contains("private reasoning"));
        assert!(!text.contains("secret prompt"));
        assert!(!text.contains("private detail"));
    }

    #[test]
    fn terminal_states_snapshot() {
        let mut rendered = Vec::new();
        for terminal in [
            ActivityTerminal::Completed,
            ActivityTerminal::Failed,
            ActivityTerminal::Cancelled,
            ActivityTerminal::Interrupted,
        ] {
            let mut projection = RunActivityProjection::default();
            projection.begin_at(at(0));
            projection.finish_at(terminal, at(4));
            rendered.push(rendered_state(&projection, 4));
        }
        insta::assert_snapshot!(rendered.join("\n"), @r###"
        ⠋ Completed · 0s · esc to interrupt
        ⠋ Failed · 0s · esc to interrupt
        ⠋ Cancelled · 0s · esc to interrupt
        ⠋ Interrupted · 0s · esc to interrupt
        "###);
    }

    #[test]
    fn concurrent_tools_keep_one_stable_group_count() {
        let mut projection = RunActivityProjection::default();
        projection.begin_at(at(0));
        for index in 0..5 {
            observed(
                &mut projection,
                EventKind::TOOL_CALL,
                object([
                    ("id", format!("read-{index}").into()),
                    ("name", "read_file".into()),
                    ("input", json!({"path": format!("src/{index}.rs")})),
                ]),
                index + 1,
            );
        }
        observed(
            &mut projection,
            EventKind::TOOL_RESULT,
            object([
                ("id", "read-0".into()),
                ("name", "read_file".into()),
                ("ok", true.into()),
            ]),
            7,
        );
        assert_eq!(projection.phase(), &ActivityPhase::Inspecting(5));
        assert!(rendered_state(&projection, 8).contains("Inspecting 5 files"));
    }

    #[test]
    fn mixed_check_batch_stays_a_check_phase() {
        let mut projection = RunActivityProjection::default();
        projection.begin_at(at(0));
        for (index, command) in ["cargo check", "cargo test"].into_iter().enumerate() {
            observed(
                &mut projection,
                EventKind::TOOL_CALL,
                object([
                    ("id", format!("check-{index}").into()),
                    ("name", "run_shell".into()),
                    ("input", json!({"command": command})),
                ]),
                index as i64 + 1,
            );
        }
        assert_eq!(
            projection.phase(),
            &ActivityPhase::RunningChecks {
                count: 2,
                kind: CheckKind::Checks,
            }
        );
    }

    #[test]
    fn check_result_exit_101_overrides_legacy_ok_true() {
        let mut projection = RunActivityProjection::default();
        projection.begin_at(at(0));
        observed(
            &mut projection,
            EventKind::CHECK_STARTED,
            object([("name", "cargo check".into())]),
            1,
        );
        observed(
            &mut projection,
            EventKind::CHECK_RESULT,
            object([
                ("name", "cargo check".into()),
                ("ok", true.into()),
                ("exit_code", 101.into()),
            ]),
            2,
        );
        assert_eq!(
            projection.snapshot_at(at(2)).latest_milestone.as_deref(),
            Some("cargo check failed (exit 101)")
        );
    }

    #[test]
    fn ordinary_error_is_recoverable_and_later_progress_advances() {
        let mut projection = RunActivityProjection::default();
        projection.begin_at(at(0));
        observed(
            &mut projection,
            EventKind::ERROR,
            object([
                ("source", "extension".into()),
                ("message", "private extension failure".into()),
            ]),
            1,
        );
        assert_eq!(
            projection.phase(),
            &ActivityPhase::PreparingNextStep,
            "an ordinary error has no terminal authority"
        );
        observed(
            &mut projection,
            EventKind::MODEL_CALL,
            object([("provider", "fixture".into()), ("model", "echo".into())]),
            2,
        );
        observed(
            &mut projection,
            EventKind::MODEL_DELTA,
            object([
                ("kind", "text".into()),
                ("delta", "visible recovery".into()),
            ]),
            3,
        );
        let rendered = rendered_state(&projection, 3);
        assert_eq!(projection.phase(), &ActivityPhase::ReceivingResponse);
        assert!(!rendered.contains("private extension failure"));
        assert!(!rendered.contains("visible recovery"));
    }

    #[test]
    fn incident_8kq_failure_then_model_stall_snapshot() {
        let mut projection = RunActivityProjection::default();
        projection.begin_at(at(0));
        observed(
            &mut projection,
            EventKind::TOOL_CALL,
            object([
                ("id", "cargo-check".into()),
                ("name", "run_shell".into()),
                ("input", json!({"command":"cargo check --workspace"})),
            ]),
            2,
        );
        observed(
            &mut projection,
            EventKind::TOOL_RESULT,
            object([
                ("id", "cargo-check".into()),
                ("name", "run_shell".into()),
                ("ok", true.into()),
                ("exit_code", 101.into()),
                ("output", "compiler output omitted".into()),
            ]),
            10,
        );
        observed(&mut projection, EventKind::CANVAS_SNAPSHOT, object([]), 11);
        observed(
            &mut projection,
            EventKind::MODEL_CALL,
            object([("provider", "fixture".into()), ("model", "blocked".into())]),
            12,
        );

        insta::assert_snapshot!(rendered_state(&projection, 47), @r###"
        ■ Stalled: Waiting for model · 35s · esc to interrupt
          Last completed: cargo check failed (exit 101) · last progress 37s ago
        "###);
        assert_eq!(
            projection.snapshot_at(at(47)).last_event_age,
            Some(Duration::from_secs(35))
        );
    }

    #[test]
    fn canvas_snapshot_does_not_reset_meaningful_progress() {
        let mut projection = RunActivityProjection::default();
        projection.begin_at(at(0));
        observed(
            &mut projection,
            EventKind::MODEL_DELTA,
            object([
                ("kind", "text".into()),
                ("delta", "visible response".into()),
            ]),
            5,
        );
        observed(&mut projection, EventKind::CANVAS_SNAPSHOT, object([]), 25);
        let snapshot = projection.snapshot_at(at(35));
        assert_eq!(snapshot.progress_age, Some(Duration::from_secs(30)));
        assert_eq!(snapshot.last_event_age, Some(Duration::from_secs(10)));
        assert!(snapshot.stalled);
    }

    #[test]
    fn provider_control_updates_liveness_without_resetting_progress() {
        let mut projection = RunActivityProjection::default();
        projection.begin_at(at(0));
        observed(
            &mut projection,
            EventKind::MODEL_DELTA,
            object([("kind", "text".into()), ("delta", "visible".into())]),
            5,
        );

        projection.observe_provider_runtime(
            &provider_attempt(
                ProviderRuntimeScope::Root,
                ProviderAttemptEvent::ResponseHeaders {
                    attempt_id: "attempt-1".to_owned(),
                    elapsed_ms: 20,
                },
            ),
            at(25),
        );

        let snapshot = projection.snapshot_at(at(35));
        assert_eq!(snapshot.phase, ActivityPhase::WaitingForFirstByte);
        assert_eq!(snapshot.progress_age, Some(Duration::from_secs(30)));
        assert_eq!(snapshot.last_event_age, Some(Duration::from_secs(10)));
        assert!(snapshot.stalled);
    }

    #[test]
    fn provider_attempt_timeout_retry_and_cancellation_are_visible() {
        let mut projection = RunActivityProjection::default();
        projection.begin_at(at(0));
        observed(
            &mut projection,
            EventKind::TOOL_CALL,
            object([
                ("id", "cargo-check".into()),
                ("name", "run_shell".into()),
                ("input", json!({"command":"cargo check"})),
            ]),
            1,
        );
        observed(
            &mut projection,
            EventKind::TOOL_RESULT,
            object([
                ("id", "cargo-check".into()),
                ("name", "run_shell".into()),
                ("ok", false.into()),
                ("exit_code", 101.into()),
            ]),
            2,
        );

        projection.observe_provider_runtime(
            &provider_attempt(
                ProviderRuntimeScope::Root,
                ProviderAttemptEvent::Started {
                    attempt_id: "attempt-1".to_owned(),
                },
            ),
            at(3),
        );
        assert_eq!(
            projection.phase(),
            &ActivityPhase::WaitingForResponseHeaders
        );
        projection.observe_provider_runtime(
            &provider_attempt(
                ProviderRuntimeScope::Root,
                ProviderAttemptEvent::ResponseHeaders {
                    attempt_id: "attempt-1".to_owned(),
                    elapsed_ms: 1,
                },
            ),
            at(4),
        );
        assert_eq!(projection.phase(), &ActivityPhase::WaitingForFirstByte);
        projection.observe_provider_runtime(
            &provider_attempt(
                ProviderRuntimeScope::Root,
                ProviderAttemptEvent::FirstByte {
                    attempt_id: "attempt-1".to_owned(),
                    elapsed_ms: 2,
                },
            ),
            at(5),
        );
        assert_eq!(projection.phase(), &ActivityPhase::WaitingForSemanticOutput);
        projection.observe_provider_runtime(
            &provider_attempt(
                ProviderRuntimeScope::Root,
                ProviderAttemptEvent::FirstSemantic {
                    attempt_id: "attempt-1".to_owned(),
                    elapsed_ms: 3,
                },
            ),
            at(6),
        );
        assert_eq!(projection.phase(), &ActivityPhase::ReceivingResponse);
        projection.observe_provider_runtime(
            &provider_attempt(
                ProviderRuntimeScope::Root,
                provider_ended(ProviderAttemptOutcome::TimedOut(
                    ProviderTimeoutStage::SemanticIdle,
                )),
            ),
            at(35),
        );
        assert_eq!(
            projection.snapshot_at(at(35)).verb(),
            "Model response timed out without meaningful output"
        );

        projection.observe_provider_runtime(&provider_retry(ProviderRuntimeScope::Root), at(36));
        let retry = projection.snapshot_at(at(36));
        assert_eq!(
            retry.verb(),
            "Stalled: Retrying model request (retry 1 in 250ms)"
        );
        assert_eq!(
            retry.latest_milestone.as_deref(),
            Some("cargo check failed (exit 101)")
        );
        assert_eq!(retry.progress_age, Some(Duration::from_secs(34)));

        projection.observe_provider_runtime(
            &provider_attempt(
                ProviderRuntimeScope::Root,
                provider_ended(ProviderAttemptOutcome::Cancelled),
            ),
            at(37),
        );
        assert_eq!(projection.phase(), &ActivityPhase::ProviderCancelled);
        assert_eq!(
            projection.snapshot_at(at(37)).latest_milestone,
            retry.latest_milestone
        );
    }

    #[test]
    fn nonroot_provider_control_is_ignored_completely() {
        let mut projection = RunActivityProjection::default();
        projection.begin_at(at(0));
        observed(
            &mut projection,
            EventKind::MODEL_CALL,
            object([("provider", "fixture".into()), ("model", "echo".into())]),
            1,
        );
        let expected = projection.clone();

        for scope in [
            ProviderRuntimeScope::Companion,
            ProviderRuntimeScope::ParallelReviewer,
            ProviderRuntimeScope::Compaction,
        ] {
            projection.observe_provider_runtime(
                &provider_attempt(
                    scope,
                    ProviderAttemptEvent::Started {
                        attempt_id: "child-attempt".to_owned(),
                    },
                ),
                at(20),
            );
            projection.observe_provider_runtime(&provider_retry(scope), at(21));
        }

        assert_eq!(projection, expected);
    }

    #[test]
    fn opaque_reasoning_is_liveness_not_semantic_progress() {
        let mut projection = RunActivityProjection::default();
        projection.begin_at(at(0));
        observed(
            &mut projection,
            EventKind::MODEL_DELTA,
            object([("kind", "text".into()), ("delta", "visible".into())]),
            5,
        );
        observed(
            &mut projection,
            EventKind::MODEL_REASONING,
            object([
                ("fidelity", "opaque".into()),
                ("content", "".into()),
                ("artifact", "provider-private-artifact".into()),
            ]),
            25,
        );

        let snapshot = projection.snapshot_at(at(35));
        assert_eq!(snapshot.phase, ActivityPhase::ReceivingResponse);
        assert_eq!(snapshot.progress_age, Some(Duration::from_secs(30)));
        assert_eq!(snapshot.last_event_age, Some(Duration::from_secs(10)));
        assert!(!rendered_state(&projection, 35).contains("provider-private-artifact"));
    }

    #[test]
    fn out_of_order_timestamps_cannot_move_activity_clocks_backward() {
        let mut projection = RunActivityProjection::default();
        projection.begin_at(at(0));
        observed(
            &mut projection,
            EventKind::MODEL_DELTA,
            object([("kind", "text".into()), ("delta", "visible".into())]),
            10,
        );
        observed(&mut projection, EventKind::CANVAS_SNAPSHOT, object([]), 5);

        let snapshot = projection.snapshot_at(at(12));
        assert_eq!(snapshot.phase, ActivityPhase::PreparingContext);
        assert_eq!(snapshot.phase_age, Duration::from_secs(2));
        assert_eq!(snapshot.progress_age, Some(Duration::from_secs(2)));
        assert_eq!(snapshot.last_event_age, Some(Duration::from_secs(2)));
    }

    #[test]
    fn fold_is_deterministic_from_event_timestamps() {
        let events = vec![
            event_at(
                EventKind::USER_MESSAGE,
                object([("content", "go".into())]),
                T0,
            ),
            event_at(
                EventKind::MODEL_CALL,
                object([("provider", "fixture".into()), ("model", "echo".into())]),
                "2026-07-31T12:00:02Z",
            ),
        ];
        assert_eq!(
            RunActivityProjection::from_events(&events),
            RunActivityProjection::from_events(&events)
        );
    }
}
