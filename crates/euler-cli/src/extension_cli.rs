//! Justification for >1000 lines: this module owns the extension management
//! CLI and linked-package registry workflow; runtime-specific observer logic
//! is extracted into focused submodules.

mod observer;
mod output;

use crate::cli::set_once;
use crate::offline_extension_runner::{execute_offline_extension_run, OfflineExtensionRun};
use anyhow::{anyhow, Result};
use euler_core::{
    EulerHome, ExtensionAuditErrorReport, ExtensionMaterialization, ExtensionRegistry,
    LinkedExtension, LinkedExtensionStatus,
};
use euler_managed_process::ManagedProcessExtension;
use euler_sdk::{
    load_extension_package, managed_process_entrypoint_from_manifest_bytes,
    valid_extension_identifier, CommandDescriptor,
};
use output::{
    installed_info_summary, linked_info, linked_link_info, linked_status, package_validation_info,
    search_matches, search_result_for_linked, sort_search_results, SearchOutput, UninstallInfo,
    UnlinkInfo,
};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ExtensionArgs {
    pub(super) action: ExtensionAction,
}

impl ExtensionArgs {
    pub(super) fn parse(args: &mut impl Iterator<Item = String>) -> Result<Self> {
        let Some(subcommand) = args.next() else {
            return Err(anyhow!("extension requires a subcommand"));
        };
        let action = match subcommand.as_str() {
            "list" => {
                ensure_no_extra_args("extension list", args)?;
                ExtensionAction::List
            }
            "status" => {
                let id = required_arg(args, "extension status requires an extension id")?;
                ensure_no_extra_args("extension status", args)?;
                ExtensionAction::Status { id }
            }
            "info" => {
                let id = required_arg(args, "extension info requires an extension id")?;
                ensure_no_extra_args("extension info", args)?;
                ExtensionAction::Info { id }
            }
            "search" => ExtensionAction::Search(parse_extension_search(args)?),
            "audit" => {
                ensure_no_extra_args("extension audit", args)?;
                ExtensionAction::Audit
            }
            "validate" => {
                let path = PathBuf::from(required_arg(
                    args,
                    "extension validate requires an extension directory",
                )?);
                ensure_no_extra_args("extension validate", args)?;
                ExtensionAction::Validate { path }
            }
            "link" => {
                let path = PathBuf::from(required_arg(
                    args,
                    "extension link requires an extension directory",
                )?);
                parse_scope(args, "extension link")?;
                ExtensionAction::Link { path }
            }
            "install" => {
                let path = PathBuf::from(required_arg(
                    args,
                    "extension install requires a local extension directory",
                )?);
                parse_scope(args, "extension install")?;
                ExtensionAction::Install { path }
            }
            "reload" => {
                let id = required_arg(args, "extension reload requires an extension id")?;
                parse_scope(args, "extension reload")?;
                ExtensionAction::Reload { id }
            }
            "unlink" => {
                let id = required_arg(args, "extension unlink requires an extension id")?;
                parse_scope(args, "extension unlink")?;
                ExtensionAction::Unlink { id }
            }
            "uninstall" => {
                let id = required_arg(args, "extension uninstall requires an extension id")?;
                parse_scope(args, "extension uninstall")?;
                ExtensionAction::Uninstall { id }
            }
            "enable" => {
                let id = required_arg(args, "extension enable requires an extension id")?;
                ensure_no_extra_args("extension enable", args)?;
                ExtensionAction::Enable { id }
            }
            "disable" => {
                let id = required_arg(args, "extension disable requires an extension id")?;
                ensure_no_extra_args("extension disable", args)?;
                ExtensionAction::Disable { id }
            }
            "run" => ExtensionAction::Run(parse_extension_run(args)?),
            _ => return Err(anyhow!("unknown extension subcommand: {subcommand}")),
        };
        Ok(Self { action })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ExtensionAction {
    List,
    Status { id: String },
    Info { id: String },
    Search(ExtensionSearchArgs),
    Audit,
    Validate { path: PathBuf },
    Link { path: PathBuf },
    Install { path: PathBuf },
    Reload { id: String },
    Unlink { id: String },
    Uninstall { id: String },
    Enable { id: String },
    Disable { id: String },
    Run(ExtensionRunArgs),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ExtensionRunArgs {
    pub(super) id: String,
    pub(super) command: String,
    pub(super) target: PathBuf,
    pub(super) input: serde_json::Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ExtensionSearchArgs {
    pub(super) query: Option<String>,
    pub(super) capabilities: Vec<String>,
    pub(super) runtime_kind: Option<String>,
}

pub(super) fn run_extension_command(extension: ExtensionArgs) -> Result<()> {
    let mut stdout = io::stdout();
    run_extension_command_with_writer(extension, &mut stdout)
}

fn run_extension_command_with_writer(
    extension: ExtensionArgs,
    stdout: &mut dyn Write,
) -> Result<()> {
    match extension.action {
        ExtensionAction::List => run_list(stdout),
        ExtensionAction::Status { id } => run_status(&id, stdout),
        ExtensionAction::Info { id } => run_info(&id, stdout),
        ExtensionAction::Search(search) => run_search(&search, stdout),
        ExtensionAction::Audit => run_audit(stdout),
        ExtensionAction::Validate { path } => run_validate(&path, stdout),
        ExtensionAction::Link { path } => run_link(&path, stdout),
        ExtensionAction::Install { path } => run_install(&path, stdout),
        ExtensionAction::Reload { id } => run_reload(&id, stdout),
        ExtensionAction::Unlink { id } => run_unlink(&id, stdout),
        ExtensionAction::Uninstall { id } => run_uninstall(&id, stdout),
        ExtensionAction::Enable { id } => run_enable(&id, stdout),
        ExtensionAction::Disable { id } => run_disable(&id, stdout),
        ExtensionAction::Run(run) => run_extension(run, stdout),
    }
}

fn extension_registry() -> Result<ExtensionRegistry> {
    Ok(ExtensionRegistry::new(EulerHome::resolve()?)?)
}

fn extension_registry_read_only() -> Result<ExtensionRegistry> {
    Ok(ExtensionRegistry::open_read_only(EulerHome::resolve()?))
}

fn run_list(stdout: &mut dyn Write) -> Result<()> {
    let registry = extension_registry()?;
    for linked in registry.linked_extensions()? {
        let execution_enabled = current_linked_execution_enabled(&registry, &linked)?;
        writeln!(
            stdout,
            "{} {} {}",
            linked.id,
            linked_status(&linked, execution_enabled),
            linked.materialization.as_str()
        )?;
    }
    Ok(())
}

fn run_status(id: &str, stdout: &mut dyn Write) -> Result<()> {
    let registry = extension_registry()?;
    let Some(linked) = linked_extension(&registry, id)? else {
        return Err(anyhow!("unknown extension id: {id}"));
    };
    let execution_enabled = current_linked_execution_enabled(&registry, &linked)?;
    writeln!(
        stdout,
        "{} {} {}",
        linked.id,
        linked_status(&linked, execution_enabled),
        linked.materialization.as_str()
    )?;
    Ok(())
}

fn run_info(id: &str, stdout: &mut dyn Write) -> Result<()> {
    let registry = extension_registry()?;
    if let Some(linked) = linked_extension(&registry, id)? {
        let entrypoint = linked_process_entrypoint(&linked)?;
        let execution_enabled =
            entrypoint.is_some() && registry.linked_execution_enabled(&linked.id)?.is_enabled();
        writeln!(
            stdout,
            "{}",
            serde_json::to_string(&linked_info(
                &linked,
                entrypoint.as_ref(),
                execution_enabled,
            ))?
        )?;
    } else {
        return Err(anyhow!("unknown extension id: {id}"));
    }
    Ok(())
}

fn run_search(search: &ExtensionSearchArgs, stdout: &mut dyn Write) -> Result<()> {
    let registry = extension_registry().ok();
    let mut results = Vec::new();
    if let Some(registry) = &registry {
        for linked in linked_extensions_for_search(registry) {
            let execution_enabled = current_linked_execution_enabled(registry, &linked)?;
            results.push(search_result_for_linked(&linked, execution_enabled));
        }
    }
    results.retain(|result| search_matches(search, result));
    sort_search_results(&mut results);

    serde_json::to_writer_pretty(&mut *stdout, &SearchOutput::new(search, results))?;
    writeln!(stdout)?;
    Ok(())
}

fn run_audit(stdout: &mut dyn Write) -> Result<()> {
    let registry = extension_registry_read_only()?;
    match registry.audit() {
        Ok(report) => {
            serde_json::to_writer_pretty(&mut *stdout, &report)?;
            writeln!(stdout)?;
            Ok(())
        }
        Err(error) => {
            serde_json::to_writer_pretty(
                &mut *stdout,
                &ExtensionAuditErrorReport::from_registry_error(&error),
            )?;
            writeln!(stdout)?;
            Err(anyhow!("extension audit failed"))
        }
    }
}

fn run_validate(path: &Path, stdout: &mut dyn Write) -> Result<()> {
    let package = load_extension_package(path)?;
    let entrypoint = package_process_entrypoint(&package)?;
    writeln!(
        stdout,
        "{}",
        serde_json::to_string(&package_validation_info(
            &package,
            entrypoint.as_ref(),
            "valid"
        ))?
    )?;
    Ok(())
}

fn run_link(path: &Path, stdout: &mut dyn Write) -> Result<()> {
    let package = load_extension_package(path)?;
    let entrypoint = package_process_entrypoint(&package)?;
    let registry = extension_registry()?;
    let linked = registry.link_package(package)?;
    writeln!(
        stdout,
        "{}",
        serde_json::to_string(&linked_link_info(&linked, entrypoint.as_ref(), false))?
    )?;
    Ok(())
}

fn run_install(path: &Path, stdout: &mut dyn Write) -> Result<()> {
    let package = load_extension_package(path)?;
    let registry = extension_registry()?;
    let installed = registry.install_package(package)?;
    writeln!(
        stdout,
        "{}",
        serde_json::to_string(&installed_info_summary(&installed))?
    )?;
    Ok(())
}

fn run_reload(id: &str, stdout: &mut dyn Write) -> Result<()> {
    validate_extension_id_shape(id)?;
    let registry = extension_registry()?;
    let linked = registry.reload_link(id)?;
    let entrypoint = linked_process_entrypoint(&linked)?;
    writeln!(
        stdout,
        "{}",
        serde_json::to_string(&linked_link_info(&linked, entrypoint.as_ref(), false))?
    )?;
    Ok(())
}

fn run_unlink(id: &str, stdout: &mut dyn Write) -> Result<()> {
    validate_extension_id_shape(id)?;
    let registry = extension_registry()?;
    registry.unlink(id)?;
    writeln!(
        stdout,
        "{}",
        serde_json::to_string(&UnlinkInfo {
            id,
            status: "unlinked"
        })?
    )?;
    Ok(())
}

fn run_uninstall(id: &str, stdout: &mut dyn Write) -> Result<()> {
    validate_extension_id_shape(id)?;
    let registry = extension_registry()?;
    let uninstalled = registry.uninstall_installed(id)?;
    writeln!(
        stdout,
        "{}",
        serde_json::to_string(&UninstallInfo {
            id: &uninstalled.id,
            source_kind: uninstalled.materialization.as_str(),
            status: "uninstalled",
        })?
    )?;
    Ok(())
}

fn run_enable(id: &str, stdout: &mut dyn Write) -> Result<()> {
    let registry = extension_registry()?;
    if let Some(linked) = linked_extension(&registry, id)? {
        let entrypoint = validate_linked_process_for_activation(&linked)?;
        registry.set_linked_execution_enabled(id, true)?;
        writeln!(
            stdout,
            "{id} enabled: {}",
            serde_json::to_string(&entrypoint.command)?
        )?;
        return Ok(());
    }
    Err(anyhow!("unknown extension id: {id}"))
}

fn run_disable(id: &str, stdout: &mut dyn Write) -> Result<()> {
    let registry = extension_registry()?;
    if let Some(linked) = linked_extension(&registry, id)? {
        if linked.materialization != ExtensionMaterialization::Linked {
            return Err(non_runnable_extension_error(&linked, "disable"));
        }
        if linked.descriptor.runtime_kind != "managed-process" {
            return Err(non_runnable_extension_error(&linked, "disable"));
        }
        registry.set_linked_execution_enabled(id, false)?;
        writeln!(stdout, "{id} disabled")?;
        return Ok(());
    }
    Err(anyhow!("unknown extension id: {id}"))
}

fn run_extension(run: ExtensionRunArgs, stdout: &mut dyn Write) -> Result<()> {
    let registry = extension_registry()?;
    if let Some(linked) = linked_extension(&registry, &run.id)? {
        validate_linked_command(&linked, &run.command)?;
        let package = load_enabled_linked_process(&registry, &linked)?;
        let extension = ManagedProcessExtension::from_package(&package)
            .map_err(|error| anyhow!(error.to_string()))?;
        let command = extension
            .command_descriptor(&run.command)
            .ok_or_else(|| anyhow!("unknown command for extension {}: {}", run.id, run.command))?
            .clone();
        if command.invocation.is_agent_only() {
            return Err(anyhow!(
                "{}.{} is agent-only: it is run by an agent in a live session, not by `euler \
                 extension run`. Start a session and ask for it in ordinary turn text.",
                run.id,
                run.command
            ));
        }
        announce_managed_process_capability_grant(&run.id, &command);
        let output = execute_offline_extension_run(OfflineExtensionRun {
            extension_id: &run.id,
            command: &command,
            extension: &extension,
            target: run.target,
            input: run.input,
        })?;
        writeln!(stdout, "{}", serde_json::to_string(&output)?)?;
        return Ok(());
    }
    Err(anyhow!("unknown extension id: {}", run.id))
}

fn announce_managed_process_capability_grant(id: &str, command: &CommandDescriptor) {
    if command.required_capabilities.is_empty() {
        return;
    }
    let granted = command
        .required_capabilities
        .iter()
        .map(|capability| capability.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!(
        "extension {id}.{}: granting declared capabilities for this run: {granted}",
        command.name
    );
}

fn non_runnable_extension_error(linked: &LinkedExtension, action: &str) -> anyhow::Error {
    match linked.materialization {
        ExtensionMaterialization::Linked if action == "run" => {
            anyhow!("linked extension is not runnable yet: {}", linked.id)
        }
        ExtensionMaterialization::Linked if action == "disable" => {
            anyhow!("linked extension is not enabled; use unlink: {}", linked.id)
        }
        ExtensionMaterialization::Linked => {
            anyhow!("linked extension needs review before {action}: {}", linked.id)
        }
        ExtensionMaterialization::Installed if action == "disable" => {
            anyhow!("installed extension is inert and not enabled; use uninstall: {}", linked.id)
        }
        ExtensionMaterialization::Installed => anyhow!(
            "installed extension is inert; reviewed execution grants are not implemented before {action}: {}",
            linked.id
        ),
    }
}

fn validate_linked_process_for_activation(
    linked: &LinkedExtension,
) -> Result<euler_sdk::ManagedProcessEntrypoint> {
    let package = load_linked_process_for_action(linked, "enable")?;
    managed_process_entrypoint_from_manifest_bytes(&package.manifest_bytes).map_err(Into::into)
}

fn package_process_entrypoint(
    package: &euler_sdk::LoadedExtensionPackage,
) -> Result<Option<euler_sdk::ManagedProcessEntrypoint>> {
    if package.descriptor.runtime_kind == "managed-process" {
        return managed_process_entrypoint_from_manifest_bytes(&package.manifest_bytes)
            .map(Some)
            .map_err(Into::into);
    }
    Ok(None)
}

fn linked_process_entrypoint(
    linked: &LinkedExtension,
) -> Result<Option<euler_sdk::ManagedProcessEntrypoint>> {
    if linked.status == LinkedExtensionStatus::Broken
        || linked.descriptor.runtime_kind != "managed-process"
    {
        return Ok(None);
    }
    let package = match load_extension_package(&linked.source_path) {
        Ok(package) if package.manifest_sha256 == linked.manifest_sha256 => package,
        Ok(_) | Err(_) => return Ok(None),
    };
    package_process_entrypoint(&package)
}

fn load_linked_process_for_action(
    linked: &LinkedExtension,
    action: &str,
) -> Result<euler_sdk::LoadedExtensionPackage> {
    if linked.materialization != ExtensionMaterialization::Linked {
        return Err(non_runnable_extension_error(linked, action));
    }
    if linked.status == LinkedExtensionStatus::Broken {
        return Err(anyhow!(
            "linked extension is broken; reload it before enabling: {}",
            linked.id
        ));
    }
    if linked.descriptor.runtime_kind != "managed-process" {
        return Err(anyhow!(
            "linked extension runtime is not runnable yet: {}",
            linked.descriptor.runtime_kind
        ));
    }
    let package = load_extension_package(&linked.source_path)?;
    if package.manifest_sha256 != linked.manifest_sha256 {
        return Err(anyhow!(
            "linked extension manifest changed; run `euler extension reload {}`, inspect it, then enable it",
            linked.id
        ));
    }
    Ok(package)
}

fn load_enabled_linked_process(
    registry: &ExtensionRegistry,
    linked: &LinkedExtension,
) -> Result<euler_sdk::LoadedExtensionPackage> {
    let package = load_linked_process_for_action(linked, "run")?;
    if !registry.linked_execution_enabled(&linked.id)?.is_enabled() {
        return Err(anyhow!(
            "linked extension is not enabled; run `euler extension enable {}` first",
            linked.id
        ));
    }
    Ok(package)
}

/// Resolve an explicitly enabled linked managed-process command for a live
/// session. The package is reloaded and fingerprint-checked on every run, so a
/// manifest change revokes launch consent just as it does for offline runs.
pub(crate) fn resolve_live_linked_process_command(
    id: &str,
    command: &str,
) -> Result<Option<(ManagedProcessExtension, CommandDescriptor)>> {
    let registry = extension_registry()?;
    let Some(linked) = linked_extension(&registry, id)? else {
        return Ok(None);
    };
    validate_linked_command(&linked, command)?;
    let package = load_enabled_linked_process(&registry, &linked)?;
    let extension = ManagedProcessExtension::from_package(&package)
        .map_err(|error| anyhow!(error.to_string()))?;
    let descriptor = extension
        .command_descriptor(command)
        .ok_or_else(|| anyhow!("unknown command for extension {id}: {command}"))?
        .clone();
    Ok(Some((extension, descriptor)))
}

pub(crate) use observer::{live_linked_extension_arc, resolve_round_observer, ObserveOptions};

/// Change linked-process launch consent through the same validation boundary
/// as the CLI enable/disable actions. Returns `false` when `id` is not linked.
pub(crate) fn set_live_linked_process_enabled(id: &str, enabled: bool) -> Result<bool> {
    let registry = extension_registry()?;
    let Some(linked) = linked_extension(&registry, id)? else {
        return Ok(false);
    };
    if enabled {
        validate_linked_process_for_activation(&linked)?;
    } else if linked.materialization != ExtensionMaterialization::Linked
        || linked.descriptor.runtime_kind != "managed-process"
    {
        return Err(non_runnable_extension_error(&linked, "disable"));
    }
    registry.set_linked_execution_enabled(id, enabled)?;
    Ok(true)
}

pub(crate) fn current_linked_execution_enabled(
    registry: &ExtensionRegistry,
    linked: &LinkedExtension,
) -> Result<bool> {
    Ok(registry.linked_execution_enabled(&linked.id)?.is_enabled()
        && linked_process_entrypoint(linked)?.is_some())
}

fn linked_extension(registry: &ExtensionRegistry, id: &str) -> Result<Option<LinkedExtension>> {
    validate_extension_id_shape(id)?;
    Ok(registry.linked_extension(id)?)
}

fn linked_extensions_for_search(registry: &ExtensionRegistry) -> Vec<LinkedExtension> {
    registry.linked_extensions().unwrap_or_default()
}

fn parse_extension_run(args: &mut impl Iterator<Item = String>) -> Result<ExtensionRunArgs> {
    let reference = required_arg(
        args,
        "extension run requires an extension command reference",
    )?;
    let (id, command) = parse_extension_command_reference(&reference)?;
    validate_extension_id_shape(&id)?;
    validate_extension_command_shape(&command)?;
    let target = required_arg(
        args,
        format!("extension run {reference} requires a session id, name, or events path"),
    )?;
    // A flag here means the caller forgot the session positional; swallowing
    // it as a session id produces a baffling error about the flag's value.
    if target.starts_with("--") {
        return Err(anyhow!(
            "extension run {reference} requires a session id, name, or events path before `{target}`"
        ));
    }
    let target = PathBuf::from(target);
    let input = parse_managed_process_input(&reference, args)?;
    Ok(ExtensionRunArgs {
        id,
        command,
        target,
        input,
    })
}

fn parse_managed_process_input(
    reference: &str,
    args: &mut impl Iterator<Item = String>,
) -> Result<serde_json::Value> {
    let Some(flag) = args.next() else {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    };
    if flag != "--input-file" {
        return Err(anyhow!(
            "{reference} accepts only --input-file <json-object-file> until it is loaded as a managed-process package"
        ));
    }
    let input = parse_json_object_file(args, "input-file", 64 * 1024, None)?;
    ensure_no_extra_args("managed-process extension run", args)?;
    Ok(input)
}

fn parse_extension_search(args: &mut impl Iterator<Item = String>) -> Result<ExtensionSearchArgs> {
    let mut query = None;
    let mut capabilities = Vec::new();
    let mut runtime_kind = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--capability" => {
                capabilities.push(required_arg(
                    args,
                    "--capability requires a capability value",
                )?);
            }
            "--runtime" => {
                set_once(&mut runtime_kind, "--runtime", || {
                    required_arg(args, "--runtime requires a runtime kind")
                })?;
            }
            "--" => {
                let value = required_arg(args, "extension search requires a query after --")?;
                set_search_query(&mut query, value)?;
                ensure_no_extra_args("extension search", args)?;
                break;
            }
            _ if arg.starts_with("--") => {
                return Err(anyhow!("unknown extension search argument: {arg}"));
            }
            _ => set_search_query(&mut query, arg)?,
        }
    }
    let query = query.and_then(|query: String| {
        let trimmed = query.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    });
    if query.is_none() && capabilities.is_empty() && runtime_kind.is_none() {
        return Err(anyhow!(
            "extension search requires a query or at least one filter"
        ));
    }
    Ok(ExtensionSearchArgs {
        query,
        capabilities,
        runtime_kind,
    })
}

fn set_search_query(query: &mut Option<String>, value: String) -> Result<()> {
    if query.is_some() {
        return Err(anyhow!("extension search accepts only one query"));
    }
    *query = Some(value);
    Ok(())
}

fn parse_extension_command_reference(reference: &str) -> Result<(String, String)> {
    let Some((id, command)) = reference.split_once('.') else {
        return Err(anyhow!("invalid extension command reference: {reference}"));
    };
    if id.is_empty() || command.is_empty() || command.contains('.') {
        return Err(anyhow!("invalid extension command reference: {reference}"));
    }
    Ok((id.to_owned(), command.to_owned()))
}

fn parse_json_object_file(
    args: &mut impl Iterator<Item = String>,
    flag: &str,
    max_bytes: usize,
    reject_wrapper_key: Option<&str>,
) -> Result<serde_json::Value> {
    let value = required_arg(args, format!("--{flag} requires a JSON file path"))?;
    if value.is_empty() {
        return Err(anyhow!("--{flag} requires a JSON file path"));
    }
    if value == "-" {
        return Err(anyhow!(
            "--{flag} does not support stdin; provide a JSON file"
        ));
    }
    let path = Path::new(&value);
    let metadata = std::fs::metadata(path)
        .map_err(|error| anyhow!("could not read --{flag} JSON file: {error}"))?;
    if !metadata.is_file() {
        return Err(anyhow!("--{flag} must be a regular JSON file"));
    }
    if metadata.len() > max_bytes as u64 {
        return Err(anyhow!("--{flag} file exceeds {max_bytes} bytes"));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| anyhow!("could not read --{flag} JSON file: {error}"))?;
    if bytes.is_empty() {
        return Err(anyhow!("--{flag} JSON file is empty"));
    }
    if bytes.len() > max_bytes {
        return Err(anyhow!("--{flag} file exceeds {max_bytes} bytes"));
    }
    let json: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| anyhow!("--{flag} must contain one JSON object: {error}"))?;
    let object = json
        .as_object()
        .ok_or_else(|| anyhow!("--{flag} must contain a JSON object"))?;
    if let Some(key) = reject_wrapper_key {
        if object.contains_key(key) {
            return Err(anyhow!(
                "--{flag} expects the raw object, not a {key} wrapper"
            ));
        }
    }
    Ok(json)
}

fn validate_linked_command(linked: &LinkedExtension, command: &str) -> Result<()> {
    validate_extension_command_shape(command)?;
    if linked.descriptor.command(command).is_some() {
        Ok(())
    } else {
        Err(anyhow!(
            "unknown command for extension {}: {command}",
            linked.id
        ))
    }
}

fn validate_extension_id_shape(id: &str) -> Result<()> {
    if valid_extension_identifier(id) {
        Ok(())
    } else {
        Err(anyhow!("invalid extension id: {id}"))
    }
}

fn validate_extension_command_shape(command: &str) -> Result<()> {
    if valid_extension_identifier(command) {
        Ok(())
    } else {
        Err(anyhow!("invalid extension command: {command}"))
    }
}

fn required_arg(
    args: &mut impl Iterator<Item = String>,
    message: impl Into<String>,
) -> Result<String> {
    args.next().ok_or_else(|| anyhow!(message.into()))
}

fn ensure_no_extra_args(
    context: &'static str,
    args: &mut impl Iterator<Item = String>,
) -> Result<()> {
    if let Some(arg) = args.next() {
        Err(anyhow!("{context} does not accept arguments: {arg}"))
    } else {
        Ok(())
    }
}

fn parse_scope(args: &mut impl Iterator<Item = String>, context: &'static str) -> Result<()> {
    let Some(arg) = args.next() else {
        return Ok(());
    };
    if arg != "--scope" {
        return Err(anyhow!("{context} does not accept arguments: {arg}"));
    }
    let scope = required_arg(args, "--scope requires a value")?;
    if scope != "user" {
        return Err(anyhow!("unsupported extension scope: {scope}"));
    }
    ensure_no_extra_args(context, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_run_flag_in_session_position_is_rejected_plainly() {
        let mut args = [
            "session-export.session-export".to_owned(),
            "--problem".to_owned(),
            "prove it".to_owned(),
        ]
        .into_iter();
        let error = parse_extension_run(&mut args).expect_err("flag as session");
        let message = error.to_string();
        assert!(
            message.contains("requires a session id, name, or events path before `--problem`"),
            "message: {message}"
        );
    }

    #[test]
    fn extension_info_parse_shape_is_distinct_from_status() {
        assert_eq!(
            ExtensionArgs::parse(&mut ["info".to_owned(), "session-export".to_owned()].into_iter())
                .expect("parse"),
            ExtensionArgs {
                action: ExtensionAction::Info {
                    id: "session-export".to_owned()
                }
            }
        );
    }
}
