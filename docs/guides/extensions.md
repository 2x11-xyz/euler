# Building extensions

This describes the extension surface that is in this tree today. Euler ships
native Rust extensions compiled into the `euler` binary. Local extension
packages can be validated, linked, installed, searched, and audited, but linked
or installed packages are not runnable yet.

## SDK surface

Use `euler-sdk`. The native interface is small:

- `Extension::manifest(&self) -> ExtensionManifest`
- `Extension::register(&self, registrar: &mut dyn CommandRegistrar)`
- `CommandRegistrar::register_command(name, Box<dyn ExtensionCommand>)`
- `ExtensionCommand::descriptor(&self) -> CommandDescriptor`
- `ExtensionCommand::execute(CommandContext, &dyn HostApi) -> serde_json::Value`

`ExtensionManifest` declares:

```rust
pub struct ExtensionManifest {
    pub id: String,
    pub version: String,
    pub display_name: String,
    pub capabilities: Vec<Capability>,
}
```

`CommandDescriptor` declares the command name, display name, summary, required
capabilities, CLI args, and whether the host should inject `session_id` into the
command input.

Minimal native extension crate:

```rust
use euler_sdk::{
    CommandContext, CommandDescriptor, CommandRegistrar, Extension, ExtensionCommand,
    ExtensionError, ExtensionManifest, HostApi, Invocation,
};
use serde_json::{json, Value};

pub struct HelloExtension;

impl Extension for HelloExtension {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: "hello".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            display_name: "Hello".to_owned(),
            capabilities: Vec::new(),
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command("hello", Box::new(HelloCommand));
        Ok(())
    }
}

struct HelloCommand;

impl ExtensionCommand for HelloCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            invocation: Invocation::User,
            name: "hello".to_owned(),
            display_name: "Say hello".to_owned(),
            summary: "Return a small JSON object.".to_owned(),
            required_capabilities: Vec::new(),
            args: Vec::new(),
            accepts_session_id: false,
        }
    }

    fn execute(&self, _context: CommandContext, _host: &dyn HostApi) -> Result<Value, ExtensionError> {
        Ok(json!({"message": "hello"}))
    }
}
```

For a smallest real extension, read `crates/euler-extension-session-export`.
It queries provenance and writes a JSON artifact.

## Command args

Commands declare supported CLI flags with `ArgSpec`:

- `flag`: CLI spelling without `--`.
- `input_key`: JSON key inserted into `CommandContext.input`; one nested level
  is supported with `outer.inner`.
- `value_kind`:
  - `PositiveInt { max }`
  - `BoundedString { max_bytes }`
  - `StringList` for repeatable string flags
  - `JsonObjectFile { max_bytes, reject_wrapper_key }`
- `required`
- `repeatable`

Unknown flags are rejected before the command runs.

## Capabilities and host APIs

Declare every capability in the manifest envelope, then repeat the subset each
command needs in its descriptor. A command cannot require a capability outside
the extension manifest.

Capabilities are:

- `fs-read`
- `fs-write`
- `provenance-read`
- `diagnostics-read`
- `artifact-write`
- `agent-record`
- `shell-exec`
- `network`
- `config-write`
- `secret-resolve`
- `context-slot`

Host calls gate on those capabilities:

- `query_provenance` needs `provenance-read`.
- `read_diagnostics` needs `diagnostics-read`.
- `state_dir`, checkpoint load/store use `fs-write` / `fs-read` as declared by
  the method.
- `write_artifact` needs `artifact-write` and persists an
  `extension.artifact` provenance event.
- `record_agent_task_result` needs `agent-record`.
- `update_context_slot` needs `context-slot`; content is capped at 4096 bytes,
  and a session has at most 8 context slots.

Example: `causal-dag.record-observation` declares only
`provenance-read` and `agent-record`, because it queries the log and records the
observer audit as agent spawn/result events; it does not write a graph artifact.

## Discovery, enablement, and running

Bundled native extensions are compiled into the CLI and exposed by descriptor:

```sh
euler extension list
euler extension enable causal-dag
euler extension disable causal-dag
euler extension run causal-dag.export ./session.jsonl --limit 128
```

`extension run` accepts `EXTENSION.COMMAND`, then a session id, session name, or
events path, then descriptor-backed flags. The extension must be enabled first.

`euler exec --extensions` controls which bundled extensions are enabled for a
live headless session:

```sh
euler exec --extensions causal-dag,maxproof "work on this task"
euler exec --extensions none "work without extensions"
```

If `--extensions` is omitted, Euler folds the user registry and the project
overlay at `.euler/extensions.json`:

```json
{"enable": ["causal-dag"], "disable": ["session-export"]}
```

Local package commands exist for review and inventory:

```sh
euler extension validate ./my-extension
euler extension link ./my-extension --scope user
euler extension install ./my-extension --scope user
euler extension reload my-extension --scope user
euler extension unlink my-extension --scope user
euler extension uninstall my-extension --scope user
euler extension audit
euler extension search dag --capability provenance-read --runtime native-rust
```

Package manifests are `Euler.extension.json`, version `1`, with
`runtime_kind: "native-rust"`. Today, linked packages report `needs-review`, and
installed packages report `installed-inert`; neither path executes user code.

## Bundled examples

- `session-export`: bounded provenance query plus JSON artifact write.
- `causal-dag`: graph projection, checkpoints, artifacts, context slots, and
  observer audit records.
- `code-swarm`: review-only companion-agent brief generation and report folding.
- `diagnostics-report`: diagnostics tail aggregation into an artifact.
- `autoresearch`: objective brief/report flow with a context slot.
- `maxproof`: population/verifier brief generation and deterministic tournament
  artifacting.

## Out-of-process status

Native crates today; out-of-process stdio transport is the second path. In this
release tree, `euler-sdk` and `euler-core` do not contain a wired stdio
subprocess extension transport.
