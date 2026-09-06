use super::{
    extension_registry, linked_extension, load_enabled_linked_process,
    load_linked_process_for_action,
};
use anyhow::{anyhow, Result};
use euler_managed_process::ManagedProcessExtension;
use euler_sdk::{
    CancellationToken, CommandContext, CommandDescriptor, CommandRegistrar, Extension,
    ExtensionCommand, ExtensionError, ExtensionManifest, HostApi, IdleContributionDescriptor,
    RequestTickDescriptor,
};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Resolve a reviewed linked managed-process extension as a revalidating
/// session handle. Wiring starts no process and grants no capability. Every
/// declaration read and command execution reloads registry state, checks the
/// reviewed fingerprint, and requires current launch consent.
pub(crate) fn live_linked_extension_arc(id: &str) -> Result<Option<Arc<dyn Extension>>> {
    let registry = extension_registry()?;
    let Some(linked) = linked_extension(&registry, id)? else {
        return Ok(None);
    };
    let package = load_linked_process_for_action(&linked, "run")?;
    let extension = ManagedProcessExtension::from_package(&package)
        .map_err(|error| anyhow!(error.to_string()))?;
    Ok(Some(Arc::new(RevalidatedLinkedExtension {
        id: id.to_owned(),
        manifest_sha256: linked.manifest_sha256,
        manifest: extension.manifest(),
        idle_contribution: extension.idle_contribution(),
        request_tick: extension.request_tick(),
    })))
}

/// Resolve only packages that contribute to root-session model or idle
/// boundaries. Ordinary command-only packages remain available through the
/// explicit extension-run surfaces and incur no per-request registration.
pub(crate) fn live_linked_session_extension_arc(id: &str) -> Result<Option<Arc<dyn Extension>>> {
    let registry = extension_registry()?;
    let Some(linked) = linked_extension(&registry, id)? else {
        return Ok(None);
    };
    let package = load_linked_process_for_action(&linked, "run")?;
    let contributes = package.descriptor.idle_contribution.is_some()
        || package.descriptor.request_tick.is_some()
        || package
            .descriptor
            .commands
            .iter()
            .any(|command| command.model_tool.is_some());
    if !contributes {
        return Ok(None);
    }
    let extension = ManagedProcessExtension::from_package(&package)
        .map_err(|error| anyhow!(error.to_string()))?;
    Ok(Some(Arc::new(RevalidatedLinkedExtension {
        id: id.to_owned(),
        manifest_sha256: linked.manifest_sha256,
        manifest: extension.manifest(),
        idle_contribution: extension.idle_contribution(),
        request_tick: extension.request_tick(),
    })))
}

#[derive(Clone)]
struct RevalidatedLinkedExtension {
    id: String,
    manifest_sha256: String,
    manifest: ExtensionManifest,
    idle_contribution: Option<IdleContributionDescriptor>,
    request_tick: Option<RequestTickDescriptor>,
}

impl Extension for RevalidatedLinkedExtension {
    fn manifest(&self) -> ExtensionManifest {
        self.manifest.clone()
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        let extension = self.current_extension()?;
        let mut descriptors = DescriptorRegistrar::default();
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

    fn idle_contribution(&self) -> Option<IdleContributionDescriptor> {
        self.idle_contribution.clone()
    }

    fn request_tick(&self) -> Option<RequestTickDescriptor> {
        self.request_tick.clone()
    }
}

impl RevalidatedLinkedExtension {
    fn current_extension(&self) -> Result<ManagedProcessExtension, ExtensionError> {
        let registry =
            extension_registry().map_err(|error| ExtensionError::Message(error.to_string()))?;
        let linked = linked_extension(&registry, &self.id)
            .map_err(|error| ExtensionError::Message(error.to_string()))?
            .ok_or_else(|| {
                ExtensionError::Message("linked extension is no longer available".to_owned())
            })?;
        if linked.manifest_sha256 != self.manifest_sha256 {
            return Err(ExtensionError::Message(
                "linked extension changed after session startup; restart or resume the session to use the reviewed package"
                    .to_owned(),
            ));
        }
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
        self.execute_cancellable(context, host, &CancellationToken::new())
    }

    fn execute_cancellable(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
        cancellation: &CancellationToken,
    ) -> Result<serde_json::Value, ExtensionError> {
        // Revalidate immediately before constructing and invoking the managed
        // command, rather than only when core reads its declarations.
        if cancellation.is_cancelled() {
            return Err(ExtensionError::Cancelled);
        }
        let extension = self.extension.current_extension()?;
        let mut commands = CommandCollector::default();
        extension.register(&mut commands)?;
        let command = commands.0.remove(&self.descriptor.name).ok_or_else(|| {
            ExtensionError::Message("linked extension command is no longer registered".to_owned())
        })?;
        command.execute_cancellable(context, host, cancellation)
    }
}

#[derive(Default)]
struct DescriptorRegistrar(Vec<CommandDescriptor>);

impl CommandRegistrar for DescriptorRegistrar {
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
