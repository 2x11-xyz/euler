use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ExtensionOutcome {
    Complete(serde_json::Value),
    Failed(String),
}

#[derive(Clone)]
pub(super) struct ExtensionRunRequest {
    pub(super) id: String,
    pub(super) command: String,
    input: serde_json::Value,
    extension: &'static dyn Extension,
    capabilities: Vec<Capability>,
}

impl ExtensionRunRequest {
    fn label(&self) -> String {
        format!("extension {}.{}", self.id, self.command)
    }
}

impl AppCore {
    pub(super) fn dag_export(&mut self) -> CoreEffect {
        let enabled = match &self.state {
            AppState::Idle { session } => session.extension_enabled("causal-dag"),
            _ => {
                // Check registry/session context when not idle.
                self.current_extension_context()
                    .0
                    .iter()
                    .find(|item| item.id == "causal-dag")
                    .is_some_and(|item| item.enabled)
            }
        };
        if !enabled {
            return self.teach_notice(crate::ui::commands::disabled_extension_teach(
                "/dag",
                "causal-dag",
            ));
        }
        self.extension_run(
            "causal-dag".to_owned(),
            "export".to_owned(),
            serde_json::Value::Object(serde_json::Map::new()),
            None,
        )
    }

    pub(super) fn open_extension_manager(&mut self) -> CoreEffect {
        self.rebuild_bottom_surface();
        self.bottom.open_extension_manager();
        CoreEffect::Render
    }

    pub(super) fn toggle_extension(&mut self, id: String, enable: bool) -> CoreEffect {
        match set_extension_enabled(&id, enable) {
            Ok(()) => {
                if let AppState::Idle { session } = &mut self.state {
                    session.set_extension_enabled(&id, enable);
                }
                self.rebuild_bottom_surface();
                let verb = if enable { "enabled" } else { "disabled" };
                // Decision-record line in the ledger.
                self.push_notice_item(format!("✓ extension {verb}: {id}"));
                self.bottom.open_extension_manager();
                CoreEffect::Render
            }
            Err(error) => self.notice_item(format!("extension toggle failed: {error}")),
        }
    }

    pub(super) fn show_extension_details(&mut self, id: String) -> CoreEffect {
        let (items, _) = self.current_extension_context();
        match items.into_iter().find(|item| item.id == id) {
            Some(item) => self.summary_item(item.details_text()),
            None => self.notice_item(format!("unknown extension: {id}")),
        }
    }

    pub(super) fn remove_extension(&mut self, id: String) -> CoreEffect {
        match remove_linked_extension(&id) {
            Ok(message) => {
                if let AppState::Idle { session } = &mut self.state {
                    session.set_extension_enabled(&id, false);
                }
                self.rebuild_bottom_surface();
                self.push_notice_item(format!("✓ extension removed: {id} · {message}"));
                CoreEffect::Render
            }
            Err(error) => self.notice_item(format!("extension remove failed: {error}")),
        }
    }

    pub(super) fn add_extension(&mut self, path: String) -> CoreEffect {
        match add_local_extension(std::path::Path::new(&path)) {
            Ok(report) => {
                if let AppState::Idle { session } = &mut self.state {
                    session.set_extension_enabled(&report.id, true);
                }
                self.rebuild_bottom_surface();
                self.push_notice_item(format!(
                    "✓ extension installed · {} · enabled for session",
                    report.id
                ));
                self.summary_item(report.steps_text());
                CoreEffect::Render
            }
            Err(error) => self.notice_item(format!("extension add failed: {error}")),
        }
    }

    pub(super) fn extension_run(
        &mut self,
        id: String,
        command: String,
        input: serde_json::Value,
        raw_args: Option<String>,
    ) -> CoreEffect {
        let request = match self.resolve_extension_run(id, command, input, raw_args) {
            Ok(request) => request,
            Err(error) => return self.notice_item(format!("extension run failed: {error}")),
        };
        match std::mem::replace(&mut self.state, AppState::Empty) {
            AppState::Idle { session } => {
                self.spawn_extension_run(request, session);
                CoreEffect::Render
            }
            state @ AppState::TurnInFlight { .. } => {
                let label = request.label();
                self.state = state;
                self.pending_runs
                    .push_back(PendingRunRequest::Extension(request));
                self.notice = Some(format!("queued {label}"));
                CoreEffect::Render
            }
            AppState::Empty => {
                self.state = AppState::Empty;
                self.notice_item("extension run needs an active session".to_owned())
            }
        }
    }

    pub(super) fn resolve_extension_run(
        &self,
        id: String,
        command: String,
        input: serde_json::Value,
        raw_args: Option<String>,
    ) -> Result<ExtensionRunRequest> {
        let descriptor = bundled_descriptor_by_id(&id)?.ok_or_else(|| {
            // Linked/installed extensions are manageable but not yet runnable
            // in-session: teach the CLI path instead of "unknown id"
            // (calibration finding E3).
            if self
                .bottom
                .context()
                .extension_items
                .iter()
                .any(|item| item.id == id && !item.bundled)
            {
                anyhow!(
                    "{id} is a linked extension — in-session runs are not supported yet; \
                     use `euler extension run {id}.{command} <session>` from the CLI"
                )
            } else {
                anyhow!("unknown extension id: {id}")
            }
        })?;
        let command_descriptor = descriptor
            .command(&command)
            .ok_or_else(|| anyhow!("unknown command for extension {id}: {command}"))?;
        let bundled =
            bundled_extension_by_id(&id).ok_or_else(|| anyhow!("unknown extension id: {id}"))?;
        let input = match raw_args {
            None => input,
            Some(raw) => {
                // Same ArgSpec contract as `euler extension run` argv parsing.
                // Whitespace tokenization: flag values with spaces need the
                // JSON input form instead.
                let reference = format!("{id}.{command}");
                let mut args = raw.split_whitespace().map(str::to_owned);
                crate::extension_cli::parse_extension_run_input(
                    &reference,
                    Some(command_descriptor),
                    &mut args,
                )?
            }
        };
        Ok(ExtensionRunRequest {
            id,
            command,
            input,
            extension: bundled.extension,
            capabilities: command_descriptor.required_capabilities.clone(),
        })
    }

    pub(super) fn spawn_extension_run(
        &mut self,
        request: ExtensionRunRequest,
        mut session: Box<Session<TuiDecider>>,
    ) {
        let (worker_tx, worker_rx) = mpsc::channel();
        let worker_request = request.clone();
        let label = request.label();
        std::thread::spawn(move || {
            let start = session.events().len();
            let result = session.execute_extension_command(
                worker_request.extension,
                &worker_request.command,
                worker_request.input.clone(),
                worker_request.capabilities.iter().copied(),
            );
            let events = session.events()[start..].to_vec();
            let outcome = match result {
                Ok(output) => ExtensionOutcome::Complete(output),
                Err(error) => ExtensionOutcome::Failed(error.to_string()),
            };
            let _ = worker_tx.send(TurnEvent::ExtensionDone {
                request: worker_request,
                outcome,
                events,
                session,
            });
        });
        self.state = AppState::TurnInFlight {
            worker_rx,
            interrupt_flag: Arc::new(AtomicBool::new(false)),
            started_at: Instant::now(),
        };
        self.in_flight_label = Some(label);
        self.in_flight_companion_name = None;
        self.in_flight_cancellable = false;
        self.last_working_elapsed_secs = None;
        self.interrupted_guidance = false;
        self.in_flight_error = None;
    }
}

pub(super) fn list_extension_manager_items(
    session_enabled: Option<&std::collections::BTreeSet<String>>,
) -> Vec<crate::ui::commands::ExtensionManagerItem> {
    let Ok(home) = EulerHome::resolve() else {
        return Vec::new();
    };
    let registry = ExtensionRegistry::open_read_only(home);
    let enablement = registry.enablement_states().unwrap_or_default();
    let audit_by_id = audit_status_by_id(&registry);
    let mut items = bundled_manager_items(session_enabled, &enablement, &audit_by_id);
    append_linked_manager_items(
        &mut items,
        &registry,
        session_enabled,
        &enablement,
        &audit_by_id,
    );
    items
}

fn audit_status_by_id(registry: &ExtensionRegistry) -> std::collections::BTreeMap<String, String> {
    registry
        .audit()
        .ok()
        .map(|report| {
            report
                .entries
                .into_iter()
                .map(|entry| (entry.id, format!("{:?}", entry.issue_code).to_lowercase()))
                .collect()
        })
        .unwrap_or_default()
}

fn extension_is_enabled(
    id: &str,
    session_enabled: Option<&std::collections::BTreeSet<String>>,
    enablement: &std::collections::BTreeMap<String, ExtensionEnablement>,
) -> bool {
    let registry_enabled = enablement
        .get(id)
        .copied()
        .unwrap_or(ExtensionEnablement::Disabled)
        .is_enabled();
    session_enabled
        .map(|set| set.contains(id))
        .unwrap_or(registry_enabled)
}

fn bundled_manager_items(
    session_enabled: Option<&std::collections::BTreeSet<String>>,
    enablement: &std::collections::BTreeMap<String, ExtensionEnablement>,
    audit_by_id: &std::collections::BTreeMap<String, String>,
) -> Vec<crate::ui::commands::ExtensionManagerItem> {
    let Ok(descriptors) = bundled_descriptors() else {
        return Vec::new();
    };
    descriptors
        .into_iter()
        .map(|descriptor| crate::ui::commands::ExtensionManagerItem {
            id: descriptor.id.clone(),
            display_name: descriptor.display_name.clone(),
            enabled: extension_is_enabled(&descriptor.id, session_enabled, enablement),
            bundled: true,
            materialization: None,
            version: descriptor.version.clone(),
            commands: descriptor.commands.iter().map(|c| c.name.clone()).collect(),
            capabilities: descriptor
                .capabilities
                .iter()
                .map(|c| c.as_str().to_owned())
                .collect(),
            audit_status: audit_by_id.get(&descriptor.id).cloned(),
        })
        .collect()
}

fn append_linked_manager_items(
    items: &mut Vec<crate::ui::commands::ExtensionManagerItem>,
    registry: &ExtensionRegistry,
    session_enabled: Option<&std::collections::BTreeSet<String>>,
    enablement: &std::collections::BTreeMap<String, ExtensionEnablement>,
    audit_by_id: &std::collections::BTreeMap<String, String>,
) {
    let Ok(linked) = registry.linked_extensions() else {
        return;
    };
    for link in linked {
        if items.iter().any(|item| item.id == link.id) {
            continue;
        }
        items.push(crate::ui::commands::ExtensionManagerItem {
            id: link.id.clone(),
            display_name: link.descriptor.display_name.clone(),
            enabled: extension_is_enabled(&link.id, session_enabled, enablement),
            bundled: false,
            materialization: Some(link.materialization.as_str().to_owned()),
            version: link.descriptor.version.clone(),
            commands: link
                .descriptor
                .commands
                .iter()
                .map(|c| c.name.clone())
                .collect(),
            capabilities: link.descriptor.capabilities.clone(),
            audit_status: audit_by_id.get(&link.id).cloned(),
        });
    }
}

fn set_extension_enabled(id: &str, enable: bool) -> Result<()> {
    let registry = ExtensionRegistry::new(EulerHome::resolve()?)?;
    if enable {
        registry.enable(id)?;
    } else {
        registry.disable(id)?;
    }
    Ok(())
}

fn remove_linked_extension(id: &str) -> Result<String> {
    let registry = ExtensionRegistry::new(EulerHome::resolve()?)?;
    if let Some(linked) = registry.linked_extension(id)? {
        match linked.materialization {
            ExtensionMaterialization::Installed => {
                registry.uninstall_installed(id)?;
                Ok("uninstalled".to_owned())
            }
            ExtensionMaterialization::Linked => {
                registry.unlink(id)?;
                Ok("unlinked".to_owned())
            }
        }
    } else {
        Err(anyhow!("extension {id} is not linked or installed"))
    }
}

struct ExtensionAddReport {
    id: String,
    steps: Vec<String>,
}

impl ExtensionAddReport {
    fn steps_text(&self) -> String {
        self.steps.join("\n")
    }
}

fn add_local_extension(path: &Path) -> Result<ExtensionAddReport> {
    let mut steps = Vec::new();
    let package = load_extension_package(path)?;
    let id = package.descriptor.id.clone();
    steps.push(format!(
        "validate · ok · {id} v{}",
        package.descriptor.version
    ));
    let registry = ExtensionRegistry::new(EulerHome::resolve()?)?;
    let linked = registry.link_package(package.clone())?;
    steps.push(format!(
        "link · {} · {}",
        linked.materialization.as_str(),
        linked.source_path.display()
    ));
    let installed = registry.install_package(package)?;
    steps.push(format!(
        "install · {} · {}",
        installed.materialization.as_str(),
        installed.source_path.display()
    ));
    match registry.audit() {
        Ok(report) => {
            let warnings: Vec<_> = report
                .entries
                .iter()
                .filter(|entry| entry.id == id)
                .filter(|entry| {
                    !matches!(entry.issue_code, euler_core::ExtensionAuditIssueCode::Ok)
                })
                .map(|entry| format!("audit · {} · {:?}", entry.id, entry.issue_code))
                .collect();
            if warnings.is_empty() {
                steps.push("audit · ok".to_owned());
            } else {
                steps.extend(warnings);
            }
        }
        Err(error) => steps.push(format!("audit · unavailable: {error}")),
    }
    registry.enable(&id)?;
    steps.push("enable · ok".to_owned());
    Ok(ExtensionAddReport { id, steps })
}
