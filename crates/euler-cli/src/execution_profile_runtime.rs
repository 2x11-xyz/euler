//! Execution profile selection for a launch or a resume (ADR 0026 sections
//! 5 and 9).

use anyhow::Result;
use euler_core::execution_profile::PROJECT_OVERLAY_PATH;
use euler_core::{
    recorded_execution_profile, EulerHome, ExecutionProfile, PermissionReviewer, ProfileContext,
    ProfilesConfig, ProjectOverlay, ResumeProfile, SessionConfig,
};
use euler_event::EventEnvelope;
use std::path::Path;

/// What selects a profile: `--profile`, `--permission-reviewer`, and the
/// context whose default applies when `--profile` is absent.
#[derive(Clone, Copy)]
pub(crate) struct ProfileRequest<'a> {
    pub(crate) profile: Option<&'a str>,
    pub(crate) reviewer: Option<PermissionReviewer>,
    pub(crate) context: ProfileContext,
}

/// Resolve the profile for a fresh launch: `--profile`, then the context
/// default in `profiles.json`, then the built-in default, then the
/// repository's tightening overlay. A malformed file or an unavailable
/// profile is a startup error. Prints the startup summary to stderr.
pub(crate) fn launch_profile(request: ProfileRequest<'_>, root: &Path) -> Result<ExecutionProfile> {
    let (profiles, overlay, mut notices) = load_profile_sources(root)?;
    let mut profile = profiles.resolve(request.profile, request.context, overlay.as_ref())?;
    notices.extend(overlay_notice(overlay.as_ref(), &profile));
    print_notices(&notices);
    if let Some(reviewer) = request.reviewer {
        profile.fields.reviewer = reviewer;
    }
    Ok(profile)
}

/// Resolve as a launch does, then apply the resume rule against the profile
/// the log recorded: a looser resolution keeps the recorded fields unless
/// `--profile` was passed. Installs the profile on `config` and returns the
/// startup-summary lines for the caller to show.
pub(crate) fn install_resume_profile(
    config: &mut SessionConfig,
    events: &[EventEnvelope],
    request: ProfileRequest<'_>,
) -> Result<Vec<String>> {
    let (profiles, overlay, mut notices) = load_profile_sources(&config.root)?;
    let resolved = profiles.resolve(request.profile, request.context, overlay.as_ref())?;
    let choice = ResumeProfile::choose(
        recorded_execution_profile(events)?,
        resolved,
        request.profile.is_some(),
        overlay.as_ref(),
    )?;
    notices.extend(overlay_notice(overlay.as_ref(), &choice.profile));
    notices.extend(choice.summary());
    let mut profile = choice.profile;
    if let Some(reviewer) = request.reviewer {
        profile.fields.reviewer = reviewer;
    }
    config.set_execution_profile(profile);
    Ok(notices)
}

/// The in-app `/resume`: no `--profile`, so a resolved profile looser than
/// the recorded one keeps the recorded.
pub(crate) fn install_in_app_resume_profile(
    config: &mut SessionConfig,
    events: &[EventEnvelope],
) -> Result<Vec<String>> {
    let request = ProfileRequest {
        profile: None,
        reviewer: None,
        context: ProfileContext::Interactive,
    };
    install_resume_profile(config, events, request)
}

pub(crate) fn print_notices(notices: &[String]) {
    for notice in notices {
        eprintln!("{notice}");
    }
}

fn overlay_notice(overlay: Option<&ProjectOverlay>, profile: &ExecutionProfile) -> Option<String> {
    overlay.map(|_| {
        format!(
            "execution profile: {}, tightened by {PROJECT_OVERLAY_PATH}",
            profile.name
        )
    })
}

fn load_profile_sources(
    root: &Path,
) -> Result<(ProfilesConfig, Option<ProjectOverlay>, Vec<String>)> {
    let mut notices = Vec::new();
    let profiles = match EulerHome::resolve() {
        Ok(home) => ProfilesConfig::load(&home.profiles_path())?,
        Err(error) => {
            notices.push(format!(
                "warning: no profiles.json read, using built-in profiles: {error}"
            ));
            ProfilesConfig::default()
        }
    };
    Ok((profiles, ProjectOverlay::load(root)?, notices))
}
