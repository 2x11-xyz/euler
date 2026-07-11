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

Redaction happens at the boundary where secrets **enter** the record from
outside or are **injected** by the host — never over the model's own
cognition. Faithful capture of what the model thought and did is the point
of euler provenance; redacting it would corrupt the record (owner decision,
2026-07-11).

**Redacted (secret entering / injected):**

- logs and debug dumps,
- **tool RESULTS** shown to models (external data a tool read in — the #56
  incident: a granted `cat` of a foreign secrets file),
- provider error messages (external HTTP bodies),
- context-slot content (extension-supplied data that enters model context),
- resolved provider/auth secrets (registered with the redactor at
  resolution time so their values are caught wherever they surface).

**NOT redacted (model / user cognition — kept faithful):**

- model reasoning, model content, assistant messages,
- reviewer findings and the guardian's rationale (a reviewer model's own
  reasoning),
- tool-call arguments (the faithful record of the action the model chose —
  including a credential the model placed in a command),
- user messages.

When a secret nonetheless lands in a faithful-cognition payload (the model
echoes it, or a credential sits in a tool-call argument), euler does not
silently rewrite the record. It **detects and warns** the user, and offers
an explicit **scrub** operation that removes the value from every surface
(provenance, blobs, checkpoints, sidecars, projections) on demand. Default
is faithful; scrub is opt-in.

Redaction is implemented in two layers:

1. **Known values** — secret environment variables read at session start,
   stored auth credentials, and any value the host registers at runtime are
   replaced by exact match.
2. **Known token shapes** — well-known credential prefixes (`sk-or-v1-`,
   `sk-ant-`, `ghp_`, `AKIA…`, …) are masked even when the value was never
   resolved through euler — e.g. a granted shell command reading a foreign
   secrets file. This layer is a heuristic, not a guarantee: novel token
   formats pass through, and over-matching costs only a masked token, which
   is the safe direction.

### Known-value seeding sources

Every path a credential can enter euler must register it with the session
redactor the moment it exists:

- configured secret environment variables, at session construction;
- stored auth-file credentials (including a `--auth-file` override), at
  launch, exec, CLI resume, AND in-app resume;
- custom-provider secrets (`$ENV` / `!command` / literal api_key and header
  values), reported by the provider at request-time resolution through the
  resolved-secret sink the session installs — before the request that
  carries them departs;
- values the host registers explicitly at runtime.

Redactor handles share one value set: a value registered on any thread
(e.g. during a parallel-reviewer provider call) is visible to every
emission site immediately.

### Emission chokepoints

Redaction applies where text that arrived from OUTSIDE the model is
persisted to the ledger (and from there replays into model context):

- `tool.result` output and error — root session, companion loop, and the
  code-swarm review tool;
- `patch.proposed` / `patch.applied` old/new content and `file.diff` diff
  fields;
- provider `error` messages (HTTP error bodies can echo request
  fragments) — root session, companion loop, and the parallel-reviewer
  buffered append;
- `context.slot.updated` content (extension-injected text that replays
  into every later round);
- the guardian's permission-decision rationale (prompted to hunt secret
  exfiltration, it may quote the offending token).

Model-authored text (`model.result` content, `model.reasoning`,
`assistant.message`, agent results) and model-authored tool-call arguments
are NOT redaction chokepoints: provenance keeps model cognition faithful.
Secrets are caught where they enter (tool results, provider errors,
extension content) rather than by rewriting what the model said.

## Non-Goals

Euler is not a multi-user secrets manager.

Do not build:

- a custom secret vault,
- a keychain abstraction in v0,
- secret syncing,
- secret rotation,
- opaque secret handles that complicate normal local use.
