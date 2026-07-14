#![allow(clippy::too_many_lines)]

use euler_core::extensions::{ExtensionHost, ExtensionHostError};
use euler_sdk::{
    CommandContext, CommandDescriptor, CommandRegistrar, Extension, ExtensionCommand,
    ExtensionError, ExtensionManifest, HostApi,
};
use serde_json::{json, Value};
use std::process::{Command, Output};
use std::sync::{Arc, Barrier};
use std::thread;

const CHILD_ENV: &str = "EULER_EXTENSION_PANIC_HOOK_CHILD";
const MANIFEST_SECRET: &str = "manifest panic secret";
const REGISTRATION_SECRET: &str = "registration panic secret";
const COMMAND_SECRET: &str = "command panic secret";
const ANY_SECRET: &str = "panic-any secret";
const UNGUARDED_MARKER: &str = "unguarded panic marker";
const CONCURRENT_MARKER: &str = "unguarded concurrent panic marker";
const AFTER_MARKER: &str = "unguarded after guarded panic marker";
const CUSTOM_HOOK_MARKER: &str = "custom panic hook marker";

#[test]
fn extension_guarded_panics_do_not_emit_hook_payloads() {
    for child in [
        "extension_manifest_panic_child",
        "extension_registration_panic_child",
        "extension_command_panic_child",
        "extension_panic_any_child",
    ] {
        let output = run_child(child);
        assert!(
            output.status.success(),
            "{child} failed:\n{}",
            combined_output(&output)
        );
        assert_no_extension_secrets(child, &output);
    }
}

#[test]
fn extension_panic_hook_preserves_unguarded_panic_output() {
    let output = run_child("unguarded_panic_child");
    assert!(!output.status.success(), "unguarded child should fail");
    let combined = combined_output(&output);
    assert!(
        combined.contains(UNGUARDED_MARKER),
        "unguarded panic output was suppressed:\n{combined}"
    );
}

#[test]
fn extension_panic_hook_preserves_preexisting_custom_hook() {
    let output = run_child("preexisting_custom_hook_child");
    assert!(!output.status.success(), "custom-hook child should fail");
    let combined = combined_output(&output);
    assert_no_extension_secrets("preexisting_custom_hook_child", &output);
    assert!(
        combined.contains(CUSTOM_HOOK_MARKER),
        "preexisting custom panic hook was not preserved:\n{combined}"
    );
}

#[test]
fn extension_guarded_panic_suppresses_backtrace_payload() {
    let output = run_child_with_env("extension_command_panic_child", [("RUST_BACKTRACE", "1")]);
    assert!(
        output.status.success(),
        "backtrace child failed:\n{}",
        combined_output(&output)
    );
    assert_no_extension_secrets("extension_command_panic_child with backtrace", &output);
}

#[test]
fn extension_panic_hook_restores_after_guarded_panic() {
    let output = run_child("unguarded_after_guarded_panic_child");
    assert!(!output.status.success(), "unguarded child should fail");
    let combined = combined_output(&output);
    assert_no_extension_secrets("unguarded_after_guarded_panic_child", &output);
    assert!(
        combined.contains(AFTER_MARKER),
        "unguarded panic after guarded call was suppressed:\n{combined}"
    );
}

#[test]
fn extension_panic_hook_is_thread_local_under_concurrent_panic() {
    let output = run_child("concurrent_guarded_and_unguarded_panic_child");
    assert!(
        output.status.success(),
        "concurrent child failed:\n{}",
        combined_output(&output)
    );
    let combined = combined_output(&output);
    assert_no_extension_secrets("concurrent_guarded_and_unguarded_panic_child", &output);
    assert!(
        combined.contains(CONCURRENT_MARKER),
        "unguarded concurrent panic output was suppressed:\n{combined}"
    );
}

#[test]
fn extension_manifest_panic_child() {
    if !is_child("extension_manifest_panic_child") {
        return;
    }
    let temp = tempfile::tempdir().expect("temp dir");
    let mut host = ExtensionHost::new(temp.path().join("events.jsonl"), []);
    assert_eq!(
        host.register_extension(&ManifestPanicExtension)
            .expect_err("manifest panic"),
        ExtensionHostError::RegistrationPanic(None)
    );
}

#[test]
fn extension_registration_panic_child() {
    if !is_child("extension_registration_panic_child") {
        return;
    }
    let temp = tempfile::tempdir().expect("temp dir");
    let mut host = ExtensionHost::new(temp.path().join("events.jsonl"), []);
    assert_eq!(
        host.register_extension(&RegistrationPanicExtension)
            .expect_err("registration panic"),
        ExtensionHostError::RegistrationPanic(Some("registration-panic".to_owned()))
    );
}

#[test]
fn extension_command_panic_child() {
    if !is_child("extension_command_panic_child") {
        return;
    }
    let mut host = command_panic_host();
    assert_eq!(
        host.execute_command("panic", json!(null))
            .expect_err("command panic"),
        ExtensionHostError::CommandPanic("command-panic".to_owned(), "panic".to_owned())
    );
}

#[test]
fn extension_panic_any_child() {
    if !is_child("extension_panic_any_child") {
        return;
    }
    let temp = tempfile::tempdir().expect("temp dir");
    let mut host = ExtensionHost::new(temp.path().join("events.jsonl"), []);
    host.register_extension(&CommandExtension {
        id: "panic-any",
        command: PanicCommandKind::Any,
    })
    .expect("register");
    assert_eq!(
        host.execute_command("panic", json!(null))
            .expect_err("command panic"),
        ExtensionHostError::CommandPanic("panic-any".to_owned(), "panic".to_owned())
    );
}

#[test]
fn unguarded_panic_child() {
    if !is_child("unguarded_panic_child") {
        return;
    }
    panic!("{UNGUARDED_MARKER}");
}

#[test]
fn unguarded_after_guarded_panic_child() {
    if !is_child("unguarded_after_guarded_panic_child") {
        return;
    }
    let mut host = command_panic_host();
    let _ = host.execute_command("panic", json!(null));
    panic!("{AFTER_MARKER}");
}

#[test]
fn preexisting_custom_hook_child() {
    if !is_child("preexisting_custom_hook_child") {
        return;
    }
    std::panic::set_hook(Box::new(|_| {
        eprintln!("{CUSTOM_HOOK_MARKER}");
    }));
    let mut host = command_panic_host();
    let _ = host.execute_command("panic", json!(null));
    panic!("{UNGUARDED_MARKER}");
}

#[test]
fn concurrent_guarded_and_unguarded_panic_child() {
    if !is_child("concurrent_guarded_and_unguarded_panic_child") {
        return;
    }
    let barrier = Arc::new(Barrier::new(2));
    let guarded_barrier = Arc::clone(&barrier);
    let guarded = thread::spawn(move || {
        let mut host = command_panic_host();
        guarded_barrier.wait();
        let _ = host.execute_command("panic", json!(null));
    });
    let unguarded = thread::spawn(move || {
        barrier.wait();
        panic!("{CONCURRENT_MARKER}");
    });
    guarded.join().expect("guarded child thread");
    assert!(unguarded.join().is_err());
}

fn run_child(test_name: &str) -> Output {
    run_child_with_env(test_name, [])
}

fn run_child_with_env<const N: usize>(test_name: &str, vars: [(&str, &str); N]) -> Output {
    let mut command = Command::new(std::env::current_exe().expect("current test binary"));
    for (key, value) in vars {
        command.env(key, value);
    }
    command
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .env(CHILD_ENV, test_name);
    command.output().expect("run child test")
}

fn is_child(test_name: &str) -> bool {
    std::env::var(CHILD_ENV).as_deref() == Ok(test_name)
}

fn assert_no_extension_secrets(context: &str, output: &Output) {
    let combined = combined_output(output);
    for secret in [
        MANIFEST_SECRET,
        REGISTRATION_SECRET,
        COMMAND_SECRET,
        ANY_SECRET,
    ] {
        assert!(
            !combined.contains(secret),
            "{context} leaked `{secret}`:\n{combined}"
        );
    }
}

fn combined_output(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn command_panic_host() -> ExtensionHost {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut host = ExtensionHost::new(temp.path().join("events.jsonl"), []);
    host.register_extension(&CommandExtension {
        id: "command-panic",
        command: PanicCommandKind::String,
    })
    .expect("register");
    host
}

struct ManifestPanicExtension;

impl Extension for ManifestPanicExtension {
    fn manifest(&self) -> ExtensionManifest {
        panic!("{MANIFEST_SECRET}");
    }

    fn register(&self, _registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        Ok(())
    }
}

struct RegistrationPanicExtension;

impl Extension for RegistrationPanicExtension {
    fn manifest(&self) -> ExtensionManifest {
        manifest("registration-panic")
    }

    fn register(&self, _registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        panic!("{REGISTRATION_SECRET}");
    }
}

struct CommandExtension {
    id: &'static str,
    command: PanicCommandKind,
}

#[derive(Clone, Copy)]
enum PanicCommandKind {
    String,
    Any,
}

impl Extension for CommandExtension {
    fn manifest(&self) -> ExtensionManifest {
        manifest(self.id)
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        let command: Box<dyn ExtensionCommand> = match self.command {
            PanicCommandKind::String => Box::new(StringPanicCommand),
            PanicCommandKind::Any => Box::new(PanicAnyCommand),
        };
        registrar.register_command("panic", command);
        Ok(())
    }
}

struct StringPanicCommand;

impl ExtensionCommand for StringPanicCommand {
    fn descriptor(&self) -> CommandDescriptor {
        empty_command_descriptor()
    }

    fn execute(
        &self,
        _context: CommandContext,
        _host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        panic!("{COMMAND_SECRET}");
    }
}

struct PanicAnyCommand;

impl ExtensionCommand for PanicAnyCommand {
    fn descriptor(&self) -> CommandDescriptor {
        empty_command_descriptor()
    }

    fn execute(
        &self,
        _context: CommandContext,
        _host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        std::panic::panic_any(SecretPayload);
    }
}

fn empty_command_descriptor() -> CommandDescriptor {
    CommandDescriptor {
        invocation: euler_sdk::Invocation::User,
        name: String::new(),
        display_name: String::new(),
        summary: String::new(),
        required_capabilities: Vec::new(),
        args: Vec::new(),
        accepts_session_id: false,
    }
}

struct SecretPayload;

impl std::fmt::Debug for SecretPayload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(ANY_SECRET)
    }
}

fn manifest(id: &str) -> ExtensionManifest {
    ExtensionManifest {
        id: id.to_owned(),
        version: "0.1.0".to_owned(),
        display_name: id.to_owned(),
        capabilities: vec![],
    }
}
