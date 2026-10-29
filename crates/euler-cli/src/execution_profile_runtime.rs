//! Execution profile selection for a launch or a resume (ADR 0026 sections
//! 5 and 9).

use anyhow::{Context as _, Result};
use euler_core::execution_profile::{
    write_interactive_default, ApprovalsSetting, Preset, SandboxSetting, PROJECT_OVERLAY_PATH,
};
use euler_core::{
    recorded_execution_profile, EulerHome, ExecutionProfile, PermissionReviewer, ProfileContext,
    ProfilesConfig, ProjectOverlay, ResumeProfile, SessionConfig,
};
use euler_event::EventEnvelope;
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

/// What selects a profile: `--profile`, `--permission-reviewer`, and the
/// context whose default applies when `--profile` is absent.
#[derive(Clone, Copy)]
pub(crate) struct ProfileRequest<'a> {
    pub(crate) profile: Option<&'a str>,
    pub(crate) reviewer: Option<PermissionReviewer>,
    pub(crate) context: ProfileContext,
}

/// The profiles a launch resolves: the session's, and `defaults.subagent`
/// for its child agents (`None` is `inherit`).
pub(crate) struct LaunchProfile {
    pub(crate) profile: ExecutionProfile,
    pub(crate) subagent: Option<ExecutionProfile>,
}

impl LaunchProfile {
    pub(crate) fn install(self, config: &mut SessionConfig) {
        config.set_execution_profile(self.profile);
        config.subagent_profile = self.subagent;
    }
}

/// Resolve the profile for a fresh launch: `--profile`, then the context
/// default in `profiles.json`, then the built-in default, then the
/// repository's tightening overlay; and `defaults.subagent` with the same
/// overlay. A malformed file or an unavailable profile is a startup error.
/// Prints the startup summary to stderr.
pub(crate) fn launch_profile(request: ProfileRequest<'_>, root: &Path) -> Result<LaunchProfile> {
    let (profiles, overlay, mut notices) = load_profile_sources(root)?;
    let mut profile = profiles.resolve(request.profile, request.context, overlay.as_ref())?;
    let subagent = resolve_subagent(&profiles, overlay.as_ref())?;
    notices.extend(overlay_notice(overlay.as_ref(), &profile));
    print_notices(&notices);
    if let Some(reviewer) = request.reviewer {
        profile.fields.reviewer = reviewer;
    }
    Ok(LaunchProfile { profile, subagent })
}

/// A resume's profile resolved as a launch resolves it, before the resume
/// rule compares it with the profile the log recorded. Resolving first lets
/// a bad `profiles.json` or overlay fail resume before anything is appended
/// to the log.
pub(crate) struct ResolvedResumeProfile {
    resolved: ExecutionProfile,
    subagent: Option<ExecutionProfile>,
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
    let subagent = resolve_subagent(profiles, overlay.as_ref())?;
    Ok(ResolvedResumeProfile {
        resolved,
        subagent,
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
            subagent,
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
        config.subagent_profile = subagent;
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

/// The user's `profiles.json` when an interactive launch names no
/// `--profile`: where the first-run picker (ADR 0026 section 5) looks for
/// `defaults.interactive` and saves the choice. `None` for headless runs, for
/// `--profile`, and when the Euler home does not resolve.
pub(crate) fn first_run_profiles_file(request: ProfileRequest<'_>) -> Option<PathBuf> {
    if request.context != ProfileContext::Interactive || request.profile.is_some() {
        return None;
    }
    EulerHome::resolve().ok().map(|home| home.profiles_path())
}

/// The line-oriented first-run picker: on a terminal (stdin and stderr),
/// when `profiles.json` has no `defaults.interactive`, ask once before the
/// session starts and save the choice. The TUI shows the picker in the app
/// instead.
pub(crate) fn prompt_first_run_profile_on_terminal(request: ProfileRequest<'_>) -> Result<()> {
    let Some(file) = first_run_profiles_file(request) else {
        return Ok(());
    };
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(());
    }
    if ProfilesConfig::load(&file)?.has_interactive_default() {
        return Ok(());
    }
    let choice = prompt_first_run_profile(&file)?;
    write_interactive_default(&file, choice.name())?;
    Ok(())
}

/// The first-run picker as a line prompt on stderr: the presets, `full`
/// preselected, each with its one-line boundary.
fn prompt_first_run_profile(file: &Path) -> Result<Preset> {
    const CHOICES: [(Preset, &str); 3] = [
        (
            Preset::Full,
            "commands run on this machine with your permissions; only dangerous commands ask",
        ),
        (
            Preset::Workspace,
            "commands run in a sandbox with network; dangerous commands and sensitive paths ask",
        ),
        (
            Preset::Locked,
            "commands run in a sandbox without network; every command asks",
        ),
    ];
    eprintln!(
        "How should Euler run commands? Saved to {}; change it later with /profile.",
        file.display()
    );
    for (index, (preset, boundary)) in CHOICES.iter().enumerate() {
        eprintln!("  {}) {:<7} {boundary}", index + 1, preset.name());
    }
    loop {
        eprint!("Choose [1]: ");
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer)? == 0 {
            anyhow::bail!("no execution profile chosen; pass --profile or run euler again");
        }
        if let Some(preset) = first_run_choice(answer.trim(), &CHOICES) {
            return Ok(preset);
        }
        eprintln!("Type 1, 2, or 3, or a profile name.");
    }
}

/// Read one first-run answer: empty picks the first (preselected) choice; a
/// number or a preset name picks that choice.
fn first_run_choice(answer: &str, choices: &[(Preset, &str)]) -> Option<Preset> {
    if answer.is_empty() {
        return choices.first().map(|(preset, _)| *preset);
    }
    if let Ok(number) = answer.parse::<usize>() {
        return number
            .checked_sub(1)
            .and_then(|index| choices.get(index))
            .map(|(preset, _)| *preset);
    }
    choices
        .iter()
        .find(|(preset, _)| preset.name() == answer)
        .map(|(preset, _)| *preset)
}

/// What `/profile` reads: the user's `profiles.json` (and its path, for
/// writing `defaults.interactive`) and the repository overlay under `root`.
pub(crate) struct ProfileSources {
    pub(crate) profiles: ProfilesConfig,
    pub(crate) overlay: Option<ProjectOverlay>,
    pub(crate) file: PathBuf,
}

impl ProfileSources {
    pub(crate) fn load(root: &Path) -> Result<Self> {
        Self::load_file(EulerHome::resolve()?.profiles_path(), root)
    }

    /// Read `file` as the user's `profiles.json`, with the overlay under
    /// `root`.
    pub(crate) fn load_file(file: PathBuf, root: &Path) -> Result<Self> {
        Ok(Self {
            profiles: ProfilesConfig::load(&file)?,
            overlay: ProjectOverlay::load(root)?,
            file,
        })
    }

    /// Resolve `name` for an interactive session, overlay included.
    pub(crate) fn resolve(&self, name: &str) -> Result<ExecutionProfile> {
        Ok(self.profiles.resolve(
            Some(name),
            ProfileContext::Interactive,
            self.overlay.as_ref(),
        )?)
    }

    /// Every profile this host can run here, with its boundary in words.
    pub(crate) fn runnable(&self) -> Vec<ExecutionProfile> {
        self.profiles
            .profile_names()
            .iter()
            .filter_map(|name| self.resolve(name).ok())
            .collect()
    }
}

/// A profile's boundary in one line, for `/profile` and its notices.
pub(crate) fn profile_boundary(profile: &ExecutionProfile) -> String {
    let fields = &profile.fields;
    let place = if fields.sandbox == SandboxSetting::Off {
        "runs on this machine with your permissions".to_owned()
    } else {
        format!("sandboxed, network {}", fields.network.as_str())
    };
    let asks = match fields.approvals {
        ApprovalsSetting::Never => "only dangerous commands ask",
        ApprovalsSetting::EscalationOnly => "dangerous commands and sensitive paths ask",
        ApprovalsSetting::Always => "every command asks",
    };
    format!("{place} · {asks}")
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

/// `defaults.subagent` resolved for child agents; an unusable one is a
/// startup error that names the setting.
fn resolve_subagent(
    profiles: &ProfilesConfig,
    overlay: Option<&ProjectOverlay>,
) -> Result<Option<ExecutionProfile>> {
    profiles
        .resolve_subagent(overlay)
        .context("defaults.subagent in profiles.json cannot be applied to child agents")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_run_choice_preselects_full_and_reads_numbers_and_names() {
        let choices = [
            (Preset::Full, "host"),
            (Preset::Workspace, "sandbox with network"),
            (Preset::Locked, "sandbox"),
        ];
        assert_eq!(first_run_choice("", &choices), Some(Preset::Full));
        assert_eq!(first_run_choice("1", &choices), Some(Preset::Full));
        assert_eq!(first_run_choice("2", &choices), Some(Preset::Workspace));
        assert_eq!(first_run_choice("locked", &choices), Some(Preset::Locked));
        for answer in ["0", "4", "fulll", "yes"] {
            assert_eq!(first_run_choice(answer, &choices), None, "{answer}");
        }
    }

    #[test]
    fn profile_boundary_states_where_commands_run_and_what_asks() {
        let profiles = ProfilesConfig::default();
        let full = profiles
            .resolve(Some("full"), ProfileContext::Interactive, None)
            .expect("full");
        let workspace = profiles
            .resolve(Some("workspace"), ProfileContext::Interactive, None)
            .expect("workspace");
        let locked = profiles
            .resolve(Some("locked"), ProfileContext::Interactive, None)
            .expect("locked");
        assert_eq!(
            profile_boundary(&full),
            "runs on this machine with your permissions · only dangerous commands ask"
        );
        assert_eq!(
            profile_boundary(&workspace),
            "sandboxed, network on · dangerous commands and sensitive paths ask"
        );
        assert_eq!(
            profile_boundary(&locked),
            "sandboxed, network off · every command asks"
        );
    }
}
