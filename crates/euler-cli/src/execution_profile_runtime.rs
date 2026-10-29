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

/// A resume's profile resolved as a launch resolves it, before the resume
/// rule compares it with the profile the log recorded. Resolving first lets
/// a bad `profiles.json` or overlay fail resume before anything is appended
/// to the log.
pub(crate) struct ResolvedResumeProfile {
    resolved: ExecutionProfile,
    overlay: Option<ProjectOverlay>,
    notices: Vec<String>,
    explicit: bool,
    reviewer: Option<PermissionReviewer>,
}

/// Resolve a resume's profile from the user's `profiles.json` and the
/// repository overlay under `root`.
pub(crate) fn resolve_resume_profile(
    request: ProfileRequest<'_>,
    root: &Path,
) -> Result<ResolvedResumeProfile> {
    let (profiles, overlay, notices) = load_profile_sources(root)?;
    resolve_resume_profile_from(&profiles, overlay, notices, request)
}

fn resolve_resume_profile_from(
    profiles: &ProfilesConfig,
    overlay: Option<ProjectOverlay>,
    notices: Vec<String>,
    request: ProfileRequest<'_>,
) -> Result<ResolvedResumeProfile> {
    let resolved = profiles.resolve(request.profile, request.context, overlay.as_ref())?;
    Ok(ResolvedResumeProfile {
        resolved,
        overlay,
        notices,
        explicit: request.profile.is_some(),
        reviewer: request.reviewer,
    })
}

impl ResolvedResumeProfile {
    /// Apply the resume rule against the profile the log recorded: a looser
    /// resolution keeps the recorded fields unless `--profile` was passed.
    /// Installs the profile on `config` and returns the startup-summary lines
    /// for the caller to show.
    pub(crate) fn install(
        self,
        config: &mut SessionConfig,
        events: &[EventEnvelope],
    ) -> Result<Vec<String>> {
        let Self {
            resolved,
            overlay,
            mut notices,
            explicit,
            reviewer,
        } = self;
        let mut choice = ResumeProfile::choose(
            recorded_execution_profile(events)?,
            resolved,
            explicit,
            overlay.as_ref(),
        )?;
        // `--permission-reviewer` applies before the summary, so it compares
        // the profile that actually runs.
        if let Some(reviewer) = reviewer {
            choice.profile.fields.reviewer = reviewer;
        }
        notices.extend(overlay_notice(overlay.as_ref(), &choice.profile));
        notices.extend(choice.summary());
        config.set_execution_profile(choice.profile);
        Ok(notices)
    }
}

/// The in-app `/resume`: no `--profile`, so a resolved profile looser than
/// the recorded one keeps the recorded. `home` is the Euler home the app's
/// session store uses.
pub(crate) fn install_in_app_resume_profile(
    config: &mut SessionConfig,
    events: &[EventEnvelope],
    home: &EulerHome,
) -> Result<Vec<String>> {
    let request = ProfileRequest {
        profile: None,
        reviewer: None,
        context: ProfileContext::Interactive,
    };
    let profiles = ProfilesConfig::load(&home.profiles_path())?;
    let overlay = ProjectOverlay::load(&config.root)?;
    resolve_resume_profile_from(&profiles, overlay, Vec::new(), request)?.install(config, events)
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
