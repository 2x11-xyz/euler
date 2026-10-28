//! ADR 0023 rollout step 3: run continuation with parking. Every
//! interrupted log here is a real session log cut at a line boundary, which
//! is exactly the accepted prefix a crash leaves behind.

use euler_core::permissions::{DeciderVerdict, ScriptedDecider};
use euler_core::{
    check_run_continuation, fold_session, read_resume_prefix, resume_session,
    resume_session_continuing_run, ContinuationRefusal, DecidedRunOutcome, ProvenanceWriter,
    ResumeError, ResumeOutcome, Session, SessionConfig, RUN_RESTART_LIMIT,
};
use euler_event::{EventEnvelope, EventKind};
use euler_provider::{FixtureResponse, ProviderSet, ScriptedProvider, ToolCall};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn read_note() -> FixtureResponse {
    FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-read".to_owned(),
        name: "read_file".to_owned(),
        input: json!({ "path": "note.txt" }),
    }])
}

fn answer(text: &str) -> FixtureResponse {
    FixtureResponse::Assistant(text.to_owned())
}

fn providers(responses: Vec<FixtureResponse>) -> ProviderSet {
    ProviderSet::single(ScriptedProvider::new(responses))
}

/// Run one live turn that reads `note.txt` and answers, then cut the log
/// after its `tool.call`: the crash happened while the tool ran.
fn crashed_during_tool(root: &Path) -> PathBuf {
    fs::write(root.join("note.txt"), "alpha\n").expect("write note");
    let log = root.join("events.jsonl");
    let mut session = Session::new_with_providers(
        SessionConfig::new(root),
        providers(vec![read_note(), answer("done")]),
        ScriptedDecider::new(vec![]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("writer"));
    session.run_turn("read the note").expect("live turn");
    drop(session);
    cut_after_last(&log, EventKind::TOOL_CALL);
    log
}

/// Keep the log through the last event of `kind`.
fn cut_after_last(log: &Path, kind: &str) {
    let content = fs::read_to_string(log).expect("read log");
    let lines = content.lines().collect::<Vec<_>>();
    let keep = lines
        .iter()
        .rposition(|line| {
            serde_json::from_str::<Value>(line).expect("json line")["kind"].as_str() == Some(kind)
        })
        .expect("event kind present");
    fs::write(log, format!("{}\n", lines[..=keep].join("\n"))).expect("cut log");
}

fn events(log: &Path) -> Vec<EventEnvelope> {
    read_resume_prefix(log).expect("read prefix")
}

fn of_kind<'a>(events: &'a [EventEnvelope], kind: &str) -> Vec<&'a EventEnvelope> {
    events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .collect()
}

fn resume_continuing(
    root: &Path,
    log: &Path,
    responses: Vec<FixtureResponse>,
    retry: bool,
) -> Result<ResumeOutcome<ScriptedDecider>, ResumeError> {
    let config = SessionConfig::new(root);
    let writer = ProvenanceWriter::new(log.to_path_buf()).expect("writer");
    let folded = fold_session(&config, events(log)).expect("fold");
    resume_session_continuing_run(
        config,
        providers(responses),
        ScriptedDecider::new(vec![]),
        writer,
        folded,
        retry,
    )
}

/// Continue and drive the run to its end with `responses`.
fn continue_and_drive(root: &Path, log: &Path, responses: Vec<FixtureResponse>, retry: bool) {
    let mut outcome = resume_continuing(root, log, responses, retry).expect("continue");
    outcome
        .session
        .continue_run_with_sink(Arc::new(AtomicBool::new(false)), |_| {})
        .expect("continued run");
}

fn refusal(result: Result<ResumeOutcome<ScriptedDecider>, ResumeError>) -> ContinuationRefusal {
    match result {
        Err(ResumeError::ContinuationRefused(refusal)) => refusal,
        Err(other) => panic!("expected a continuation refusal, got {other}"),
        Ok(_) => panic!("expected a continuation refusal, got a continued session"),
    }
}

fn assert_refused_without_append(root: &Path, log: &Path, retry: bool) -> ContinuationRefusal {
    let before = fs::read(log).expect("read log");
    let refused = refusal(resume_continuing(root, log, vec![], retry));
    assert_eq!(
        fs::read(log).expect("read log"),
        before,
        "refusal appends nothing"
    );
    refused
}

#[test]
fn continuation_closes_the_cut_off_call_and_finishes_the_same_run() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = crashed_during_tool(temp.path());
    let run_id = of_kind(&events(&log), EventKind::RUN_STARTED)[0]
        .run
        .clone()
        .expect("run id");

    let mut outcome =
        resume_continuing(temp.path(), &log, vec![answer("resumed")], false).expect("continue");
    let continued = outcome.continued_run.clone().expect("continued run");
    assert_eq!(continued.run_id, run_id);
    assert_eq!(continued.restart_count, 1);
    outcome
        .session
        .continue_run_with_sink(Arc::new(AtomicBool::new(false)), |_| {})
        .expect("continued run");
    drop(outcome);

    let logged = events(&log);
    let closure = of_kind(&logged, EventKind::TOOL_RESULT)[0];
    assert_eq!(closure.payload["recovery_closure"], true);
    let marker = of_kind(&logged, EventKind::RUN_CONTINUED);
    assert_eq!(marker.len(), 1);
    assert_eq!(marker[0].run.as_deref(), Some(run_id.as_str()));
    assert_eq!(marker[0].parent.as_deref(), Some(closure.id.as_str()));
    assert!(marker[0].payload.get("retry").is_none());
    assert_eq!(of_kind(&logged, EventKind::RUN_STARTED).len(), 1);
    assert!(of_kind(&logged, EventKind::USER_MESSAGE).len() == 1);
    let terminals = of_kind(&logged, EventKind::RUN_TERMINAL);
    assert_eq!(terminals.len(), 1, "the run still ends exactly once");
    assert_eq!(terminals[0].run.as_deref(), Some(run_id.as_str()));
    assert_eq!(terminals[0].payload["status"], "completed");
    let result = of_kind(&logged, EventKind::MODEL_RESULT);
    let resumed = result.last().expect("resumed model result");
    assert_eq!(resumed.payload["content"], "resumed");
    assert_eq!(resumed.run.as_deref(), Some(run_id.as_str()));

    // The finished run has nothing left to continue, and a plain resume of
    // the continued log appends nothing.
    let refused = assert_refused_without_append(temp.path(), &log, false);
    assert_eq!(refused, ContinuationRefusal::NoOpenRun);
    let lines = fs::read(&log).expect("read log");
    drop(
        resume_session(
            SessionConfig::new(temp.path()),
            providers(vec![]),
            ScriptedDecider::new(vec![]),
            &log,
        )
        .expect("plain resume"),
    );
    assert_eq!(fs::read(&log).expect("read log"), lines);
}

#[test]
fn continuation_refuses_a_crash_partial_admission() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = crashed_during_tool(temp.path());
    cut_after_last(&log, EventKind::RUN_STARTED);

    let refused = assert_refused_without_append(temp.path(), &log, false);
    assert!(
        matches!(refused, ContinuationRefusal::IncompleteAdmission { .. }),
        "{refused:?}"
    );
}

#[test]
fn continuation_refuses_a_completed_run_whose_terminal_was_not_persisted() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = crashed_during_tool(temp.path());
    continue_and_drive(temp.path(), &log, vec![answer("done")], false);
    // The run decided `completed` in memory; cut before its terminal.
    cut_after_last(&log, EventKind::MODEL_RESULT);

    let refused = assert_refused_without_append(temp.path(), &log, false);
    assert!(
        matches!(
            refused,
            ContinuationRefusal::DecidedOutcome {
                outcome: DecidedRunOutcome::Completed,
                ..
            }
        ),
        "{refused:?}"
    );
}

#[test]
fn continuation_refuses_a_cancelled_run() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\n").expect("write note");
    let log = temp.path().join("events.jsonl");
    let cancel = Arc::new(AtomicBool::new(false));
    let mut session = Session::new_with_providers(
        SessionConfig::new(temp.path()),
        providers(vec![answer("never")]),
        ScriptedDecider::new(vec![]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("writer"));
    // Cancel as soon as the model call starts, as Escape would.
    let flag = Arc::clone(&cancel);
    let _ = session.run_turn_with_sink("stop at once", cancel, |event| {
        if event.kind.as_str() == EventKind::MODEL_CALL {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });
    drop(session);
    let logged = events(&log);
    let cancelled = logged
        .iter()
        .rposition(|event| event.payload.get("cancelled") == Some(&Value::Bool(true)))
        .expect("cancelled event");
    let kind = logged[cancelled].kind.to_string();
    cut_after_last(&log, &kind);

    let refused = assert_refused_without_append(temp.path(), &log, false);
    assert!(
        matches!(
            refused,
            ContinuationRefusal::DecidedOutcome {
                outcome: DecidedRunOutcome::Cancelled,
                ..
            }
        ),
        "{refused:?}"
    );
}

/// A tool that is unsafe to rerun and crashes every time: continue, close
/// the call, the model requests it again, crash. The run parks after five
/// continuations, and the sixth is refused without appending anything.
#[test]
fn a_call_that_crashes_on_every_continuation_parks_the_run() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = crashed_during_tool(temp.path());
    for expected in 1..=RUN_RESTART_LIMIT {
        let mut outcome =
            resume_continuing(temp.path(), &log, vec![read_note(), answer("done")], false)
                .expect("continue");
        assert_eq!(
            outcome.continued_run.as_ref().map(|run| run.restart_count),
            Some(expected)
        );
        outcome
            .session
            .continue_run_with_sink(Arc::new(AtomicBool::new(false)), |_| {})
            .expect("continued run");
        drop(outcome);
        cut_after_last(&log, EventKind::TOOL_CALL);
    }

    let refused = assert_refused_without_append(temp.path(), &log, false);
    assert!(
        matches!(
            refused,
            ContinuationRefusal::Parked {
                restart_count: RUN_RESTART_LIMIT,
                ..
            }
        ),
        "{refused:?}"
    );
    assert!(refused.to_string().contains("restart_limit"));
    assert!(refused.to_string().contains('5'));

    // `--retry` continues a parked run and starts the count again at one.
    let mut outcome =
        resume_continuing(temp.path(), &log, vec![answer("done")], true).expect("retry parked run");
    assert_eq!(
        outcome.continued_run.as_ref().map(|run| run.restart_count),
        Some(1)
    );
    outcome
        .session
        .continue_run_with_sink(Arc::new(AtomicBool::new(false)), |_| {})
        .expect("retried run");
    drop(outcome);
    let logged = events(&log);
    let retried = of_kind(&logged, EventKind::RUN_CONTINUED);
    assert_eq!(retried.last().expect("retry").payload["retry"], true);
    assert_eq!(
        of_kind(&logged, EventKind::RUN_TERMINAL)[0].payload["status"],
        "completed"
    );
}

/// A crash during every continued model call also parks the run: the
/// unknown-outcome closure of a cut-off model call is not progress.
#[test]
fn a_model_call_that_crashes_on_every_continuation_parks_the_run() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = crashed_during_tool(temp.path());
    for expected in 1..=RUN_RESTART_LIMIT {
        let mut outcome =
            resume_continuing(temp.path(), &log, vec![answer("done")], false).expect("continue");
        assert_eq!(
            outcome.continued_run.as_ref().map(|run| run.restart_count),
            Some(expected)
        );
        outcome
            .session
            .continue_run_with_sink(Arc::new(AtomicBool::new(false)), |_| {})
            .expect("continued run");
        drop(outcome);
        cut_after_last(&log, EventKind::MODEL_CALL);
    }

    let refused = assert_refused_without_append(temp.path(), &log, false);
    assert!(
        matches!(
            refused,
            ContinuationRefusal::Parked {
                restart_count: RUN_RESTART_LIMIT,
                ..
            }
        ),
        "{refused:?}"
    );
}

#[test]
fn retry_is_refused_for_a_run_that_is_not_parked() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = crashed_during_tool(temp.path());

    let refused = assert_refused_without_append(temp.path(), &log, true);
    assert_eq!(
        refused,
        ContinuationRefusal::RetryNotParked {
            run_id: of_kind(&events(&log), EventKind::RUN_STARTED)[0]
                .run
                .clone()
                .expect("run id"),
            restart_count: 0,
        }
    );
}

/// A process that dies right after recording `run.continued` leaves a run
/// whose next round never started; that still counts as a restart.
#[test]
fn a_continuation_after_progress_starts_the_count_at_one() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = crashed_during_tool(temp.path());
    for expected in 1..=3 {
        let outcome = resume_continuing(temp.path(), &log, vec![], false).expect("continue");
        assert_eq!(
            outcome.continued_run.as_ref().map(|run| run.restart_count),
            Some(expected)
        );
    }
    // The fourth continuation reads the note for real (progress), then the
    // process dies during the next requested call.
    continue_and_drive(
        temp.path(),
        &log,
        vec![read_note(), read_note(), answer("done")],
        false,
    );
    cut_after_last(&log, EventKind::TOOL_CALL);
    assert!(
        events(&log)
            .iter()
            .any(|event| event.kind.as_str() == EventKind::TOOL_RESULT
                && event.payload.get("recovery_closure").is_none()),
        "a real tool result was recorded"
    );

    let continuation = check_run_continuation(&events(&log), false).expect("continuable");
    assert_eq!(continuation.restart_count, 1);
}

#[test]
fn continuation_without_any_run_is_refused() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let mut session = Session::new_with_providers(
        SessionConfig::new(temp.path()),
        providers(vec![answer("done")]),
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("writer"));
    session.run_turn("hello").expect("turn");
    drop(session);

    let refused = assert_refused_without_append(temp.path(), &log, false);
    assert_eq!(refused, ContinuationRefusal::NoOpenRun);
}
