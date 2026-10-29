use super::config::ProfileOverrides;
use super::*;
use std::path::Path;

fn parse(text: &str) -> Result<ProfilesConfig, ProfileError> {
    ProfilesConfig::parse(Path::new("/home/ada/.euler/profiles.json"), text)
}

fn error_text(result: Result<impl std::fmt::Debug, ProfileError>) -> String {
    result.expect_err("expected a profile error").to_string()
}

fn overlay(root: &Path, text: &str) -> ProjectOverlay {
    let directory = root.join(".euler");
    std::fs::create_dir_all(&directory).expect("overlay dir");
    std::fs::write(directory.join("profile-overlay.json"), text).expect("overlay");
    ProjectOverlay::load(root)
        .expect("overlay parses")
        .expect("overlay present")
}

/// Fields of a profile looser than `locked` on network: the `workspace`
/// preset, built by hand so the tests state each field.
fn networked_fields() -> ProfileFields {
    ProfileFields {
        network: NetworkSetting::On,
        tmp: TmpSetting::Host,
        caches: CachesSetting::Writable,
        approvals: ApprovalsSetting::EscalationOnly,
        ..locked::fields()
    }
}

/// A networked profile this build cannot enforce: escalation ships later.
fn escalating_fields() -> ProfileFields {
    ProfileFields {
        escalation: EscalationSetting::Allowed,
        ..networked_fields()
    }
}

#[test]
fn built_in_defaults_resolve_locked_in_every_context() {
    let config = ProfilesConfig::default();
    for context in [ProfileContext::Interactive, ProfileContext::Headless] {
        let profile = config.resolve(None, context, None).expect("resolves");
        assert_eq!(profile, ExecutionProfile::default());
        assert_eq!(profile.name, "locked");
    }
}

#[test]
fn missing_profiles_file_is_the_built_in_configuration() {
    let temp = tempfile::tempdir().expect("temp");
    let config = ProfilesConfig::load(&temp.path().join("profiles.json")).expect("loads");
    let profile = config
        .resolve(None, ProfileContext::Interactive, None)
        .expect("resolves");
    assert_eq!(profile.name, "locked");
}

#[test]
fn presets_not_shipped_yet_are_refused_never_substituted() {
    let config = ProfilesConfig::default();
    let message = error_text(config.resolve(Some("full"), ProfileContext::Headless, None));
    assert!(
        message.contains("the full preset is not available"),
        "{message}"
    );

    // A profile extending an unshipped preset loads, and is refused only
    // when selected.
    let config = parse(r#"{"profiles": {"mine": {"extends": "full", "reviewer": "guardian"}}}"#)
        .expect("loads");
    let message = error_text(config.resolve(Some("mine"), ProfileContext::Interactive, None));
    assert!(
        message.contains("profile \"mine\" extends the full preset"),
        "{message}"
    );
}

#[test]
fn the_workspace_preset_resolves_to_its_boundary() {
    let profile = ProfilesConfig::default()
        .resolve(Some("workspace"), ProfileContext::Headless, None)
        .expect("workspace ships");
    assert_eq!(profile.name, "workspace");
    assert_eq!(profile.fields, networked_fields());
    assert_eq!(profile.fields.escalation, EscalationSetting::NotAllowed);
    assert_eq!(profile.fields.credentials, CredentialsSetting::Private);
    assert!(matches!(
        profile.subprocess_sandbox(),
        Ok(SubprocessSandbox::Enforce(
            crate::sandbox::SandboxProfile::WorkspaceNetwork(access)
        )) if access == crate::sandbox::WorkspaceNetworkAccess::default()
    ));

    // A user profile extending it adds writable paths and passes
    // credentials.
    let temp = tempfile::tempdir().expect("temp");
    let data = temp.path().canonicalize().expect("canonical");
    let text = format!(
        r#"{{"profiles": {{"agents": {{"extends": "workspace", "credentials": "pass", "writable_paths": ["{}"]}}}}}}"#,
        data.display()
    );
    let profile = parse(&text)
        .expect("loads")
        .resolve(Some("agents"), ProfileContext::Headless, None)
        .expect("resolves");
    assert_eq!(
        profile.subprocess_sandbox(),
        Ok(SubprocessSandbox::Enforce(
            crate::sandbox::SandboxProfile::WorkspaceNetwork(
                crate::sandbox::WorkspaceNetworkAccess {
                    writable_paths: vec![data],
                    pass_credentials: true,
                }
            )
        ))
    );
}

#[test]
fn context_defaults_and_the_profile_flag_select_by_precedence() {
    let config = parse(
        r#"{
            "defaults": {"interactive": "locked", "headless": "ci", "subagent": "inherit"},
            "profiles": {"ci": {"extends": "locked", "reviewer": "guardian"}}
        }"#,
    )
    .expect("loads");

    let headless = config
        .resolve(None, ProfileContext::Headless, None)
        .expect("headless default");
    assert_eq!(headless.name, "ci");
    assert_eq!(headless.fields.reviewer, PermissionReviewer::Guardian);
    assert_eq!(
        ExecutionProfile {
            name: "locked".to_owned(),
            fields: ProfileFields {
                reviewer: PermissionReviewer::User,
                ..headless.fields.clone()
            }
        },
        ExecutionProfile::default()
    );

    let interactive = config
        .resolve(None, ProfileContext::Interactive, None)
        .expect("interactive default");
    assert_eq!(interactive.name, "locked");

    let flagged = config
        .resolve(Some("ci"), ProfileContext::Interactive, None)
        .expect("--profile wins");
    assert_eq!(flagged.name, "ci");
}

#[test]
fn unknown_profile_name_lists_the_defined_profiles() {
    let config = parse(r#"{"profiles": {"ci": {"extends": "locked"}}}"#).expect("loads");
    let message = error_text(config.resolve(Some("cl"), ProfileContext::Headless, None));
    assert_eq!(
        message,
        "unknown execution profile \"cl\"; defined profiles: full, workspace, locked, ci"
    );
}

#[test]
fn unknown_fields_and_values_are_errors_naming_the_file_and_field() {
    let cases = [
        (r#"{"default": {}}"#, "unknown field `default`"),
        (
            r#"{"defaults": {"batch": "locked"}}"#,
            "unknown field `batch`",
        ),
        (
            r#"{"profiles": {"ci": {"extends": "locked", "netwrk": "off"}}}"#,
            "profile \"ci\": unknown field `netwrk`",
        ),
        (
            r#"{"profiles": {"ci": {"extends": "locked", "network": "maybe"}}}"#,
            "profile \"ci\": network: unknown variant `maybe`",
        ),
        (
            r#"{"profiles": {"ci": {"extends": "locked", "writable_paths": ["data"]}}}"#,
            "writable_paths: \"data\" must be an absolute path or start with ~/",
        ),
        (
            r#"{"profiles": {"ci": {"extends": "locked", "approvals": "always", "approvals": "never"}}}"#,
            "duplicate key `approvals`",
        ),
        (
            r#"{"profiles": {"ci": {"extends": "locked"}, "ci": {"extends": "full"}}}"#,
            "duplicate key `ci`",
        ),
        (
            r#"{"defaults": {}, "defaults": {}}"#,
            "duplicate field `defaults`",
        ),
    ];
    for (text, expected) in cases {
        let message = error_text(parse(text));
        assert!(
            message.starts_with("/home/ada/.euler/profiles.json: "),
            "{message}"
        );
        assert!(message.contains(expected), "{expected} not in {message}");
    }
}

#[test]
fn profile_names_and_extends_are_validated() {
    for (text, expected) in [
        (
            r#"{"profiles": {"locked": {"extends": "locked"}}}"#,
            "\"locked\" is reserved",
        ),
        (
            r#"{"profiles": {"inherit": {"extends": "locked"}}}"#,
            "\"inherit\" is reserved",
        ),
        (
            r#"{"profiles": {"ci": {"network": "off"}}}"#,
            "extends is required",
        ),
        (
            r#"{"profiles": {"ci": {"extends": "ci2"}}}"#,
            "extends: \"ci2\" is not a preset",
        ),
        (
            r#"{"profiles": {"my ci": {"extends": "locked"}}}"#,
            "letters, digits",
        ),
        (
            r#"{"defaults": {"headless": "ci"}}"#,
            "defaults.headless: unknown profile \"ci\"",
        ),
        (
            r#"{"defaults": {"interactive": "inherit"}}"#,
            "defaults.interactive: unknown profile \"inherit\"",
        ),
    ] {
        let message = error_text(parse(text));
        assert!(message.contains(expected), "{expected} not in {message}");
    }
}

#[test]
fn contradictory_combinations_are_load_errors() {
    for (body, expected) in [
        (
            r#""approvals": "never", "escalation": "allowed""#,
            "approvals \"never\" with escalation \"allowed\"",
        ),
        (
            r#""sandbox": "off", "escalation": "allowed""#,
            "sandbox \"off\" with escalation \"allowed\"",
        ),
        (
            r#""sandbox": "off", "credentials": "private""#,
            "credentials applies only with the sandbox on",
        ),
    ] {
        let text = format!(r#"{{"profiles": {{"bad": {{"extends": "locked", {body}}}}}}}"#);
        let message = error_text(parse(&text));
        assert!(message.contains("profile \"bad\""), "{message}");
        assert!(message.contains(expected), "{expected} not in {message}");
    }
}

/// Only the `locked` and `workspace` boundaries are enforced; a profile
/// mixing their fields, or setting one that ships later, is refused naming
/// the field.
#[test]
fn a_user_profile_no_enforced_boundary_delivers_is_refused() {
    let config = parse(
        r#"{"profiles": {
            "net": {"extends": "locked", "network": "on"},
            "data": {"extends": "locked", "writable_paths": ["~/data"]},
            "keys": {"extends": "locked", "credentials": "pass"},
            "offline": {"extends": "workspace", "network": "off"},
            "missing": {"extends": "workspace", "writable_paths": ["/nonexistent/euler-data"]},
            "escalate": {"extends": "workspace", "escalation": "allowed"},
            "quiet": {"extends": "workspace", "approvals": "never"}
        }}"#,
    )
    .expect("loads");
    for (name, field) in [
        ("net", "tmp"),
        ("data", "writable_paths"),
        ("keys", "credentials"),
        ("offline", "tmp"),
        ("missing", "writable_paths"),
        ("escalate", "escalation"),
        ("quiet", "approvals"),
    ] {
        assert_eq!(
            config.resolve(Some(name), ProfileContext::Headless, None),
            Err(ProfileError::Unenforced {
                profile: name.to_owned(),
                field,
            }),
            "{name}"
        );
    }
}

#[test]
fn project_overlay_tightens_and_never_loosens() {
    let temp = tempfile::tempdir().expect("temp");
    let config = ProfilesConfig::default();

    let same = overlay(
        temp.path(),
        r#"{"approvals": "always", "writable_paths": []}"#,
    );
    let profile = config
        .resolve(None, ProfileContext::Headless, Some(&same))
        .expect("an equal value is at least as tight");
    assert_eq!(profile, ExecutionProfile::default());

    let loosening = overlay(temp.path(), r#"{"network": "on"}"#);
    let message =
        error_text(config.resolve(Some("locked"), ProfileContext::Headless, Some(&loosening)));
    assert!(message.contains("profile-overlay.json: "), "{message}");
    assert!(
        message.contains("network \"on\" is looser than the profile's \"off\""),
        "{message}"
    );

    let reviewer = overlay(temp.path(), r#"{"reviewer": "guardian"}"#);
    let message = error_text(config.resolve(None, ProfileContext::Headless, Some(&reviewer)));
    assert!(message.contains("cannot set it"), "{message}");

    std::fs::write(
        temp.path().join(PROJECT_OVERLAY_PATH),
        r#"{"extends": "locked"}"#,
    )
    .expect("overlay");
    let message = error_text(ProjectOverlay::load(temp.path()));
    assert!(message.contains("unknown field `extends`"), "{message}");

    std::fs::write(
        temp.path().join(PROJECT_OVERLAY_PATH),
        r#"{"network": "off", "network": "on"}"#,
    )
    .expect("overlay");
    let message = error_text(ProjectOverlay::load(temp.path()));
    assert!(message.contains("duplicate key `network`"), "{message}");
}

#[test]
fn project_overlay_is_absent_without_a_file() {
    let temp = tempfile::tempdir().expect("temp");
    assert!(ProjectOverlay::load(temp.path()).expect("loads").is_none());
}

#[test]
fn overlay_tightening_checks_each_field_against_the_resolved_profile() {
    let mut fields = networked_fields();
    let overrides = ProfileOverrides {
        network: Some(NetworkSetting::Off),
        approvals: Some(ApprovalsSetting::Always),
        ..ProfileOverrides::default()
    };
    overrides.tighten(&mut fields).expect("tightens");
    assert_eq!(fields.network, NetworkSetting::Off);
    assert_eq!(fields.approvals, ApprovalsSetting::Always);

    let mut fields = networked_fields();
    let loosening = ProfileOverrides {
        approvals: Some(ApprovalsSetting::Never),
        ..ProfileOverrides::default()
    };
    let message = loosening.tighten(&mut fields).expect_err("loosens");
    assert!(message.starts_with("approvals \"never\""), "{message}");
}

#[test]
fn first_looser_field_follows_the_tighten_order() {
    let locked = locked::fields();
    assert_eq!(locked.first_looser_field(&locked), None);
    assert_eq!(
        networked_fields().first_looser_field(&locked),
        Some("network")
    );
    assert_eq!(locked.first_looser_field(&networked_fields()), None);

    let host = ProfileFields {
        sandbox: SandboxSetting::Off,
        ..networked_fields()
    };
    assert_eq!(host.first_looser_field(&locked), Some("sandbox"));
    // With the baseline's sandbox off, its boundary fields are the host's.
    assert_eq!(locked.first_looser_field(&host), None);

    let named_backend = ProfileFields {
        sandbox: SandboxSetting::Seatbelt,
        ..locked.clone()
    };
    assert_eq!(named_backend.first_looser_field(&locked), None);

    let extra_path = ProfileFields {
        writable_paths: vec!["~/data".to_owned()],
        ..locked.clone()
    };
    assert_eq!(
        extra_path.first_looser_field(&locked),
        Some("writable_paths")
    );

    let guardian = ProfileFields {
        reviewer: PermissionReviewer::Guardian,
        ..locked.clone()
    };
    assert_eq!(guardian.first_looser_field(&locked), None);
    assert_eq!(locked.first_looser_field(&guardian), None);
}

#[test]
fn resume_keeps_the_recorded_profile_when_the_resolved_one_is_looser() {
    let recorded = ExecutionProfile::default();
    let resolved = ExecutionProfile {
        name: "net".to_owned(),
        fields: networked_fields(),
    };

    let kept = ResumeProfile::choose(Some(recorded.clone()), resolved.clone(), false, None)
        .expect("choose");
    assert_eq!(kept.profile, recorded);
    assert_eq!(
        kept.basis,
        ResumeProfileBasis::KeptRecorded {
            resolved_name: "net".to_owned(),
            looser_field: "network",
        }
    );
    assert_eq!(
        kept.summary().as_deref(),
        Some(
            "execution profile: kept the recorded profile locked; net is looser on network. \
             Pass --profile to change it"
        )
    );

    // --profile applies the resolved profile even when it is looser, but
    // only one this host can enforce.
    let explicit_looser =
        ResumeProfile::choose(Some(recorded.clone()), resolved.clone(), true, None)
            .expect("choose");
    assert_eq!(explicit_looser.profile, resolved);
    assert_eq!(
        ResumeProfile::choose(
            Some(recorded.clone()),
            ExecutionProfile {
                name: "escalate".to_owned(),
                fields: escalating_fields(),
            },
            true,
            None
        ),
        Err(ProfileError::Unenforced {
            profile: "escalate".to_owned(),
            field: "escalation",
        })
    );
    let explicit = ResumeProfile::choose(Some(resolved.clone()), recorded.clone(), true, None)
        .expect("choose");
    assert_eq!(explicit.profile, recorded);
    assert_eq!(explicit.basis, ResumeProfileBasis::Explicit);
    assert_eq!(
        explicit.summary().as_deref(),
        Some(
            "execution profile: locked (recorded: net; changed approvals escalation-only -> \
             always, caches writable -> private, network on -> off, tmp host -> private)"
        )
    );

    // A tighter resolution applies as resolved.
    let tighter =
        ResumeProfile::choose(Some(resolved), recorded.clone(), false, None).expect("choose");
    assert_eq!(tighter.profile, recorded);
    assert_eq!(tighter.basis, ResumeProfileBasis::Resolved);
    assert_eq!(
        tighter.summary().as_deref(),
        Some(
            "execution profile: locked (recorded: net; changed approvals escalation-only -> \
             always, caches writable -> private, network on -> off, tmp host -> private)"
        )
    );

    let unchanged = ResumeProfile::choose(Some(recorded.clone()), recorded.clone(), false, None)
        .expect("choose");
    assert_eq!(unchanged.summary(), None);
}

/// `reviewer` has no order, so it never keeps the recorded profile, but a
/// change to it under the same name is named in the summary.
#[test]
fn resume_summary_names_the_fields_that_changed() {
    let recorded = ExecutionProfile {
        fields: ProfileFields {
            reviewer: PermissionReviewer::Guardian,
            ..locked::fields()
        },
        ..ExecutionProfile::default()
    };
    let resolved = ExecutionProfile::default();
    let choice = ResumeProfile::choose(Some(recorded), resolved, false, None).expect("choose");
    assert_eq!(choice.basis, ResumeProfileBasis::Resolved);
    assert_eq!(
        choice.summary().as_deref(),
        Some("execution profile: locked (recorded: locked; changed reviewer guardian -> user)")
    );

    // A kept recorded profile names the reviewer the flag changed on it.
    let mut kept = ResumeProfile::choose(
        Some(ExecutionProfile::default()),
        ExecutionProfile {
            name: "net".to_owned(),
            fields: networked_fields(),
        },
        false,
        None,
    )
    .expect("choose");
    kept.profile.fields.reviewer = PermissionReviewer::Guardian;
    assert_eq!(
        kept.summary().as_deref(),
        Some(
            "execution profile: kept the recorded profile locked (changed reviewer user -> \
             guardian); net is looser on network. Pass --profile to change it"
        )
    );
}

/// A kept recorded profile still gets the project overlay, and one this
/// host cannot enforce (for example `background_jobs: end-with-command`
/// recorded on Linux and resumed on macOS) is refused, never substituted.
#[test]
fn a_kept_recorded_profile_is_overlaid_and_must_be_enforceable() {
    let recorded = ExecutionProfile {
        name: "escalate".to_owned(),
        fields: escalating_fields(),
    };
    let resolved = ExecutionProfile {
        name: "host".to_owned(),
        fields: ProfileFields {
            sandbox: SandboxSetting::Off,
            ..escalating_fields()
        },
    };

    assert_eq!(
        ResumeProfile::choose(Some(recorded.clone()), resolved.clone(), false, None),
        Err(ProfileError::RecordedUnenforced {
            profile: "escalate".to_owned(),
            field: "escalation",
        })
    );

    let temp = tempfile::tempdir().expect("temp");
    let loosening = overlay(temp.path(), r#"{"approvals": "never"}"#);
    let message = error_text(ResumeProfile::choose(
        Some(recorded),
        resolved,
        false,
        Some(&loosening),
    ));
    assert!(
        message.contains("approvals \"never\" is looser"),
        "{message}"
    );
}

#[test]
fn resume_of_a_log_without_a_profile_resolves_as_a_new_session() {
    let resolved = ExecutionProfile::default();
    let choice = ResumeProfile::choose(None, resolved.clone(), false, None).expect("choose");
    assert_eq!(choice.profile, resolved);
    assert_eq!(choice.basis, ResumeProfileBasis::NotRecorded);
    assert_eq!(
        choice.summary().as_deref(),
        Some("execution profile: locked (this session recorded no profile)")
    );
}

#[test]
fn record_round_trips_and_rejects_tampering() {
    let profile = ExecutionProfile {
        name: "ci".to_owned(),
        fields: ProfileFields {
            reviewer: PermissionReviewer::Guardian,
            ..locked::fields()
        },
    };
    let record = profile.record();
    assert_eq!(record["name"], "ci");
    assert_eq!(record["fields"]["network"], "off");
    assert_eq!(record["fields"]["approvals"], "always");
    assert_eq!(record["fields"]["reviewer"], "guardian");
    assert_eq!(ExecutionProfile::from_record(&record), Ok(profile));

    let mut tampered = record.clone();
    tampered["fields"]["network"] = "on".into();
    assert_eq!(
        ExecutionProfile::from_record(&tampered),
        Err("fields_sha256 does not match the recorded fields".to_owned())
    );

    let mut unknown = record;
    unknown["fields"]["sandbox_mode"] = "on".into();
    assert!(ExecutionProfile::from_record(&unknown).is_err());
}
