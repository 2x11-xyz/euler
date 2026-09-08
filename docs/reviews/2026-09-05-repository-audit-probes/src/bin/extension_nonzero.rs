use euler_core::extensions::ExtensionHost;
use euler_managed_process::{ManagedProcessExtension, ManagedProcessLimits};
use euler_sdk::{
    Invocation, ManagedProcessEntrypoint, StaticCommandDescriptor, StaticExtensionDescriptor,
};
use std::time::{Duration, Instant};
fn main() {
    let fixture = tempfile::tempdir().unwrap();
    let dir = fixture.path();
    std::fs::write(
        dir.join("child_exit.py"),
        include_str!("../../assets/child_exit.py"),
    )
    .unwrap();
    let _ = std::fs::remove_file(dir.join("descendant-after-return"));
    let ext = ManagedProcessExtension::new(
        dir,
        &StaticExtensionDescriptor {
            id: "audit-nonzero".into(),
            display_name: "Audit".into(),
            version: "0.0.0".into(),
            runtime_kind: "managed-process".into(),
            request_tick: None,
            capabilities: vec![],
            commands: vec![StaticCommandDescriptor {
                name: "go".into(),
                display_name: "Go".into(),
                summary: "Go".into(),
                required_capabilities: vec![],
                invocation: Invocation::User,
                model_tool: None,
            }],
            observer: None,
            idle_contribution: None,
        },
        ManagedProcessEntrypoint {
            command: vec!["python3".into(), "child_exit.py".into()],
        },
    )
    .unwrap()
    .with_limits(ManagedProcessLimits {
        cancel_grace: Duration::from_millis(20),
        ..Default::default()
    });
    let mut host = ExtensionHost::new(dir.join("nonexistent.jsonl"), []);
    host.register_extension(&ext).unwrap();
    let t = Instant::now();
    println!(
        "result={:?}",
        host.execute_command("go", serde_json::json!({}))
    );
    println!("elapsed_ms={}", t.elapsed().as_millis());
    std::thread::sleep(Duration::from_millis(1200));
    println!(
        "descendant_ran_after_failed_command={}",
        dir.join("descendant-after-return").exists()
    );
}
