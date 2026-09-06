use super::*;

impl AppCore {
    pub(super) fn check_stall_notification(&mut self) {
        if !self.turn_in_flight()
            || self.in_flight_label.as_deref() != Some(MODEL_TURN_IN_FLIGHT_LABEL)
            || self.stall_notified
        {
            return;
        }
        if !self.activity.is_stalled_at(Utc::now()) {
            return;
        }
        self.stall_notified = true;
        self.queue_notification(NotifyEvent::Stall);
    }

    /// Recap suppression (owner preference, 2026-07-16, superseding review
    /// v3 §R5(b)): a turn that changed **no files** renders no recap line at
    /// all — regardless of tests run or context moved. The divider still
    /// renders via the caller on its own timing rule.
    fn push_turn_recap(&mut self) {
        let recap = super::turn_recap::turn_recap_from_events(
            self.transcript.events(),
            self.turn_event_start,
        );
        // The recap is for what the turn changed to the workspace: a turn that
        // touched no files earns no recap line at all (owner preference,
        // 2026-07-16), regardless of tests run or context moved. The
        // `── Worked for Ns ──` divider is a separate element and still
        // renders on its own timing rule (see `handle_turn_outcome`).
        if recap.file_count == 0 {
            return;
        }
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
        if matches!(
            event,
            TurnEvent::TurnDone { .. }
                | TurnEvent::ExtensionDone { .. }
                | TurnEvent::CompanionDone { .. }
        ) {
            // The worker's terminal event carries the live session back to
            // this thread; license exactly one replacement of the
            // `TurnInFlight` state (consumed by `install_state`).
            self.in_flight_session_returned = true;
        }
        match event {
            TurnEvent::Event(event) => {
                let is_tool_call = event.kind.as_str() == EventKind::TOOL_CALL;
                if self.activity.observe(&event) {
                    self.stall_notified = false;
                }
                self.record_in_flight_error(&event);
                self.update_token_usage_from_event(&event);
                self.transcript.push_event(event);
                self.queue_finalized_visual_output_for_latest_event();
                if is_tool_call {
                    self.refresh_patch_modal_preview();
                }
            }
            TurnEvent::ProviderRuntime(event) => {
                self.activity.observe_provider_runtime(&event, Utc::now());
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
        update_token_usage(
            &mut self.token_usage,
            event,
            context_window_tokens,
            self.primary_agent_id.as_deref(),
        );
    }

    fn accept_worker_session_or_continue(
        &mut self,
        mut session: Box<Session<TuiDecider>>,
        auto_flush: bool,
    ) {
        session.set_model_catalog(self.model_catalog.clone());
        if self.active_session_home_managed {
            let session_id = session.session_id().to_owned();
            if let Err(error) = self.refresh_current_session_metadata(&session_id) {
                self.notice = Some(format!("session metadata refresh failed: {error}"));
            }
        }
        let compaction_requested = self.compaction_request.swap(false, Ordering::SeqCst);
        if compaction_requested && !session.compaction_in_progress() {
            let start = session.events().len();
            let outcome = session
                .begin_compaction()
                .map_err(|error| error.to_string());
            let events = session.events()[start..].to_vec();
            self.record_compaction_update(outcome, events, false);
        }
        if let Some(request) = self.pending_runs.pop_front() {
            match request {
                PendingRunRequest::Extension(request) => self.spawn_extension_run(request, session),
                PendingRunRequest::Companion(request) => self.spawn_companion_run(request, session),
            }
            return;
        }
        if auto_flush && !self.queued_inputs.paused() && session.can_accept_turn() {
            if let Some(input) = self.pop_next_queued_input() {
                self.bottom.record_submission(input.content());
                self.spawn_queued_turn(input, session);
                return;
            }
        }
        self.install_state(AppState::Idle { session });
        // The session is back on this thread: refresh the last-known
        // authenticated-provider snapshot used by bottom-surface rebuilds
        // that happen while a turn is in flight.
        self.refresh_authenticated_providers();
        self.in_flight_label = None;
        self.in_flight_companion_name = None;
        self.in_flight_cancellable = false;
        self.spinner_frame = 0;
        self.spinner_last_tick = None;
    }

    pub(super) fn drain_idle_compaction(&mut self) -> bool {
        let update = match &mut self.state {
            AppState::Idle { session } if session.compaction_in_progress() => {
                let start = session.events().len();
                let outcome = session.poll_compaction().map_err(|error| error.to_string());
                let events = session.events()[start..].to_vec();
                if outcome == Ok(CompactionStatus::InProgress) && events.is_empty() {
                    None
                } else {
                    Some((outcome, events))
                }
            }
            _ => None,
        };
        let Some((outcome, events)) = update else {
            return false;
        };
        self.record_compaction_update(outcome, events, false);
        true
    }

    pub(super) fn record_compaction_update(
        &mut self,
        outcome: Result<CompactionStatus, String>,
        events: Vec<EventEnvelope>,
        announce_pending: bool,
    ) {
        self.record_compaction_events(events);
        match outcome {
            Ok(CompactionStatus::Applied) => {
                self.push_notice_item("compaction complete".to_owned())
            }
            Ok(CompactionStatus::Cancelled) => {
                self.push_notice_item("compaction cancelled · active canvas unchanged".to_owned())
            }
            Ok(CompactionStatus::Failed) => {
                self.push_notice_item("compaction failed · active canvas unchanged".to_owned())
            }
            Ok(CompactionStatus::Unchanged) => {
                self.push_notice_item("nothing eligible to compact".to_owned())
            }
            Ok(CompactionStatus::InProgress) if announce_pending => {
                self.push_notice_item("compaction in progress · you can keep typing".to_owned())
            }
            Ok(CompactionStatus::InProgress) => {}
            Err(error) => self.push_notice_item(format!("compaction failed: {error}")),
        }
    }

    fn record_compaction_events(&mut self, events: Vec<EventEnvelope>) {
        for event in events {
            self.update_token_usage_from_event(&event);
            self.transcript.push_event(event);
            self.queue_finalized_visual_output_for_latest_event();
        }
    }

    pub(super) fn cancel_idle_compaction_for_lifecycle(
        &mut self,
        reason: &'static str,
    ) -> Result<CompactionStatus, String> {
        let update = match &mut self.state {
            AppState::Idle { session } if session.compaction_in_progress() => {
                let start = session.events().len();
                let outcome = session
                    .cancel_compaction(reason)
                    .map_err(|error| error.to_string());
                let events = session.events()[start..].to_vec();
                Some((outcome, events))
            }
            _ => None,
        };
        let Some((outcome, events)) = update else {
            return Ok(CompactionStatus::Unchanged);
        };
        self.record_compaction_events(events);
        outcome
    }

    pub(super) fn interrupt_idle_compaction(
        &mut self,
        reason: &'static str,
    ) -> Result<CompactionStatus, String> {
        let update = match &mut self.state {
            AppState::Idle { session } if session.compaction_in_progress() => {
                let start = session.events().len();
                let outcome = session
                    .interrupt_compaction(reason)
                    .map_err(|error| error.to_string());
                let events = session.events()[start..].to_vec();
                Some((outcome, events))
            }
            _ => None,
        };
        let Some((outcome, events)) = update else {
            return Ok(CompactionStatus::Unchanged);
        };
        self.record_compaction_events(events);
        outcome
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
                self.activity
                    .finish_at(ActivityTerminal::Completed, Utc::now());
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
                self.activity
                    .finish_at(ActivityTerminal::Failed, Utc::now());
                self.push_finalized_visual_item(TranscriptItem::Error {
                    source: format!("extension {}.{}", request.id, request.command),
                    message: message.clone(),
                });
                self.notice = Some(format!(
                    "extension {}.{} failed: {message}",
                    request.id, request.command
                ));
            }
            ExtensionOutcome::Cancelled => {
                self.record_auxiliary_interruption();
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
                self.activity
                    .finish_at(ActivityTerminal::Completed, Utc::now());
                self.push_finalized_visual_item(TranscriptItem::SessionSummary(format!(
                    "companion run result: {}",
                    serde_json::to_string(&crate::companion_run::agent_result_json(&result))
                        .unwrap_or_else(|_| "null".to_owned())
                )));
                self.notice = Some("companion run complete".to_owned());
            }
            CompanionOutcome::Failed(message) => {
                self.activity
                    .finish_at(ActivityTerminal::Failed, Utc::now());
                self.push_finalized_visual_item(TranscriptItem::Error {
                    source: "companion run".to_owned(),
                    message: message.clone(),
                });
                self.notice = Some(format!("companion run failed: {message}"));
            }
            CompanionOutcome::Cancelled => {
                self.record_auxiliary_interruption();
            }
        }
    }

    fn record_auxiliary_interruption(&mut self) {
        self.activity
            .finish_at(ActivityTerminal::Interrupted, Utc::now());
        self.queued_inputs.set_paused(true);
        self.transcript.clear_transient_live_tail();
        self.interrupted_guidance = false;
        self.in_flight_error = None;
        self.push_finalized_visual_item(TranscriptItem::Interrupted);
        self.notice = None;
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
        if !self.turn_in_flight()
            || event.kind.as_str() != EventKind::ERROR
            || event
                .payload
                .get("cancelled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        {
            return;
        }
        if event
            .payload
            .get("purpose")
            .and_then(serde_json::Value::as_str)
            == Some("compaction")
        {
            return;
        }
        let source = event
            .payload
            .get("source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("error");
        // Only a provider error owned by the primary agent terminalizes this
        // live model call. Child/reviewer provider failures and ordinary
        // extension, guardian, or session errors are recoverable milestones;
        // replacing the Activity block for them would falsely claim that the
        // whole turn had failed. Cancellation has its own path above, while a
        // recovery closure is a resume boundary rather than a live turn gap.
        if source != "provider" || self.primary_agent_id.as_deref() != Some(event.agent.as_str()) {
            return;
        }
        let message = event
            .payload
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("turn failed");
        self.in_flight_error = Some(format!("{source}: {message}"));
        self.interrupted_guidance = false;
    }

    pub(super) fn handle_turn_outcome(&mut self, outcome: TurnOutcome, elapsed: Option<Duration>) {
        let terminal = match &outcome {
            TurnOutcome::Complete => ActivityTerminal::Completed,
            TurnOutcome::Failed(_) => ActivityTerminal::Failed,
            TurnOutcome::Cancelled => ActivityTerminal::Cancelled,
        };
        self.activity.finish_at(terminal, Utc::now());
        let emit_recap = match &outcome {
            TurnOutcome::Complete => {
                self.interrupted_guidance = false;
                self.in_flight_error = None;
                self.notice = None;
                true
            }
            TurnOutcome::Cancelled => {
                self.queued_inputs.set_paused(true);
                self.transcript.clear_transient_live_tail();
                self.interrupted_guidance = false;
                self.in_flight_error = None;
                self.push_finalized_visual_item(TranscriptItem::Interrupted);
                self.notice = None;
                false
            }
            TurnOutcome::Failed(message) => {
                self.queued_inputs.set_paused(true);
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
        let worked_divider_shown = elapsed.is_some_and(|elapsed| elapsed >= MIN_WORKED_DURATION);
        if worked_divider_shown {
            self.push_finalized_visual_item(TranscriptItem::WorkedDuration(format_live_elapsed(
                elapsed.expect("worked_divider_shown implies elapsed is Some"),
            )));
        }
        // Review v3 §R5(a): the recap and its `── Worked for Ns ──` divider
        // are one unit — a recap must never render without the divider (a
        // turn too short to get a divider has nothing worth recapping
        // either).
        if emit_recap && worked_divider_shown {
            self.push_turn_recap();
        }
        match outcome {
            TurnOutcome::Complete => self.queue_notification(NotifyEvent::TurnDone),
            TurnOutcome::Failed(_) => self.queue_notification(NotifyEvent::Failure),
            TurnOutcome::Cancelled => {}
        }
        self.stall_notified = false;
    }

    fn last_event_is_error(&self) -> bool {
        self.transcript
            .events()
            .last()
            .is_some_and(|event| event.kind.as_str() == EventKind::ERROR)
    }
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
