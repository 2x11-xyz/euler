use crate::session_lifecycle::resolve_resume_target;
use anyhow::{anyhow, Result};
use euler_core::extensions::ExtensionHost;
use euler_core::{read_resume_prefix, ProvenanceWriter};
use euler_event::EventEnvelope;
use euler_sdk::{CommandDescriptor, Extension};
use std::path::PathBuf;
use std::sync::Arc;

pub(super) struct OfflineExtensionRun<'a> {
    pub(super) extension_id: &'a str,
    pub(super) command: &'a CommandDescriptor,
    pub(super) extension: &'static dyn Extension,
    pub(super) target: PathBuf,
    pub(super) input: serde_json::Value,
}

pub(super) fn execute_offline_extension_run(
    run: OfflineExtensionRun<'_>,
) -> Result<serde_json::Value> {
    let target = resolve_resume_target(run.target)?;
    let prefix = read_resume_prefix(&target.log_path)?;
    let session_id = session_id_from_events(&prefix)
        .ok_or_else(|| {
            anyhow!(
                "{} {} requires a persisted session event",
                run.extension_id,
                run.command.name
            )
        })?
        .to_owned();
    let writer = Arc::new(ProvenanceWriter::new(target.log_path.clone())?);
    let mut host = ExtensionHost::with_artifact_writer(
        &target.log_path,
        session_id.clone(),
        "root",
        writer,
        run.command.required_capabilities.iter().copied(),
    );
    host.register_extension_for_command(run.extension, &run.command.name)
        .map_err(|error| {
            anyhow!(
                "{}.{} registration failed: {error:?}",
                run.extension_id,
                run.command.name
            )
        })?;
    let mut input = run.input;
    if run.command.accepts_session_id {
        let object = input.as_object_mut().ok_or_else(|| {
            anyhow!(
                "{}.{} input builder returned non-object JSON",
                run.extension_id,
                run.command.name
            )
        })?;
        object.insert("session_id".to_owned(), session_id.into());
    }
    let output = host
        .execute_command(&run.command.name, input)
        .map_err(|error| {
            anyhow!(
                "{}.{} failed: {error:?}",
                run.extension_id,
                run.command.name
            )
        })?;
    if let Some(refresh) = target.refresh {
        if let Err(error) = refresh.refresh() {
            eprintln!("warning: failed to refresh session metadata: {error}");
        }
    }
    Ok(output)
}

fn session_id_from_events(events: &[EventEnvelope]) -> Option<&str> {
    events.first().map(|event| event.session.as_str())
}
