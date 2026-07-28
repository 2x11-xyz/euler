use super::{extension_registry, linked_extension, load_enabled_linked_process};
use anyhow::{anyhow, Result};
use euler_core::RoundObserverConfig;
use euler_sdk::Extension;
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
    let package = load_enabled_linked_process(&registry, &linked)?;
    // The loaded package is parsed from the source manifest and its SHA was
    // checked against the reviewed linked record above. Never trust duplicate
    // descriptor metadata from links.json for observer command selection.
    let observer = package.descriptor.observer.clone().ok_or_else(|| {
        anyhow!("--observe {id} is not supported: extension {id} declares no observer command pair")
    })?;
    let cadence_rounds = cadence_override
        .or_else(|| NonZeroU64::new(observer.default_cadence_rounds))
        .ok_or_else(|| anyhow!("extension {id} declares an invalid zero observer cadence"))?;
    Ok(Some((
        RoundObserverConfig {
            cadence_rounds,
            brief_command: observer.brief_command,
            apply_command: observer.apply_command,
        },
        super::runtime::live_linked_extension_arc(id)?
            .ok_or_else(|| anyhow!("linked extension {id} disappeared during resolution"))?,
    )))
}

/// `--observe` selection: which extension observes round boundaries, and how
/// often. Parsed from CLI flags; `None` extension means no observer.
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

/// Resolve the round observer for a session. Only linked/installed
/// managed-process extensions can observe; an unknown id is an honest error
/// that names the way in.
pub(crate) fn resolve_round_observer(
    options: &ObserveOptions,
) -> Result<Option<(RoundObserverConfig, Arc<dyn Extension>)>> {
    let Some(id) = options.extension_id.as_deref() else {
        return Ok(None);
    };
    match resolve(id, options.cadence_rounds)? {
        Some(observer) => Ok(Some(observer)),
        None => Err(anyhow!(
            "--observe {id}: unknown extension id; link or install extension {id} first"
        )),
    }
}
