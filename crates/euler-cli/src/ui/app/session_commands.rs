use super::*;

impl AppCore {
    pub(super) fn show_session_diff(&mut self) -> CoreEffect {
        let AppState::Idle { session } = &self.state else {
            return self.notice_item("diff waits for the active turn".to_owned());
        };
        let diffs = session_attributed_diffs(session.events());
        if diffs.is_empty() {
            return self.summary_item("no session-attributed file changes yet".to_owned());
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
            return self.summary_item(text);
        };
        let text = format_session_usage(session.events(), &self.status, &self.token_usage);
        self.summary_item(text)
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
            Err(error) => self.notice_item(format!("reasoning effort rejected: {error}")),
        }
    }

    pub(super) fn show_compaction_status(&mut self) -> CoreEffect {
        let AppState::Idle { session } = &self.state else {
            return self.notice_item("compaction status waits for the active turn".to_owned());
        };
        let policy = session.auto_compaction_policy();
        let demoted = self.token_usage.demoted_items;
        let retained = self
            .token_usage
            .canvas_retained_bytes
            .map(|bytes| bytes.to_string())
            .unwrap_or_else(|| "?".to_owned());
        let limit = session
            .context_limit_tokens()
            .map(|tokens| tokens.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let used = session
            .latest_model_usage_used_tokens()
            .map(|tokens| tokens.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        self.notice_item(format!(
            "compaction tier={} budget_bytes={} retained_bytes={retained} demoted={demoted} limit_tokens={limit} used_tokens={used} reserve={}",
            policy.tier.as_str(),
            policy.budget_bytes,
            session.compaction_reserve_tokens()
        ))
    }

    pub(super) fn compact_session(&mut self) -> CoreEffect {
        let AppState::Idle { session } = &mut self.state else {
            return self.notice_item("compaction waits for the active turn".to_owned());
        };
        let start = session.events().len();
        let projection = heuristic_projection(session.events());
        if session.try_compact(&projection) {
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
                Err(error) => return self.notice_item(format!("export failed: {error}")),
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
            Err(error) => self.notice_item(format!("export failed: {error}")),
        }
    }

    pub(super) fn show_status(&mut self) -> CoreEffect {
        let session = self.status.session_id.as_deref().unwrap_or("none");
        self.summary_item(format!(
            "session: {session}\nmodel: {}::{}\neffort: {}\ntheme: {} ({})",
            self.status.provider,
            self.status.model,
            self.current_reasoning_effort().as_str(),
            self.theme_choice.label(),
            self.theme_choice.as_str()
        ))
    }

    pub(super) fn set_theme(&mut self, choice: ThemeChoice) -> CoreEffect {
        self.theme_choice = choice;
        self.theme = Theme::for_choice(choice);
        self.rebuild_bottom_surface();
        match self
            .theme_preference_path
            .as_deref()
            .map(|path| model_preference::save_theme_preference(path, choice.as_str()))
        {
            Some(Err(error)) => {
                self.push_notice_item(format!("theme set; preference not saved: {error}"));
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
                self.notice = match self.refresh_current_session_metadata(&session_id) {
                    Ok(()) => Some(format!("session named {normalized}")),
                    Err(error) => Some(format!(
                        "session named {normalized}; metadata refresh failed: {error}"
                    )),
                };
                CoreEffect::Render
            }
            Err(error) => self.notice_item(format!("session naming failed: {error}")),
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
                    .and_then(|entry| entry.context_window_tokens())
            })
            .and_then(euler_core::ContextLimitConfig::from_catalog_window);
        match session.switch_model(&provider, &model, "user", context_limit) {
            Ok(true) => self.accept_model_switch(provider, model, true),
            Ok(false) => self.accept_model_switch(provider, model, false),
            Err(error) => self.notice_item(format!("model switch rejected: {error}")),
        }
    }

    fn accept_model_switch(
        &mut self,
        provider: String,
        model: String,
        switched: bool,
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
            Ok(()) => self.notice_item(format!("model set to {provider}/{model}")),
            Err(error) => self.notice_item(format!("model set; preference not saved: {error}")),
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

    pub(super) fn open_permissions_picker(&mut self) -> CoreEffect {
        let AppState::Idle { session } = &self.state else {
            return self.notice_item("permissions wait for the active turn".to_owned());
        };
        let grants = session.list_grants();
        let choices = crate::ui::commands::permission_choices_with_grants(&grants);
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
                return self.notice_item(format!("invalid grant pattern: {error}"));
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
            Err(error) => self.notice_item(format!("revoke failed: {error}")),
        }
    }
}
