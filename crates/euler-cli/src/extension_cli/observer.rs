use super::{
    extension_registry, linked_extension, load_enabled_linked_process,
    reject_bundled_link_collision,
};
use anyhow::{anyhow, Result};
use euler_core::RoundObserverConfig;
use euler_managed_process::ManagedProcessExtension;
use euler_sdk::{
    CommandContext, CommandDescriptor, CommandRegistrar, Extension, ExtensionCommand,
    ExtensionError, ExtensionManifest, HostApi,
};
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::Arc;

pub(crate) fn resolve(
    id: &str,
    cadence_override: Option<NonZeroU64>,
) -> Result<Option<(RoundObserverConfig, Arc<dyn Extension>)>> {
    let registry = extension_registry()?;
    let Some(linked) = linked_extension(&registry, id)? else {
        return Ok(None);
    };
    reject_bundled_link_collision(id)?;
    let package = load_enabled_linked_process(&registry, &linked)?;
    // The loaded package is parsed from the source manifest and its SHA was
    // checked against the reviewed linked record above. Never trust duplicate
    // descriptor metadata from links.json for observer command selection.
    let observer = package.descriptor.observer.clone().ok_or_else(|| {
        anyhow!("--observe {id} is not supported: extension {id} declares no observer command pair")
    })?;
    let extension = ManagedProcessExtension::from_package(&package)
        .map_err(|error| anyhow!(error.to_string()))?;
    let cadence_rounds = cadence_override
        .or_else(|| NonZeroU64::new(observer.default_cadence_rounds))
        .ok_or_else(|| anyhow!("extension {id} declares an invalid zero observer cadence"))?;
    Ok(Some((
        RoundObserverConfig {
            cadence_rounds,
            brief_command: observer.brief_command,
            apply_command: observer.apply_command,
        },
        Arc::new(RevalidatedLinkedExtension {
            id: id.to_owned(),
            manifest_sha256: linked.manifest_sha256,
            manifest: extension.manifest(),
        }),
    )))
}

#[derive(Clone)]
struct RevalidatedLinkedExtension {
    id: String,
    manifest_sha256: String,
    manifest: ExtensionManifest,
}

impl Extension for RevalidatedLinkedExtension {
    fn manifest(&self) -> ExtensionManifest {
        self.manifest.clone()
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        let extension = self.current_extension()?;
        let mut descriptors = ObserverDescriptorRegistrar::default();
        extension.register(&mut descriptors)?;
        for descriptor in descriptors.0 {
            let name = descriptor.name.clone();
            registrar.register_command(
                &name,
                Box::new(RevalidatedLinkedCommand {
                    extension: self.clone(),
                    descriptor,
                }),
            );
        }
        Ok(())
    }
}

impl RevalidatedLinkedExtension {
    fn current_extension(&self) -> Result<ManagedProcessExtension, ExtensionError> {
        let registry =
            extension_registry().map_err(|error| ExtensionError::Message(error.to_string()))?;
        let linked = linked_extension(&registry, &self.id)
            .map_err(|error| ExtensionError::Message(error.to_string()))?
            .ok_or_else(|| {
                ExtensionError::Message("linked observer is no longer available".to_owned())
            })?;
        if linked.manifest_sha256 != self.manifest_sha256 {
            return Err(ExtensionError::Message(
                "linked observer changed after session startup; restart or resume the session to use the reviewed package"
                    .to_owned(),
            ));
        }
        reject_bundled_link_collision(&self.id)
            .map_err(|error| ExtensionError::Message(error.to_string()))?;
        let package = load_enabled_linked_process(&registry, &linked)
            .map_err(|error| ExtensionError::Message(error.to_string()))?;
        ManagedProcessExtension::from_package(&package)
            .map_err(|error| ExtensionError::Message(error.to_string()))
    }
}

struct RevalidatedLinkedCommand {
    extension: RevalidatedLinkedExtension,
    descriptor: CommandDescriptor,
}

impl ExtensionCommand for RevalidatedLinkedCommand {
    fn descriptor(&self) -> CommandDescriptor {
        self.descriptor.clone()
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<serde_json::Value, ExtensionError> {
        // Revalidate immediately before constructing and invoking the managed
        // command, rather than only when core registers extension commands.
        let extension = self.extension.current_extension()?;
        let mut commands = CommandCollector::default();
        extension.register(&mut commands)?;
        let command = commands.0.remove(&self.descriptor.name).ok_or_else(|| {
            ExtensionError::Message("observer command is no longer registered".to_owned())
        })?;
        command.execute(context, host)
    }
}

#[derive(Default)]
struct ObserverDescriptorRegistrar(Vec<CommandDescriptor>);

impl CommandRegistrar for ObserverDescriptorRegistrar {
    fn register_command(&mut self, _name: &str, command: Box<dyn ExtensionCommand>) {
        self.0.push(command.descriptor());
    }
}

#[derive(Default)]
struct CommandCollector(BTreeMap<String, Box<dyn ExtensionCommand>>);

impl CommandRegistrar for CommandCollector {
    fn register_command(&mut self, name: &str, command: Box<dyn ExtensionCommand>) {
        self.0.insert(name.to_owned(), command);
    }
}
