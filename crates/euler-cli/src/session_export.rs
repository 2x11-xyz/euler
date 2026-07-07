use super::RawArgs;
use crate::bundled_extensions::{bundled_descriptor_by_id, bundled_extension_by_id};
use crate::offline_extension_runner::{execute_offline_extension_run, OfflineExtensionRun};
use anyhow::{anyhow, Result};
use std::path::PathBuf;

const COMMAND_NAME: &str = "session-export";

#[derive(Clone, Debug, Default)]
pub(super) struct RawProvenanceExportArgs {
    active: bool,
    target: Option<PathBuf>,
    limit: Option<usize>,
    scan_limit: Option<usize>,
    after_event_id: Option<String>,
    kinds: Vec<String>,
}

impl RawProvenanceExportArgs {
    pub(super) fn is_active(&self) -> bool {
        self.active
    }

    pub(super) fn start(&mut self, args: &mut impl Iterator<Item = String>) -> Result<()> {
        let Some(target) = args.next() else {
            return Err(anyhow!(
                "session-export requires a session id, name, or events path"
            ));
        };
        // A flag here would be silently swallowed as the target path.
        if target.starts_with("--") {
            return Err(anyhow!(
                "session-export requires a session id, name, or events path before `{target}`"
            ));
        }
        self.active = true;
        self.target = Some(PathBuf::from(target));
        Ok(())
    }

    pub(super) fn set_limit(&mut self, args: &mut impl Iterator<Item = String>) -> Result<()> {
        self.require_active("--limit")?;
        if self.limit.is_some() {
            return Err(anyhow!("--limit was provided more than once"));
        }
        self.limit = Some(parse_positive_usize(args, "--limit")?);
        Ok(())
    }

    pub(super) fn set_scan_limit(&mut self, args: &mut impl Iterator<Item = String>) -> Result<()> {
        self.require_active("--scan-limit")?;
        if self.scan_limit.is_some() {
            return Err(anyhow!("--scan-limit was provided more than once"));
        }
        self.scan_limit = Some(parse_positive_usize(args, "--scan-limit")?);
        Ok(())
    }

    pub(super) fn set_after_event_id(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<()> {
        self.require_active("--after-event-id")?;
        if self.after_event_id.is_some() {
            return Err(anyhow!("--after-event-id was provided more than once"));
        }
        self.after_event_id = Some(
            args.next()
                .ok_or_else(|| anyhow!("--after-event-id requires an event id"))?,
        );
        Ok(())
    }

    pub(super) fn add_kind(&mut self, args: &mut impl Iterator<Item = String>) -> Result<()> {
        self.require_active("--kind")?;
        self.kinds.push(
            args.next()
                .ok_or_else(|| anyhow!("--kind requires an event kind"))?,
        );
        Ok(())
    }

    fn require_active(&self, flag: &'static str) -> Result<()> {
        if self.active {
            Ok(())
        } else {
            Err(anyhow!("{flag} is only supported with session-export"))
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ProvenanceExportArgs {
    pub(super) target: PathBuf,
    pub(super) limit: Option<usize>,
    pub(super) scan_limit: Option<usize>,
    pub(super) after_event_id: Option<String>,
    pub(super) kinds: Vec<String>,
}

impl ProvenanceExportArgs {
    pub(super) fn input(&self) -> serde_json::Value {
        let mut input = serde_json::Map::new();
        if let Some(limit) = self.limit {
            input.insert("limit".to_owned(), limit.into());
        }
        if let Some(scan_limit) = self.scan_limit {
            input.insert("scan_limit".to_owned(), scan_limit.into());
        }
        if let Some(after_event_id) = &self.after_event_id {
            input.insert("after_event_id".to_owned(), after_event_id.clone().into());
        }
        if !self.kinds.is_empty() {
            input.insert(
                "kinds".to_owned(),
                serde_json::Value::Array(
                    self.kinds
                        .iter()
                        .cloned()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
        }
        serde_json::Value::Object(input)
    }
}

pub(super) fn build_session_export_args(parsed: &RawArgs) -> Result<ProvenanceExportArgs> {
    super::ensure_no_extensions(parsed, "session-export")?;
    if parsed.provider_from_cli {
        return Err(anyhow!("--provider is not supported with session-export"));
    }
    if parsed.model_from_cli {
        return Err(anyhow!("--model is not supported with session-export"));
    }
    if parsed.auth_file_from_cli {
        return Err(anyhow!("--auth-file is not supported with session-export"));
    }
    if parsed.provenance_from_cli {
        return Err(anyhow!("--provenance is not supported with session-export"));
    }
    if parsed.no_tty {
        return Err(anyhow!("--no-tty is not supported with session-export"));
    }
    let target = parsed
        .session_export
        .target
        .clone()
        .ok_or_else(|| anyhow!("session-export requires a session id, name, or events path"))?;
    Ok(ProvenanceExportArgs {
        target,
        limit: parsed.session_export.limit,
        scan_limit: parsed.session_export.scan_limit,
        after_event_id: parsed.session_export.after_event_id.clone(),
        kinds: parsed.session_export.kinds.clone(),
    })
}

pub(super) fn run_session_export(export: ProvenanceExportArgs) -> Result<()> {
    let output = execute_session_export(export)?;
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

pub(super) fn execute_session_export(export: ProvenanceExportArgs) -> Result<serde_json::Value> {
    let descriptor = bundled_descriptor_by_id("session-export")?
        .ok_or_else(|| anyhow!("unknown extension id: session-export"))?;
    let command = descriptor
        .command(COMMAND_NAME)
        .ok_or_else(|| anyhow!("unknown command for extension session-export: {COMMAND_NAME}"))?;
    let bundled = bundled_extension_by_id("session-export")
        .ok_or_else(|| anyhow!("unknown extension id: session-export"))?;
    let input = export.input();
    execute_offline_extension_run(OfflineExtensionRun {
        extension_id: "session-export",
        command,
        extension: bundled.extension,
        target: export.target,
        input,
    })
}

fn parse_positive_usize(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<usize> {
    let message = format!("{flag} requires a positive integer");
    let value = args.next().ok_or_else(|| anyhow!(message.clone()))?;
    let parsed = value
        .parse::<usize>()
        .map_err(|_| anyhow!(message.clone()))?;
    if parsed == 0 {
        return Err(anyhow!(message));
    }
    Ok(parsed)
}
