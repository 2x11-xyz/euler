# Secrets Contract

Euler uses simple local-agent secret resolution.

Euler is a coding agent for a user's laptop, dev box, or VM. It should not pretend to be a hardened secrets platform. Real isolation comes from the OS, SSH/Tailscale access, containers, or the user's password manager.

## Secret Value Syntax

Provider API keys and custom header values may be specified as:

- **Environment variable:** `$OPENROUTER_API_KEY` or `${KEY_PREFIX}_API_KEY`
- **Shell command:** `!op read 'op://vault/item/credential'`
- **Literal value:** `sk-...`
- **Escapes:** `$$` for a literal `$`, `$!` for a literal leading `!`

Example:

```toml
[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
api_key = "$OPENROUTER_API_KEY"

[providers.custom.headers]
x-secret = "!op read 'op://vault/item/secret'"
```

## Resolution Rules

- Resolve secrets at request time.
- Missing environment variables make the value unresolved.
- Shell commands are executed only when the secret is needed.
- Euler does not add built-in TTL, stale-value reuse, or secret recovery logic for arbitrary commands.
- If a command needs caching or retry behavior, the user should wrap it in their own script.
- Model availability checks may use configured auth presence but must not execute shell secret commands.


## Subscription Auth Tokens

Some providers, especially the first ChatGPT subscription provider, obtain OAuth-style tokens through Euler rather than user-supplied API-key references.

These tokens are stored in a dedicated local auth file, such as `~/.euler/auth.json`, with restrictive file permissions. They are secret-tainted and follow all redaction rules in this contract. Provider refresh is handled inside the provider layer.

This is not a custom secret vault; it is a pragmatic local token file for a local coding agent.

## Config-Edit Guardrail

Config files containing secret references are privileged paths. Agent-initiated edits that add, remove, or modify `$ENV` secret references, literal credentials, subscription auth files, or `!command` secret values require explicit user approval regardless of session permission mode.

Euler must not execute a secret shell command that was written or modified by an agent in the current session unless the user explicitly approved that config edit.

A new or changed `!command` secret value may also prompt on first execution.

## Storage Rules

- Euler does not need a built-in encrypted secrets database for v0.
- Do not store resolved secret values in provenance, logs, transcripts, or model canvas.
- Store only redacted presence/status, such as `configured`, `missing`, or `command_failed`.
- Config files may contain secret references or literal local secrets at the user's discretion.

## Redaction Rules

Any value resolved through this contract is secret-tainted.

Secret-tainted values must be redacted from:

- logs,
- provenance payloads,
- tool output shown to models,
- error messages,
- debug dumps,
- review artifacts.

Tool output redaction is implemented in two layers at the tool-result
emission chokepoint (before the canvas and the ledger both):

1. **Known values** — secret environment variables read at session start,
   stored auth credentials, and any value the host registers at runtime are
   replaced by exact match.
2. **Known token shapes** — well-known credential prefixes (`sk-or-v1-`,
   `sk-ant-`, `ghp_`, `AKIA…`, …) are masked even when the value was never
   resolved through euler — e.g. a granted shell command reading a foreign
   secrets file. This layer is a heuristic, not a guarantee: novel token
   formats pass through, and over-matching costs only a masked token, which
   is the safe direction.

## Non-Goals

Euler is not a multi-user secrets manager.

Do not build:

- a custom secret vault,
- a keychain abstraction in v0,
- secret syncing,
- secret rotation,
- opaque secret handles that complicate normal local use.
