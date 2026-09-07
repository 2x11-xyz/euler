use euler_core::extensions::ExtensionHost;
use euler_sdk::{
    Capability, CommandContext, CommandDescriptor, CommandRegistrar, Extension, ExtensionCommand,
    ExtensionError, ExtensionManifest, HostApi, Invocation,
};
use serde_json::{json, Value};
use std::panic::{catch_unwind, AssertUnwindSafe};
struct Ext {
    descriptor_panic: bool,
}
struct Cmd {
    invalid: bool,
    descriptor_panic: bool,
}
impl Extension for Ext {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: "audit-ext".into(),
            version: "1.0".into(),
            display_name: "Audit".into(),
            capabilities: vec![],
        }
    }
    fn register(&self, r: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        r.register_command(
            "valid",
            Box::new(Cmd {
                invalid: false,
                descriptor_panic: self.descriptor_panic,
            }),
        );
        r.register_command(
            "invalid",
            Box::new(Cmd {
                invalid: true,
                descriptor_panic: false,
            }),
        );
        Ok(())
    }
}
impl ExtensionCommand for Cmd {
    fn descriptor(&self) -> CommandDescriptor {
        if self.descriptor_panic {
            panic!("SYNTHETIC_DESCRIPTOR_PANIC");
        }
        CommandDescriptor {
            name: "".into(),
            display_name: "".into(),
            summary: "".into(),
            required_capabilities: if self.invalid {
                vec![Capability::FsWrite]
            } else {
                vec![]
            },
            accepts_session_id: false,
            args: vec![],
            invocation: Invocation::User,
            model_tool: None,
        }
    }
    fn execute(&self, _: CommandContext, _: &dyn HostApi) -> Result<Value, ExtensionError> {
        Ok(json!({"executed":true}))
    }
}
fn main() {
    let fixture = tempfile::tempdir().unwrap();
    let mut host = ExtensionHost::new(fixture.path().join("nonexistent.jsonl"), []);
    println!(
        "registration={:?}",
        host.register_extension(&Ext {
            descriptor_panic: false
        })
    );
    println!(
        "command_after_failed_registration={:?}",
        host.execute_command("valid", json!({}))
    );
    println!(
        "retry={:?}",
        host.register_extension(&Ext {
            descriptor_panic: false
        })
    );
    let mut panic_host = ExtensionHost::new(fixture.path().join("nonexistent.jsonl"), []);
    let result = catch_unwind(AssertUnwindSafe(|| {
        panic_host.register_extension(&Ext {
            descriptor_panic: true,
        })
    }));
    println!("descriptor_panic_escaped={}", result.is_err());
}
