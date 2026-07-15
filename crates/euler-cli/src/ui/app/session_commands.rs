use super::*;

impl AppCore {
    pub(super) fn show_session_diff(&mut self) -> CoreEffect {
        let AppState::Idle { session } = &self.state else {
            return self.notice_item("diff waits for the active turn".to_owned());
        };
        let diffs = session_attributed_diffs(session.events());
        if diffs.is_empty() {
            return self.notice_item("no session-attributed file changes yet".to_owned());
        }
        let mut header = format!("session diff · {} file(s)", diffs.len());
        let mut total_added = 0usize;
        let mut total_removed = 0usize;
        for diff in &diffs {
            let (a, r) = count_diff_lines(diff.diff.as_deref().unwrap_or(""));
            total_added += a;
            total_removed += r;
        }
        header.push_str(&format!(" · +{total_added} −{total_removed}"));
        self.push_finalized_visual_item(TranscriptItem::SessionSummary(header));
        for diff in diffs {
            self.push_finalized_visual_item(TranscriptItem::FileDiff {
                path: diff.path,
                action: diff.action,
                origin: "session".to_owned(),
                diff: diff.diff,
                truncated: diff.truncated,
                truncation: diff.truncation,
                omitted_reason: diff.omitted_reason,
                checkpoint_event_id: None,
            });
        }
        CoreEffect::Render
    }

    pub(super) fn show_session_usage(&mut self) -> CoreEffect {
        // Cost display is deferred until provider price catalogs exist.
        let AppState::Idle { session } = &self.state else {
            // Fall back to live snapshot when a turn is in flight.
            let text = format_usage_from_snapshot(&self.token_usage, &self.status);
            return self.notice_item(text);
        };
        let text = format_session_usage(session.events(), &self.status, &self.token_usage);
        self.notice_item(text)
    }

    pub(super) fn set_reasoning_effort(&mut self, effort: ReasoningEffort) -> CoreEffect {
        let AppState::Idle { session } = &mut self.state else {
            return self.notice_item("reasoning effort waits for the active turn".to_owned());
        };
        match session.set_reasoning_effort(effort, "user") {
            Ok(true) => {
                self.status.reasoning_effort = Some(effort.as_str().to_owned());
                self.rebuild_bottom_surface();
                self.notice_item(format!("reasoning effort set to {}", effort.as_str()))
            }
            Ok(false) => self.notice_item(format!("reasoning effort already {}", effort.as_str())),
            Err(error) => self.error_item(format!("reasoning effort rejected: {error}")),
        }
    }

    pub(super) fn set_compaction_policy(&mut self, automatic: bool, stubs: bool) -> CoreEffect {
        let AppState::Idle { session } = &mut self.state else {
            return self.notice_item("compaction settings wait for the active turn".to_owned());
        };
        match session.set_auto_compaction_policy(automatic, stubs) {
            Ok(true) => {
                self.rebuild_bottom_surface();
                self.notice_item(format!(
                    "compaction settings · automatic {} · tool stubs {}",
                    if automatic { "on" } else { "off" },
                    if stubs { "on" } else { "off" },
                ))
            }
            Ok(false) => self.notice_item("compaction settings unchanged".to_owned()),
            Err(error) => self.error_item(format!("compaction settings rejected: {error}")),
        }
    }

    pub(super) fn compact_session(&mut self) -> CoreEffect {
        let AppState::Idle { session } = &mut self.state else {
            return self.notice_item("compaction waits for the active turn".to_owned());
        };
        let start = session.events().len();
        if session.compact_now() {
            let new_events = session.events()[start..].to_vec();
            for event in new_events {
                self.transcript.push_event(event);
                self.queue_finalized_visual_output_for_latest_event();
            }
            return self.notice_item("compacted eligible history".to_owned());
        }
        self.notice_item("nothing eligible to compact".to_owned())
    }

    pub(super) fn export_session(&mut self, path: Option<String>) -> CoreEffect {
        let (session_id, events) = match &self.state {
            AppState::Idle { session } => {
                let events: Vec<_> = session
                    .events()
                    .iter()
                    .filter(|event| !event_is_runtime_only(event.kind.as_str()))
                    .cloned()
                    .collect();
                (session.session_id().to_owned(), events)
            }
            _ => return self.notice_item("export waits for the active turn".to_owned()),
        };
        let path = match path.map(PathBuf::from) {
            Some(path) => path,
            None => match self.default_export_path(&session_id) {
                Ok(path) => path,
                Err(error) => return self.error_item(format!("export failed: {error}")),
            },
        };
        let payload = serde_json::json!({
            "session_id": session_id,
            "provider": self.status.provider,
            "model": self.status.model,
            "reasoning_effort": self.current_reasoning_effort().as_str(),
            "events": events,
        });
        match serde_json::to_vec_pretty(&payload)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| write_new_file(&path, &bytes).map_err(anyhow::Error::from))
        {
            Ok(()) => self.notice_item(format!("session exported to {}", path.display())),
            Err(error) => self.error_item(format!("export failed: {error}")),
        }
    }

    pub(super) fn show_status(&mut self) -> CoreEffect {
        let session = self.status.session_id.as_deref().unwrap_or("none");
        let mut status = format!(
            "session: {session}\nmodel: {}::{}\neffort: {}\ntheme: {} ({})",
            self.status.provider,
            self.status.model,
            self.current_reasoning_effort().as_str(),
            self.theme_choice.label(),
            self.theme_choice.as_str()
        );
        // §5.1 status indicator: the active posture *and its envelope*, never
        // just the name — the boundary in force has to be legible without
        // knowing what each posture means. `custom` is the honest reading of
        // per-capability modes that no posture describes.
        if let AppState::Idle { session } = &self.state {
            let envelope = crate::ui::commands::PermissionPosture::active(|capability| {
                session.configured_mode(capability)
            })
            .map_or("custom · per-capability modes", |posture| {
                posture.envelope()
            });
            status.push_str(&format!("\npermissions: {envelope}"));
        }
        // ADR 0011 visibility: say so whenever a non-default reviewer
        // resolves permission asks in place of the user.
        if let Some(reviewer) = self.status.permission_reviewer.as_deref() {
            status.push_str(&format!("\npermission reviewer: {reviewer}"));
        }
        self.notice_item(status)
    }

    pub(super) fn set_theme(&mut self, choice: ThemeChoice) -> CoreEffect {
        self.theme_choice = choice;
        // #64: carry forward the color level detected at startup — switching
        // themes must not silently reintroduce truecolor SGR on a terminal
        // that can't render it.
        let color_level = self.theme.color_level;
        self.theme = Theme::for_choice_with_color_level(choice, color_level);
        self.rebuild_bottom_surface();
        match self
            .theme_preference_path
            .as_deref()
            .map(|path| model_preference::save_theme_preference(path, choice.as_str()))
        {
            Some(Err(error)) => {
                self.push_error_item(format!("theme set; preference not saved: {error}"));
            }
            _ => {
                self.push_notice_item(format!("theme set to {}", choice.as_str()));
            }
        }
        CoreEffect::ThemeChanged
    }

    pub(super) fn name_current_session(&mut self, name: String) -> CoreEffect {
        let AppState::Idle { session } = &mut self.state else {
            return self.notice_item("session naming waits for the active turn".to_owned());
        };
        let session_id = session.session_id().to_owned();
        let result = session.rename_session(&name);
        match result {
            Ok(normalized) => {
                // Footer #46: the footer reads straight off `self.status` on
                // every render, so set it here rather than waiting on the
                // metadata refresh below — renaming updates the footer
                // immediately even if the store refresh fails.
                self.status.session_name = Some(normalized.clone());
                self.rebuild_bottom_surface();
                let message = match self.refresh_current_session_metadata(&session_id) {
                    Ok(()) => format!("session named {normalized}"),
                    Err(error) => {
                        format!("session named {normalized}; metadata refresh failed: {error}")
                    }
                };
                // Route through the shared spine notice (review v4 dogfood):
                // this used to set `self.notice` directly, which renders as
                // a transient bottom-surface banner flush at column 0 with
                // no `•` anchor — unlike every other setting confirmation
                // (e.g. `theme set to …`), which lives on the spine via
                // `push_notice_item`.
                self.push_notice_item(message);
                CoreEffect::Render
            }
            Err(error) => self.error_item(format!("session naming failed: {error}")),
        }
    }

    /// `/scrub [value]` (issue #100): remove a credential from every
    /// provenance surface of the live session. Bare form scrubs the values
    /// detected in tool-call arguments this session (the ones the exposure
    /// warning flagged); an explicit value scrubs exactly that string.
    pub(super) fn scrub_current_session(&mut self, value: Option<String>) -> CoreEffect {
        let secrets = match (&self.state, value) {
            (AppState::Idle { .. }, Some(value)) => vec![value],
            (AppState::Idle { session }, None) => session.scrub_candidates().to_vec(),
            _ => return self.notice_item("scrub waits for the active turn".to_owned()),
        };
        if secrets.is_empty() {
            return self.notice_item(
                "scrub: no credential detected this session — pass a value: /scrub <value>"
                    .to_owned(),
            );
        }
        let prepared = euler_core::scrub::prepare_secrets(&secrets);
        if prepared.is_empty() {
            return self.notice_item(format!(
                "scrub value must be at least {} characters",
                euler_core::scrub::MIN_SCRUB_VALUE_LEN
            ));
        }
        let result = match &mut self.state {
            AppState::Idle { session } => session
                .scrub_live(&prepared)
                .map(|report| (report, session.events().to_vec())),
            _ => unreachable!("state checked above"),
        };
        let (report, events) = match result {
            Ok(result) => result,
            Err(error) => return self.error_item(format!("scrub failed: {error}")),
        };

        if let Some(name) = self.status.session_name.as_mut() {
            *name = euler_core::redaction::scrub_secrets_in_text(name, &prepared).0;
        }
        self.rebuild_transcript_from_events(&events);
        self.rebuild_bottom_surface();
        if report.audit_event_id.is_some() {
            CoreEffect::Render
        } else {
            self.notice_item(report.summary_line())
        }
    }

    pub(super) fn switch_model(&mut self, provider: String, model: String) -> CoreEffect {
        let AppState::Idle { session } = &mut self.state else {
            return self.notice_item("model switch waits for the active turn".to_owned());
        };
        let context_limit = self
            .model_catalog
            .provider(&provider)
            .and_then(|descriptor| {
                descriptor
                    .models()
                    .find(|entry| entry.id() == model)
                    .and_then(|entry| {
                        euler_core::ContextLimitConfig::from_catalog_model(
                            entry.effective_context_window_tokens()?,
                            entry.auto_compact_token_limit(),
                        )
                    })
            });
        let previous_effort = session.reasoning_effort();
        let result = session.switch_model(&provider, &model, "user", context_limit);
        let current_effort = session.reasoning_effort();
        match result {
            Ok(switched) => {
                self.status.reasoning_effort = Some(current_effort.as_str().to_owned());
                let effort_changed = (current_effort != previous_effort).then_some(current_effort);
                self.accept_model_switch(provider, model, switched, effort_changed)
            }
            Err(error) => self.error_item(format!("model switch rejected: {error}")),
        }
    }

    fn accept_model_switch(
        &mut self,
        provider: String,
        model: String,
        switched: bool,
        effort_changed: Option<ReasoningEffort>,
    ) -> CoreEffect {
        self.status.provider = provider.clone();
        self.status.model = model.clone();
        if switched {
            self.token_usage = TokenUsageSnapshot {
                context_window_tokens: self.active_context_window_tokens(),
                ..TokenUsageSnapshot::default()
            };
        }
        self.rebuild_bottom_surface();
        match model_preference::save_model_preference_to_default(&provider, &model) {
            Ok(()) => {
                let mut message = format!("model set to {provider}/{model}");
                if let Some(effort) = effort_changed {
                    message.push_str(&format!(" · reasoning reduced to {}", effort.as_str()));
                }
                self.notice_item(message)
            }
            Err(error) => self.error_item(format!("model set; preference not saved: {error}")),
        }
    }

    pub(super) fn set_permission_mode(
        &mut self,
        capability: Capability,
        mode: ApprovalMode,
    ) -> CoreEffect {
        let AppState::Idle { session } = &mut self.state else {
            return self.notice_item("permission mode waits for the active turn".to_owned());
        };
        session.set_permission_mode(capability, mode);
        self.notice_item(format!(
            "permission {} set to {:?}",
            capability.as_str(),
            mode
        ))
    }

    /// Apply a session-local posture as an explicit mapping over every
    /// capability. The mapping lives at the UI boundary: core continues to
    /// own individual capability grants and never treats a posture as an OS
    /// sandbox claim.
    pub(super) fn set_permission_posture(&mut self, posture: PermissionPosture) -> CoreEffect {
        let AppState::Idle { session } = &mut self.state else {
            return self.notice_item("permission posture waits for the active turn".to_owned());
        };
        // A posture is an intentional reset of transient consent. Otherwise
        // "Ask every time" could silently inherit an earlier session grant,
        // and switching away from Read only could revive access the user
        // thought they had turned off. Durable project/user rules remain
        // visible and explicit in the advanced controls.
        let session_grants = session
            .list_grants()
            .into_iter()
            .filter(|(source, _)| *source == GrantSource::Session)
            .collect::<Vec<_>>();
        let mut cleared = 0;
        for (_, grant) in session_grants {
            match session.revoke_grant(grant.capability, &grant.pattern, GrantSource::Session) {
                Ok(revoked) => cleared += revoked,
                Err(error) => {
                    return self
                        .error_item(format!("could not reset session permission grant: {error}"));
                }
            }
        }
        for &capability in Capability::ALL {
            session.set_permission_mode(capability, posture.mode_for(capability));
        }
        let grants = if cleared == 1 { "grant" } else { "grants" };
        self.notice_item(format!(
            "permission posture set to {} · cleared {cleared} session {grants}",
            posture.label()
        ))
    }

    pub(super) fn open_permissions_picker(&mut self) -> CoreEffect {
        let AppState::Idle { session } = &self.state else {
            return self.notice_item("permissions wait for the active turn".to_owned());
        };
        let grants = session.list_grants();
        let active = crate::ui::commands::PermissionPosture::active(|capability| {
            session.configured_mode(capability)
        });
        let choices = crate::ui::commands::permission_choices_with_state(&grants, active);
        self.bottom
            .open_picker(crate::ui::commands::PickerSpec::Permissions(choices));
        CoreEffect::Render
    }

    pub(super) fn revoke_grant(
        &mut self,
        capability: Capability,
        pattern: String,
        source: GrantSource,
    ) -> CoreEffect {
        let AppState::Idle { session } = &mut self.state else {
            return self.notice_item("revoke waits for the active turn".to_owned());
        };
        let pattern = match ScopePattern::new(pattern) {
            Ok(pattern) => pattern,
            Err(error) => {
                return self.error_item(format!("invalid grant pattern: {error}"));
            }
        };
        match session.revoke_grant(capability, &pattern, source) {
            Ok(0) => self.notice_item(format!(
                "no {} grant for {} ({})",
                source.as_str(),
                capability.as_str(),
                if pattern.is_unscoped() {
                    "all"
                } else {
                    pattern.as_str()
                }
            )),
            Ok(_) => self.notice_item(format!(
                "revoked {} {} ({})",
                source.as_str(),
                capability.as_str(),
                if pattern.is_unscoped() {
                    "all"
                } else {
                    pattern.as_str()
                }
            )),
            Err(error) => self.error_item(format!("revoke failed: {error}")),
        }
    }
}
