//! Run continuation and crash-loop parking (ADR 0023, Decisions 1 and 4).
//!
//! `--continue` keeps an interrupted run open instead of closing it. It is
//! allowed only for a cleanly admitted run whose root work was visibly cut
//! off, and only while the run is not parked. Every refusal happens before
//! anything is appended.

use super::{
    call_recovery_closures, logical_parent_frontier, tool_call_has_result, unresolved_model_calls,
    ResumeError,
};
use crate::guardian::GUARDIAN_TURN_INTERRUPT_MESSAGE;
use crate::session::run_lifecycle::{
    fold_run_lifecycle, is_run_transaction_cancellation, QueueCancellationReason,
    RunLifecycleProjection,
};
use crate::session::TOOL_ROUNDS_LIMIT_MESSAGE;
use euler_event::{object, EventEnvelope, EventKind, JsonObject};
use serde_json::Value;
use thiserror::Error;

/// A run continued this many times without progress is parked (ADR 0023,
/// resolved question 2).
pub const RUN_RESTART_LIMIT: u32 = 5;

/// An accepted continuation of the session's open run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunContinuation {
    pub run_id: String,
    /// The run's restart count once this continuation is recorded.
    pub restart_count: u32,
    pub retry: bool,
}

/// An outcome the live session had already decided before the restart,
/// although its `run.terminal` was never persisted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecidedRunOutcome {
    Completed,
    Cancelled,
    ProviderError,
    SessionError,
    GuardianInterrupt,
    ToolRoundsLimit,
}

impl DecidedRunOutcome {
    fn describe(self) -> &'static str {
        match self {
            Self::Completed => "had already finished",
            Self::Cancelled => "had already been cancelled",
            Self::ProviderError => "had already failed with a provider error",
            Self::SessionError => "had already failed",
            Self::GuardianInterrupt => "had already been stopped by the guardian",
            Self::ToolRoundsLimit => "had already reached its tool-round limit",
        }
    }
}

/// Why a run cannot be continued. Each refusal appends nothing.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum ContinuationRefusal {
    #[error("the session has no interrupted run to continue")]
    NoOpenRun,
    #[error(
        "run {run_id} was interrupted while it was being started, before its prompt was saved"
    )]
    IncompleteAdmission { run_id: String },
    #[error("run {run_id} was interrupted partway through recording how it ended or continued")]
    PartialTerminalBatch { run_id: String },
    #[error("run {run_id} {} before it was interrupted", outcome.describe())]
    DecidedOutcome {
        run_id: String,
        outcome: DecidedRunOutcome,
    },
    #[error("run {run_id} shows no work that was cut off")]
    NotInFlight { run_id: String },
    #[error(
        "tool call {call_id} in run {run_id} stopped partway and has no result resume can record"
    )]
    UnclosedToolCall { run_id: String, call_id: String },
    #[error(
        "run {run_id} is parked (restart_limit): it was continued {restart_count} times without progress"
    )]
    Parked { run_id: String, restart_count: u32 },
    #[error(
        "run {run_id} is not parked ({restart_count} of {RUN_RESTART_LIMIT} restarts used), so there is nothing to retry"
    )]
    RetryNotParked { run_id: String, restart_count: u32 },
}

/// Check whether the session's open run can be continued, without
/// appending anything. Resume repeats this check at its mutation boundary.
pub fn check_run_continuation(
    events: &[EventEnvelope],
    retry: bool,
) -> Result<RunContinuation, ResumeError> {
    let lifecycle = fold_run_lifecycle(events)?;
    plan_run_continuation(events, &lifecycle, retry).map(|(continuation, _)| continuation)
}

/// The continuation and the events resume appends for it: today's closures
/// for every unresolved call, `queue.cancelled` for the run's pending
/// steering, and `run.continued`.
pub(super) fn plan_run_continuation(
    events: &[EventEnvelope],
    lifecycle: &RunLifecycleProjection,
    retry: bool,
) -> Result<(RunContinuation, Vec<EventEnvelope>), ResumeError> {
    let (continuation, session_id, agent_id) = classify_open_run(events, lifecycle, retry)?;
    let mut appended = call_recovery_closures(events)?;
    let mut candidate = events.to_vec();
    candidate.extend(appended.iter().cloned());
    if let Some(call_id) = unmatched_root_tool_call(&candidate, &continuation.run_id, &agent_id) {
        return Err(ContinuationRefusal::UnclosedToolCall {
            run_id: continuation.run_id,
            call_id,
        }
        .into());
    }
    let mut parent = appended
        .last()
        .map(|event| event.id.clone())
        .or_else(|| logical_parent_frontier(events));
    for item in lifecycle.pending_steering(&continuation.run_id) {
        let cancelled = EventEnvelope::new(
            session_id.clone(),
            agent_id.clone(),
            parent,
            EventKind::QUEUE_CANCELLED,
            object([
                ("queue_id", item.queue_id().to_owned().into()),
                (
                    "reason",
                    QueueCancellationReason::RunInterrupted.as_str().into(),
                ),
                ("recovery_closure", true.into()),
            ]),
        )
        .with_run(continuation.run_id.clone());
        parent = Some(cancelled.id.clone());
        appended.push(cancelled);
    }
    let mut payload = JsonObject::new();
    if retry {
        payload.insert("retry".to_owned(), true.into());
    }
    appended.push(
        EventEnvelope::new(
            session_id,
            agent_id,
            parent,
            EventKind::RUN_CONTINUED,
            payload,
        )
        .with_run(continuation.run_id.clone()),
    );
    Ok((continuation, appended))
}

/// Whether the candidate prefix leaves exactly the continued state: no call
/// left to close, no pending steering, and the run still open.
pub(super) fn continuation_is_settled(
    events: &[EventEnvelope],
    lifecycle: &RunLifecycleProjection,
    run_id: &str,
) -> Result<bool, ResumeError> {
    Ok(lifecycle.is_open(run_id)
        && lifecycle.pending_steering(run_id).is_empty()
        && call_recovery_closures(events)?.is_empty())
}

fn classify_open_run(
    events: &[EventEnvelope],
    lifecycle: &RunLifecycleProjection,
    retry: bool,
) -> Result<(RunContinuation, String, String), ResumeError> {
    let Some((run_id, session_id, agent_id)) = lifecycle.open_runs().next() else {
        return Err(no_open_run_refusal(events, lifecycle).into());
    };
    let refuse = |refusal: ContinuationRefusal| Err(ResumeError::from(refusal));
    let driver = |event: &EventEnvelope| {
        event.agent == agent_id
            && event.run.as_deref() == Some(run_id)
            && !event.payload.contains_key("purpose")
    };
    // A committed admission always carries its `user.message`.
    let Some(admission) = events
        .iter()
        .position(|event| event.kind.as_str() == EventKind::USER_MESSAGE && driver(event))
    else {
        return refuse(ContinuationRefusal::NotInFlight {
            run_id: run_id.to_owned(),
        });
    };
    let after_admission = &events[admission + 1..];
    let since_continuation = after_admission
        .iter()
        .rposition(|event| event.kind.as_str() == EventKind::RUN_CONTINUED && driver(event))
        .map_or(after_admission, |index| &after_admission[index + 1..]);
    if since_continuation
        .iter()
        .any(|event| driver(event) && is_run_transaction_cancellation(event))
    {
        return refuse(ContinuationRefusal::PartialTerminalBatch {
            run_id: run_id.to_owned(),
        });
    }
    let driver_work = after_admission
        .iter()
        .filter(|event| driver(event))
        .collect::<Vec<_>>();
    if let Some(outcome) = decided_outcome(&driver_work) {
        return refuse(ContinuationRefusal::DecidedOutcome {
            run_id: run_id.to_owned(),
            outcome,
        });
    }
    if !root_work_in_flight(events, &driver_work, run_id, agent_id, driver)? {
        return refuse(ContinuationRefusal::NotInFlight {
            run_id: run_id.to_owned(),
        });
    }
    Ok((
        count_restart(lifecycle, run_id, retry)?,
        session_id.to_owned(),
        agent_id.to_owned(),
    ))
}

/// Whether the run's root work was visibly cut off: an unresolved
/// `model.call`, an unmatched `tool.call`, or a settled tool batch, steering
/// message, or earlier continuation not yet followed by a `model.call`.
fn root_work_in_flight(
    events: &[EventEnvelope],
    driver_work: &[&EventEnvelope],
    run_id: &str,
    agent_id: &str,
    driver: impl Fn(&EventEnvelope) -> bool,
) -> Result<bool, ResumeError> {
    if unresolved_model_calls(events)?.into_iter().any(driver)
        || unmatched_root_tool_call(events, run_id, agent_id).is_some()
    {
        return Ok(true);
    }
    Ok(driver_work
        .iter()
        .rev()
        .find(|event| {
            matches!(
                event.kind.as_str(),
                EventKind::MODEL_CALL
                    | EventKind::TOOL_RESULT
                    | EventKind::USER_MESSAGE
                    | EventKind::RUN_CONTINUED
            )
        })
        .is_some_and(|event| event.kind.as_str() != EventKind::MODEL_CALL))
}

/// Apply the restart limit: a parked run continues only with `retry`, and
/// `retry` is accepted only for a parked run, where it starts the count again
/// at one.
fn count_restart(
    lifecycle: &RunLifecycleProjection,
    run_id: &str,
    retry: bool,
) -> Result<RunContinuation, ResumeError> {
    let restart_count = lifecycle.restart_count(run_id);
    let parked = restart_count >= RUN_RESTART_LIMIT;
    if parked && !retry {
        return Err(ContinuationRefusal::Parked {
            run_id: run_id.to_owned(),
            restart_count,
        }
        .into());
    }
    if retry && !parked {
        return Err(ContinuationRefusal::RetryNotParked {
            run_id: run_id.to_owned(),
            restart_count,
        }
        .into());
    }
    Ok(RunContinuation {
        run_id: run_id.to_owned(),
        restart_count: if retry {
            1
        } else {
            restart_count.saturating_add(1)
        },
        retry,
    })
}

fn no_open_run_refusal(
    events: &[EventEnvelope],
    lifecycle: &RunLifecycleProjection,
) -> ContinuationRefusal {
    let latest_start = events
        .iter()
        .rev()
        .find(|event| event.kind.as_str() == EventKind::RUN_STARTED)
        .and_then(|event| event.run.as_deref());
    match latest_start {
        Some(run_id) if !lifecycle.has_run(run_id) => ContinuationRefusal::IncompleteAdmission {
            run_id: run_id.to_owned(),
        },
        _ => ContinuationRefusal::NoOpenRun,
    }
}

/// The first root driver event after the admission that shows the live
/// session had already decided the run's outcome (ADR 0023, Decision 1).
fn decided_outcome(driver_work: &[&EventEnvelope]) -> Option<DecidedRunOutcome> {
    for (index, event) in driver_work.iter().enumerate() {
        let text = |key: &str| event.payload.get(key).and_then(Value::as_str);
        let cancelled = event.payload.get("cancelled").and_then(Value::as_bool) == Some(true);
        let outcome = match event.kind.as_str() {
            EventKind::ERROR | EventKind::TOOL_RESULT if cancelled => {
                Some(DecidedRunOutcome::Cancelled)
            }
            EventKind::ERROR if text("source") == Some("provider") => {
                Some(DecidedRunOutcome::ProviderError)
            }
            // The driver records a session error just before it fails the
            // run. A resume's unknown-outcome model closure uses the same
            // source and leaves the run continuable.
            EventKind::ERROR
                if text("source") == Some("session")
                    && event
                        .payload
                        .get("recovery_closure")
                        .and_then(Value::as_bool)
                        != Some(true) =>
            {
                Some(DecidedRunOutcome::SessionError)
            }
            EventKind::ERROR
                if text("source") == Some("guardian")
                    && text("message") == Some(GUARDIAN_TURN_INTERRUPT_MESSAGE) =>
            {
                Some(DecidedRunOutcome::GuardianInterrupt)
            }
            EventKind::ASSISTANT_MESSAGE if text("content") == Some(TOOL_ROUNDS_LIMIT_MESSAGE) => {
                Some(DecidedRunOutcome::ToolRoundsLimit)
            }
            EventKind::MODEL_RESULT
                if requests_no_tools(event)
                    && !driver_work[index + 1..]
                        .iter()
                        .any(|later| later.kind.as_str() == EventKind::USER_MESSAGE) =>
            {
                Some(DecidedRunOutcome::Completed)
            }
            _ => None,
        };
        if outcome.is_some() {
            return outcome;
        }
    }
    None
}

fn requests_no_tools(model_result: &EventEnvelope) -> bool {
    model_result
        .payload
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
}

/// The provider call id of the first root `tool.call` in `run_id` that has
/// no `tool.result`.
fn unmatched_root_tool_call(
    events: &[EventEnvelope],
    run_id: &str,
    agent_id: &str,
) -> Option<String> {
    events.iter().enumerate().find_map(|(index, event)| {
        let unmatched = event.kind.as_str() == EventKind::TOOL_CALL
            && event.agent == agent_id
            && event.run.as_deref() == Some(run_id)
            && !tool_call_has_result(events, index);
        unmatched.then(|| {
            event
                .payload
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(event.id.as_str())
                .to_owned()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::run_lifecycle::RunLifecycleError;
    use ulid::Ulid;

    fn root_event(kind: &'static str, run_id: &str, payload: serde_json::Value) -> EventEnvelope {
        let serde_json::Value::Object(payload) = payload else {
            panic!("object payload");
        };
        EventEnvelope::new("session", "root", None, kind, payload).with_run(run_id)
    }

    fn chain(mut events: Vec<EventEnvelope>) -> Vec<EventEnvelope> {
        for index in 1..events.len() {
            events[index].parent = Some(events[index - 1].id.clone());
        }
        events
    }

    fn model_call(run_id: &str) -> EventEnvelope {
        root_event(
            EventKind::MODEL_CALL,
            run_id,
            serde_json::json!({"provider": "fixture", "model": "fixture"}),
        )
    }

    fn model_result(run_id: &str, tool_calls: serde_json::Value) -> EventEnvelope {
        root_event(
            EventKind::MODEL_RESULT,
            run_id,
            serde_json::json!({
                "provider": "fixture",
                "model": "fixture",
                "content": "",
                "tool_calls": tool_calls,
            }),
        )
    }

    /// An admitted run with one steering row, cut off while its single
    /// requested tool call ran.
    fn interrupted_run_with_steering() -> (String, String, Vec<EventEnvelope>) {
        let run_id = Ulid::new().to_string();
        let queue_id = Ulid::new().to_string();
        let events = chain(vec![
            root_event(
                EventKind::RUN_STARTED,
                &run_id,
                serde_json::json!({"trigger": "direct"}),
            ),
            root_event(
                EventKind::USER_MESSAGE,
                &run_id,
                serde_json::json!({"content": "start"}),
            ),
            root_event(
                EventKind::QUEUE_ENQUEUED,
                &run_id,
                serde_json::json!({
                    "queue_id": queue_id,
                    "mode": "steering",
                    "position": "back",
                    "content": "steer",
                }),
            ),
            model_call(&run_id),
            model_result(
                &run_id,
                serde_json::json!([{"id": "c1", "name": "read_file"}]),
            ),
            root_event(
                EventKind::TOOL_CALL,
                &run_id,
                serde_json::json!({"id": "c1", "name": "read_file", "input": {}}),
            ),
        ]);
        (run_id, queue_id, events)
    }

    #[test]
    fn continuation_cancels_pending_steering_and_keeps_it_recoverable() {
        let (run_id, queue_id, mut events) = interrupted_run_with_steering();
        let lifecycle = fold_run_lifecycle(&events).expect("lifecycle");

        let (continuation, appended) =
            plan_run_continuation(&events, &lifecycle, false).expect("continuable");

        assert_eq!(continuation.restart_count, 1);
        let kinds = appended
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                EventKind::TOOL_RESULT,
                EventKind::QUEUE_CANCELLED,
                EventKind::RUN_CONTINUED
            ]
        );
        assert_eq!(appended[1].payload["queue_id"], queue_id.as_str());
        assert_eq!(appended[1].payload["reason"], "run_interrupted");
        assert_eq!(appended[1].payload["recovery_closure"], true);
        events.extend(appended);
        let continued = fold_run_lifecycle(&events).expect("continued lifecycle");
        assert!(continued.is_open(&run_id));
        assert_eq!(continued.restart_count(&run_id), 1);
        assert_eq!(continued.recoverable().len(), 1);
        assert_eq!(continued.recoverable()[0].queue_id(), queue_id);

        // The continued run then completes; the steering row cancelled by
        // the continuation does not have to match the completed terminal.
        events.push(model_call(&run_id));
        events.push(model_result(&run_id, serde_json::json!([])));
        events.push(root_event(
            EventKind::RUN_TERMINAL,
            &run_id,
            serde_json::json!({"status": "completed"}),
        ));
        let events = chain(events);
        let finished = fold_run_lifecycle(&events).expect("finished lifecycle");
        assert_eq!(finished.open_runs().count(), 0);
        assert_eq!(
            finished.restart_count(&run_id),
            0,
            "a final answer is progress"
        );
    }

    #[test]
    fn continuation_refuses_a_partial_terminal_batch() {
        let (run_id, queue_id, mut events) = interrupted_run_with_steering();
        events.push(root_event(
            EventKind::TOOL_RESULT,
            &run_id,
            serde_json::json!({"id": "c1", "name": "read_file", "ok": true, "output": "x"}),
        ));
        events.push(root_event(
            EventKind::QUEUE_CANCELLED,
            &run_id,
            serde_json::json!({"queue_id": queue_id, "reason": "run_failed"}),
        ));
        let events = chain(events);
        let lifecycle = fold_run_lifecycle(&events).expect("lifecycle");

        let refused = plan_run_continuation(&events, &lifecycle, false).expect_err("refused");

        assert!(
            matches!(
                refused,
                ResumeError::ContinuationRefused(ContinuationRefusal::PartialTerminalBatch { .. })
            ),
            "{refused}"
        );
    }

    fn settled_run_then(last: Vec<EventEnvelope>) -> (String, Vec<EventEnvelope>) {
        let run_id = Ulid::new().to_string();
        let mut events = vec![
            root_event(
                EventKind::RUN_STARTED,
                &run_id,
                serde_json::json!({"trigger": "direct"}),
            ),
            root_event(
                EventKind::USER_MESSAGE,
                &run_id,
                serde_json::json!({"content": "start"}),
            ),
        ];
        for mut event in last {
            event.run = Some(run_id.clone());
            events.push(event);
        }
        (run_id, chain(events))
    }

    fn outcome_of(events: &[EventEnvelope]) -> Result<RunContinuation, ResumeError> {
        let lifecycle = fold_run_lifecycle(events).expect("lifecycle");
        plan_run_continuation(events, &lifecycle, false).map(|(continuation, _)| continuation)
    }

    #[test]
    fn decided_outcomes_refuse_continuation() {
        let cases = [
            (
                serde_json::json!({"source": "provider", "message": "HTTP 500"}),
                EventKind::ERROR,
                DecidedRunOutcome::ProviderError,
            ),
            (
                serde_json::json!({"source": "session", "message": "context budget exhausted"}),
                EventKind::ERROR,
                DecidedRunOutcome::SessionError,
            ),
            (
                serde_json::json!({"source": "guardian", "message": GUARDIAN_TURN_INTERRUPT_MESSAGE}),
                EventKind::ERROR,
                DecidedRunOutcome::GuardianInterrupt,
            ),
            (
                serde_json::json!({"content": TOOL_ROUNDS_LIMIT_MESSAGE}),
                EventKind::ASSISTANT_MESSAGE,
                DecidedRunOutcome::ToolRoundsLimit,
            ),
        ];
        for (payload, kind, expected) in cases {
            let decided = root_event(kind, "", payload);
            let (_, events) = settled_run_then(vec![model_call(""), decided]);
            match outcome_of(&events) {
                Err(ResumeError::ContinuationRefused(ContinuationRefusal::DecidedOutcome {
                    outcome,
                    ..
                })) => assert_eq!(outcome, expected),
                other => panic!(
                    "expected {expected:?}, got {:?}",
                    other.map_err(|e| e.to_string())
                ),
            }
        }
    }

    #[test]
    fn a_session_error_after_a_settled_tool_batch_refuses_continuation() {
        // `prepare_model_request` records the error before any `model.call`,
        // so the run would otherwise read as a settled batch in flight.
        let (_, events) = settled_run_then(vec![
            model_call(""),
            model_result("", serde_json::json!([{"id": "c1", "name": "read_file"}])),
            root_event(
                EventKind::TOOL_CALL,
                "",
                serde_json::json!({"id": "c1", "name": "read_file", "input": {}}),
            ),
            root_event(
                EventKind::TOOL_RESULT,
                "",
                serde_json::json!({"id": "c1", "name": "read_file", "ok": true, "output": "x"}),
            ),
            root_event(
                EventKind::ERROR,
                "",
                serde_json::json!({"source": "session", "message": "context budget exhausted"}),
            ),
        ]);

        let refused = outcome_of(&events).expect_err("refused");

        assert!(
            matches!(
                refused,
                ResumeError::ContinuationRefused(ContinuationRefusal::DecidedOutcome {
                    outcome: DecidedRunOutcome::SessionError,
                    ..
                })
            ),
            "{refused}"
        );
    }

    #[test]
    fn an_older_unmatched_tool_call_that_resume_cannot_close_refuses_continuation() {
        // Resume closes only the tail call outside an interrupted batch, so
        // `c1` stays unmatched after the closures and nothing is appended.
        let (run_id, events) = settled_run_then(vec![
            model_call(""),
            model_result("", serde_json::json!([{"id": "c1", "name": "read_file"}])),
            root_event(
                EventKind::TOOL_CALL,
                "",
                serde_json::json!({"id": "c1", "name": "read_file", "input": {}}),
            ),
            model_call(""),
            model_result("", serde_json::json!([{"id": "c2", "name": "read_file"}])),
            root_event(
                EventKind::TOOL_CALL,
                "",
                serde_json::json!({"id": "c2", "name": "read_file", "input": {}}),
            ),
            root_event(
                EventKind::TOOL_RESULT,
                "",
                serde_json::json!({"id": "c2", "name": "read_file", "ok": true, "output": "x"}),
            ),
        ]);
        let lifecycle = fold_run_lifecycle(&events).expect("lifecycle");

        let refused = plan_run_continuation(&events, &lifecycle, false).expect_err("refused");

        let ResumeError::ContinuationRefused(refusal) = refused else {
            panic!("expected a continuation refusal, got {refused}");
        };
        assert_eq!(
            refusal,
            ContinuationRefusal::UnclosedToolCall {
                run_id,
                call_id: "c1".to_owned(),
            }
        );
    }

    #[test]
    fn a_final_answer_followed_by_steering_is_not_a_decided_outcome() {
        let (_, events) = settled_run_then(vec![
            model_call(""),
            model_result("", serde_json::json!([])),
            root_event(
                EventKind::USER_MESSAGE,
                "",
                serde_json::json!({"content": "steer"}),
            ),
        ]);
        // Only the decided-outcome rule is under test; a real steering
        // message arrives in a `queue.delivered` admission.
        let driver_work = events[2..].iter().collect::<Vec<_>>();
        assert_eq!(decided_outcome(&driver_work), None);
        assert_eq!(
            decided_outcome(&driver_work[..2]),
            Some(DecidedRunOutcome::Completed)
        );
    }

    #[test]
    fn a_model_result_whose_tool_calls_were_never_recorded_is_not_in_flight() {
        let (_, events) = settled_run_then(vec![
            model_call(""),
            model_result("", serde_json::json!([{"id": "c1", "name": "read_file"}])),
        ]);

        let refused = outcome_of(&events).expect_err("refused");

        assert!(
            matches!(
                refused,
                ResumeError::ContinuationRefused(ContinuationRefusal::NotInFlight { .. })
            ),
            "{refused}"
        );
    }

    #[test]
    fn fold_rejects_a_continuation_of_a_closed_run_and_a_false_retry() {
        let (run_id, mut events) = settled_run_then(vec![]);
        events.push(root_event(
            EventKind::RUN_CONTINUED,
            &run_id,
            serde_json::json!({"retry": false}),
        ));
        let events = chain(events);
        assert!(matches!(
            fold_run_lifecycle(&events),
            Err(RunLifecycleError::InvalidContinuationRetry { .. })
        ));

        let (run_id, mut events) = settled_run_then(vec![]);
        events.push(root_event(
            EventKind::RUN_TERMINAL,
            &run_id,
            serde_json::json!({"status": "interrupted"}),
        ));
        events.push(root_event(
            EventKind::RUN_CONTINUED,
            &run_id,
            serde_json::json!({}),
        ));
        let events = chain(events);
        assert!(matches!(
            fold_run_lifecycle(&events),
            Err(RunLifecycleError::ContinuationWithoutOpenRun { .. })
        ));
    }
}
