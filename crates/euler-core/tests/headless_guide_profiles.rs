//! The team `profiles.json` in `docs/guides/headless.md` ("Team profiles",
//! ADR 0027 rollout step 7) parses with the real parser, resolves to
//! profiles this build enforces, and decides the commands and paths the
//! guide describes the way the guide says. Editing the example without
//! updating these claims, or changing the parser or the rules so the example
//! no longer behaves as written, fails here.

use euler_core::command_safety::{rule_segments, shell_path_targets, PathAccess, PathTarget};
use euler_core::execution_profile::{
    rule_match, ExecutionProfile, InstalledExtensions, ProfileContext, ProfileError,
    ProfilesConfig, RequestPaths, RuleMatch, RuleState,
};
use euler_core::guardian::PermissionReviewer;
use std::path::{Path, PathBuf};

const SECTION: &str = "## Team profiles";

/// The one `json` block under the guide's "Team profiles" heading.
fn guide_example() -> String {
    let guide = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/guides/headless.md");
    let text = std::fs::read_to_string(&guide).expect("read docs/guides/headless.md");
    let (_, section) = text
        .split_once(&format!("\n{SECTION}\n"))
        .expect("the guide has a Team profiles section");
    let section = section.split("\n## ").next().unwrap_or_default();
    let blocks: Vec<&str> = section
        .split("```json\n")
        .skip(1)
        .map(|rest| rest.split_once("\n```").expect("closed json fence").0)
        .collect();
    assert_eq!(blocks.len(), 1, "Team profiles has one json example");
    blocks[0].to_owned()
}

fn parse(text: &str) -> Result<ProfilesConfig, ProfileError> {
    ProfilesConfig::parse(
        Path::new("/home/ada/.euler/profiles.json"),
        text,
        &InstalledExtensions::default(),
    )
}

fn guide_config() -> ProfilesConfig {
    parse(&guide_example()).expect("the guide's profiles.json parses")
}

fn resolve(
    config: &ProfilesConfig,
    name: Option<&str>,
    context: ProfileContext,
) -> ExecutionProfile {
    config
        .resolve(name, context, None)
        .unwrap_or_else(|error| panic!("{name:?} resolves and is enforced: {error}"))
}

/// The state a matched rule sets. An unresolved operand is asserted on
/// [`RuleMatch::Unresolved`] directly, so it is never mistaken for an `ask`
/// rule.
fn state(found: Option<RuleMatch>) -> Option<RuleState> {
    match found? {
        RuleMatch::Rule { rule, .. } => Some(rule.state),
        unresolved @ RuleMatch::Unresolved { .. } => panic!("unexpected {unresolved:?}"),
    }
}

/// A `run_shell` command as the gate sees it: its commands on the `shell`
/// surface and the paths it names on the path surfaces.
fn run_shell(profile: &ExecutionProfile, command: &str, root: &Path) -> Option<RuleMatch> {
    let segments = rule_segments(command);
    let targets = shell_path_targets(command);
    let paths = RequestPaths {
        targets: &targets,
        workspace_root: root,
    };
    rule_match(&profile.fields.rules, Some(&segments), Some(paths), None)
}

fn shell(profile: &ExecutionProfile, command: &str, root: &Path) -> Option<RuleState> {
    state(run_shell(profile, command, root))
}

/// A structured file tool writing `path`.
fn write_file(profile: &ExecutionProfile, path: &Path, root: &Path) -> Option<RuleState> {
    let targets = [PathTarget::new(
        path.display().to_string(),
        PathAccess::Write,
    )];
    let paths = RequestPaths {
        targets: &targets,
        workspace_root: root,
    };
    state(rule_match(&profile.fields.rules, None, Some(paths), None))
}

fn tool(profile: &ExecutionProfile, name: &str) -> Option<RuleState> {
    state(rule_match(&profile.fields.rules, None, None, Some(name)))
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME"))
}

#[test]
fn guide_team_profiles_parse_and_resolve_to_enforced_profiles() {
    let config = guide_config();
    let dev = resolve(&config, None, ProfileContext::Interactive);
    assert_eq!(dev.name, "dev");
    let agent = resolve(&config, None, ProfileContext::Headless);
    assert_eq!(agent.name, "agent");
    assert_eq!(agent.fields.ask_timeout, Some(300));
    assert_eq!(agent.fields.reviewer, PermissionReviewer::Guardian);
    let ci = resolve(&config, Some("ci"), ProfileContext::Headless);
    let workspace = resolve(&config, Some("workspace"), ProfileContext::Headless);
    let mut ci_without_rules = ci.fields.clone();
    assert!(!ci_without_rules.rules.is_empty());
    ci_without_rules.rules = Vec::new();
    assert_eq!(
        ci_without_rules, workspace.fields,
        "ci is workspace plus rules"
    );
    assert!(config
        .resolve_subagent(None)
        .expect("subagent default")
        .is_none());
}

/// The guide says extension reviewers are not available yet: a profile that
/// sets them is refused at startup, naming the field. When they ship, this
/// fails and the guide's paragraph needs updating.
#[test]
fn guide_example_with_extension_reviewers_is_refused_naming_the_field() {
    let mut example: serde_json::Value =
        serde_json::from_str(&guide_example()).expect("example is JSON");
    example["profiles"]["agent"]["extension_reviewers"] =
        serde_json::json!(["policy-check.review"]);
    let config = parse(&example.to_string()).expect("extension_reviewers parses");
    let error = config
        .resolve(None, ProfileContext::Headless, None)
        .expect_err("extension_reviewers is not enforced yet")
        .to_string();
    assert!(error.contains("extension_reviewers"), "{error}");
}

#[test]
fn guide_dev_and_agent_shell_rules_decide_as_described() {
    let config = guide_config();
    let dev = resolve(&config, Some("dev"), ProfileContext::Interactive);
    let agent = resolve(&config, Some("agent"), ProfileContext::Headless);
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path();
    for profile in [&dev, &agent] {
        let shell = |command| shell(profile, command, root);
        assert_eq!(shell("git push origin main"), Some(RuleState::Ask));
        assert_eq!(shell("git push --force origin main"), Some(RuleState::Deny));
        assert_eq!(
            shell("git push --force-with-lease origin main"),
            Some(RuleState::Deny)
        );
        // Patterns match words, not Git semantics.
        assert_eq!(shell("git push -f origin main"), Some(RuleState::Ask));
        assert_eq!(shell("git -C repo push --force"), None);
        assert_eq!(shell("cargo test"), None);
    }
    assert_eq!(
        shell(&dev, "terraform apply -auto-approve", root),
        Some(RuleState::Deny)
    );
    let shell = |command| shell(&agent, command, root);
    assert_eq!(shell("gh pr merge 12"), Some(RuleState::Deny));
    assert_eq!(shell("kubectl delete pod web"), Some(RuleState::Deny));
    assert_eq!(shell("kubectl get pods"), Some(RuleState::Allow));
    // Across surfaces the strictest match wins: the shell surface allows
    // `kubectl get`, the write outside the workspace asks.
    assert_eq!(shell("kubectl get pods > /opt/pods"), Some(RuleState::Ask));
}

#[test]
fn guide_agent_path_rules_decide_as_described() {
    let config = guide_config();
    let agent = resolve(&config, Some("agent"), ProfileContext::Headless);
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path();
    assert_eq!(
        shell(&agent, "tee /opt/result /tmp/result", root),
        Some(RuleState::Ask)
    );
    assert_eq!(
        shell(&agent, "touch /tmp/result", root),
        Some(RuleState::Allow)
    );
    assert_eq!(shell(&agent, "touch result.txt", root), None);
    assert_eq!(
        write_file(&agent, &home().join(".cache/tool/state"), root),
        Some(RuleState::Allow)
    );
    assert_eq!(
        write_file(&agent, &home().join(".euler/profiles.json"), root),
        Some(RuleState::Deny)
    );
    assert_eq!(
        write_file(&agent, &home().join(".ssh/config"), root),
        Some(RuleState::Deny)
    );
    // A `~` operand is dynamic, so it is unresolved and asks.
    assert!(matches!(
        run_shell(&agent, "cat ~/.ssh/id_ed25519", root),
        Some(RuleMatch::Unresolved { .. })
    ));
}

/// The guide says only a person (never the guardian) can answer the ask for
/// a `dev` or `agent` shell command with an operand Euler cannot resolve,
/// because their `path` surface has a `deny` rule, so plain `exec` denies
/// it. It suggests literal paths and `git commit -F <file>` instead.
#[test]
fn guide_dev_and_agent_unresolved_shell_operands_are_human_only() {
    let config = guide_config();
    let dev = resolve(&config, Some("dev"), ProfileContext::Interactive);
    let agent = resolve(&config, Some("agent"), ProfileContext::Headless);
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path();
    for profile in [&dev, &agent] {
        for command in ["ls src/*.rs", "echo $HOME", "git commit -m \"$MSG\""] {
            assert!(
                matches!(
                    run_shell(profile, command, root),
                    Some(RuleMatch::Unresolved {
                        human_only: true,
                        ..
                    })
                ),
                "{}: {command}",
                profile.name
            );
        }
        assert_eq!(shell(profile, "git commit -F msg.txt", root), None);
    }
}

#[test]
fn guide_ci_allows_only_the_listed_cargo_commands() {
    let config = guide_config();
    let ci = resolve(&config, Some("ci"), ProfileContext::Headless);
    let workspace = tempfile::tempdir().expect("workspace");
    let shell = |command| shell(&ci, command, workspace.path());
    for allowed in [
        "cargo build --release",
        "cargo test --workspace",
        "cargo clippy",
    ] {
        assert_eq!(shell(allowed), Some(RuleState::Allow), "{allowed}");
    }
    for denied in [
        "ls",
        "curl https://example.com",
        "cargo build && curl https://example.com",
        "cargo test $FLAGS",
    ] {
        assert_eq!(shell(denied), Some(RuleState::Deny), "{denied}");
    }
    assert_eq!(tool(&ci, "code-swarm.review"), Some(RuleState::Deny));
    assert_eq!(tool(&ci, "read_file"), None);
}
