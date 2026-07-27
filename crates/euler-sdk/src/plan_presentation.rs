use crate::extension_model_text_is_format_safe;
use thiserror::Error;

pub const MAX_PLAN_PRESENTATION_ITEMS: usize = 16;
pub const MAX_PLAN_PRESENTATION_STEP_BYTES: usize = 1024;
pub const MAX_PLAN_PRESENTATION_EXPLANATION_BYTES: usize = 4096;
pub const MAX_PLAN_PRESENTATION_REVISION: u64 = i64::MAX as u64;

/// Workflow-owned plan state projected for canonical host presentation.
///
/// This DTO deliberately contains no transition rules. An extension decides
/// when a plan exists, how revisions advance, whether an explanation is
/// required, and which item combinations are meaningful. The host validates
/// only the bounded presentation shape before emitting `plan.update`.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanPresentation {
    pub revision: u64,
    pub status: PlanPresentationStatus,
    pub explanation: Option<String>,
    pub items: Vec<PlanPresentationItem>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanPresentationStatus {
    Active,
    Blocked,
    Waiting,
    Completed,
}

impl PlanPresentationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "blocked" => Some(Self::Blocked),
            "waiting" => Some(Self::Waiting),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanPresentationItem {
    pub step: String,
    pub status: PlanItemStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanItemStatus {
    Pending,
    InProgress,
    Completed,
}

impl PlanItemStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("{message}")]
pub struct PlanPresentationValidationError {
    message: String,
}

impl PlanPresentationValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub fn validate_plan_presentation(
    presentation: &PlanPresentation,
) -> Result<(), PlanPresentationValidationError> {
    if !(1..=MAX_PLAN_PRESENTATION_REVISION).contains(&presentation.revision) {
        return Err(PlanPresentationValidationError::new(format!(
            "plan revision must be between 1 and {MAX_PLAN_PRESENTATION_REVISION}"
        )));
    }
    if presentation.items.is_empty() || presentation.items.len() > MAX_PLAN_PRESENTATION_ITEMS {
        return Err(PlanPresentationValidationError::new(format!(
            "plan must contain between 1 and {MAX_PLAN_PRESENTATION_ITEMS} items"
        )));
    }
    if let Some(explanation) = &presentation.explanation {
        validate_text(
            explanation,
            "plan explanation",
            MAX_PLAN_PRESENTATION_EXPLANATION_BYTES,
        )?;
    }
    for (index, item) in presentation.items.iter().enumerate() {
        validate_text(
            &item.step,
            &format!("plan item {} step", index + 1),
            MAX_PLAN_PRESENTATION_STEP_BYTES,
        )?;
    }
    Ok(())
}

fn validate_text(
    text: &str,
    label: &str,
    max_bytes: usize,
) -> Result<(), PlanPresentationValidationError> {
    if text.trim().is_empty() || text.len() > max_bytes {
        return Err(PlanPresentationValidationError::new(format!(
            "{label} must be nonempty and at most {max_bytes} bytes"
        )));
    }
    if text.chars().any(char::is_control) || !extension_model_text_is_format_safe(text) {
        return Err(PlanPresentationValidationError::new(format!(
            "{label} contains an unsupported control character"
        )));
    }
    Ok(())
}
