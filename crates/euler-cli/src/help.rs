//! Help surface for the hand-rolled CLI parser. Content contract: every
//! flag, value, and default below is verified against the code that consumes
//! it (`RawArgsParser` in `main.rs`, `extension_cli.rs`, `session_export.rs`).

use anyhow::{anyhow, Result};

/// Options shared by `run`, `tui`, and `exec` (consumed by `RawArgsParser`
/// and applied through `build_run_args`).
const SESSION_OPTIONS: &str = "  \
--provider <id>            chatgpt|openai|anthropic|openrouter|xai|fixture|<custom>
  --model <name>             Model id (default: the provider's default model)
  --provider-option <k=v>    Provider-specific option (repeatable)
  --extensions <ids|none>    Comma-separated extension ids to enable
  --observe <extension-id>   Run that enabled extension's round observer
  --observe-cadence <n>      Observer cadence in driver rounds
                             (default: extension-defined; bundled default: 8)
  --permission-reviewer <r>  user or guardian: who reviews permission asks
                             (default: user)
  --auth-file <path>         Read credentials from <path>
  --provenance <path>        Write a standalone provenance log to <path>
                             instead of the home session store
";

const HELP_LINE: &str = "  -h, --help                 Show this help\n";

/// Detects a help invocation. Returns `Ok(Some(text))` when `args` request
/// help (print to stdout, exit 0), `Ok(None)` for normal parsing, and `Err`
/// for `euler help <unknown-topic>`. `--help`/`-h` anywhere before `--` wins
/// over all parse and validation errors; the topic is the first token that
/// names a subcommand.
pub(crate) fn help_output(args: &[String]) -> Result<Option<String>> {
    if args.first().map(String::as_str) == Some("help") {
        return match args.get(1).map(String::as_str) {
            None | Some("-h" | "--help") => Ok(Some(top_help())),
            Some(topic) => subcommand_help(topic)
                .map(Some)
                .ok_or_else(|| anyhow!("unknown help topic: {topic} (try 'euler --help')")),
        };
    }
    let mut topic = None;
    let mut requested = false;
    for arg in args {
        match arg.as_str() {
            "--" => break,
            "-h" | "--help" => requested = true,
            name => {
                if topic.is_none() {
                    topic = subcommand_help(name);
                }
            }
        }
    }
    Ok(requested.then(|| topic.unwrap_or_else(top_help)))
}

fn subcommand_help(name: &str) -> Option<String> {
    match name {
        "run" => Some(run_help()),
        "tui" => Some(tui_help()),
        "exec" => Some(exec_help()),
        "login" => Some(credential_help("Sign in and store", "login")),
        "logout" => Some(credential_help("Remove stored", "logout")),
        "auth" => Some(AUTH_HELP.to_owned()),
        "models" => Some(MODELS_HELP.to_owned()),
        "session-export" => Some(SESSION_EXPORT_HELP.to_owned()),
        "extension" => Some(EXTENSION_HELP.to_owned()),
        _ => None,
    }
}

fn top_help() -> String {
    format!(
        "Euler — research agent, coding included

Usage: euler [SUBCOMMAND] [OPTIONS]
       euler help <SUBCOMMAND>

With no subcommand, euler starts an interactive session: the full-screen
TUI when stdin and stdout are terminals, line-oriented otherwise.

Subcommands:
  run             Line-oriented interactive session
  tui             Full-screen terminal UI session
  exec            Run one non-interactive turn and print the transcript
  login           Store provider credentials (--provider chatgpt)
  logout          Remove stored provider credentials (--provider chatgpt)
  auth status     Show stored credential status
  models          List the model catalog; `models refresh` updates it
  session-export  Export session events as JSON
  extension       Manage and run extensions
  help            Show help for a subcommand

Options:
{SESSION_OPTIONS}  \
--replay <path>            Render an existing provenance log and exit
  --resume <path>            Resume a session from a provenance log
  --no-tty                   Never launch the TUI; stay line-oriented
{HELP_LINE}"
    )
}

fn run_help() -> String {
    format!(
        "Start a line-oriented interactive session.

Usage: euler run [OPTIONS]

Each stdin line is sent as one turn; type 'exit' to quit.

Options:
{SESSION_OPTIONS}{HELP_LINE}"
    )
}

fn tui_help() -> String {
    format!(
        "Start the full-screen terminal UI session.

Usage: euler tui [OPTIONS]

Options:
{SESSION_OPTIONS}  \
--experimental-tui-linefeed-history
                             Commit transcript lines into terminal scrollback
  --no-tui-linefeed-history  Disable linefeed history insertion
{HELP_LINE}"
    )
}

fn exec_help() -> String {
    format!(
        "Run one non-interactive turn and print the transcript.

Usage: euler exec [OPTIONS] [PROMPT]...
       euler exec [OPTIONS] -- <PROMPT>...
       euler exec --resume <path-or-id> [OPTIONS] [PROMPT]...

Non-flag arguments are joined into the prompt; after `--` every argument
is a prompt word, even if it starts with `-`. With no prompt arguments
the prompt is read from piped stdin.

Options:
{SESSION_OPTIONS}  \
--auto-approve <tier>      read-only or trusted-local (default: read-only)
  --reasoning-effort <e>     xsmall, small, medium, large, xlarge, or max
  --max-output-tokens <n>    Cap output tokens per model response
  --max-tool-rounds <n>      Cap tool rounds per turn (default: unlimited)
  --auto-compaction <tier>   off or stubs (default: automatic stubs)
  --compaction-budget-bytes <n>
                             Canvas byte budget for compaction (default: 640000)
{HELP_LINE}"
    )
}

fn credential_help(action: &str, verb: &str) -> String {
    format!(
        "{action} provider credentials.

Usage: euler {verb} --provider chatgpt

Options:
  --provider chatgpt         Only the chatgpt provider supports {verb}
{HELP_LINE}"
    )
}

const AUTH_HELP: &str = "\
Show stored credential status.

Usage: euler auth status

Options:
  -h, --help                 Show this help
";

const MODELS_HELP: &str = "\
List the model catalog, or refresh it.

Usage: euler models
       euler models refresh [--force]

Options:
  --force                    Overwrite a models.json that was not generated
                             by `euler models refresh` (refresh only)
  -h, --help                 Show this help
";

const SESSION_EXPORT_HELP: &str = "\
Export session events as JSON.

Usage: euler session-export <SESSION> [OPTIONS]

<SESSION> is a session id, name, or events path.

Options:
  --limit <n>                Maximum events to export
  --scan-limit <n>           Maximum events to scan
  --after-event-id <id>      Export only events after this event id
  --kind <kind>              Filter by event kind (repeatable)
  -h, --help                 Show this help
";

const EXTENSION_HELP: &str = "\
Manage and run extensions.

Usage: euler extension <SUBCOMMAND>

Subcommands:
  list                       List extensions and their enablement
  status <id>                Show enablement for one extension
  info <id>                  Show extension metadata and commands
  search [QUERY]             Search by query, --capability <c>, --runtime <k>
  audit                      Audit installed extensions
  validate <dir>             Validate an extension package directory
  link <dir>                 Link a local extension directory
  install <dir>              Install an extension from a local directory
  reload <id>                Reload a linked or installed extension
  unlink <id>                Unlink a linked extension
  uninstall <id>             Uninstall an installed extension
  enable <id>                Enable an extension
  disable <id>               Disable an extension
  run <id>.<command> <SESSION> [--<arg> <value>]...
                             Run an extension command against a session
  -h, --help                 Show this help

link, install, reload, unlink, and uninstall accept `--scope user`.
";
