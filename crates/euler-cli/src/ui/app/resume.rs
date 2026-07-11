use super::*;

pub(super) struct TuiResume {
    pub(super) session: Session<TuiDecider>,
    pub(super) channels: PermissionChannels,
    pub(super) events: Vec<EventEnvelope>,
    pub(super) active_target: ModelTarget,
    pub(super) display_label: String,
    /// Footer #46: the user-set `/name`, distinct from `display_label`
    /// (which falls back to the auto title, then the session id, for the
    /// resume-boundary banner line — the footer only ever shows a real
    /// name or nothing).
    pub(super) session_name: Option<String>,
    pub(super) recovery_closure_appended: bool,
    pub(super) warning_count: usize,
    pub(super) events_replayed: usize,
}

impl AppCore {
    pub(super) fn resume_session_from_picker(&mut self, session_id: String) -> CoreEffect {
        let current_session_id = match &self.state {
            AppState::Idle { session } => session.session_id().to_owned(),
            AppState::TurnInFlight { .. } => {
                // Faint line above composer; composer/queue input stays intact.
                self.notice = Some("resume waits for the active turn".to_owned());
                return CoreEffect::Render;
            }
            AppState::Empty => {
                return self.teach_notice("resume needs an active session".to_owned())
            }
        };
        if current_session_id == session_id {
            return self.teach_notice(format!("already using session {session_id}"));
        }

        match self.build_tui_resume(&session_id) {
            Ok(resume) => self.accept_tui_resume(session_id, resume),
            Err(error) => self.notice_item(format!("resume failed: {error}")),
        }
    }

    pub(super) fn preview_resume_ledger_tail(&mut self) -> CoreEffect {
        let Some(session_id) = self.bottom.resume_picker_selected_session_id() else {
            return CoreEffect::None;
        };
        match self.load_resume_ledger_tail_preview(&session_id) {
            Ok(lines) => {
                self.bottom.set_resume_ledger_preview(lines);
                CoreEffect::Render
            }
            Err(error) => {
                self.notice = Some(format!("preview failed: {error}"));
                CoreEffect::Render
            }
        }
    }

    fn load_resume_ledger_tail_preview(&mut self, session_id: &str) -> Result<Vec<String>> {
        const PREVIEW_TAIL_LINES: usize = 16;
        let record = self
            .session_store()?
            .find_session(session_id)?
            .ok_or_else(|| anyhow!("no session found with id {session_id}"))?;
        let events = read_resume_prefix(record.events_path())?;
        let items = transcript::project_events(&events);
        let width = self.composer_navigation_width.max(40);
        let rendered = crate::ui::text::with_timestamp_gutter(self.show_timestamp_gutter, || {
            transcript::render_items_for_history(&items, &self.theme, width)
        });
        let lines: Vec<String> = rendered
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let start = lines.len().saturating_sub(PREVIEW_TAIL_LINES);
        Ok(lines[start..].to_vec())
    }

    fn build_tui_resume(&mut self, session_id: &str) -> Result<TuiResume> {
        let record = self
            .session_store()?
            .find_session(session_id)?
            .ok_or_else(|| anyhow!("no session found with id {session_id}"))?;
        let prefix = read_resume_prefix(record.events_path())?;
        let root = std::env::current_dir().unwrap_or_else(|_| session_root_status_path());
        let mut seed_config = crate::session_config(
            root.clone(),
            self.status.provider.clone(),
            self.status.model.clone(),
            session_id.to_owned(),
        );
        seed_config.extensions_enabled =
            resolve_session_extensions(&seed_config.root, &self.extensions)?;
        let observer = bundled_round_observer(&self.observe, &seed_config.extensions_enabled)?;
        if let Some((observer_config, _)) = &observer {
            seed_config.round_observer = Some(observer_config.clone());
        }
        let folded = fold_session(&seed_config, prefix)?;
        let original = folded
            .original_target
            .as_ref()
            .unwrap_or(&folded.active_target);
        let mut config = crate::session_config(
            root,
            original.provider.clone(),
            original.model.clone(),
            session_id.to_owned(),
        );
        config.extensions_enabled = seed_config.extensions_enabled;
        config.round_observer = seed_config.round_observer;
        // Compaction window follows the active model after fold (post-switch).
        config.provider = folded.active_target.provider.clone();
        config.model = folded.active_target.model.clone();
        crate::session_lifecycle::apply_catalog_context_limit(&mut config, &self.model_catalog);
        let providers = crate::resume_provider_set(
            folded
                .original_target
                .as_ref()
                .unwrap_or(&folded.active_target),
            &folded.active_target,
            None,
        )?;
        let writer = ProvenanceWriter::new(record.events_path())?;
        let (decider, channels) = TuiDecider::new();
        let outcome =
            resume_session_from_folded_prefix(config, providers, decider, writer, folded)?;
        let mut session = outcome.session;
        if let Some((_, extension)) = observer {
            session.set_observer_extension(extension);
        }
        crate::wire_code_swarm(&mut session);
        let events = session.events().to_vec();
        let events_replayed = outcome.events_folded;
        Ok(TuiResume {
            session,
            channels,
            events,
            active_target: outcome.active_target,
            display_label: session_resume_label(&record),
            session_name: record.name().map(str::to_owned),
            recovery_closure_appended: outcome.recovery_closure_appended,
            warning_count: outcome.warnings.len(),
            events_replayed,
        })
    }

    pub(super) fn accept_tui_resume(
        &mut self,
        session_id: String,
        resume: TuiResume,
    ) -> CoreEffect {
        let reasoning_effort = resume.session.reasoning_effort();
        self.permission_rx = resume.channels.request_rx;
        self.reply_tx = resume.channels.reply_tx;
        self.state = AppState::Idle {
            session: Box::new(resume.session),
        };
        self.status.provider = resume.active_target.provider.clone();
        self.status.model = resume.active_target.model.clone();
        self.status.session_id = Some(session_id.clone());
        self.status.reasoning_effort = Some(reasoning_effort.as_str().to_owned());
        self.status.git_branch = detect_git_branch(&self.status.cwd);
        self.status.session_name = resume.session_name.clone();
        self.active_session_home_managed = true;
        self.replace_bottom_surface_for_session();
        // Rebuild first so token_usage reflects the resumed event stream under
        // the post-fold active model window (footer ctx% = model budget).
        self.rebuild_transcript_from_events(&resume.events);
        self.token_usage.context_window_tokens = self.active_context_window_tokens();
        let label = if resume.display_label == "Untitled session" {
            session_id.clone()
        } else {
            resume.display_label.clone()
        };
        self.push_finalized_visual_item(TranscriptItem::ResumeBoundary {
            label,
            recovery_closure_appended: resume.recovery_closure_appended,
            warning_count: resume.warning_count,
            events_replayed: resume.events_replayed,
        });
        self.visual_scroll_offset = 0;
        self.modal = None;
        self.quit_armed = None;
        self.last_working_elapsed_secs = None;
        self.interrupted_guidance = false;
        self.in_flight_error = None;
        self.clear_queued_inputs();
        self.notice = None;
        CoreEffect::ReplayHistoryWithScrollbackPurge
    }
}
