//! Extension permission reviewers (ADR 0027 section 5): the deny-only
//! vetoes named in the profile's `extension_reviewers`, which an uncovered
//! ask goes to before the guardian and the human. A reviewer can deny or
//! defer, never allow. One that fails, times out, lacks standing authority,
//! or returns a malformed verdict defers and is recorded as degraded.

use super::extension_bridge::ExtensionCommandBoundary;
use super::{Session, SessionError};
use crate::deadline::DeadlineCancellation;
use crate::execution_profile::{ExecutionProfile, RuleMatch};
use crate::extensions::extension_declaration;
use crate::guardian::{bounded_text, MAX_RATIONALE_BYTES, MAX_TASK_FIELD_BYTES};
use crate::permissions::{PermissionDecider, PermissionRequest};
use euler_event::JsonObject;
use euler_sdk::CancellationToken;
use serde_json::Value;
use std::time::{Duration, Instant};

/// The host budget for one reviewer and for every reviewer of one ask
/// (ADR 0027 resolved question 8). Expiry cancels the reviewer's work.
const REVIEWER_BUDGET: Duration = Duration::from_secs(10);
const ASK_BUDGET: Duration = Duration::from_secs(30);

#[cfg(test)]
thread_local! {
    /// Shorter `(reviewer, ask)` budgets for tests on this thread.
    pub(crate) static REVIEWER_BUDGETS_FOR_TEST: std::cell::Cell<Option<(Duration, Duration)>> =
        const { std::cell::Cell::new(None) };
}

fn budgets() -> (Duration, Duration) {
    #[cfg(test)]
    if let Some(budgets) = REVIEWER_BUDGETS_FOR_TEST.with(std::cell::Cell::get) {
        return budgets;
    }
    (REVIEWER_BUDGET, ASK_BUDGET)
}

/// A reviewer's deny: the `<extension-id>.<command>` that denied and its
/// redacted, bounded reason.
pub(crate) struct ReviewerDenial {
    pub(crate) reviewer: String,
    pub(crate) reason: String,
}

/// What the extension reviewers said about one ask.
#[derive(Default)]
pub(crate) struct ExtensionReview {
    pub(crate) denial: Option<ReviewerDenial>,
    /// The reviewers that deferred because they failed, timed out, lacked
    /// authority, or returned a malformed verdict.
    pub(crate) degraded: Vec<String>,
}

enum ReviewerOutcome {
    Deny(String),
    Defer,
    Degraded,
}

impl<D: PermissionDecider> Session<D> {
    /// Ask each extension reviewer of the profile, in order, about the
    /// requests of one ask (one, or every request of a batch) until one
    /// denies. Every reviewer of the ask shares one budget.
    pub(super) fn review_with_extension_reviewers(
        &mut self,
        requests: &[(&PermissionRequest, Option<&RuleMatch>)],
        cancellation: &CancellationToken,
    ) -> Result<ExtensionReview, SessionError> {
        let mut review = ExtensionReview::default();
        let reviewers = self
            .config
            .execution_profile
            .fields
            .extension_reviewers
            .clone();
        // An ask raised while a reviewer runs is reviewed by no one.
        if reviewers.is_empty() || self.permissions.no_review() {
            return Ok(review);
        }
        let (reviewer_budget, ask_budget) = budgets();
        let ask_deadline = Instant::now() + ask_budget;
        for (request, rule) in requests {
            let input = self.reviewer_input(request, *rule);
            for reviewer in &reviewers {
                let left = ask_deadline.saturating_duration_since(Instant::now());
                let outcome = if left.is_zero() {
                    ReviewerOutcome::Degraded
                } else {
                    self.run_extension_reviewer(
                        reviewer,
                        input.clone(),
                        reviewer_budget.min(left),
                        cancellation,
                    )?
                };
                match outcome {
                    ReviewerOutcome::Deny(reason) => {
                        review.denial = Some(ReviewerDenial {
                            reviewer: reviewer.clone(),
                            reason,
                        });
                        return Ok(review);
                    }
                    ReviewerOutcome::Defer => {}
                    ReviewerOutcome::Degraded => {
                        if !review.degraded.contains(reviewer) {
                            review.degraded.push(reviewer.clone());
                        }
                    }
                }
            }
        }
        Ok(review)
    }

    /// The `{capability, tool, reason, command, path, escalation, rule,
    /// profile}` a reviewer sees: each text redacted, then bounded like
    /// guardian input.
    fn reviewer_input(&self, request: &PermissionRequest, rule: Option<&RuleMatch>) -> Value {
        let text = |text: &str| -> Value {
            bounded_text(&self.redactor.redact(text), MAX_TASK_FIELD_BYTES).into()
        };
        let mut input = JsonObject::new();
        input.insert("capability".to_owned(), request.capability.as_str().into());
        if let Some(tool) = &request.tool {
            input.insert("tool".to_owned(), text(tool));
        }
        input.insert("reason".to_owned(), text(&request.reason));
        if let Some(command) = &request.command {
            input.insert("command".to_owned(), text(command));
            if request.command_truncated {
                input.insert("command_truncated".to_owned(), true.into());
            }
        }
        if let Some(path) = &request.path {
            input.insert("path".to_owned(), text(&path.to_string_lossy()));
        }
        if let Some(escalation) = &request.escalation {
            input.insert("escalation".to_owned(), text(escalation));
        }
        if let Some(rule) = rule {
            input.insert("rule".to_owned(), rule.record());
        }
        input.insert(
            "profile".to_owned(),
            self.config.execution_profile.name.clone().into(),
        );
        Value::Object(input)
    }

    /// Run one reviewer with standing authority only, within `budget`. It
    /// never opens a prompt: an ask its host calls or spawned agents raise
    /// is denied without review while it runs.
    fn run_extension_reviewer(
        &mut self,
        reviewer: &str,
        input: Value,
        budget: Duration,
        cancellation: &CancellationToken,
    ) -> Result<ReviewerOutcome, SessionError> {
        let Some((extension_id, command)) = reviewer.split_once('.') else {
            return Ok(ReviewerOutcome::Degraded);
        };
        let Some(extension) = self
            .extensions
            .get(extension_id)
            .filter(|_| self.extension_enabled(extension_id))
            .cloned()
        else {
            return Ok(ReviewerOutcome::Degraded);
        };
        let Ok(declaration) = extension_declaration(extension.as_ref()) else {
            return Ok(ReviewerOutcome::Degraded);
        };
        let names_command = declaration
            .permission_reviewer
            .is_some_and(|descriptor| descriptor.command == command);
        let Some(descriptor) = declaration.commands.get(command).filter(|_| names_command) else {
            return Ok(ReviewerOutcome::Degraded);
        };
        let required = descriptor.required_capabilities.clone();
        if !self.implicit_capabilities_preauthorized(extension_id, command, &required) {
            return Ok(ReviewerOutcome::Degraded);
        }
        let deadline = DeadlineCancellation::start(cancellation.clone(), budget);
        self.permissions.set_no_review(true);
        let result = self.execute_extension_command_at_boundary(
            extension.as_ref(),
            command,
            input,
            required,
            ExtensionCommandBoundary::permission_reviewer(extension_id, deadline.token()),
        );
        self.permissions.set_no_review(false);
        deadline.finish();
        if cancellation.is_cancelled() {
            return Err(SessionError::Cancelled);
        }
        Ok(
            match result.ok().and_then(|verdict| parse_verdict(&verdict)) {
                Some(Verdict::Deny(reason)) => ReviewerOutcome::Deny(bounded_text(
                    &self.redactor.redact(&reason),
                    MAX_RATIONALE_BYTES,
                )),
                Some(Verdict::Defer) => ReviewerOutcome::Defer,
                None => ReviewerOutcome::Degraded,
            },
        )
    }
}

impl<D> Session<D> {
    /// Refuse `profile` when this session cannot run an extension reviewer
    /// it names: its extension is not enabled, or has no reviewer
    /// descriptor for that command. A profile never runs without a reviewer
    /// it names (ADR 0027 resolved question 16).
    pub fn check_extension_reviewers(
        &self,
        profile: &ExecutionProfile,
    ) -> Result<(), SessionError> {
        for reviewer in &profile.fields.extension_reviewers {
            let (extension_id, command) = reviewer.split_once('.').unwrap_or((reviewer, ""));
            let why = if !self.extension_enabled(extension_id) {
                format!("extension {extension_id} is not installed and enabled")
            } else {
                match self
                    .extensions
                    .get(extension_id)
                    .and_then(|extension| extension_declaration(extension.as_ref()).ok())
                {
                    None => format!("extension {extension_id} could not be loaded"),
                    Some(declaration)
                        if declaration
                            .permission_reviewer
                            .as_ref()
                            .is_some_and(|descriptor| descriptor.command == command) =>
                    {
                        continue;
                    }
                    Some(_) => format!(
                        "extension {extension_id} declares no permission reviewer command \
                         {command}"
                    ),
                }
            };
            return Err(SessionError::ExtensionReviewerUnavailable {
                profile: profile.name.clone(),
                reviewer: reviewer.clone(),
                why,
            });
        }
        Ok(())
    }
}

enum Verdict {
    Deny(String),
    Defer,
}

/// A reviewer's `{"verdict": "deny" | "defer", "reason"}`. A deny needs a
/// reason; any other shape is malformed.
fn parse_verdict(value: &Value) -> Option<Verdict> {
    let object = value.as_object()?;
    if object.keys().any(|key| key != "verdict" && key != "reason") {
        return None;
    }
    let reason = match object.get("reason") {
        None => None,
        Some(reason) => Some(reason.as_str()?),
    };
    match (object.get("verdict")?.as_str()?, reason) {
        ("deny", Some(reason)) => Some(Verdict::Deny(reason.to_owned())),
        ("defer", _) => Some(Verdict::Defer),
        _ => None,
    }
}

/// The tool error for a request an extension reviewer denied. It covers
/// that request only (ADR 0027 section 4).
pub(crate) fn reviewer_denial(denial: &ReviewerDenial) -> String {
    format!(
        "permission denied by extension reviewer {}: {}. Do not work around it with another \
         command or tool; this denial covers this request only, so carry on with other work or \
         ask the user",
        denial.reviewer, denial.reason
    )
}

/// Add `decision_source: "reviewer"` and the denying reviewer to a deny
/// payload.
pub(crate) fn insert_reviewer_denial(payload: &mut JsonObject, denial: &ReviewerDenial) {
    payload.insert("decision_source".to_owned(), "reviewer".into());
    payload.insert(
        "reviewer".to_owned(),
        serde_json::json!({ "name": denial.reviewer, "reason": denial.reason }),
    );
}

/// Add the reviewers that deferred because they failed to a decision
/// payload, when any did.
pub(crate) fn insert_degraded_reviewers(payload: &mut JsonObject, degraded: &[String]) {
    if !degraded.is_empty() {
        payload.insert("reviewers_degraded".to_owned(), degraded.to_vec().into());
    }
}
