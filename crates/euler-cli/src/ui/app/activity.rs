//! Deterministic high-level activity projection for the pinned TUI HUD.
//!
//! This projection reports observable session activity. It intentionally
//! never reads model reasoning or delta text, so it cannot expose cognition.

use super::turn_recap::{
    effective_tool_result_ok, shell_exit_code, TestStatus, TurnRecapAccumulator,
};
use euler_event::{EventEnvelope, EventKind};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ActivityPhase {
    Idle,
    Starting,
    PreparingContext,
    WaitingForModel,
    StreamingResponse,
    Inspecting(usize),
    Editing(usize),
    RunningChecks { count: usize, tests: bool },
    Publishing(usize),
    RunningTools(usize),
    WaitingForApproval,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl Default for ActivityPhase {
    fn default() -> Self {
        Self::Idle
    }
}

impl ActivityPhase {
    fn family(&self) -> PhaseFamily {
        match self {
            Self::Idle => PhaseFamily::Idle,
            Self::Starting => PhaseFamily::Starting,
            Self::PreparingContext => PhaseFamily::PreparingContext,
            Self::WaitingForModel => PhaseFamily::WaitingForModel,
            Self::StreamingResponse => PhaseFamily::StreamingResponse,
            Self::Inspecting(_) => PhaseFamily::Inspecting,
            Self::Editing(_) => PhaseFamily::Editing,
            Self::RunningChecks { tests: true, .. } => PhaseFamily::RunningTests,
            Self::RunningChecks { tests: false, .. } => PhaseFamily::RunningChecks,
            Self::Publishing(_) => PhaseFamily::Publishing,
            Self::RunningTools(_) => PhaseFamily::RunningTools,
            Self::WaitingForApproval => PhaseFamily::WaitingForApproval,
            Self::Completed => PhaseFamily::Completed,
            Self::Failed => PhaseFamily::Failed,
            Self::Cancelled => PhaseFamily::Cancelled,
            Self::Interrupted => PhaseFamily::Interrupted,
        }
    }

    pub(super) fn label(&self) -> String {
        match self {
            Self::Idle => "Idle".to_owned(),
            Self::Starting => "Starting run".to_owned(),
            Self::PreparingContext => "Preparing context".to_owned(),
            Self::WaitingForModel => "Waiting for model".to_owned(),
            Self::StreamingResponse => "Receiving response".to_owned(),
            Self::Inspecting(count) => counted("Inspecting workspace", "Inspecting", *count),
            Self::Editing(count) => counted("Editing files", "Editing", *count),
            Self::RunningChecks { count, tests } => {
                let label = if *tests { "Running tests" } else { "Running checks" };
                counted(label, label, *count)
            }
            Self::Publishing(count) => counted("Publishing changes", "Publishing", *count),
            Self::RunningTools(count) => counted("Running tool", "Running", *count),
            Self::WaitingForApproval => "Waiting for approval".to_owned(),
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
        )
    }

    pub(super) fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

fn counted(singular: &str, plural: &str, count: usize) -> String {
    if count <= 1 {
        singular.to_owned()
    } else {
        format!("{plural} {count} tools")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PhaseFamily {
    Idle,
    Starting,
    PreparingContext,
    WaitingForModel,
    StreamingResponse,
    Inspecting,
    Editing,
    RunningTests,
    RunningChecks,
    Publishing,
    RunningTools,
    WaitingForApproval,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolKind {
    Inspection,
    Edit,
    Check { tests: bool },
    Publish,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActiveTool {
    call_id: Option<String>,
    kind: ToolKind,
    milestone_name: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ActivityObservation {
    pub accepted: bool,
    pub meaningful_progress: bool,
    pub restart_phase_clock: bool,
}
