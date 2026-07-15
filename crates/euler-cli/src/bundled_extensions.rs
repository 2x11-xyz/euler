use anyhow::{anyhow, Result};
use euler_core::RoundObserverConfig;
use euler_extension_autoresearch::AutoresearchExtension;
use euler_extension_causal_dag::CausalDagExtension;
use euler_extension_code_swarm::CodeSwarmExtension;
use euler_extension_diagnostics_report::DiagnosticsReportExtension;
use euler_extension_maxproof::MaxProofExtension;
use euler_extension_session_export::SessionExportExtension;
use euler_sdk::{
    CommandDescriptor, CommandRegistrar, Extension, ExtensionCommand, ExtensionError,
    ExtensionManifest,
};
use serde::Serialize;
use std::collections::BTreeSet;
use std::num::NonZeroU64;
use std::sync::Arc;

const DEFAULT_OBSERVE_CADENCE_ROUNDS: u64 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ObserverCommandPair {
    pub(crate) brief: &'static str,
    pub(crate) apply: &'static str,
}

pub(crate) struct BundledExtension {
    pub(crate) extension: &'static dyn Extension,
    pub(crate) observer_commands: Option<ObserverCommandPair>,
}

impl BundledExtension {
    const fn new(extension: &'static dyn Extension) -> Self {
        Self {
            extension,
            observer_commands: None,
        }
    }

    const fn with_observer(
        extension: &'static dyn Extension,
        brief: &'static str,
        apply: &'static str,
    ) -> Self {
        Self {
            extension,
            observer_commands: Some(ObserverCommandPair { brief, apply }),
        }
    }
}

pub(crate) static BUNDLED_EXTENSIONS: &[BundledExtension] = &[
    BundledExtension::new(&SessionExportExtension),
    BundledExtension::with_observer(&CausalDagExtension, "observer-brief", "observer-apply"),
    BundledExtension::new(&CodeSwarmExtension),
    BundledExtension::new(&DiagnosticsReportExtension),
    BundledExtension::new(&AutoresearchExtension),
    BundledExtension::new(&MaxProofExtension),
];

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ObserveOptions {
    pub(crate) extension_id: Option<String>,
    pub(crate) cadence_rounds: Option<NonZeroU64>,
}

impl ObserveOptions {
    pub(crate) fn normalized(self) -> Result<Self> {
        match (&self.extension_id, self.cadence_rounds) {
            (None, Some(_)) => Err(anyhow!("--observe-cadence requires --observe")),
            _ => Ok(self),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BundledDescriptor {
    pub(crate) id: String,
    pub(crate) display_name: String,
    pub(crate) version: String,
    pub(crate) source_kind: &'static str,
    pub(crate) runtime_kind: &'static str,
    pub(crate) capabilities: Vec<euler_sdk::Capability>,
    pub(crate) commands: Vec<CommandDescriptor>,
    pub(crate) observer_commands: Option<ObserverCommandPair>,
    pub(crate) table_index: usize,
}

impl BundledDescriptor {
    pub(crate) fn command(&self, name: &str) -> Option<&CommandDescriptor> {
        self.commands.iter().find(|command| command.name == name)
    }

    pub(crate) fn to_info(&self) -> ExtensionInfo<'_> {
        ExtensionInfo {
            id: &self.id,
            display_name: &self.display_name,
            version: &self.version,
            source_kind: self.source_kind,
            runtime_kind: self.runtime_kind,
            capabilities: capability_strings(&self.capabilities),
            commands: self
                .commands
                .iter()
                .map(|command| CommandInfo {
                    name: &command.name,
                    display_name: &command.display_name,
                    summary: &command.summary,
                    required_capabilities: capability_strings(&command.required_capabilities),
                    invocation: command.invocation.as_str(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ExtensionInfo<'a> {
    id: &'a str,
    display_name: &'a str,
    version: &'a str,
    source_kind: &'a str,
    runtime_kind: &'a str,
    capabilities: Vec<&'static str>,
    commands: Vec<CommandInfo<'a>>,
}

#[derive(Debug, Serialize)]
struct CommandInfo<'a> {
    name: &'a str,
    display_name: &'a str,
    summary: &'a str,
    required_capabilities: Vec<&'static str>,
    /// Always emitted, including the `user` default: a reader must be able to
    /// tell an agent-only command from an invocable one without inferring it
    /// from the field's absence.
    invocation: &'static str,
}

#[derive(Default)]
struct DescriptorRegistrar(Vec<CommandDescriptor>);

impl CommandRegistrar for DescriptorRegistrar {
    fn register_command(&mut self, name: &str, command: Box<dyn ExtensionCommand>) {
        let mut descriptor = command.descriptor();
        if descriptor.name.is_empty() {
            descriptor.name = name.to_owned();
        }
        self.0.push(descriptor);
    }
}

pub(crate) fn bundled_descriptors() -> Result<Vec<BundledDescriptor>> {
    BUNDLED_EXTENSIONS
        .iter()
        .enumerate()
        .map(|(table_index, bundled)| bundled_descriptor(table_index, bundled))
        .collect()
}

pub(crate) fn bundled_descriptor_by_id(id: &str) -> Result<Option<BundledDescriptor>> {
    for (table_index, bundled) in BUNDLED_EXTENSIONS.iter().enumerate() {
        let manifest = bundled.extension.manifest();
        if manifest.id == id {
            return bundled_descriptor(table_index, bundled).map(Some);
        }
    }
    Ok(None)
}

pub(crate) fn bundled_extension_by_id(id: &str) -> Option<&'static BundledExtension> {
    BUNDLED_EXTENSIONS
        .iter()
        .find(|bundled| bundled.extension.manifest().id == id)
}

pub(crate) fn bundled_round_observer(
    options: &ObserveOptions,
    enabled: &BTreeSet<String>,
) -> Result<Option<(RoundObserverConfig, Arc<dyn Extension>)>> {
    let Some(id) = options.extension_id.as_deref() else {
        return Ok(None);
    };
    let (descriptor, commands) = observer_descriptor(id)?;
    if !enabled.contains(id) {
        return Err(anyhow!(
            "--observe {id} requires extension {id} to be enabled; enable it with --extensions {id} or your Euler extension registry/project config"
        ));
    }
    let cadence_rounds = options
        .cadence_rounds
        .unwrap_or_else(default_observe_cadence);
    let extension = bundled_extension_arc(&descriptor.id)?;
    Ok(Some((
        RoundObserverConfig {
            cadence_rounds,
            brief_command: commands.brief.to_owned(),
            apply_command: commands.apply.to_owned(),
        },
        extension,
    )))
}

pub(crate) fn resolve_round_observer(
    options: &ObserveOptions,
    enabled: &BTreeSet<String>,
) -> Result<Option<(RoundObserverConfig, Arc<dyn Extension>)>> {
    let Some(id) = options.extension_id.as_deref() else {
        return Ok(None);
    };
    if let Some(observer) =
        crate::extension_cli::resolve_live_linked_observer(id, options.cadence_rounds)?
    {
        return Ok(Some(observer));
    }
    bundled_round_observer(options, enabled)
}

fn observer_descriptor(id: &str) -> Result<(BundledDescriptor, ObserverCommandPair)> {
    let descriptor = bundled_descriptor_by_id(id)?.ok_or_else(|| unknown_observe_id_error(id))?;
    let commands = descriptor.observer_commands.ok_or_else(|| {
        anyhow!("--observe {id} is not supported: extension {id} declares no observer command pair")
    })?;
    if descriptor.command(commands.brief).is_none() {
        return Err(anyhow!(
            "bundled extension {id} observer brief command {} is not registered",
            commands.brief
        ));
    }
    if descriptor.command(commands.apply).is_none() {
        return Err(anyhow!(
            "bundled extension {id} observer apply command {} is not registered",
            commands.apply
        ));
    }
    Ok((descriptor, commands))
}

fn bundled_extension_arc(id: &str) -> Result<Arc<dyn Extension>> {
    let bundled = bundled_extension_by_id(id).ok_or_else(|| unknown_observe_id_error(id))?;
    Ok(Arc::new(StaticExtension(bundled.extension)))
}

fn unknown_observe_id_error(id: &str) -> anyhow::Error {
    let valid = bundled_descriptors()
        .map(|descriptors| {
            descriptors
                .into_iter()
                .filter(|descriptor| descriptor.observer_commands.is_some())
                .map(|descriptor| descriptor.id)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|_| "<registry unavailable>".to_owned());
    anyhow!("unknown extension id for --observe: {id}; observer-capable extensions: {valid}")
}

fn default_observe_cadence() -> NonZeroU64 {
    NonZeroU64::new(DEFAULT_OBSERVE_CADENCE_ROUNDS).expect("default observer cadence is non-zero")
}

struct StaticExtension(&'static dyn Extension);

impl Extension for StaticExtension {
    fn manifest(&self) -> ExtensionManifest {
        self.0.manifest()
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        self.0.register(registrar)
    }
}

fn bundled_descriptor(
    table_index: usize,
    bundled: &'static BundledExtension,
) -> Result<BundledDescriptor> {
    let manifest = bundled.extension.manifest();
    let mut registrar = DescriptorRegistrar::default();
    bundled
        .extension
        .register(&mut registrar)
        .map_err(|error| {
            anyhow!(
                "bundled extension {} registration failed: {error:?}",
                manifest.id
            )
        })?;
    Ok(BundledDescriptor {
        id: manifest.id,
        display_name: manifest.display_name,
        version: manifest.version,
        source_kind: "bundled",
        runtime_kind: "native-rust",
        capabilities: manifest.capabilities,
        commands: registrar.0,
        observer_commands: bundled.observer_commands,
        table_index,
    })
}

fn capability_strings(capabilities: &[euler_sdk::Capability]) -> Vec<&'static str> {
    capabilities
        .iter()
        .map(|capability| capability.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn bundled_registry_upholds_descriptor_invariants() {
        let descriptors = bundled_descriptors().expect("bundled descriptors");
        assert_eq!(descriptors.len(), BUNDLED_EXTENSIONS.len());
        let mut ids = BTreeSet::new();
        let mut command_names = BTreeSet::new();
        for descriptor in &descriptors {
            assert!(
                euler_sdk::extension_package::valid_extension_identifier(&descriptor.id),
                "invalid bundled extension id: {}",
                descriptor.id
            );
            assert!(ids.insert(descriptor.id.clone()), "duplicate extension id");
            assert!(!descriptor.display_name.is_empty());
            assert!(!descriptor.version.is_empty());
            assert!(!descriptor.commands.is_empty());
            let manifest: BTreeSet<_> = descriptor.capabilities.iter().copied().collect();
            for command in &descriptor.commands {
                assert!(!command.name.is_empty());
                assert!(
                    command_names.insert(command.name.clone()),
                    "duplicate command name: {}",
                    command.name
                );
                for capability in &command.required_capabilities {
                    assert!(
                        manifest.contains(capability),
                        "command {} requires {capability} missing from manifest of {}",
                        command.name,
                        descriptor.id
                    );
                }
            }
        }
    }
}
