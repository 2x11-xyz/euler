//! ADR 0027 rollout step 1: parsing, validation, the layered record, and the
//! resume and child rules for `rules`, `ask_timeout`, and
//! `extension_reviewers`.

use super::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;

const PROFILES: &str = "/home/ada/.euler/profiles.json";

fn parse(text: &str) -> Result<ProfilesConfig, ProfileError> {
    ProfilesConfig::parse(Path::new(PROFILES), text, &InstalledExtensions::default())
}

fn overlay(root: &Path, text: &str) -> ProjectOverlay {
    let directory = root.join(".euler");
    std::fs::create_dir_all(&directory).expect("overlay dir");
    std::fs::write(directory.join("profile-overlay.json"), text).expect("overlay");
    ProjectOverlay::load(root, &InstalledExtensions::default())
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

fn path_layer(layer: RuleLayerLabel, rules: Vec<PermissionRule>) -> RuleLayer {
    RuleLayer {
        layer,
        rules: ProfileRules {
            path: rules,
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
fn profile_rules_parse_into_one_profile_layer_and_every_field_is_enforced() {
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
            "reviewed": {"extends": "full", "extension_reviewers": ["policy-check.review"]},
            "outside": {"extends": "full", "rules": {
                "outside_workspace": [{"pattern": "/tmp", "state": "ask"}]
            }},
            "enforced": {"extends": "full", "rules": {
                "shell": [{"pattern": "git push *", "state": "ask"}],
                "tools": [{"pattern": "code-swarm.*", "state": "deny"}]
            }}
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

    // `shell` and `tools` rules are enforced (ADR 0027 rollout step 2).
    let enforced = config
        .resolve(Some("enforced"), ProfileContext::Headless, None)
        .expect("shell and tools rules are enforced");
    assert_eq!(enforced.fields.rules.len(), 1);

    // Every rule surface is enforced (ADR 0027 rollout step 3).
    let outside = config
        .resolve(Some("outside"), ProfileContext::Headless, None)
        .expect("outside_workspace rules are enforced");
    assert_eq!(outside.fields.rules.len(), 1);

    // `ask_timeout` is enforced (ADR 0027 rollout step 5).
    let timeout = config
        .resolve(Some("timeout"), ProfileContext::Headless, None)
        .expect("ask_timeout is enforced");
    assert_eq!(timeout.fields.ask_timeout, Some(0));

    // `extension_reviewers` is enforced (ADR 0027 rollout step 6); a
    // reviewer that is not enabled is refused when the session wires its
    // extensions.
    for name in ["agent", "reviewed"] {
        let reviewed = config
            .resolve(Some(name), ProfileContext::Headless, None)
            .expect("extension_reviewers is enforced");
        assert_eq!(
            reviewed.fields.extension_reviewers,
            vec!["policy-check.review".to_owned()]
        );
    }
}

#[test]
fn a_subagent_default_with_every_permission_field_resolves() {
    let config = parse(
        r#"{"defaults": {"subagent": "helper"}, "profiles": {
            "helper": {"extends": "full", "ask_timeout": 30,
                       "extension_reviewers": ["policy-check.review"],
                       "rules": {"path": [{"pattern": "src/**", "state": "deny"}]}}
        }}"#,
    )
    .expect("parses");
    let helper = config
        .resolve_subagent(None)
        .expect("resolves")
        .expect("named subagent");
    assert_eq!(helper.fields.ask_timeout, Some(30));
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
    let message = ProjectOverlay::load(temp.path(), &InstalledExtensions::default())
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
    // The overlay's timeout is enforced (ADR 0027 rollout step 5).
    let resolved = config
        .resolve(
            Some("plain"),
            ProfileContext::Interactive,
            Some(&tightening),
        )
        .expect("the overlay's timeout is enforced");
    assert_eq!(resolved.fields.ask_timeout, Some(60));

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
    // A child's asks do not reach the extension reviewers in this build
    // (ADR 0027 section 6 amendment), so its record lists none.
    assert!(child.fields.extension_reviewers.is_empty());
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

    // `inherit` is the parent's profile, allow rules included, without
    // its extension reviewers.
    let mut inherited = parent.clone();
    inherited.fields.extension_reviewers.clear();
    assert_eq!(parent.child_profile(None), inherited);
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
        Ok(joined.clone())
    );

    // Resume applies the joined profile: the recorded extension reviewers
    // stay (ADR 0027 rollout step 6), so does a resolved profile without
    // any.
    let chosen = ResumeProfile::choose(Some(recorded.clone()), resolved.clone(), false, None)
        .expect("extension_reviewers is enforced");
    assert_eq!(chosen.profile.fields, joined);
    let kept = ResumeProfile::choose(
        Some(recorded.clone()),
        with_rules("dev", Vec::new()),
        false,
        None,
    )
    .expect("the recorded reviewers stay");
    assert_eq!(
        kept.profile.fields.extension_reviewers,
        vec!["a.review".to_owned()]
    );
    // A recorded timeout is enforced (ADR 0027 rollout step 5): resume
    // keeps the lower one.
    let timed = ExecutionProfile {
        fields: ProfileFields {
            extension_reviewers: Vec::new(),
            ..recorded.fields
        },
        name: "dev".to_owned(),
    };
    let kept = ResumeProfile::choose(Some(timed), with_rules("dev", Vec::new()), false, None)
        .expect("ask_timeout is enforced");
    assert_eq!(kept.profile.fields.ask_timeout, Some(120));
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
    // has none.
    let recorded_layer = path_layer(
        RuleLayerLabel::Profile,
        vec![rule("secrets/**", RuleState::Deny)],
    );
    let kept = ResumeProfile::choose(
        Some(locked_with(vec![recorded_layer.clone()], None)),
        ExecutionProfile {
            name: "dev".to_owned(),
            fields: full::fields(),
        },
        false,
        None,
    )
    .expect("path rules are enforced");
    assert_eq!(kept.profile.name, "careful");
    assert_eq!(kept.profile.fields.rules, vec![recorded_layer.clone()]);

    // A deny rule added to the resolved profile joins the kept recorded
    // profile as a further layer.
    let added_layer = path_layer(
        RuleLayerLabel::Profile,
        vec![rule("build/**", RuleState::Deny)],
    );
    let joined = ResumeProfile::choose(
        Some(locked_with(vec![recorded_layer.clone()], None)),
        ExecutionProfile {
            name: "dev".to_owned(),
            fields: ProfileFields {
                rules: vec![added_layer.clone()],
                ..full::fields()
            },
        },
        false,
        None,
    )
    .expect("path rules are enforced");
    assert_eq!(joined.profile.name, "careful");
    assert_eq!(
        joined.profile.fields.rules,
        vec![recorded_layer, added_layer]
    );
    assert_eq!(joined.joined_fields, vec!["rules"]);

    let resolved = ExecutionProfile {
        name: "dev".to_owned(),
        fields: ProfileFields {
            ask_timeout: Some(120),
            ..full::fields()
        },
    };
    // `resolved` stands for a profile whose timeout the overlay already
    // lowered to 120. A recorded 60 does not make the overlay's 120 an
    // error, and the join keeps the recorded 60.
    let recorded = locked_with(Vec::new(), Some(60));
    let kept = ResumeProfile::choose(Some(recorded), resolved, false, Some(&lower_timeout))
        .expect("ask_timeout is enforced");
    assert_eq!(kept.profile.name, "careful");
    assert_eq!(kept.profile.fields.ask_timeout, Some(60));
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

/// An installed `code-swarm` extension whose `review` command declares the
/// model tool `code_swarm_review`, and a `lint` extension whose `check`
/// command declares `lint_review`.
fn installed_extensions() -> InstalledExtensions {
    let linked = |id: &str, command: &str, tool: &str| euler_sdk::LinkedExtension {
        id: id.to_owned(),
        materialization: euler_sdk::ExtensionMaterialization::Linked,
        source_path: std::path::PathBuf::from("/opt/extensions").join(id),
        manifest_sha256: String::new(),
        updated_ts_ms: 0,
        status: euler_sdk::LinkedExtensionStatus::NeedsReview,
        descriptor: serde_json::from_value(json!({
            "id": id,
            "display_name": id,
            "version": "0.1.0",
            "runtime_kind": "managed-process",
            "capabilities": [],
            "commands": [{
                "name": command,
                "display_name": command,
                "summary": "",
                "required_capabilities": [],
                "invocation": "agent-only",
                "model_tool": {"name": tool, "description": "", "input_schema": {"type": "object"}}
            }]
        }))
        .expect("descriptor"),
        broken_reason: None,
    };
    InstalledExtensions::from_linked(&[
        linked("lint", "check", "lint_review"),
        linked("code-swarm", "review", "code_swarm_review"),
    ])
}

fn tools_profile(pattern: &str) -> String {
    json!({"profiles": {"ci": {"extends": "full", "rules": {
        "tools": [{"pattern": pattern, "state": "deny"}]
    }}}})
    .to_string()
}

/// ADR 0027 section 8: a `tools` pattern without `.` that names an
/// installed extension's model-visible tool, not a core tool, is an error
/// that names the `<extension-id>.<command>` form to write.
#[test]
fn a_tools_pattern_naming_an_extension_model_tool_names_its_command() {
    let installed = installed_extensions();
    let parse_with = |pattern: &str| {
        ProfilesConfig::parse(Path::new(PROFILES), &tools_profile(pattern), &installed)
            .expect_err("not a core tool")
            .to_string()
    };
    let message = parse_with("code_swarm_review");
    assert!(
        message.contains(
            "rules.tools[0]: tools pattern \"code_swarm_review\" matches no core tool; it is the \
             model tool of extension command code-swarm.review, so write \"code-swarm.review\""
        ),
        "{message}"
    );
    let message = parse_with("*_*_review*");
    assert!(
        message.contains("extension command code-swarm.review"),
        "{message}"
    );
    let message = parse_with("lint_*");
    assert!(message.contains("lint.check"), "{message}");
    // A glob that matches several model tools lists every command.
    let message = parse_with("*_review");
    assert!(
        message.contains(
            "it matches the model tools of extension commands \"code-swarm.review\", \
             \"lint.check\", so name each as <extension-id>.<command>"
        ),
        "{message}"
    );
    // A name no core or extension tool has keeps the core-tool error.
    let message = parse_with("code_swarm_reveiw");
    assert!(
        message.contains("name an extension command as"),
        "{message}"
    );

    // The overlay is checked the same way.
    let temp = tempfile::tempdir().expect("temp");
    std::fs::create_dir_all(temp.path().join(".euler")).expect("overlay dir");
    std::fs::write(
        temp.path().join(".euler/profile-overlay.json"),
        r#"{"rules": {"tools": [{"pattern": "code_swarm_review", "state": "deny"}]}}"#,
    )
    .expect("overlay");
    let message = ProjectOverlay::load(temp.path(), &installed)
        .expect_err("not a core tool")
        .to_string();
    assert!(
        message.contains("so write \"code-swarm.review\""),
        "{message}"
    );
}

/// ADR 0027 resolved question 19: a `tools` rule whose literal extension id
/// names no installed extension keeps the rule and gives a notice naming
/// the rule and listing the installed ids.
#[test]
fn a_tools_rule_naming_an_uninstalled_extension_gives_a_notice() {
    let temp = tempfile::tempdir().expect("temp");
    let overlay = overlay(
        temp.path(),
        r#"{"rules": {"tools": [{"pattern": "policy.*", "state": "ask"}]}}"#,
    );
    let config = parse(
        &json!({"profiles": {"ci": {"extends": "full", "rules": {"tools": [
            {"pattern": "code-swarm.*", "state": "deny"},
            {"pattern": "code_swarm.*", "state": "deny"},
            {"pattern": "*.review", "state": "deny"},
            {"pattern": "run_shell", "state": "ask"}
        ]}}}})
        .to_string(),
    )
    .expect("parses");
    let profile = config
        .resolve(Some("ci"), ProfileContext::Headless, Some(&overlay))
        .expect("resolves");
    assert_eq!(profile.fields.rules[0].rules.tools.len(), 4, "rules kept");
    assert_eq!(
        profile.missing_extension_notices(&installed_extensions()),
        vec![
            "notice: profile \"ci\" rules.tools[1] \"code_swarm.*\" names extension \
             \"code_swarm\", which is not installed, so the rule matches nothing until it is; \
             installed extensions: code-swarm, lint"
                .to_owned(),
            "notice: .euler/profile-overlay.json rules.tools[0] \"policy.*\" names extension \
             \"policy\", which is not installed, so the rule matches nothing until it is; \
             installed extensions: code-swarm, lint"
                .to_owned(),
        ]
    );
    assert_eq!(
        profile.missing_extension_notices(&InstalledExtensions::default())[0],
        "notice: profile \"ci\" rules.tools[0] \"code-swarm.*\" names extension \
         \"code-swarm\", which is not installed, so the rule matches nothing until it is; no \
         extensions are installed"
    );
}
