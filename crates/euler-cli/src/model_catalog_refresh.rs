use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::path::Path;

/// `euler models refresh`: the managed provider catalog, then every
/// discovery provider in `providers.json` (ADR 0025). A discovery failure
/// keeps that provider's cache and fails the command after the others run.
pub(crate) fn refresh_model_catalog(
    model_path: Option<&Path>,
    provider_config_path: Option<&Path>,
    mut stdout: impl Write,
    mut stderr: impl Write,
) -> Result<()> {
    let catalog = refresh_provider_catalog(model_path, &mut stdout, &mut stderr);
    let discovery_succeeded = match provider_config_path {
        Some(path) => {
            let load = crate::provider_config_runtime::load_provider_config(Some(path));
            for warning in &load.warnings {
                writeln!(stderr, "warning: ignored provider config: {warning}")?;
            }
            crate::provider_discovery::refresh_all(path, &load.registry, &mut stdout, &mut stderr)?
        }
        None => true,
    };
    catalog?;
    if !discovery_succeeded {
        return Err(anyhow!(
            "provider discovery refresh failed; cached models left untouched"
        ));
    }
    Ok(())
}

fn refresh_provider_catalog(
    model_path: Option<&Path>,
    mut stdout: impl Write,
    mut stderr: impl Write,
) -> Result<()> {
    let model_path = model_path.ok_or_else(|| anyhow!("could not resolve ~/.euler"))?;
    let cache_dir = crate::provider_catalog::managed_catalog_dir_for_model_path(model_path);
    let report = crate::provider_catalog::refresh_managed_catalog(&cache_dir)
        .context("provider catalog refresh failed; last-known-good catalog left untouched")?;
    for warning in report.warnings {
        writeln!(stderr, "warning: {warning}")?;
    }
    if report.outcome.was_updated() {
        writeln!(
            stdout,
            "updated provider catalog to {}",
            report.outcome.release_id()
        )?;
    } else {
        writeln!(
            stdout,
            "provider catalog is current ({})",
            report.outcome.release_id()
        )?;
    }
    Ok(())
}
