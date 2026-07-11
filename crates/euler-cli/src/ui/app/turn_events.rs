use super::*;

impl AppCore {
    pub(super) fn check_stall_notification(&mut self) {
        if !self.turn_in_flight() || self.stall_notified {
            return;
        }
        let Some(last) = self.last_turn_activity_at else {
            return;
        };
        if last.elapsed() < STALL_THRESHOLD {
            return;
        }
        self.stall_notified = true;
        self.queue_notification(NotifyEvent::Stall);
    }

    fn push_turn_recap(&mut self) {
        let ctx = super::turn_recap::ctx_percent(
            self.token_usage.input_tokens,
            self.token_usage.context_window_tokens,
        );
        let recap = super::turn_recap::turn_recap_from_events(
            self.transcript.events(),
            self.turn_event_start,
            ctx,
        );
        self.push_finalized_visual_item(TranscriptItem::TurnRecap {
            summary: recap.summary_line(),
            files: recap.files_line(),
        });
    }

    pub(super) fn drain_turn_events(&mut self) -> bool {
        let mut changed = false;
        while let Some(event) = self.next_turn_event() {
            self.handle_turn_event(event);
            changed = true;
        }
        changed
    }

    pub(super) fn next_turn_event(&mut self) -> Option<TurnEvent> {
        let AppState::TurnInFlight { worker_rx, .. } = &mut self.state else {
            return None;
        };
        worker_rx.try_recv().ok()
    }

    pub(super) fn handle_turn_event(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::Event(event) => {
                let is_tool_call = event.kind.as_str() == EventKind::TOOL_CALL;
                self.note_turn_activity();
                self.record_in_flight_error(&event);
                self.update_token_usage_from_event(&event);
                self.update_phase_verb(&event);
                self.update_reasoning_stream_tail(&event);
                self.transcript.push_event(event);
                self.queue_finalized_visual_output_for_latest_event();
                if is_tool_call {
                    self.refresh_patch_modal_preview();
                }
            }
            TurnEvent::TurnDone { outcome, session } => {
                let elapsed = self.working_elapsed();
                let auto_flush = outcome == TurnOutcome::Complete;
                self.last_working_elapsed_secs = None;
                self.handle_turn_outcome(outcome, elapsed);
                self.status.git_branch = detect_git_branch(&self.status.cwd);
                self.accept_worker_session_or_continue(session, auto_flush);
            }
            TurnEvent::ExtensionDone {
                request,
                outcome,
                events,
                session,
            } => {
                let elapsed = self.working_elapsed();
                for event in events {
                    self.update_token_usage_from_event(&event);
                    self.transcript.push_event(event);
                    self.queue_finalized_visual_output_for_latest_event();
                }
                self.last_working_elapsed_secs = None;
                let auto_flush = matches!(&outcome, ExtensionOutcome::Complete(_));
                self.handle_extension_outcome(&request, outcome, elapsed);
                self.accept_worker_session_or_continue(session, auto_flush);
            }
            TurnEvent::CompanionDone {
                request,
                outcome,
                events,
                session,
            } => {
                let elapsed = self.working_elapsed();
                for event in events {
                    self.update_token_usage_from_event(&event);
                    self.transcript.push_event(event);
                    self.queue_finalized_visual_output_for_latest_event();
                }
                self.last_working_elapsed_secs = None;
                let auto_flush = matches!(&outcome, CompanionOutcome::Complete(_));
                self.handle_companion_outcome(&request, outcome, elapsed);
                self.accept_worker_session_or_continue(session, auto_flush);
            }
        }
    }

    fn update_token_usage_from_event(&mut self, event: &EventEnvelope) {
        let context_window_tokens = self.active_context_window_tokens();
        update_token_usage(&mut self.token_usage, event, context_window_tokens);
    }

    /// Working HUD phase verb (issue #27): thinking / exploring / reading X /
    /// writing X / running bash / running tests, falling back to "working"
    /// only when nothing more specific applies. Only reasoning and tool-call
    /// events carry a new phase — other event kinds (streamed text deltas,
    /// tool results) leave the verb in place so it swaps only when the phase
    /// actually changes, not on every event.
    fn update_phase_verb(&mut self, event: &EventEnvelope) {
        match event.kind.as_str() {
            EventKind::MODEL_REASONING => {
                self.current_phase_verb = Some("thinking".to_owned());
            }
            EventKind::TOOL_CALL => {
                self.current_phase_verb = Some(phase_verb_for_tool_call(event));
            }
            _ => {}
        }
    }

    /// #47: accumulate a bounded tail of the reasoning text streaming under
    /// the live thinking line. `MODEL_DELTA{kind: "reasoning"}` deltas append;
    /// a `MODEL_DELTA{kind: "text"}` (answer text has started) or a
    /// finalized `MODEL_REASONING` item clears it — the same moments the
    /// live thinking line itself clears — so a late reasoning delta can
    /// never reopen it once the answer has begun streaming.
    fn update_reasoning_stream_tail(&mut self, event: &EventEnvelope) {
        match event.kind.as_str() {
            EventKind::MODEL_DELTA => {
                let Some(kind) = event
                    .payload
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                else {
                    return;
                };
                match kind {
                    "reasoning" => {
                        if self.reasoning_tail_locked {
                            return;
                        }
                        if let Some(delta) = event
                            .payload
                            .get("delta")
                            .and_then(serde_json::Value::as_str)
                        {
                            self.reasoning_stream_tail
                                .push_str(&delta.replace('\n', " "));
                            truncate_reasoning_tail_left(
                                &mut self.reasoning_stream_tail,
                                REASONING_STREAM_TAIL_MAX_CHARS,
                            );
                        }
                    }
                    "text" => {
                        self.reasoning_stream_tail.clear();
                        self.reasoning_tail_locked = true;
                    }
                    _ => {}
                }
            }
            EventKind::MODEL_REASONING => self.reasoning_stream_tail.clear(),
            _ => {}
        }
    }

    fn accept_worker_session_or_continue(
        &mut self,
        session: Box<Session<TuiDecider>>,
        auto_flush: bool,
    ) {
        if self.active_session_home_managed {
            let session_id = session.session_id().to_owned();
            if let Err(error) = self.refresh_current_session_metadata(&session_id) {
                self.notice = Some(format!("session metadata refresh failed: {error}"));
            }
        }
        if let Some(request) = self.pending_runs.pop_front() {
            match request {
                PendingRunRequest::Extension(request) => self.spawn_extension_run(request, session),
                PendingRunRequest::Companion(request) => self.spawn_companion_run(request, session),
            }
            return;
        }
        if auto_flush && !self.queue_auto_flush_paused {
            if let Some(prompt) = self.pop_next_queued_input() {
                self.bottom.record_submission(&prompt);
                self.spawn_turn(prompt, session);
                return;
            }
        }
        self.state = AppState::Idle { session };
        // The session is back on this thread: refresh the last-known
        // authenticated-provider snapshot used by bottom-surface rebuilds
        // that happen while a turn is in flight.
        self.refresh_authenticated_providers();
        self.in_flight_label = None;
        self.in_flight_companion_name = None;
        self.in_flight_cancellable = false;
        self.current_phase_verb = None;
        self.reasoning_stream_tail.clear();
        self.reasoning_tail_locked = false;
        self.spinner_frame = 0;
        self.spinner_last_tick = None;
    }

    fn handle_extension_outcome(
        &mut self,
        request: &ExtensionRunRequest,
        outcome: ExtensionOutcome,
        elapsed: Option<Duration>,
    ) {
        if let Some(duration) = elapsed.filter(|duration| *duration >= MIN_WORKED_DURATION) {
            self.push_finalized_visual_item(TranscriptItem::WorkedDuration(format_live_elapsed(
                duration,
            )));
        }
        match outcome {
            ExtensionOutcome::Complete(output) => {
                // Foldable artifact row with pretty JSON, not a one-line dump
                // (calibration finding E4).
                let rendered =
                    serde_json::to_string_pretty(&output).unwrap_or_else(|_| "null".to_owned());
                self.push_finalized_visual_item(TranscriptItem::ExtensionResult {
                    reference: format!("{}.{}", request.id, request.command),
                    ok: true,
                    output: rendered,
                });
                self.notice = Some(format!(
                    "extension {}.{} complete",
                    request.id, request.command
                ));
                if request.id == "code-swarm" && request.command == "review" {
                    let _ = self.summary_item(code_swarm_summary_line(&output));
                }
            }
            ExtensionOutcome::Failed(message) => {
                self.push_finalized_visual_item(TranscriptItem::Error {
                    source: format!("extension {}.{}", request.id, request.command),
                    message: message.clone(),
                });
                self.notice = Some(format!(
                    "extension {}.{} failed: {message}",
                    request.id, request.command
                ));
            }
        }
    }

    fn handle_companion_outcome(
        &mut self,
        _request: &CompanionRunRequest,
        outcome: CompanionOutcome,
        elapsed: Option<Duration>,
    ) {
        if let Some(duration) = elapsed.filter(|duration| *duration >= MIN_WORKED_DURATION) {
            self.push_finalized_visual_item(TranscriptItem::WorkedDuration(format_live_elapsed(
                duration,
            )));
        }
        match outcome {
            CompanionOutcome::Complete(result) => {
                self.push_finalized_visual_item(TranscriptItem::SessionSummary(format!(
                    "companion run result: {}",
                    serde_json::to_string(&crate::companion_run::agent_result_json(&result))
                        .unwrap_or_else(|_| "null".to_owned())
                )));
                self.notice = Some("companion run complete".to_owned());
            }
            CompanionOutcome::Failed(message) => {
                self.push_finalized_visual_item(TranscriptItem::Error {
                    source: "companion run".to_owned(),
                    message: message.clone(),
                });
                self.notice = Some(format!("companion run failed: {message}"));
            }
        }
    }

    fn refresh_patch_modal_preview(&mut self) {
        if !matches!(
            self.modal,
            Some(Modal::PatchApproval(PatchApprovalModal {
                preview: PatchPreview::Fallback(_),
                ..
            }))
        ) {
            return;
        }
        let preview = patch_approval::preview_from_events(self.transcript.events());
        if let Some(Modal::PatchApproval(modal)) = &mut self.modal {
            modal.preview = preview;
        }
    }

    fn record_in_flight_error(&mut self, event: &EventEnvelope) {
        if !self.turn_in_flight() || event.kind.as_str() != EventKind::ERROR {
            return;
        }
        let source = event
            .payload
            .get("source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("error");
        let message = event
            .payload
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("turn failed");
        self.in_flight_error = Some(format!("{source}: {message}"));
        self.interrupted_guidance = false;
    }

    pub(super) fn handle_turn_outcome(&mut self, outcome: TurnOutcome, elapsed: Option<Duration>) {
        let emit_recap = match &outcome {
            TurnOutcome::Complete => {
                self.interrupted_guidance = false;
                self.in_flight_error = None;
                self.notice = None;
                true
            }
            TurnOutcome::Cancelled => {
                self.queue_auto_flush_paused = true;
                self.transcript.clear_transient_live_tail();
                self.interrupted_guidance = false;
                self.in_flight_error = None;
                self.push_finalized_visual_item(TranscriptItem::Interrupted);
                self.notice = None;
                false
            }
            TurnOutcome::Failed(message) => {
                self.queue_auto_flush_paused = true;
                self.interrupted_guidance = false;
                self.in_flight_error = None;
                self.transcript.clear_transient_live_tail();
                if !self.last_event_is_error() {
                    self.push_finalized_visual_item(TranscriptItem::Error {
                        source: "run_turn".to_owned(),
                        message: message.clone(),
                    });
                }
                self.notice = None;
                true
            }
        };
        if let Some(elapsed) = elapsed.filter(|elapsed| *elapsed >= MIN_WORKED_DURATION) {
            self.push_finalized_visual_item(TranscriptItem::WorkedDuration(format_live_elapsed(
                elapsed,
            )));
        }
        if emit_recap {
            self.push_turn_recap();
        }
        match outcome {
            TurnOutcome::Complete => self.queue_notification(NotifyEvent::TurnDone),
            TurnOutcome::Failed(_) => self.queue_notification(NotifyEvent::Failure),
            TurnOutcome::Cancelled => {}
        }
        self.last_turn_activity_at = None;
        self.stall_notified = false;
    }

    fn last_event_is_error(&self) -> bool {
        self.transcript
            .events()
            .last()
            .is_some_and(|event| event.kind.as_str() == EventKind::ERROR)
    }
}

/// Phase verb for a `tool.call` event, matching the tool taxonomy the
/// transcript projector already uses (`tool_projection_from_call` /
/// `exploration_summary_from_call` in transcript.rs): `run_shell` -> running
/// bash (or running tests, judged from the command text — there is no
/// dedicated "test" tool), `edit_file`/`apply_patch`/`write_file` -> writing
/// X, `read_file` -> reading X, everything else exploration-shaped
/// (`git_status`, `git_diff`, `list_files`, `search`) -> exploring.
fn phase_verb_for_tool_call(event: &EventEnvelope) -> String {
    let name = event
        .payload
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let input = event.payload.get("input");
    match name {
        "run_shell" => {
            let command = input
                .and_then(|input| input.get("command"))
                .and_then(serde_json::Value::as_str)
                .map(super::transcript::normalized_shell_command)
                .unwrap_or_default();
            if is_test_runner_command(&command) {
                "running tests".to_owned()
            } else {
                "running bash".to_owned()
            }
        }
        "read_file" => tool_call_path(input)
            .map(|path| format!("reading {path}"))
            .unwrap_or_else(|| "reading".to_owned()),
        "edit_file" | "apply_patch" | "apply-patch" | "write_file" => tool_call_path(input)
            .map(|path| format!("writing {path}"))
            .unwrap_or_else(|| "writing".to_owned()),
        "git_status" | "git_diff" | "list_files" | "search" | "tool_result_get" => {
            "exploring".to_owned()
        }
        _ => "working".to_owned(),
    }
}

fn tool_call_path(input: Option<&serde_json::Value>) -> Option<&str> {
    input
        .and_then(|input| input.get("path"))
        .and_then(serde_json::Value::as_str)
}

/// Judged from the command text — there is no dedicated "test" tool, so a
/// `run_shell` call reads as "running tests" when it plainly looks like one
/// (deliberate heuristic, not exhaustive: matches common test-runner
/// invocations from CLAUDE.md's own convention — `cargo nextest run` — plus
/// other ecosystems' idiomatic commands).
fn is_test_runner_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower
        .split_whitespace()
        .any(|token| token == "test" || token == "tests")
        || ["nextest", "pytest", "jest", "vitest"]
            .iter()
            .any(|needle| lower.contains(needle))
}

/// #58: the completion line must read per-reviewer `ok` flags from the
/// output JSON, not just `reviewer_count` — a line reading "complete" while
/// every reviewer failed is a dishonest summary. `reviewers[].ok` is what the
/// extension actually records per spawn outcome; `reviewer_count` alone
/// cannot distinguish "3 reviewers, 3 ok" from "3 reviewers, 0 ok".
fn code_swarm_summary_line(output: &serde_json::Value) -> String {
    let path = output["relative_path"].as_str().unwrap_or("(unknown path)");
    let reviewers = output["reviewers"].as_array();
    let total = reviewers.map_or(0, Vec::len);
    let ok_count = reviewers
        .map(|reviewers| {
            reviewers
                .iter()
                .filter(|reviewer| reviewer["ok"].as_bool().unwrap_or(false))
                .count()
        })
        .unwrap_or(0);
    if total > 0 && ok_count == total {
        format!("✓ code-swarm review complete · {total} reviewers · artifact {path}")
    } else {
        format!("✗ code-swarm review · {ok_count}/{total} reviewers succeeded · artifact {path}")
    }
}

/// #47: bounded tail length for the live streaming-reasoning preview under
/// the thinking HUD line — enough for a couple of wrapped lines at typical
/// composer widths, never the full reasoning transcript.
pub(super) const REASONING_STREAM_TAIL_MAX_CHARS: usize = 400;

/// Keeps only the trailing `max_chars` characters of `tail`, dropping from
/// the front. Operates on `char`s (never a byte index), so it can never
/// split a multibyte glyph.
fn truncate_reasoning_tail_left(tail: &mut String, max_chars: usize) {
    let overflow = tail.chars().count().saturating_sub(max_chars);
    if overflow == 0 {
        return;
    }
    let byte_offset = tail
        .char_indices()
        .nth(overflow)
        .map_or(tail.len(), |(index, _)| index);
    tail.drain(..byte_offset);
}

#[cfg(test)]
mod code_swarm_summary_tests {
    use super::code_swarm_summary_line;
    use serde_json::json;

    #[test]
    fn all_reviewers_ok_reports_success() {
        let output = json!({
            "relative_path": "artifacts/review.json",
            "reviewer_count": 3,
            "reviewers": [
                {"ok": true}, {"ok": true}, {"ok": true},
            ],
        });

        assert_eq!(
            code_swarm_summary_line(&output),
            "✓ code-swarm review complete · 3 reviewers · artifact artifacts/review.json"
        );
    }

    #[test]
    fn any_failure_reports_honest_partial_count() {
        let output = json!({
            "relative_path": "artifacts/review.json",
            "reviewer_count": 3,
            "reviewers": [
                {"ok": true}, {"ok": false}, {"ok": false},
            ],
        });

        assert_eq!(
            code_swarm_summary_line(&output),
            "✗ code-swarm review · 1/3 reviewers succeeded · artifact artifacts/review.json"
        );
    }

    #[test]
    fn all_failed_reports_zero_of_total_not_dishonest_complete() {
        let output = json!({
            "relative_path": "artifacts/review.json",
            "reviewer_count": 3,
            "reviewers": [
                {"ok": false}, {"ok": false}, {"ok": false},
            ],
        });

        assert_eq!(
            code_swarm_summary_line(&output),
            "✗ code-swarm review · 0/3 reviewers succeeded · artifact artifacts/review.json"
        );
    }

    #[test]
    fn missing_reviewers_array_reports_zero_of_zero() {
        let output = json!({"relative_path": "artifacts/review.json"});

        assert_eq!(
            code_swarm_summary_line(&output),
            "✗ code-swarm review · 0/0 reviewers succeeded · artifact artifacts/review.json"
        );
    }
}
