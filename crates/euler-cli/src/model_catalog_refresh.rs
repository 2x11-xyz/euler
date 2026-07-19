use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::path::Path;

pub(crate) fn refresh_model_catalog(
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
