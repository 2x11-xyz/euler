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
        escalation: EscalationSetting::Allowed,
        ..locked::fields()
    }
}

/// A networked profile this build cannot enforce: the other platform's
/// background-job behavior.
fn unenforced_fields() -> ProfileFields {
    let networked = networked_fields();
    ProfileFields {
        background_jobs: match networked.background_jobs {
            BackgroundJobsSetting::Outlive => BackgroundJobsSetting::EndWithCommand,
            BackgroundJobsSetting::EndWithCommand => BackgroundJobsSetting::Outlive,
        },
        ..networked
    }
}

#[test]
fn built_in_defaults_are_full_interactive_and_workspace_headless() {
    let config = ProfilesConfig::default();
    let interactive = config
        .resolve(None, ProfileContext::Interactive, None)
        .expect("resolves");
    assert_eq!(interactive.name, "full");
    assert_eq!(interactive.fields, full::fields());
    let headless = config
        .resolve(None, ProfileContext::Headless, None)
        .expect("resolves");
    assert_eq!(headless.name, "workspace");
    assert_eq!(headless.fields, workspace::fields());
}

#[test]
fn full_preset_runs_on_the_host_with_approvals_off() {
    let fields = full::fields();
    assert_eq!(fields.sandbox, SandboxSetting::Off);
    assert_eq!(fields.approvals, ApprovalsSetting::Never);
    assert_eq!(fields.escalation, EscalationSetting::NotAllowed);
    assert_eq!(fields.background_jobs, BackgroundJobsSetting::Outlive);
    let profile = ProfilesConfig::default()
        .resolve(Some("full"), ProfileContext::Headless, None)
        .expect("full resolves");
    profile.check_enforced().expect("full is enforced");
}

#[test]
fn full_accepts_approvals_always_and_refuses_claims_the_host_does_not_keep() {
    let config = parse(
        r#"{"profiles": {
            "careful": {"extends": "full", "approvals": "always"},
            "half": {"extends": "full", "approvals": "escalation-only"},
            "offline": {"extends": "full", "network": "off"}
        }}"#,
    )
    .expect("loads");
    let careful = config
        .resolve(Some("careful"), ProfileContext::Interactive, None)
        .expect("approvals always works on the host");
    assert_eq!(careful.fields.approvals, ApprovalsSetting::Always);
    for (name, field) in [("half", "approvals"), ("offline", "network")] {
        let message = error_text(config.resolve(Some(name), ProfileContext::Interactive, None));
        assert!(
            message.contains(&format!(
                "sets {field} to a value this version cannot enforce"
            )),
            "{message}"
        );
    }
}

#[test]
fn interactive_default_is_written_beside_existing_entries() {
    let temp = tempfile::tempdir().expect("temp");
    let file = temp.path().join("profiles.json");
    write_interactive_default(&file, "locked").expect("creates the file");
    let config = ProfilesConfig::load(&file).expect("loads");
    assert!(config.has_interactive_default());
    assert_eq!(
        config
            .resolve(None, ProfileContext::Interactive, None)
            .expect("resolves")
            .name,
        "locked"
    );

    std::fs::write(
        &file,
        r#"{"defaults": {"headless": "ci"}, "profiles": {"ci": {"extends": "locked"}}}"#,
    )
    .expect("write");
    write_interactive_default(&file, "full").expect("updates the file");
    let config = ProfilesConfig::load(&file).expect("loads");
    assert_eq!(
        config
            .resolve(None, ProfileContext::Interactive, None)
            .expect("resolves")
            .name,
        "full"
    );
    assert_eq!(
        config
            .resolve(None, ProfileContext::Headless, None)
            .expect("resolves")
            .name,
        "ci"
    );

    std::fs::write(&file, "[]").expect("write");
    let message = error_text(write_interactive_default(&file, "full"));
    assert!(message.contains("expected a JSON object"), "{message}");
    assert_eq!(std::fs::read_to_string(&file).expect("read"), "[]");
}

#[test]
fn missing_profiles_file_is_the_built_in_configuration() {
    let temp = tempfile::tempdir().expect("temp");
    let config = ProfilesConfig::load(&temp.path().join("profiles.json")).expect("loads");
    assert!(!config.has_interactive_default());
    let profile = config
        .resolve(None, ProfileContext::Interactive, None)
        .expect("resolves");
    assert_eq!(profile.name, "full");
}

#[test]
fn the_workspace_preset_resolves_to_its_boundary() {
    let profile = ProfilesConfig::default()
        .resolve(Some("workspace"), ProfileContext::Headless, None)
        .expect("workspace ships");
    assert_eq!(profile.name, "workspace");
    assert_eq!(profile.fields, networked_fields());
    assert_eq!(profile.fields.escalation, EscalationSetting::Allowed);
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
            "quiet": {"extends": "workspace", "approvals": "never", "escalation": "not-allowed"}
        }}"#,
    )
    .expect("loads");
    for (name, field) in [
        ("net", "network"),
        ("data", "writable_paths"),
        ("keys", "credentials"),
        ("offline", "network"),
        ("missing", "writable_paths"),
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

/// `escalation` is not part of either boundary (ADR 0026 rollout step 6):
/// `workspace` without it and `locked` with it both resolve.
#[test]
fn escalation_resolves_with_either_enforced_boundary() {
    let config = parse(
        r#"{"profiles": {
            "boxed": {"extends": "workspace", "escalation": "not-allowed"},
            "door": {"extends": "locked", "escalation": "allowed"}
        }}"#,
    )
    .expect("loads");
    let boxed = config
        .resolve(Some("boxed"), ProfileContext::Headless, None)
        .expect("resolves");
    assert_eq!(boxed.fields.escalation, EscalationSetting::NotAllowed);
    let door = config
        .resolve(Some("door"), ProfileContext::Headless, None)
        .expect("resolves");
    assert_eq!(door.fields.escalation, EscalationSetting::Allowed);
    assert_eq!(door.subprocess_sandbox(), Ok(SubprocessSandbox::default()));
}

/// `approvals` is not part of either boundary: `escalation-only` and
/// `always` resolve with both.
#[test]
fn approvals_resolve_with_either_enforced_boundary() {
    let config = parse(
        r#"{"profiles": {
            "quick": {"extends": "locked", "approvals": "escalation-only"},
            "careful": {"extends": "workspace", "approvals": "always"}
        }}"#,
    )
    .expect("loads");
    let quick = config
        .resolve(Some("quick"), ProfileContext::Headless, None)
        .expect("resolves");
    assert_eq!(quick.fields.approvals, ApprovalsSetting::EscalationOnly);
    assert_eq!(quick.subprocess_sandbox(), Ok(SubprocessSandbox::default()));
    let careful = config
        .resolve(Some("careful"), ProfileContext::Headless, None)
        .expect("resolves");
    assert_eq!(careful.fields.approvals, ApprovalsSetting::Always);
    assert!(matches!(
        careful.subprocess_sandbox(),
        Ok(SubprocessSandbox::Enforce(
            crate::sandbox::SandboxProfile::WorkspaceNetwork(_)
        ))
    ));
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
        .resolve(Some("locked"), ProfileContext::Headless, Some(&same))
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
                name: "jobs".to_owned(),
                fields: unenforced_fields(),
            },
            true,
            None
        ),
        Err(ProfileError::Unenforced {
            profile: "jobs".to_owned(),
            field: "background_jobs",
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
             always, caches writable -> private, escalation allowed -> not-allowed, network on \
             -> off, tmp host -> private)"
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
             always, caches writable -> private, escalation allowed -> not-allowed, network on \
             -> off, tmp host -> private)"
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
        name: "jobs".to_owned(),
        fields: unenforced_fields(),
    };
    let resolved = ExecutionProfile {
        name: "host".to_owned(),
        fields: ProfileFields {
            sandbox: SandboxSetting::Off,
            ..unenforced_fields()
        },
    };

    assert_eq!(
        ResumeProfile::choose(Some(recorded.clone()), resolved.clone(), false, None),
        Err(ProfileError::RecordedUnenforced {
            profile: "jobs".to_owned(),
            field: "background_jobs",
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

#[test]
fn profile_mode_override_follows_each_approvals_setting() {
    use crate::permissions::{ApprovalMode, PermissionRequest};
    use euler_sdk::Capability;
    let write = PermissionRequest::new(Capability::FsWrite, "tool write_file");
    let shell = PermissionRequest::new(Capability::ShellExec, "tool run_shell");
    let spawn = PermissionRequest::new(Capability::AgentSpawn, "tool spawn_agent");
    let mut sensitive_read = PermissionRequest::new(Capability::FsRead, "tool read_file");
    sensitive_read.sensitive_path = true;
    let mut sensitive_shell = shell.clone();
    sensitive_shell.sensitive_path = true;
    let ask = ApprovalMode::Ask;
    let allow = Some(ApprovalMode::SessionAllow);

    // `never` (full): every request the gate passes on runs, a sensitive
    // path included (resolved question 15), with or without a sandbox.
    let full = full::fields();
    for request in [&write, &shell, &spawn, &sensitive_read, &sensitive_shell] {
        assert_eq!(profile_mode_override(&full, request, ask, false), allow);
    }

    // `escalation-only` (workspace): commands and writes run only inside an
    // enforced sandbox, and a shell operand naming a sensitive path asks
    // (resolved question 14).
    let workspace = networked_fields();
    for request in [&write, &shell] {
        assert_eq!(profile_mode_override(&workspace, request, ask, true), allow);
        assert_eq!(profile_mode_override(&workspace, request, ask, false), None);
    }
    assert_eq!(profile_mode_override(&workspace, &spawn, ask, true), None);
    assert_eq!(
        profile_mode_override(&workspace, &sensitive_read, ask, true),
        None
    );
    for enforced in [true, false] {
        assert_eq!(
            profile_mode_override(
                &workspace,
                &sensitive_shell,
                ApprovalMode::SessionAllow,
                enforced
            ),
            Some(ApprovalMode::Ask)
        );
    }

    // `always` (locked): no lift, and a shell operand naming a sensitive
    // path keeps the configured mode, as before profiles.
    assert_eq!(
        profile_mode_override(
            &locked::fields(),
            &sensitive_shell,
            ApprovalMode::SessionAllow,
            true
        ),
        allow
    );
    for request in [&write, &shell, &spawn] {
        assert_eq!(
            profile_mode_override(&locked::fields(), request, ask, true),
            None
        );
    }
}
