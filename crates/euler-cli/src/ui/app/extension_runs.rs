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
    extension: LiveExtension,
    capabilities: Vec<Capability>,
}

#[derive(Clone)]
enum LiveExtension {
    Bundled(&'static dyn Extension),
    Managed(Box<euler_managed_process::ManagedProcessExtension>),
}

impl LiveExtension {
    fn as_extension(&self) -> &dyn Extension {
        match self {
            Self::Bundled(extension) => *extension,
            Self::Managed(extension) => extension.as_ref(),
        }
    }
}

impl ExtensionRunRequest {
    fn label(&self) -> String {
        format!("extension {}.{}", self.id, self.command)
    }
}

impl AppCore {
    pub(super) fn open_extension_manager(&mut self) -> CoreEffect {
        // A user-initiated open re-reads the registry so out-of-band
        // `euler extension` CLI changes show up; hot-path rebuilds reuse the
        // cached listing.
        self.invalidate_extension_registry_items();
        self.rebuild_bottom_surface();
        self.bottom.open_extension_manager();
        CoreEffect::Render
    }

    pub(super) fn toggle_extension(&mut self, id: String, enable: bool) -> CoreEffect {
        match set_extension_enabled(&id, enable) {
            Ok(()) => {
                self.invalidate_extension_registry_items();
                if let AppState::Idle { session } = &mut self.state {
                    session.set_extension_enabled(&id, enable);
                }
                self.rebuild_bottom_surface();
                let verb = if enable { "enabled" } else { "disabled" };
                self.teach_notice(format!("extension {verb}: {id}"));
                self.bottom.open_extension_manager();
                CoreEffect::Render
            }
            Err(error) => self.error_item(format!("extension toggle failed: {error}")),
        }
    }

    pub(super) fn show_extension_details(&mut self, id: String) -> CoreEffect {
        let (items, _) = self.current_extension_context();
        match items.into_iter().find(|item| item.id == id) {
            Some(item) => self.notice_item(item.details_text()),
            None => self.error_item(format!("unknown extension: {id}")),
        }
    }

    pub(super) fn remove_extension(&mut self, id: String) -> CoreEffect {
        match remove_linked_extension(&id) {
            Ok(message) => {
                self.invalidate_extension_registry_items();
                if let AppState::Idle { session } = &mut self.state {
                    session.set_extension_enabled(&id, false);
                }
                self.rebuild_bottom_surface();
                self.teach_notice(format!("extension removed: {id} · {message}"));
                CoreEffect::Render
            }
            Err(error) => self.error_item(format!("extension remove failed: {error}")),
        }
    }

    pub(super) fn add_extension(&mut self, path: String) -> CoreEffect {
        match add_local_extension(std::path::Path::new(&path)) {
            Ok(report) => {
                self.invalidate_extension_registry_items();
                if let AppState::Idle { session } = &mut self.state {
                    session.set_extension_enabled(&report.id, true);
                }
                self.rebuild_bottom_surface();
                self.teach_notice(format!(
                    "extension installed · {} · enabled for session",
                    report.id
                ));
                self.teach_notice(report.steps_text());
                CoreEffect::Render
            }
            Err(error) => self.error_item(format!("extension add failed: {error}")),
        }
    }

    pub(super) fn extension_run(
        &mut self,
        id: String,
        command: String,
        input: serde_json::Value,
        raw_args: Option<String>,
    ) -> CoreEffect {
        let mut request = match self.resolve_extension_run(id, command, input, raw_args) {
            Ok(request) => request,
            Err(error) => return self.error_item(format!("extension run failed: {error}")),
        };
        // code-swarm.review rides the shared resolution chain: explicit
        // --model flags win; otherwise the persisted project/user config
        // fills in the reviewer set; neither is the honest unconfigured
        // error — never a guessed model list (swarm contract).
        if request.id == "code-swarm" && request.command == "review" {
            match crate::code_swarm_config::apply_config_to_review_input(
                &crate::code_swarm_config::workspace_root(),
                request.input,
            ) {
                Ok(input) => request.input = input,
                Err(error) => return self.notice_item(error),
            }
        }
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
        if let Some((extension, descriptor)) =
            crate::extension_cli::resolve_live_linked_process_command(&id, &command)?
        {
            if descriptor.invocation.is_agent_only() {
                return Err(anyhow!(crate::agent_only_control_line_error(&id, &command)));
            }
            if raw_args.is_some() {
                return Err(anyhow!(
                    "linked managed-process commands accept JSON input in-session"
                ));
            }
            return Ok(ExtensionRunRequest {
                id,
                command,
                input,
                extension: LiveExtension::Managed(Box::new(extension)),
                capabilities: descriptor.required_capabilities,
            });
        }
        let descriptor =
            bundled_descriptor_by_id(&id)?.ok_or_else(|| anyhow!("unknown extension id: {id}"))?;
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
            extension: LiveExtension::Bundled(bundled.extension),
            capabilities: command_descriptor.required_capabilities.clone(),
        })
    }

    pub(super) fn spawn_extension_run(
        &mut self,
        request: ExtensionRunRequest,
        mut session: Box<Session<TuiDecider>>,
    ) {
        self.snapshot_permission_envelope(&session);
        let (worker_tx, worker_rx) = mpsc::channel();
        let mut worker_request = request.clone();
        let label = request.label();
        std::thread::spawn(move || {
            let start = session.events().len();
            // A request can wait behind an in-flight turn. Re-resolve at actual
            // execution time so disable, reload, or a manifest change revokes
            // launch consent even after the command was queued.
            if matches!(worker_request.extension, LiveExtension::Managed(_)) {
                match crate::extension_cli::resolve_live_linked_process_command(
                    &worker_request.id,
                    &worker_request.command,
                ) {
                    Ok(Some((extension, descriptor))) => {
                        worker_request.extension = LiveExtension::Managed(Box::new(extension));
                        worker_request.capabilities = descriptor.required_capabilities;
                    }
                    Ok(None) => {
                        let _ = worker_tx.send(TurnEvent::ExtensionDone {
                            request: worker_request,
                            outcome: ExtensionOutcome::Failed(
                                "linked extension is no longer available".to_owned(),
                            ),
                            events: Vec::new(),
                            session,
                        });
                        return;
                    }
                    Err(error) => {
                        let _ = worker_tx.send(TurnEvent::ExtensionDone {
                            request: worker_request,
                            outcome: ExtensionOutcome::Failed(error.to_string()),
                            events: Vec::new(),
                            session,
                        });
                        return;
                    }
                }
                session.set_extension_enabled(worker_request.id.clone(), true);
            }
            // Gated: declared capabilities become grants only through the
            // permission gate (the approval panel asks; session grants
            // cover later runs). Never pass a descriptor list as authority.
            let result = session.execute_extension_command_gated(
                worker_request.extension.as_extension(),
                &worker_request.command,
                worker_request.input.clone(),
                &worker_request.capabilities,
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
    append_linked_manager_items(&mut items, &registry, &audit_by_id);
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
            commands: descriptor
                .commands
                .iter()
                .map(|c| crate::ui::commands::ExtensionCommandItem {
                    name: c.name.clone(),
                    invocation: c.invocation,
                })
                .collect(),
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
    audit_by_id: &std::collections::BTreeMap<String, String>,
) {
    let Ok(linked) = registry.linked_extensions() else {
        return;
    };
    for link in linked {
        if items.iter().any(|item| item.id == link.id) {
            continue;
        }
        let linked_enabled =
            crate::extension_cli::current_linked_execution_enabled(registry, &link)
                .unwrap_or(false);
        items.push(crate::ui::commands::ExtensionManagerItem {
            id: link.id.clone(),
            display_name: link.descriptor.display_name.clone(),
            // Linked launch consent is persisted separately from the bundled
            // session selection set. A fresh session intentionally has no
            // linked IDs in that set; the worker inserts the ID only after it
            // revalidates current consent and fingerprint at execution time.
            enabled: linked_enabled,
            bundled: false,
            materialization: Some(link.materialization.as_str().to_owned()),
            version: link.descriptor.version.clone(),
            commands: link
                .descriptor
                .commands
                .iter()
                .map(|c| crate::ui::commands::ExtensionCommandItem {
                    name: c.name.clone(),
                    invocation: c.invocation,
                })
                .collect(),
            capabilities: link.descriptor.capabilities.clone(),
            audit_status: audit_by_id.get(&link.id).cloned(),
        });
    }
}

fn set_extension_enabled(id: &str, enable: bool) -> Result<()> {
    if crate::extension_cli::set_live_linked_process_enabled(id, enable)? {
        return Ok(());
    }
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
