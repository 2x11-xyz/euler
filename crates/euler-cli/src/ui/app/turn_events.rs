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
        self.in_flight_label = None;
        self.in_flight_companion_name = None;
        self.in_flight_cancellable = false;
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
                if request.id == "code-swarm" {
                    match (request.command.as_str(), &self.code_swarm_run) {
                        ("review-brief", Some(CodeSwarmRun::Briefing)) => {
                            self.code_swarm_on_brief_complete(&output);
                        }
                        ("review-report", Some(CodeSwarmRun::Reporting { .. })) => {
                            self.code_swarm_on_report_complete(true, Some(&output));
                        }
                        _ => {}
                    }
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
                if request.id == "code-swarm" && self.code_swarm_run.is_some() {
                    if matches!(self.code_swarm_run, Some(CodeSwarmRun::Reporting { .. })) {
                        self.code_swarm_on_report_complete(false, None);
                    } else {
                        self.code_swarm_run = None;
                        let _ =
                            self.notice_item("✗ code-swarm review aborted at briefing".to_owned());
                    }
                }
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
        // A failed reviewer still advances the swarm — the report includes
        // whichever spawn/result pairs actually landed.
        self.code_swarm_on_companion_done();
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
