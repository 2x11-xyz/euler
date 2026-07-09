mod output;

use crate::bundled_extensions::{
    bundled_descriptor_by_id, bundled_descriptors, bundled_extension_by_id, BundledDescriptor,
};
use crate::extension_enablement::RegistryResolution;
use crate::offline_extension_runner::{execute_offline_extension_run, OfflineExtensionRun};
use anyhow::{anyhow, Result};
use euler_core::{
    EulerHome, ExtensionAuditErrorReport, ExtensionEnablement, ExtensionMaterialization,
    ExtensionRegistry, ExtensionRegistryError, LinkedExtension,
};
use euler_sdk::{
    load_extension_package, valid_extension_identifier, ArgSpec, ArgValueKind, CommandDescriptor,
};
use output::{
    installed_info_summary, linked_info, linked_link_info, package_validation_info, search_matches,
    search_result_for_bundled, search_result_for_linked, sort_search_results, SearchOutput,
    UninstallInfo, UnlinkInfo,
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
    for descriptor in bundled_descriptors()? {
        let status = status_label(registry.state(&descriptor.id));
        writeln!(stdout, "{} {status}", descriptor.id)?;
    }
    for linked in registry.linked_extensions()? {
        writeln!(
            stdout,
            "{} {} {}",
            linked.id,
            linked.status.as_str(),
            linked.materialization.as_str()
        )?;
    }
    Ok(())
}

fn run_status(id: &str, stdout: &mut dyn Write) -> Result<()> {
    let registry = extension_registry()?;
    if validate_known_extension_id(id).is_ok() {
        writeln!(stdout, "{} {}", id, status_label(registry.state(id)))?;
    } else if let Some(linked) = linked_extension(&registry, id)? {
        writeln!(
            stdout,
            "{} {} {}",
            linked.id,
            linked.status.as_str(),
            linked.materialization.as_str()
        )?;
    } else {
        validate_known_extension_id(id)?;
    }
    Ok(())
}

fn run_info(id: &str, stdout: &mut dyn Write) -> Result<()> {
    let registry = extension_registry()?;
    if let Some(linked) = linked_extension(&registry, id)? {
        writeln!(stdout, "{}", serde_json::to_string(&linked_info(&linked))?)?;
    } else {
        let descriptor = validate_known_extension_id(id)?;
        writeln!(stdout, "{}", serde_json::to_string(&descriptor.to_info())?)?;
    }
    Ok(())
}

fn run_search(search: &ExtensionSearchArgs, stdout: &mut dyn Write) -> Result<()> {
    let registry = extension_registry().ok();
    let mut results = Vec::new();
    for descriptor in bundled_descriptors()? {
        results.push(search_result_for_bundled(
            &descriptor,
            bundled_status(registry.as_ref(), &descriptor.id),
        ));
    }
    if let Some(registry) = &registry {
        for linked in linked_extensions_for_search(registry) {
            if bundled_extension_by_id(&linked.id).is_none() {
                results.push(search_result_for_linked(&linked));
            }
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
    reject_bundled_id(&package.descriptor.id)?;
    writeln!(
        stdout,
        "{}",
        serde_json::to_string(&package_validation_info(&package, "valid"))?
    )?;
    Ok(())
}

fn run_link(path: &Path, stdout: &mut dyn Write) -> Result<()> {
    let package = load_extension_package(path)?;
    reject_bundled_id(&package.descriptor.id)?;
    let registry = extension_registry()?;
    let linked = registry.link_package(package)?;
    writeln!(
        stdout,
        "{}",
        serde_json::to_string(&linked_link_info(&linked))?
    )?;
    Ok(())
}

fn run_install(path: &Path, stdout: &mut dyn Write) -> Result<()> {
    let package = load_extension_package(path)?;
    reject_bundled_id(&package.descriptor.id)?;
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
    writeln!(
        stdout,
        "{}",
        serde_json::to_string(&linked_link_info(&linked))?
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
    if bundled_extension_by_id(id).is_some() {
        return Err(anyhow!("bundled extension cannot be uninstalled: {id}"));
    }
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
        return Err(non_runnable_extension_error(&linked, "enable"));
    }
    validate_known_extension_id(id)?;
    registry.enable(id)?;
    writeln!(stdout, "{id} enabled")?;
    Ok(())
}

fn run_disable(id: &str, stdout: &mut dyn Write) -> Result<()> {
    let registry = extension_registry()?;
    if let Some(linked) = linked_extension(&registry, id)? {
        return Err(non_runnable_extension_error(&linked, "disable"));
    }
    validate_known_extension_id(id)?;
    registry.disable(id)?;
    writeln!(stdout, "{id} disabled")?;
    Ok(())
}

fn run_extension(run: ExtensionRunArgs, stdout: &mut dyn Write) -> Result<()> {
    let registry = extension_registry()?;
    if let Some(linked) = linked_extension(&registry, &run.id)? {
        validate_linked_command(&linked, &run.command)?;
        return Err(non_runnable_extension_error(&linked, "run"));
    }
    validate_known_extension_id(&run.id)?;
    // Registry corruption must fail closed before target-dependent work; the
    // project overlay then belongs to the TARGET session's directory, not the
    // caller's shell CWD: `euler extension run <ref> /path/to/log` invoked
    // from anywhere must gate against the session's own project policy.
    let mut resolution = RegistryResolution::load()?;
    let target = crate::session_lifecycle::resolve_resume_target(run.target)?;
    let root = target
        .log_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    resolution.apply_project(&root)?;
    if !resolution.enabled.contains(&run.id) {
        return Err(anyhow!("extension disabled: {}", run.id));
    }
    let descriptor = validate_known_extension_command(&run.id, &run.command)?;
    let bundled = bundled_extension_by_id(&run.id)
        .ok_or_else(|| anyhow!("unknown extension id: {}", run.id))?;
    let output = execute_offline_extension_run(OfflineExtensionRun {
        extension_id: &run.id,
        command: descriptor
            .command(&run.command)
            .expect("validated bundled command must exist"),
        extension: bundled.extension,
        target: target.log_path.clone(),
        input: run.input,
    })?;
    writeln!(stdout, "{}", serde_json::to_string(&output)?)?;
    Ok(())
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

fn linked_extension(registry: &ExtensionRegistry, id: &str) -> Result<Option<LinkedExtension>> {
    validate_extension_id_shape(id)?;
    Ok(registry.linked_extension(id)?)
}

fn status_label(state: Result<ExtensionEnablement, ExtensionRegistryError>) -> String {
    match state {
        Ok(ExtensionEnablement::Enabled) => "enabled".to_owned(),
        Ok(ExtensionEnablement::Disabled) => "disabled".to_owned(),
        Err(error) => format!("unavailable: {error}"),
    }
}

fn bundled_status(registry: Option<&ExtensionRegistry>, id: &str) -> &'static str {
    let Some(registry) = registry else {
        return "unavailable";
    };
    match registry.state(id) {
        Ok(ExtensionEnablement::Enabled) => "enabled",
        Ok(ExtensionEnablement::Disabled) => "disabled",
        Err(_) => "unavailable",
    }
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
    let descriptor =
        bundled_descriptor_by_id(&id)?.and_then(|descriptor| descriptor.command(&command).cloned());
    let input = parse_extension_run_input(&reference, descriptor.as_ref(), args)?;
    Ok(ExtensionRunArgs {
        id,
        command,
        target,
        input,
    })
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
                if runtime_kind.is_some() {
                    return Err(anyhow!("--runtime was provided more than once"));
                }
                runtime_kind = Some(required_arg(args, "--runtime requires a runtime kind")?);
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

/// Shared by the CLI argv path and the TUI slash path (`--flag value…` after
/// an extension token) so both surfaces honor the same ArgSpec contract.
pub(crate) fn parse_extension_run_input(
    reference: &str,
    descriptor: Option<&CommandDescriptor>,
    args: &mut impl Iterator<Item = String>,
) -> Result<serde_json::Value> {
    let mut input = serde_json::Map::new();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(arg) = args.next() {
        let Some(flag) = arg.strip_prefix("--") else {
            return Err(anyhow!("unknown extension run argument: {arg}"));
        };
        let Some(spec) =
            descriptor.and_then(|descriptor| descriptor.args.iter().find(|spec| spec.flag == flag))
        else {
            return Err(anyhow!("--{flag} is not supported by {reference}"));
        };
        if !spec.repeatable && !seen.insert(spec.flag.clone()) {
            return Err(anyhow!("--{} was provided more than once", spec.flag));
        }
        let value = parse_arg_value(spec, args)?;
        insert_input_value(&mut input, &spec.input_key, value, spec.repeatable)?;
    }
    if let Some(descriptor) = descriptor {
        for spec in descriptor.args.iter().filter(|spec| spec.required) {
            if !seen.contains(&spec.flag) {
                return Err(anyhow!(
                    "extension run {reference} requires --{} <{}>",
                    spec.flag,
                    value_shape(&spec.value_kind)
                ));
            }
        }
    }
    Ok(serde_json::Value::Object(input))
}

fn parse_arg_value(
    spec: &ArgSpec,
    args: &mut impl Iterator<Item = String>,
) -> Result<serde_json::Value> {
    match &spec.value_kind {
        ArgValueKind::PositiveInt { max } => parse_positive_value(args, &spec.flag, *max),
        ArgValueKind::BoundedString { max_bytes } => {
            parse_bounded_string(args, &spec.flag, *max_bytes).map(serde_json::Value::String)
        }
        ArgValueKind::StringList => required_arg(args, format!("--{} requires a value", spec.flag))
            .map(serde_json::Value::String),
        ArgValueKind::JsonObjectFile {
            max_bytes,
            reject_wrapper_key,
        } => parse_json_object_file(args, &spec.flag, *max_bytes, reject_wrapper_key.as_deref()),
    }
}

fn parse_positive_value(
    args: &mut impl Iterator<Item = String>,
    flag: &str,
    max: Option<usize>,
) -> Result<serde_json::Value> {
    let message = format!("--{flag} requires a positive integer");
    let value = required_arg(args, &message)?;
    let parsed = value
        .parse::<usize>()
        .map_err(|_| anyhow!(message.clone()))?;
    if parsed == 0 {
        return Err(anyhow!(message));
    }
    if let Some(max) = max {
        if parsed > max {
            return Err(anyhow!("--{flag} must be at most {max}"));
        }
    }
    Ok(parsed.into())
}

fn parse_bounded_string(
    args: &mut impl Iterator<Item = String>,
    flag: &str,
    max_bytes: usize,
) -> Result<String> {
    let value = required_arg(args, format!("--{flag} requires a value"))?;
    if value.is_empty() {
        return Err(anyhow!("--{flag} requires a value"));
    }
    if value.len() > max_bytes {
        return Err(anyhow!("--{flag} must be at most {max_bytes} bytes"));
    }
    if value.chars().any(char::is_control) {
        return Err(anyhow!("--{flag} must not contain control characters"));
    }
    Ok(value)
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

fn insert_input_value(
    input: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: serde_json::Value,
    repeatable: bool,
) -> Result<()> {
    if let Some((outer, inner)) = key.split_once('.') {
        let entry = input
            .entry(outer.to_owned())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        let object = entry
            .as_object_mut()
            .ok_or_else(|| anyhow!("conflicting nested input key: {key}"))?;
        object.insert(inner.to_owned(), value);
        return Ok(());
    }
    if repeatable {
        input
            .entry(key.to_owned())
            .or_insert_with(|| serde_json::Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| anyhow!("conflicting repeated input key: {key}"))?
            .push(value);
    } else {
        input.insert(key.to_owned(), value);
    }
    Ok(())
}

fn value_shape(kind: &ArgValueKind) -> &'static str {
    match kind {
        ArgValueKind::PositiveInt { .. } => "positive-int",
        ArgValueKind::BoundedString { .. } => "value",
        ArgValueKind::StringList => "value",
        ArgValueKind::JsonObjectFile { .. } => "json-file",
    }
}

fn validate_known_extension_id(id: &str) -> Result<BundledDescriptor> {
    validate_extension_id_shape(id)?;
    bundled_descriptor_by_id(id)?.ok_or_else(|| anyhow!("unknown extension id: {id}"))
}

fn validate_known_extension_command(id: &str, command: &str) -> Result<BundledDescriptor> {
    let descriptor = validate_known_extension_id(id)?;
    validate_extension_command_shape(command)?;
    if descriptor.command(command).is_some() {
        Ok(descriptor)
    } else {
        Err(anyhow!("unknown command for extension {id}: {command}"))
    }
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

fn reject_bundled_id(id: &str) -> Result<()> {
    if bundled_extension_by_id(id).is_some() {
        Err(anyhow!(
            "extension id is reserved by bundled extension: {id}"
        ))
    } else {
        Ok(())
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
            "maxproof.population-brief".to_owned(),
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
            ExtensionArgs::parse(&mut ["info".to_owned(), "causal-dag".to_owned()].into_iter())
                .expect("parse"),
            ExtensionArgs {
                action: ExtensionAction::Info {
                    id: "causal-dag".to_owned()
                }
            }
        );
    }
}
