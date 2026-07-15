# Building extensions

This describes the extension surface that is in this tree today. Euler ships
native Rust extensions compiled into the `euler` binary, and can run explicitly
enabled, locally linked `managed-process` packages over a versioned JSON-RPC
stdio contract. Linked commands work through standalone `extension run`, the
line-oriented live-session `extension_run` control line, and the TUI
`/extension run` form. Python is the first client SDK for that contract; it is
not a Python-only runtime mode.

Linking inventories a local package without starting it. `validate`, `link`,
and `info` show a managed package's exact argv; `enable` echoes that argv as it
records the explicit local decision to launch it. Reloading or disabling
revokes that decision, so a changed manifest must be inspected and enabled
again. Installed packages remain inert in this first delivery slice.

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

## Managed-process packages

A managed-process package uses the same extension id, declared capability
envelope, commands, and `HostApi` semantics as a native extension. Its manifest
adds a literal argv entrypoint:

```json
{
  "version": 1,
  "id": "python-proof",
  "display_name": "Python proof extension",
  "extension_version": "0.1.1",
  "runtime_kind": "managed-process",
  "entrypoint": {
    "command": [".venv/bin/python", "-B", "-u", "extension.py"]
  },
  "capabilities": ["provenance-read", "artifact-write"],
  "commands": [{
    "name": "inspect",
    "display_name": "Inspect",
    "summary": "Read a bounded provenance page and write an artifact.",
    "required_capabilities": ["provenance-read", "artifact-write"]
  }]
}
```

`entrypoint.command` is executed directly with the package directory as the
current directory. It is not a shell command: there is no quoting language,
environment interpolation, or implicit shell. Use an interpreter from a
package-local virtual environment when the package needs Python dependencies.

The Python SDK is dependency-free at runtime and supports Python 3.9 or newer.
From an Euler checkout, create a virtual environment for a package and install
the SDK into it:

```sh
python3 -m venv .venv
.venv/bin/python -m pip install -e /path/to/euler/python/euler_managed_process_sdk
```

Then the package's `extension.py` can be as small as:

```python
from euler_managed_process_sdk import CommandContext, serve

def inspect(context: CommandContext) -> dict[str, object]:
    page = context.host.query_provenance(limit=8, scan_limit=32)
    return {"event_count": len(page["events"])}

serve({"inspect": inspect})
```

See `examples/python-managed-process-extension` for a complete **repo-local
development** package that writes an artifact. Its source injection is
deliberate so contributors can run it from this checkout; a standalone package
must use the virtual-environment install above rather than copy that injection.
The protocol is documented in
`docs/contracts/extension-sdk.md`; another language can implement it directly
without a core change or this Python SDK.

Managed-process execution is Unix-only (macOS and Linux). Euler clears the
child environment and supplies only the package directory as working directory,
the inherited `PATH`, and `EULER_MANAGED_PROCESS_PROTOCOL`; package code must
not depend on ambient `HOME`, locale, certificate, or Python-path variables.
The manifest should name a package-local interpreter directly, such as
`.venv/bin/python`, rather than relying on inherited virtual-environment state.

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
- `agent-spawn`
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
- `spawn_agent` and `spawn_agents` need `agent-spawn` and remain subject to
  the host's child-agent limits and permission decisions.
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

Local package commands support review, inventory, and managed-process launch:

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

Package manifests are `Euler.extension.json`, version `1`, with either
`runtime_kind: "native-rust"` or `runtime_kind: "managed-process"`. For a
linked managed-process package, the explicit execution flow is:

```sh
euler extension validate ./my-extension
euler extension link ./my-extension
euler extension enable my-extension
euler extension run my-extension.inspect ./session.jsonl --input-file ./input.json
```

The managed-process CLI accepts only an optional `--input-file` containing one
JSON object (at most 64 KiB) in this first slice. `reload` returns the linked
package to `needs-review`, revoking its prior execution grant. Linked native
packages and all installed packages remain inert.

A linked managed-process `euler extension run` is a headless surface: naming
the command explicitly grants that command's declared capabilities for that one
invocation. Euler announces the exact capability list on stderr before launch;
declarations are never silent grants. Child-process stdout/stderr are not
forwarded there.

Linked-package enablement is explicit user-scope launch consent, separate from
the bundled-extension registry and its project/session selection overlay. It
reviews the current manifest and exact argv—not a content hash of every source
file—so trusted local package code can iterate without re-enabling after every
edit. A manifest or argv change is the review boundary: it must be reloaded and
enabled again before launch.

## Bundled examples

- `session-export`: bounded provenance query plus JSON artifact write.
- `causal-dag`: graph projection, checkpoints, artifacts, context slots, and
  observer audit records.
- `code-swarm`: review-only companion-agent brief generation and report folding.
- `diagnostics-report`: diagnostics tail aggregation into an artifact.
- `autoresearch`: objective brief/report flow with a context slot.
- `maxproof`: population/verifier brief generation and deterministic tournament
  artifacting.

## Process trust boundary

The managed-process runtime gives the host control of lifecycle, structured
protocol traffic, capability-gated host APIs, provenance attribution, artifact
writes, redaction, and transcript/canvas admission. Raw child stdout and stderr
are never rendered as canvas content.

It is not an OS sandbox. A linked local package is trusted developer code and
can still use operating-system APIs directly. Containment for untrusted
third-party extensions is a separate security milestone. The initial runnable
surface is `euler extension run`; wiring process packages into every live
session surface is follow-on work that does not change the protocol or package
identity.
