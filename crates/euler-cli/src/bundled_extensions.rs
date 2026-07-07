use anyhow::{anyhow, Result};
use euler_extension_autoresearch::AutoresearchExtension;
use euler_extension_causal_dag::CausalDagExtension;
use euler_extension_code_swarm::CodeSwarmExtension;
use euler_extension_diagnostics_report::DiagnosticsReportExtension;
use euler_extension_maxproof::MaxProofExtension;
use euler_extension_session_export::SessionExportExtension;
use euler_sdk::{CommandDescriptor, CommandRegistrar, Extension, ExtensionCommand};
use serde::Serialize;

pub(crate) struct BundledExtension(pub(crate) &'static dyn Extension);

pub(crate) static BUNDLED_EXTENSIONS: &[BundledExtension] = &[
    BundledExtension(&SessionExportExtension),
    BundledExtension(&CausalDagExtension),
    BundledExtension(&CodeSwarmExtension),
    BundledExtension(&DiagnosticsReportExtension),
    BundledExtension(&AutoresearchExtension),
    BundledExtension(&MaxProofExtension),
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BundledDescriptor {
    pub(crate) id: String,
    pub(crate) display_name: String,
    pub(crate) version: String,
    pub(crate) source_kind: &'static str,
    pub(crate) runtime_kind: &'static str,
    pub(crate) capabilities: Vec<euler_sdk::Capability>,
    pub(crate) commands: Vec<CommandDescriptor>,
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
        let manifest = bundled.0.manifest();
        if manifest.id == id {
            return bundled_descriptor(table_index, bundled).map(Some);
        }
    }
    Ok(None)
}

pub(crate) fn bundled_extension_by_id(id: &str) -> Option<&'static BundledExtension> {
    BUNDLED_EXTENSIONS
        .iter()
        .find(|bundled| bundled.0.manifest().id == id)
}

fn bundled_descriptor(
    table_index: usize,
    bundled: &'static BundledExtension,
) -> Result<BundledDescriptor> {
    let manifest = bundled.0.manifest();
    let mut registrar = DescriptorRegistrar::default();
    bundled.0.register(&mut registrar).map_err(|error| {
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
