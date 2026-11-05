//! ADR 0027 rollout step 1: parsing, validation, the layered record, and the
//! resume and child rules for `rules`, `ask_timeout`, and
//! `extension_reviewers`.

use super::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;

const PROFILES: &str = "/home/ada/.euler/profiles.json";

fn parse(text: &str) -> Result<ProfilesConfig, ProfileError> {
    ProfilesConfig::parse(Path::new(PROFILES), text)
}

fn overlay(root: &Path, text: &str) -> ProjectOverlay {
    let directory = root.join(".euler");
    std::fs::create_dir_all(&directory).expect("overlay dir");
    std::fs::write(directory.join("profile-overlay.json"), text).expect("overlay");
    ProjectOverlay::load(root)
        .expect("overlay parses")
        .expect("overlay present")
}

fn rule(pattern: &str, state: RuleState) -> PermissionRule {
    PermissionRule {
        pattern: pattern.to_owned(),
        state,
        access: None,
        reason: None,
    }
}

fn shell_layer(layer: RuleLayerLabel, rules: Vec<PermissionRule>) -> RuleLayer {
    RuleLayer {
        layer,
        rules: ProfileRules {
            shell: rules,
            ..ProfileRules::default()
        },
    }
}

fn with_rules(name: &str, rules: Vec<RuleLayer>) -> ExecutionProfile {
    ExecutionProfile {
        name: name.to_owned(),
        fields: ProfileFields {
            rules,
            ..full::fields()
        },
    }
}

#[test]
fn profile_rules_parse_into_one_profile_layer_and_are_refused_as_not_enforced() {
    let config = parse(
        r#"{"profiles": {
            "agent": {
                "extends": "full",
                "ask_timeout": 300,
                "extension_reviewers": ["policy-check.review"],
                "rules": {
                    "shell": [
                        {"pattern": "git push *", "state": "ask"},
                        {"pattern": "git push *--force*", "state": "deny",
                         "reason": "Push a new branch instead."}
                    ],
                    "path": [{"pattern": "~/.euler/**", "access": "write", "state": "deny"}],
                    "outside_workspace": [{"pattern": "/tmp", "access": "write", "state": "allow"}],
                    "tools": [
                        {"pattern": "git_*", "state": "ask"},
                        {"pattern": "code-swarm.*", "state": "deny"}
                    ]
                }
            },
            "timeout": {"extends": "full", "ask_timeout": 0},
            "reviewed": {"extends": "full", "extension_reviewers": ["policy-check.review"]}
        }}"#,
    )
    .expect("parses");

    let agent = config.named("agent").expect("named");
    assert_eq!(agent.fields.ask_timeout, Some(300));
    assert_eq!(
        agent.fields.extension_reviewers,
        vec!["policy-check.review".to_owned()]
    );
    let [layer] = agent.fields.rules.as_slice() else {
        panic!("one layer: {:?}", agent.fields.rules);
    };
    assert_eq!(layer.layer, RuleLayerLabel::Profile);
    assert_eq!(
        layer.rules.shell,
        vec![
            rule("git push *", RuleState::Ask),
            PermissionRule {
                reason: Some("Push a new branch instead.".to_owned()),
                ..rule("git push *--force*", RuleState::Deny)
            },
        ]
    );
    assert_eq!(
        layer.rules.path,
        vec![PermissionRule {
            access: Some(RuleAccess::Write),
            ..rule("~/.euler/**", RuleState::Deny)
        }]
    );
    assert_eq!(layer.rules.outside_workspace[0].state, RuleState::Allow);
    assert_eq!(layer.rules.tools.len(), 2);

    for (name, field) in [
        ("agent", "rules"),
        ("timeout", "ask_timeout"),
        ("reviewed", "extension_reviewers"),
    ] {
        let error = config
            .resolve(Some(name), ProfileContext::Headless, None)
            .expect_err("not enforced");
        assert_eq!(
            error,
            ProfileError::Unenforced {
                profile: name.to_owned(),
                field,
            }
        );
        assert_eq!(
            error.to_string(),
            format!(
                "profile \"{name}\" sets {field}, which this version parses and records but does \
                 not enforce yet; remove {field} to run this profile"
            )
        );
    }
}

#[test]
fn a_subagent_default_with_rules_is_refused_at_startup() {
    let config = parse(
        r#"{"defaults": {"subagent": "helper"}, "profiles": {
            "helper": {"extends": "full", "rules": {"shell": [{"pattern": "rm *", "state": "deny"}]}}
        }}"#,
    )
    .expect("parses");
    assert_eq!(
        config.resolve_subagent(None),
        Err(ProfileError::Unenforced {
            profile: "helper".to_owned(),
            field: "rules",
        })
    );
}

#[test]
fn empty_rules_surfaces_and_reviewer_lists_are_unset() {
    let config = parse(
        r#"{"profiles": {
            "empty": {"extends": "full", "rules": {}},
            "surfaces": {"extends": "full", "rules": {"shell": [], "tools": []}},
            "reviewers": {"extends": "full", "extension_reviewers": []}
        }}"#,
    )
    .expect("parses");
    for name in ["empty", "surfaces", "reviewers"] {
        let profile = config
            .resolve(Some(name), ProfileContext::Interactive, None)
            .expect("an unset field resolves");
        assert_eq!(profile.fields, full::fields(), "{name}");
        assert_eq!(
            profile.record()["fields"],
            full_preset_fields_json(),
            "{name}"
        );
    }
}

/// The `full` preset's fields as written before ADR 0027.
fn full_preset_fields_json() -> Value {
    json!({
        "approvals": "never",
        "background_jobs": "outlive",
        "caches": "writable",
        "credentials": "private",
        "escalation": "not-allowed",
        "network": "on",
        "reviewer": "user",
        "sandbox": "off",
        "tmp": "host",
        "writable_paths": [],
    })
}

#[test]
fn unset_permission_fields_leave_the_record_and_digest_unchanged() {
    let record = ExecutionProfile {
        name: "full".to_owned(),
        fields: full::fields(),
    }
    .record();
    assert_eq!(record["fields"], full_preset_fields_json());
    let written_before = r#"{"approvals":"never","background_jobs":"outlive","caches":"writable","credentials":"private","escalation":"not-allowed","network":"on","reviewer":"user","sandbox":"off","tmp":"host","writable_paths":[]}"#;
    assert_eq!(
        record["fields_sha256"],
        format!("{:x}", Sha256::digest(written_before.as_bytes()))
    );
}

#[test]
fn rule_validation_errors_name_the_file_profile_surface_and_index() {
    let long = "x".repeat(513);
    let many: Vec<Value> = (0..257)
        .map(|_| json!({"pattern": "ls *", "state": "allow"}))
        .collect();
    let cases: Vec<(Value, String)> = vec![
        (
            json!({"rules": {"network": []}}),
            "rules: unknown surface `network`; expected one of shell, path, outside_workspace, tools".to_owned(),
        ),
        (json!({"rules": []}), "rules: expected an object".to_owned()),
        (json!({"rules": {"shell": {}}}), "rules.shell: expected an array".to_owned()),
        (
            json!({"rules": {"shell": [{"pattern": "ls", "state": "allow"}, {"pattern": "rm *", "state": "block"}]}}),
            "rules.shell[1]: unknown variant `block`".to_owned(),
        ),
        (
            json!({"rules": {"shell": [{"pattern": "ls", "state": "allow", "why": "x"}]}}),
            "rules.shell[0]: unknown field `why`".to_owned(),
        ),
        (
            json!({"rules": {"shell": [{"state": "allow"}]}}),
            "rules.shell[0]: missing field `pattern`".to_owned(),
        ),
        (
            json!({"rules": {"shell": [{"pattern": "ls", "access": "read", "state": "allow"}]}}),
            "rules.shell[0]: access applies only to path and outside_workspace rules".to_owned(),
        ),
        (
            json!({"rules": {"tools": [{"pattern": "run_shell", "access": "any", "state": "deny"}]}}),
            "rules.tools[0]: access applies only".to_owned(),
        ),
        (
            json!({"rules": {"path": [{"pattern": "src/**", "access": "exec", "state": "deny"}]}}),
            "rules.path[0]: unknown variant `exec`".to_owned(),
        ),
        (
            json!({"rules": {"shell": [{"pattern": "ls", "state": "deny", "reason": long}]}}),
            "rules.shell[0]: reason is longer than 512 bytes".to_owned(),
        ),
        (
            json!({"rules": {"shell": [{"pattern": "  ", "state": "deny"}]}}),
            "rules.shell[0]: pattern is empty".to_owned(),
        ),
        (
            json!({"rules": {"shell": [{"pattern": long, "state": "deny"}]}}),
            "rules.shell[0]: pattern is longer than 512 bytes".to_owned(),
        ),
        (
            json!({"rules": {"shell": [{"pattern": "ls\nrm", "state": "deny"}]}}),
            "rules.shell[0]: pattern \"ls\\nrm\" has a control character".to_owned(),
        ),
        (
            json!({"rules": {"shell": many}}),
            "rules.shell: 257 rules; at most 256 per surface".to_owned(),
        ),
        (
            json!({"rules": {"path": [{"pattern": "../secrets", "state": "deny"}]}}),
            "rules.path[0]: path pattern \"../secrets\" may not contain ..".to_owned(),
        ),
        (
            json!({"rules": {"path": [{"pattern": "~ada/.ssh", "state": "deny"}]}}),
            "rules.path[0]: path pattern \"~ada/.ssh\" must be workspace-relative, absolute, or start with ~/".to_owned(),
        ),
        (
            json!({"rules": {"path": [{"pattern": "src/a**", "state": "deny"}]}}),
            "rules.path[0]: path pattern \"src/a**\": ** must be a whole path component".to_owned(),
        ),
        (
            json!({"rules": {"outside_workspace": [{"pattern": "tmp", "state": "allow"}]}}),
            "rules.outside_workspace[0]: outside_workspace pattern \"tmp\" must be an absolute path or start with ~/".to_owned(),
        ),
        (
            json!({"rules": {"outside_workspace": [{"pattern": "/tmp/*", "state": "allow"}]}}),
            "rules.outside_workspace[0]: outside_workspace pattern \"/tmp/*\" is a directory prefix, not a glob".to_owned(),
        ),
        (
            json!({"rules": {"tools": [{"pattern": "run_shel", "state": "deny"}]}}),
            "rules.tools[0]: tools pattern \"run_shel\" matches no core tool (read_file, edit_file, write_file, apply_patch, run_shell, git_status, git_diff, tool_result_get, skill_read); name an extension command as <extension-id>.<command>".to_owned(),
        ),
        (
            json!({"rules": {"tools": [{"pattern": "code-swarm.", "state": "deny"}]}}),
            "rules.tools[0]: tools pattern \"code-swarm.\" must name a core tool or <extension-id>.<command>".to_owned(),
        ),
        (
            json!({"ask_timeout": 86_401}),
            "ask_timeout: expected a whole number of seconds from 0 to 86400".to_owned(),
        ),
        (json!({"ask_timeout": -1}), "ask_timeout: expected a whole number".to_owned()),
        (json!({"ask_timeout": 1.5}), "ask_timeout: expected a whole number".to_owned()),
        (json!({"ask_timeout": "60"}), "ask_timeout: expected a whole number".to_owned()),
        (
            json!({"extension_reviewers": ["policy-check"]}),
            "extension_reviewers[0]: \"policy-check\" must be <extension-id>.<command>".to_owned(),
        ),
        (
            json!({"extension_reviewers": ["policy-check.*"]}),
            "extension_reviewers[0]: \"policy-check.*\" must be <extension-id>.<command>".to_owned(),
        ),
        (
            json!({"extension_reviewers": ["a.review", "a.review"]}),
            "extension_reviewers[1]: \"a.review\" is listed twice".to_owned(),
        ),
    ];
    for (fields, expected) in cases {
        let mut body = fields.as_object().expect("object").clone();
        body.insert("extends".to_owned(), "full".into());
        let text = json!({"profiles": {"agent": body}}).to_string();
        let message = parse(&text).expect_err("invalid").to_string();
        let prefix = format!("{PROFILES}: profile \"agent\": {expected}");
        assert!(message.starts_with(&prefix), "{message}\nexpected {prefix}");
    }

    // The core-name check accepts globs and every core tool name.
    parse(
        r#"{"profiles": {"agent": {"extends": "full", "rules": {"tools": [
            {"pattern": "*", "state": "ask"},
            {"pattern": "git_*", "state": "ask"},
            {"pattern": "*_file", "state": "ask"},
            {"pattern": "skill_read", "state": "deny"}
        ]}}}}"#,
    )
    .expect("core tool patterns parse");
}

/// A repeated key inside a rule is an error, not last-wins: `state` written
/// twice could otherwise turn a `deny` into an `allow`.
#[test]
fn duplicate_keys_inside_rules_are_errors() {
    let message = parse(
        r#"{"profiles": {"agent": {"extends": "full", "rules": {"shell": [
            {"pattern": "rm *", "state": "deny", "state": "allow"}
        ]}}}}"#,
    )
    .expect_err("duplicate key")
    .to_string();
    assert!(message.contains("duplicate key `state`"), "{message}");

    let temp = tempfile::tempdir().expect("temp");
    std::fs::create_dir_all(temp.path().join(".euler")).expect("dir");
    std::fs::write(
        temp.path().join(PROJECT_OVERLAY_PATH),
        r#"{"rules": {"shell": [], "shell": [{"pattern": "rm *", "state": "deny"}]}}"#,
    )
    .expect("overlay");
    let message = ProjectOverlay::load(temp.path())
        .expect_err("duplicate surface")
        .to_string();
    assert!(message.contains("duplicate key `shell`"), "{message}");
}

#[test]
fn record_round_trips_rule_layers_and_rejects_tampering() {
    let profile = ExecutionProfile {
        name: "agent".to_owned(),
        fields: ProfileFields {
            rules: vec![
                shell_layer(
                    RuleLayerLabel::Profile,
                    vec![
                        rule("kubectl *", RuleState::Deny),
                        rule("kubectl get *", RuleState::Allow),
                    ],
                ),
                shell_layer(
                    RuleLayerLabel::Overlay,
                    vec![rule("git push *", RuleState::Ask)],
                ),
                shell_layer(
                    RuleLayerLabel::Subagent,
                    vec![rule("rm *", RuleState::Deny)],
                ),
            ],
            ask_timeout: Some(0),
            extension_reviewers: vec!["policy-check.review".to_owned()],
            ..full::fields()
        },
    };
    let record = profile.record();
    assert_eq!(
        record["fields"]["rules"],
        json!([
            {"layer": "profile", "rules": {"shell": [
                {"pattern": "kubectl *", "state": "deny"},
                {"pattern": "kubectl get *", "state": "allow"}
            ]}},
            {"layer": "overlay", "rules": {"shell": [{"pattern": "git push *", "state": "ask"}]}},
            {"layer": "subagent", "rules": {"shell": [{"pattern": "rm *", "state": "deny"}]}}
        ])
    );
    assert_eq!(record["fields"]["ask_timeout"], 0);
    assert_eq!(
        record["fields"]["extension_reviewers"],
        json!(["policy-check.review"])
    );
    assert_eq!(ExecutionProfile::from_record(&record), Ok(profile));

    let mut tampered = record.clone();
    tampered["fields"]["rules"][0]["rules"]["shell"][0]["state"] = "allow".into();
    assert_eq!(
        ExecutionProfile::from_record(&tampered),
        Err("fields_sha256 does not match the recorded fields".to_owned())
    );
    let mut reordered = record.clone();
    let layers = reordered["fields"]["rules"].as_array_mut().expect("layers");
    layers.swap(0, 1);
    assert_eq!(
        ExecutionProfile::from_record(&reordered),
        Err("fields_sha256 does not match the recorded fields".to_owned())
    );
    let mut unknown = record;
    unknown["fields"]["rules"][1]["rules"]["shell"][0]["why"] = "x".into();
    assert!(ExecutionProfile::from_record(&unknown).is_err());
}

#[test]
fn overlay_rules_join_as_an_overlay_layer_and_only_tighten() {
    let config = parse(
        r#"{"profiles": {
            "dev": {"extends": "full", "ask_timeout": 300, "rules": {"shell": [
                {"pattern": "kubectl *", "state": "deny"},
                {"pattern": "kubectl get *", "state": "allow"}
            ]}},
            "plain": {"extends": "full"}
        }}"#,
    )
    .expect("parses");
    let temp = tempfile::tempdir().expect("temp");
    let tightening = overlay(
        temp.path(),
        r#"{"ask_timeout": 60, "rules": {"shell": [{"pattern": "kubectl get *", "state": "ask"}]}}"#,
    );
    let mut dev = config.named("dev").expect("named");
    tightening.apply(&mut dev).expect("overlay tightens");
    assert_eq!(dev.fields.ask_timeout, Some(60));
    assert_eq!(
        dev.fields.rules,
        vec![
            shell_layer(
                RuleLayerLabel::Profile,
                vec![
                    rule("kubectl *", RuleState::Deny),
                    rule("kubectl get *", RuleState::Allow),
                ],
            ),
            shell_layer(
                RuleLayerLabel::Overlay,
                vec![rule("kubectl get *", RuleState::Ask)]
            ),
        ]
    );
    // Any timeout tightens a profile without one.
    let mut plain = config.named("plain").expect("named");
    tightening.apply(&mut plain).expect("overlay tightens");
    assert_eq!(plain.fields.ask_timeout, Some(60));
    // The overlay, not the profile, carries the unenforced rules.
    assert_eq!(
        config.resolve(
            Some("plain"),
            ProfileContext::Interactive,
            Some(&tightening)
        ),
        Err(ProfileError::OverlayUnenforced {
            profile: "plain".to_owned(),
            field: "rules",
        })
    );

    let overlay_file = temp.path().join(PROJECT_OVERLAY_PATH);
    for (text, expected) in [
        (
            r#"{"rules": {"shell": [{"pattern": "rm *", "state": "deny"}, {"pattern": "ls *", "state": "allow"}]}}"#,
            "rules.shell[1]: \"ls *\" is an allow rule; a project overlay can only ask or deny",
        ),
        (
            r#"{"ask_timeout": 301}"#,
            "ask_timeout 301 is higher than the profile's 300; a project overlay can only lower it",
        ),
        (
            r#"{"extension_reviewers": ["policy-check.review"]}"#,
            "extension_reviewers would let repository content choose extension code to run, so a \
             project overlay cannot set it",
        ),
    ] {
        let loosening = overlay(temp.path(), text);
        let mut dev = config.named("dev").expect("named");
        let message = loosening.apply(&mut dev).expect_err("refused").to_string();
        assert_eq!(
            message,
            format!("{}: {expected} (profile \"dev\")", overlay_file.display())
        );
    }
}

#[test]
fn a_named_child_keeps_only_ask_and_deny_layers_and_the_lower_timeout() {
    let parent = ExecutionProfile {
        fields: ProfileFields {
            ask_timeout: Some(300),
            extension_reviewers: vec!["policy-check.review".to_owned()],
            ..with_rules(
                "dev",
                vec![
                    shell_layer(
                        RuleLayerLabel::Profile,
                        vec![
                            rule("kubectl *", RuleState::Deny),
                            rule("kubectl get *", RuleState::Allow),
                        ],
                    ),
                    shell_layer(
                        RuleLayerLabel::Overlay,
                        vec![rule("git push *", RuleState::Ask)],
                    ),
                ],
            )
            .fields
        },
        name: "dev".to_owned(),
    };
    let subagent = ExecutionProfile {
        fields: ProfileFields {
            ask_timeout: Some(60),
            extension_reviewers: vec!["other.review".to_owned()],
            ..with_rules(
                "helper",
                vec![
                    shell_layer(
                        RuleLayerLabel::Profile,
                        vec![
                            rule("cargo *", RuleState::Allow),
                            rule("rm *", RuleState::Deny),
                            rule("kubectl get pods", RuleState::Allow),
                        ],
                    ),
                    // The same overlay applied to the child profile.
                    shell_layer(
                        RuleLayerLabel::Overlay,
                        vec![rule("git push *", RuleState::Ask)],
                    ),
                ],
            )
            .fields
        },
        name: "helper".to_owned(),
    };

    let child = parent.child_profile(Some(&subagent));
    assert_eq!(child.name, "helper");
    assert_eq!(
        child.fields.rules,
        vec![
            shell_layer(
                RuleLayerLabel::Profile,
                vec![rule("kubectl *", RuleState::Deny)]
            ),
            shell_layer(
                RuleLayerLabel::Overlay,
                vec![rule("git push *", RuleState::Ask)]
            ),
            shell_layer(
                RuleLayerLabel::Subagent,
                vec![rule("rm *", RuleState::Deny)]
            ),
        ]
    );
    assert_eq!(child.fields.ask_timeout, Some(60));
    assert_eq!(
        child.fields.extension_reviewers,
        vec!["policy-check.review".to_owned()]
    );
    assert_eq!(ExecutionProfile::from_record(&child.record()), Ok(child));

    // An absent timeout is no limit, so the other side's applies.
    let untimed = ExecutionProfile {
        fields: ProfileFields {
            ask_timeout: None,
            ..subagent.fields.clone()
        },
        ..subagent.clone()
    };
    assert_eq!(
        parent.child_profile(Some(&untimed)).fields.ask_timeout,
        Some(300)
    );

    // `inherit` is the parent's profile, allow rules included.
    assert_eq!(parent.child_profile(None), parent);
}

#[test]
fn resume_keeps_recorded_layers_and_joins_added_ask_and_deny_rules() {
    let recorded = ExecutionProfile {
        fields: ProfileFields {
            ask_timeout: Some(120),
            extension_reviewers: vec!["a.review".to_owned()],
            ..with_rules(
                "dev",
                vec![shell_layer(
                    RuleLayerLabel::Profile,
                    vec![
                        rule("git push *", RuleState::Ask),
                        rule("cargo *", RuleState::Allow),
                    ],
                )],
            )
            .fields
        },
        name: "dev".to_owned(),
    };

    // Unchanged: nothing joins.
    let mut same = recorded.fields.clone();
    same.join_resumed_policy(&recorded.fields, &recorded.fields);
    assert_eq!(same, recorded.fields);
    assert!(same
        .policy_fields_differing_from(&recorded.fields)
        .is_empty());

    // The user added a deny, removed the allow, added an overlay ask, raised
    // the timeout, and swapped the reviewer.
    let resolved = ExecutionProfile {
        fields: ProfileFields {
            ask_timeout: Some(600),
            extension_reviewers: vec!["b.review".to_owned()],
            ..with_rules(
                "dev",
                vec![
                    shell_layer(
                        RuleLayerLabel::Profile,
                        vec![
                            rule("git push *", RuleState::Ask),
                            rule("rm *", RuleState::Deny),
                        ],
                    ),
                    shell_layer(
                        RuleLayerLabel::Overlay,
                        vec![rule("curl *", RuleState::Ask)],
                    ),
                ],
            )
            .fields
        },
        name: "dev".to_owned(),
    };
    let mut joined = resolved.fields.clone();
    joined.join_resumed_policy(&recorded.fields, &resolved.fields);
    assert_eq!(
        joined.rules,
        vec![
            recorded.fields.rules[0].clone(),
            resolved.fields.rules[0].clone(),
            resolved.fields.rules[1].clone(),
        ]
    );
    assert_eq!(joined.ask_timeout, Some(120));
    assert_eq!(
        joined.extension_reviewers,
        vec!["a.review".to_owned(), "b.review".to_owned()]
    );
    assert_eq!(
        joined.policy_fields_differing_from(&resolved.fields),
        vec!["ask_timeout", "extension_reviewers", "rules"]
    );
    let recorded_record = ExecutionProfile {
        name: "dev".to_owned(),
        fields: joined.clone(),
    }
    .record();

    // A second resume with the same configuration adds nothing more, and an
    // added allow rule never joins.
    let mut again = joined.clone();
    again.join_resumed_policy(&joined, &resolved.fields);
    assert_eq!(again, joined);
    let subset = ProfileFields {
        extension_reviewers: vec!["a.review".to_owned()],
        ..resolved.fields.clone()
    };
    let mut reviewers = joined.clone();
    reviewers.join_resumed_policy(&joined, &subset);
    assert_eq!(reviewers.extension_reviewers, joined.extension_reviewers);
    let with_allow = with_rules(
        "dev",
        vec![shell_layer(
            RuleLayerLabel::Profile,
            vec![
                rule("git push *", RuleState::Ask),
                rule("rm *", RuleState::Deny),
                rule("make *", RuleState::Allow),
            ],
        )],
    );
    let mut allowed = joined.clone();
    allowed.join_resumed_policy(&joined, &with_allow.fields);
    assert_eq!(allowed.rules, joined.rules);

    // The joined profile is the one resume records, and it round trips.
    assert_eq!(
        ExecutionProfile::from_record(&recorded_record).map(|profile| profile.fields),
        Ok(joined)
    );

    // This build refuses the joined profile rather than running without
    // the recorded rules.
    assert_eq!(
        ResumeProfile::choose(Some(recorded.clone()), resolved.clone(), false, None),
        Err(ProfileError::Unenforced {
            profile: "dev".to_owned(),
            field: "rules",
        })
    );
    // A resolved profile without rules still keeps the recorded ones, and
    // the refusal names the recording rather than asking the user to remove
    // rules their profile does not set.
    let error = ResumeProfile::choose(Some(recorded), with_rules("dev", Vec::new()), false, None)
        .expect_err("recorded rules are refused");
    assert_eq!(
        error,
        ProfileError::RecordedUnenforced {
            profile: "dev".to_owned(),
            field: "rules",
        }
    );
    assert_eq!(
        error.to_string(),
        "this session recorded profile \"dev\" with rules, which this version parses and \
         records but does not enforce yet, and resume keeps the recorded rules; pass --profile \
         to resume under a profile without it"
    );
}

#[test]
fn resume_that_keeps_the_recorded_profile_joins_policy_fields() {
    let locked_with = |rules: Vec<RuleLayer>, ask_timeout: Option<u32>| ExecutionProfile {
        name: "careful".to_owned(),
        fields: ProfileFields {
            rules,
            ask_timeout,
            ..locked::fields()
        },
    };
    let temp = tempfile::tempdir().expect("temp");
    let lower_timeout = overlay(temp.path(), r#"{"ask_timeout": 120}"#);

    // `full` is looser than the recorded `locked` fields, so the recorded
    // profile applies, and its rules stay even though the resolved profile
    // has none and the overlay sets only a timeout.
    let recorded = locked_with(
        vec![shell_layer(
            RuleLayerLabel::Profile,
            vec![rule("rm *", RuleState::Deny)],
        )],
        None,
    );
    let resolved = ExecutionProfile {
        name: "dev".to_owned(),
        fields: ProfileFields {
            ask_timeout: Some(120),
            ..full::fields()
        },
    };
    assert_eq!(
        ResumeProfile::choose(
            Some(recorded),
            resolved.clone(),
            false,
            Some(&lower_timeout)
        ),
        Err(ProfileError::RecordedUnenforced {
            profile: "careful".to_owned(),
            field: "rules",
        })
    );

    // A deny rule added to the resolved profile joins the kept recorded
    // profile, so the refusal names rules (not only the timeout) and the
    // resolved profile that set them, not the recording.
    let with_deny = ExecutionProfile {
        fields: ProfileFields {
            rules: vec![shell_layer(
                RuleLayerLabel::Profile,
                vec![rule("curl *", RuleState::Deny)],
            )],
            ..resolved.fields.clone()
        },
        ..resolved.clone()
    };
    assert_eq!(
        ResumeProfile::choose(
            Some(locked_with(Vec::new(), None)),
            with_deny,
            false,
            Some(&lower_timeout)
        ),
        Err(ProfileError::Unenforced {
            profile: "dev".to_owned(),
            field: "rules",
        })
    );

    // `resolved` stands for a profile whose timeout the overlay already
    // lowered to 120. A recorded 60 does not make the overlay's 120 an
    // error, and the join keeps the recorded 60, which the refusal names.
    let recorded = locked_with(Vec::new(), Some(60));
    assert_eq!(
        ResumeProfile::choose(Some(recorded), resolved, false, Some(&lower_timeout)),
        Err(ProfileError::RecordedUnenforced {
            profile: "careful".to_owned(),
            field: "ask_timeout",
        })
    );
}

#[test]
fn resume_summary_names_joined_fields_and_counts_rule_layers() {
    let recorded = with_rules(
        "dev",
        vec![shell_layer(
            RuleLayerLabel::Profile,
            vec![rule("rm *", RuleState::Deny)],
        )],
    );
    let joined = ResumeProfile {
        profile: recorded.clone(),
        recorded: Some(recorded.clone()),
        basis: ResumeProfileBasis::Resolved,
        joined_fields: vec!["rules"],
    };
    assert_eq!(
        joined.summary().as_deref(),
        Some(
            "execution profile: dev: rules kept as recorded with only tightening changes \
             joined; pass --profile to apply the profile as written"
        )
    );

    let mut two_layers = recorded.clone();
    two_layers.fields.rules.push(shell_layer(
        RuleLayerLabel::Overlay,
        vec![rule("curl *", RuleState::Ask)],
    ));
    two_layers.fields.ask_timeout = Some(30);
    let changed = ResumeProfile {
        profile: two_layers,
        recorded: Some(recorded.clone()),
        basis: ResumeProfileBasis::Resolved,
        joined_fields: Vec::new(),
    };
    assert_eq!(
        changed.summary().as_deref(),
        Some("execution profile: dev (recorded: dev; changed ask_timeout unset -> 30, rules 1 layer -> 2 layers)")
    );

    // A changed rule with the same layer count still reads as a change.
    let mut edited = recorded.clone();
    edited.fields.rules[0].rules.shell[0].state = RuleState::Ask;
    let same_count = ResumeProfile {
        profile: edited,
        recorded: Some(recorded),
        basis: ResumeProfileBasis::Resolved,
        joined_fields: Vec::new(),
    };
    assert_eq!(
        same_count.summary().as_deref(),
        Some("execution profile: dev (recorded: dev; changed rules edited (1 layer))")
    );
}
