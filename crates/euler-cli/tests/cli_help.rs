//! Help surface: `--help`/`-h`/`help` print usage to stdout and exit 0;
//! `--help` wins over validation errors; unknown arguments point at help.

use std::process::{Command, Output};

const SUBCOMMANDS: &[&str] = &[
    "run",
    "tui",
    "exec",
    "login",
    "logout",
    "auth",
    "models",
    "session-export",
    "extension",
];

fn euler(args: &[&str]) -> Output {
    let home = tempfile::tempdir().expect("isolated HOME");
    Command::new(env!("CARGO_BIN_EXE_euler"))
        .args(args)
        .current_dir(home.path())
        .env("HOME", home.path())
        .env_remove("EULER_PROVIDER")
        .env_remove("EULER_MODEL")
        .env_remove("EULER_HOME")
        .env_remove("EULER_AUTH_FILE")
        .output()
        .expect("run euler")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout utf8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr utf8")
}

#[test]
fn top_level_help_prints_usage_and_all_subcommands() {
    for invocation in [&["--help"][..], &["-h"], &["help"]] {
        let output = euler(invocation);
        assert!(output.status.success(), "{invocation:?} should exit 0");
        let text = stdout(&output);
        assert!(text.contains("Usage"), "{invocation:?} missing Usage");
        for subcommand in SUBCOMMANDS {
            assert!(
                text.contains(subcommand),
                "{invocation:?} missing {subcommand}"
            );
        }
        assert!(stderr(&output).is_empty(), "{invocation:?} wrote to stderr");
    }
}

#[test]
fn exec_help_lists_verified_flags_and_defaults() {
    let output = euler(&["exec", "--help"]);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.contains("--auto-compaction"));
    assert!(text.contains("--max-tool-rounds"));
    assert!(text.contains("default: stubs"));
    assert!(text.contains("default: 640000"));
    assert!(text.contains("default: unlimited"));
    assert!(text.contains("exec --resume"));
}

#[test]
fn help_subcommand_matches_subcommand_help_flag() {
    for subcommand in SUBCOMMANDS {
        let via_topic = euler(&["help", subcommand]);
        let via_flag = euler(&[subcommand, "--help"]);
        assert!(via_topic.status.success(), "help {subcommand} exit 0");
        assert!(via_flag.status.success(), "{subcommand} --help exit 0");
        assert_eq!(stdout(&via_topic), stdout(&via_flag), "{subcommand}");
        assert!(stdout(&via_topic).contains("Usage"), "{subcommand}");
    }
}

#[test]
fn help_flag_wins_anywhere_before_validation_errors() {
    let output = euler(&["exec", "--provider", "chatgpt", "--help"]);
    assert!(output.status.success());
    assert!(stdout(&output).contains("euler exec"));

    let output = euler(&["--provider", "bogus-provider", "--help"]);
    assert!(output.status.success(), "--help must beat validation");
    assert!(stdout(&output).contains("Usage"));
}

#[test]
fn bad_arguments_point_at_help() {
    let output = euler(&["--bogus"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("unknown argument: --bogus (try 'euler --help')"));

    let output = euler(&["help", "bogus"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("unknown help topic: bogus"));
}

#[test]
fn help_after_double_dash_is_exec_prompt_text_not_help() {
    let output = euler(&["exec", "--provider", "fixture", "--", "--help"]);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.contains("user: --help"), "prompt is literal: {text}");
    assert!(!text.contains("Usage"), "must not print help: {text}");
}

#[test]
fn flag_listed_in_exec_help_actually_parses() {
    let args = [
        "exec",
        "--provider",
        "fixture",
        "--max-tool-rounds",
        "5",
        "hi",
    ];
    let output = euler(&args);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("user: hi"));
}
