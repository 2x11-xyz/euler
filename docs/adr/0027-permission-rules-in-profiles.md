# ADR 0027: Permission rules in profiles

## Status

Proposed (2026-10-29). Adds permission rules, an ask timeout, and a deny-only
extension reviewer seam to the execution profiles of ADR 0026, and proposes
a deadline for guardian reviews (question 18). Builds on ADR
0026 as amended on 2026-10-29 (built-in headless default `full`, #285). It
keeps every ADR 0026 decision and adds two user-configured exceptions: an
`ask` rule prompts even under `approvals: never`, and a `deny` rule refuses an
escalation before it reaches the reviewer. The questions this draft leaves
open are listed at the end, each with a proposed answer; the Decision text
cites them as "question n". No contract changes until the rollout PRs land;
each PR updates its owning contract.

## Context

ADR 0026 lets a user pick a boundary and an approval policy per context. It
does not let a user say "never `git push --force`", "ask before anything
writes outside the workspace", or "no paid review tools in CI" while keeping
the rest of `full` or `workspace`. With `full` as the default for interactive
and headless runs, those statements are how a user or a team sets
requirements for long-horizon agents. Today the only tools are capability
modes (whole capabilities), scoped grants (allow-only, first token or
directory prefix), and switching to a tighter preset.

The design input is an existing rule-based permission layer for local agents:
allow, ask, and deny over shell command patterns, path rules with read and
write direction, outside-workspace directories, and per-tool rules; the most
restrictive layer wins, the last matching rule wins within one rule list;
per-agent overrides; project configuration that can only tighten; a prompt
timeout for unattended runs; child-agent asks forwarded to the parent; and an
opt-in reviewer chain consulted on an ask whose allow is capped by policy.

Euler keeps permission authority in core (ADR 0021, ADR 0026). Extensions are
consented code that runs out of process with the user's OS authority, and the
host takes no permission decision from them (ADR 0015, ADR 0018, ADR 0022,
the capabilities contract's "Trust Model Honesty"). So rules are native to
core, cover core and extension tools alike, fail closed, and land in
provenance; extensions get only a seam that can make a decision stricter.

### Current facts (main at e61588f)

1. **Profiles.** `<euler home>/profiles.json` has `defaults`
   (`interactive`, `headless`, and `subagent`, which takes `inherit` or a
   profile name) and `profiles`; a user profile names a preset in `extends`
   and overrides fields (`execution_profile/config.rs`). Unknown fields and
   duplicate keys are errors (`deny_unknown_fields`, `UniqueKeys`). The
   effective fields are `ProfileFields` (`execution_profile.rs`), recorded on
   `session.start` as `execution_profile` with `fields_sha256`, a SHA-256 over
   the fields' JSON with sorted keys. `from_record` rejects unknown keys and a
   digest mismatch.
2. **Headless default.** Main still resolves `workspace`
   (`BUILT_IN_HEADLESS`); #285 flips it to `full` and amends ADR 0026
   resolved question 11.
3. **Object order is not kept.** `write_interactive_default` rewrites
   `profiles.json` through `serde_json::Value`, and the workspace does not
   enable `preserve_order`, so every object's keys come back sorted.
4. **The gate.** `PermissionGate::effective_mode` (`permissions.rs`) decides
   a request's mode in this order: configured `always-deny`; an escalation
   asks when the profile allows escalation, else is denied; a dangerous
   request asks; `profile_mode_override` maps `approvals` (under `never`
   everything runs; under `escalation-only` with an enforced sandbox,
   `fs-write` and `shell-exec` asks become allows; a shell sensitive path
   asks); a remaining sensitive path asks. `PermissionGate::new` starts
   `fs-write`, `shell-exec`, and `agent-spawn` in `ask`.
5. **Floor.** `dangerous_command` is set by the danger walk
   (`contains_dangerous_command`) and for structured writes to
   interpreter-honored configuration or privileged Euler files. No grant
   covers it (`granted_source`), it never reaches the guardian, and a
   never-prompt decider denies it.
6. **Grants.** Session, project, and user grants sit above `ask`
   (`grants.rs`). Shell grants match segment-wise over
   `parse_plain_segments`; `fs-write` grants match a workspace-relative
   directory prefix. Grants never apply to a mode of `always-deny`.
7. **Guardian.** `PermissionReviewer` is `user` or `guardian`
   (`guardian.rs`). Guardian thresholds are enforced in code, its allows are
   once-scoped, every failure is a deny, `abstain` falls to the configured
   decider, and its decisions carry `decision_source: "guardian"`.
8. **Never-prompt deciders.** `PermissionDecider::never_prompts` marks
   headless `exec`, which denies every ask that reaches it.
9. **Agents.** `ExecutionProfile::child_profile` tightens a
   `defaults.subagent` profile field by field against the parent and keeps
   the parent's `reviewer`, because a child's asks go to the session's
   reviewer.
10. **Project overlay.** `.euler/profile-overlay.json`
    (`PROJECT_OVERLAY_PATH`) may only tighten, needs no trust
    acknowledgment, and may not set `reviewer` (`ProfileOverrides::tighten`).
11. **Extension tools.** A model tool is a descriptor on an `agent-only`
    command and runs through operation-scoped capability approval (ADR 0018);
    its model-visible name is unique across core and enabled extensions. The
    code-swarm extension's `review` command declares `code_swarm_review`
    (ADR 0022). Extensions run outside any sandbox (ADR 0026 section 8).

## Decision

A profile gains three optional fields: `rules`, `ask_timeout`, and
`reviewers`. A profile is now a boundary, an approval policy, and rules. A
profile without them behaves exactly as today.

### 1. Rule surfaces

`rules` is an object with up to four surfaces. Each surface is an ordered
array of rule objects (question 1):

```json
{ "pattern": "git push *", "state": "ask", "reason": "Pushes need review." }
```

| Surface | Matches | Pattern (question 2) |
| --- | --- | --- |
| `shell` | each segment of a `run_shell` command, and its escalated form | words with `*` |
| `path` | every path a request names: structured tools, `git_*`, host-mediated extension file access, and `run_shell` path operands (question 3) | glob; relative means workspace-relative |
| `outside_workspace` | paths that resolve outside the workspace root, on the same requests | absolute or `~/` directory prefix |
| `tools` | a tool call by core tool name (`run_shell`) or extension command (`code-swarm.review`) | name; `*` matches any run of characters (`git_*`, `code-swarm.*`) |

`path` and `outside_workspace` rules take an optional `access`: `read`,
`write`, or `any` (the default). `reason` is optional (at most 512 bytes);
it is shown to the model and the human with a denial or a prompt.

Extension tools are named by `<extension-id>.<command>`, the declaring
command, not the model-visible name, so a rule survives a renamed
descriptor and a typo in a core name is a validation error (section 8).

Rules decide approval inside the boundary; they never widen it. An `allow`
cannot make a sandboxed command reach the network, write outside
`writable_paths`, or run on the host. Escalation stays the only way out of a
sandbox.

Path rules on `run_shell` and on extension processes are a guard against
mistakes, not a boundary against a determined process, the same standing as
the dangerous-command list (ADR 0026 section 2). They see the paths a command
names in a recognized position (question 3), not every file a program
touches. Extensions run outside any sandbox, so `path` rules see only the
file access they request through the host. The sandbox profiles remain the
boundary.

### 2. States

- `deny`: refuse without asking anyone. The tool error names the rule and
  its `reason` and tells the model not to work around it.
- `ask`: take the ask path (section 5), even under `approvals: never`.
  ADR 0026's list of what asks under `never` is what Euler itself asks; an
  `ask` rule is the user's own addition to it.
- `allow`: satisfy an ask that comes from the profile's `approvals` or the
  capability's configured mode (question 5). It never satisfies a floor ask
  or a sensitive-path ask (question 4), and never overrides a configured
  `always-deny`, including the one the default headless auto-approve tier
  sets under `approvals: always` (question 17).

### 3. Composition and precedence

Within one surface the last matching rule wins, so a later carve-out
overrides an earlier broad rule; a narrow `deny` goes after the broad rules it
carves out of. Across surfaces the most restrictive match wins (`deny` >
`ask` > `allow`); a request no rule matches has no rule state.

A shell command splits into segments on `&&`, `||`, `;`, `|`, and newlines,
and each command substitution is a segment of its own; each segment is
matched in its literal and wrapper-unwrapped form. The command is `deny` if
any segment is, else `ask` if any segment is, else `allow` only if every
segment is, else it has no rule state. So `cargo build *` allowed does not
cover `cargo build && curl ...`, matching how shell grants require every
segment (fact 6). An `allow` never applies to a command or operand Euler
cannot resolve (question 3). Shell and path rules read the full command
text, as the danger walk does, never the bounded copy in
`PermissionRequest.command` that prompts and grants use.

A request can name several paths: a copy's source and destination, two
`tee` targets, a read and a write. Each path and access is a target, and
within a surface each target takes the last rule that matches it. The
surface's state for the request is then `deny` if any target is, else `ask`
if any target is, else `allow` only if every target is, else no rule state.
With the `agent` rules of section 10, `tee /opt/result /tmp/result` asks for
`/opt/result` although `/tmp` is allowed. A target Euler cannot resolve
counts as `ask` in this reduction when its surface has any `ask` or `deny`
rule, and as no rule state otherwise (question 3), so it never hides a known
`deny` on another target and a known `allow` never covers it.

Each layer is evaluated on its own, last match wins within its own lists,
and the layers' states combine by most restrictive: the user profile's
rules, the project overlay's, and in a child the parent's and the child
profile's (section 6). Every layer below the user profile contributes only
`ask` and `deny`, except in a child under `subagent: inherit`, whose rules
are the parent session's own (section 6). So no lower layer loosens one
above it. Layers
stay separate everywhere, including the recorded profile (section 9): they
are never concatenated into one list, which would turn
most-restrictive-across-layers into last-match-across-layers.

The gate evaluates a request in this order, extending `effective_mode`
(fact 4). It returns the cause with the mode (a matched rule, an unresolved
operand, a floor item, a sensitive path, or `approvals`), not a bare
`ApprovalMode`, and every later check reads the cause:

1. Configured `always-deny`: deny.
2. A `deny` rule: mode `always-deny`, recorded with the rule. This includes
   the escalated form of a denied command: the user has already answered,
   so the escalation never reaches the reviewer.
3. Escalation: ask when the profile allows escalation, else deny
   (unchanged).
4. The floor: a request marked dangerous takes the ask path (section 5) as
   a human-only ask and evaluation stops (unchanged, fact 5), so steps 5
   to 7 never see it.
5. An `ask` rule, or an unresolved target (above) that counts as `ask`:
   ask, with a rule cause. Static-safe
   analysis (issue #78), which runs an ordinary shell ask without a prompt
   today, never discharges a rule-caused ask, and neither does a grant
   (question 6).
6. Today's `profile_mode_override` and sensitive-path logic produce a mode.
7. If that mode is `ask` because of `approvals` or the configured mode, the
   request carries no sensitive path (neither the structured path flag nor
   a shell operand flag), and the rule state is `allow`: allow, recorded
   with the rule. The allow installs no grant; the rule decides again on the
   next request.

The floor is fixed and no rule, grant, or approval setting loosens it: the
dangerous-command list, structured writes to interpreter-honored
configuration and to privileged Euler files (privileged secret and config
edits), extension install and launch consent, the project-tier extension
config write started by a model tool (ADR 0022), and the escalation ask.
All but the escalation ask are human-only: they never reach the guardian.
The escalation ask may go to the guardian, and the existing
escalation-session grant (capabilities contract) may cover an eligible
escalation; no other grant and no `allow` rule discharges it. A request that
is both dangerous and escalated stays human-only and once-scoped, no grant
covers it, and its decision records both floor items. A `deny` rule that
refuses a request the floor also marks records the floor item beside the
rule.

**Grants.** Static-safe analysis and grants run, as today, only when the
resulting mode is `ask`, static-safe analysis first. They may settle an ask
from step 6, never a rule-caused ask from
step 5 (question 6) or a floor ask other than the escalation-session grant
above. A grant never overrides a `deny` rule: step 2 returns `always-deny`
and grants never apply to it (fact 6).

**Guardian.** An `ask` rule is an ordinary ask for routing. With
`reviewer: guardian` the guardian reviews it under its existing contract and
may allow it once.

**Auto-approve tiers.** `--auto-approve` tiers set capability modes as
today (ADR 0026). A tier's `always-deny` stops a request at step 1, before
any `allow` rule (question 17). A tier's `session-allow` does not satisfy an
`ask` rule, since step 5 does not read the mode.

**Routes.** Rules apply on every route a request takes: core tool calls,
including those that need no capability (a `tools` rule is checked when the
call is admitted); extension commands, including those with no
capabilities; extension batches; companion and child tool calls; and the
host-mediated file access and config writes of extensions. A `tools` deny
refuses an extension call before the extension is launched or launch
consent is asked. A batch takes the most restrictive state of its requests,
so a deny on one request denies the batch, and an ask is still one prompt
for the batch.

### 4. `ask_timeout`

`ask_timeout` is a whole number of seconds a human prompt may stay open
(question 7). When it passes, the ask is denied, the prompt closes, and the
decision records `decision_source: "timeout"`. A timeout is a deny, not a
cancellation, so the model gets a tool error and the run continues; like a
human's deny, it keeps today's denial of that capability for the rest of
the turn. It never hangs a run whose user walked away.

- Absent: an interactive prompt waits as today.
- `0`: no human is asked. An ask that the extension reviewers and the
  guardian (section 5) do not decide is denied at once and records
  `decision_source: "timeout"`.
- A never-prompt decider (headless `exec`, lifecycle hooks) has no human to
  wait for, so an undecided ask is denied at once whatever `ask_timeout`
  says. That deny is recorded as such a decider records it today, not as
  `timeout`, even when `ask_timeout` is `0`.
- The timeout bounds only the human wait. Reviewers and the guardian have
  their own deadlines (section 5).

An ask settles once: the first of an answer, the timeout, or a cancellation
wins. A late answer is discarded, installs no grant, and cannot answer a
later prompt. A cancellation stays a cancellation, not a timeout deny. Each
pending ask, including a child's and a batch's, has its own timer at its own
effective timeout (section 6).

With `full` as the headless default, `rules` plus `ask_timeout` is how a user
states requirements for unattended agents: deny what must never happen, ask
for what needs review, and let the guardian or a timeout settle each ask.

### 5. The ask path and the reviewer seam

An uncovered ask goes, in order:

1. To each extension reviewer named in the profile's `reviewers`, in order,
   whatever `reviewer` says and including human-only asks. Each returns
   `deny` with a reason or `defer`. It can never allow.
2. To the guardian when `reviewer: guardian`, unless the request is
   human-only (section 3). The guardian may allow, deny, or abstain per its
   contract, within a host deadline; expiry is a guardian failure and so a
   deny (question 18).
3. To the human through the session's decider, bounded by `ask_timeout`.
4. Otherwise deny.

The first `deny` ends the path. A reviewer that fails, times out, lacks
authority, or returns a malformed verdict defers (question 8). Deferring is
never an allow: some later step must still allow. Extension reviewers are
advisory vetoes: core policy is enforced whether or not they run, and when
one fails its extra veto is missing for that ask. Such deferrals are
recorded (section 9), so an operator can see degraded review.

An extension reviewer is an extension command with a reviewer descriptor,
named `<extension-id>.<command>` in `reviewers` (for example
`["policy-check.review"]`). It runs through the managed-process protocol
with standing authority only, as idle contributors do (ADR 0018): it never
opens a prompt, and missing authority defers. Its input is the request
(capability, tool, command or path, matched rule, reason, profile name),
redacted under the secrets contract's rules and then bounded like guardian
input. Floor requests reach reviewers too, since a deny there is always
safe; this shows a reviewer the redacted text of requests the guardian never
sees, which the user accepted by naming it.

The native guardian stays the built-in reviewer that may allow. Extension
reviewers are deny-only because the host takes no permission authority from
extension code. A reviewer is still consented executable code running with
the user's OS authority, not an isolated policy boundary (capabilities
contract, "Trust Model Honesty"): deny-only limits its verdict, not what its
process can do.

### 6. Agents

A child agent runs under `defaults.subagent`, tightened against the parent
(fact 9). With `subagent: inherit` its rules are the parent's. With a named
subagent profile its rules are the `ask` and `deny` rules of every parent
layer plus the child profile's `ask` and `deny` rules; no `allow` rule
applies, the parent's or the child's, so the child is no looser than the
profile the user chose for it (question 9). The child profile's `allow`
rules are still parsed and validated, since the same profile may serve
another context. A child keeps the parent's `reviewer` and `reviewers`, and
a named child profile's own `reviewers` are not used, because its asks go to
the parent session's ask path, so an interactive parent's human answers
them.
The timeout for a child's ask is the lower of the parent's and the child's
`ask_timeout`, an absent value counting as no limit, and the parent's wait
uses that value. The child's `execution_profile` on `agent.spawn` records
its rule layers as they apply (section 9) and that effective `ask_timeout`,
so its digest binds the rules and timeout that decide its requests.

### 7. Project overlay

`.euler/profile-overlay.json` may add `rules` with `ask` and `deny` states.
An `allow` in the overlay is a startup error naming the file, surface, and
pattern. The overlay may set `ask_timeout` to a value no higher than the
resolved one (any value when the resolved one is absent), and may not set
`reviewers` (question 12), as it may not set `reviewer` today (fact 10).
Overlay rules need no trust acknowledgment, since they only tighten, and the
startup summary names the overlay.

### 8. Validation

Rules are validated when `profiles.json` and the overlay load, with the same
policy as profile fields: errors, not warnings, naming the file, profile,
surface, and index.

- Unknown keys, surfaces, states, or `access` values are errors, and so is
  `access` on a `shell` or `tools` rule. `reason` is at most 512 bytes.
- An empty or unparsable pattern, a `path` pattern that is neither
  workspace-relative, absolute, nor `~/`, and an `outside_workspace`
  pattern that is not absolute or `~/` are errors.
- A `tools` pattern without a `.` must glob-match at least one core tool
  name (`docs/contracts/tools.md`); when it matches an extension's
  model-visible tool name instead, the error names the
  `<extension-id>.<command>` form. An extension pattern whose literal id
  names no installed extension gets a startup notice listing the installed
  ids, and the rule is kept (question 19). A rule for an installed but
  disabled extension applies once it is enabled.
- `reviewers` entries must be `<extension-id>.<command>`; a named reviewer
  that is not enabled or has no reviewer descriptor is a startup error, so a
  profile never runs without a reviewer it names (question 16). A team that
  shares such a profile shares the extension install with it.
- `ask_timeout` is an integer from 0 to 86400 (one day).
- At most 256 rules per surface per layer and 512 bytes per pattern
  (question 11).
- An empty `rules` object, empty surface, or empty `reviewers` list is
  normalized to unset at parse, so the digest, `changes_to`, and startup
  summaries all see the same value.

### 9. Provenance

Every `permission.decision` a rule, reviewer, or timeout decides says so:

- `decision_source`: `"rule"`, `"reviewer"`, or `"timeout"`, beside today's
  `"guardian"` and the omitted user value.
- `rule`: `{surface, index, pattern, state, layer}` where `layer` is
  `profile`, `overlay`, or `subagent` (a named child profile's own rules)
  and `index` is the rule's position in that layer's array for that
  surface. A child copies the parent's layers with their labels. Present on
  every decision where a rule set the state: a `deny`, an allow under step
  7, and an `ask` rule's ask however it was then settled. An ask caused by
  operands Euler could not resolve records `{surface, unresolved: true}`.
- `reviewer`: the `<extension-id>.<command>` that denied, and its bounded
  reason. A reviewer's deny of a floor request records both `reviewer` and
  `builtin`.
- `reviewers_degraded`: the reviewers that deferred because they failed,
  timed out, lacked authority, or returned a malformed verdict.
- `builtin`: on floor decisions, the floor items that applied (a list):
  `dangerous-command`, `interpreter-config`, `privileged-file`,
  `extension-config-write`, or `escalation`. Extension install and launch
  consent keep their own records.

`rules`, `ask_timeout`, and `reviewers` join `ProfileFields`, so they are part
of `execution_profile.fields` and `fields_sha256`. A user writes one `rules`
object per profile; the recorded `rules` is the ordered list of layers that
apply, each `{layer, rules}`, so the record and its digest bind the actual
policy and `from_record` rebuilds it without flattening. Each field is
omitted from the JSON when unset, so the digest of a profile without them is
unchanged and older logs still verify (fact 1). On resume, the recorded
layers stay and added `ask` and `deny` rules join as further layers, recorded
on `session.resumed` with its digest; any other change takes effect only
with `--profile` (question 10).

### 10. A shareable team configuration

A team can share one user-level `profiles.json`; each repository's overlay
can only tighten it. Example:

```json
{
  "defaults": { "interactive": "dev", "headless": "agent", "subagent": "inherit" },
  "profiles": {
    "dev": {
      "extends": "full",
      "rules": {
        "shell": [
          { "pattern": "git push *", "state": "ask" },
          { "pattern": "git push *--force*", "state": "deny",
            "reason": "Push a new branch instead of force-pushing." },
          { "pattern": "terraform apply *", "state": "deny" }
        ],
        "path": [
          { "pattern": "~/.euler/**", "access": "write", "state": "deny" }
        ]
      }
    },
    "agent": {
      "extends": "full",
      "reviewer": "guardian",
      "ask_timeout": 300,
      "rules": {
        "shell": [
          { "pattern": "git push *", "state": "ask" },
          { "pattern": "git push *--force*", "state": "deny" },
          { "pattern": "gh pr merge *", "state": "deny" },
          { "pattern": "kubectl *", "state": "deny" },
          { "pattern": "kubectl get *", "state": "allow" }
        ],
        "outside_workspace": [
          { "pattern": "/", "access": "write", "state": "ask" },
          { "pattern": "/tmp", "access": "write", "state": "allow" },
          { "pattern": "~/.cache", "access": "write", "state": "allow" }
        ],
        "path": [
          { "pattern": "~/.euler/**", "access": "write", "state": "deny" },
          { "pattern": "~/.ssh/**", "state": "deny" }
        ]
      }
    },
    "ci": {
      "extends": "workspace",
      "rules": {
        "shell": [
          { "pattern": "*", "state": "deny",
            "reason": "CI runs only the listed cargo commands." },
          { "pattern": "cargo build *", "state": "allow" },
          { "pattern": "cargo test *", "state": "allow" },
          { "pattern": "cargo clippy *", "state": "allow" }
        ],
        "tools": [
          { "pattern": "code-swarm.*", "state": "deny",
            "reason": "No paid model reviews in CI." }
        ]
      }
    }
  }
}
```

- `dev` is `full` at the keyboard with three hard stops and one review
  point.
- `agent` is the headless default. Under `full` nothing asks except the floor
  and its rules. An ask goes to the guardian (which may allow low-risk pushes
  once), then a human if one is attached (a TUI run left unattended, or a
  parent session), and is denied after 300 seconds otherwise. Plain
  `euler exec` has a never-prompt decider, which behaves as `ask_timeout: 0`
  (section 4), so an ask the guardian does not settle is denied at once.
  `kubectl` is denied except `kubectl get`, because the last match wins. A
  recognized write outside the workspace asks, except under `/tmp` and
  `~/.cache` (question 3). Writes under `~/.euler` are denied (the pattern
  assumes the default Euler home), so an agent cannot edit its own rules
  through a structured tool or a recognized shell write; a program that
  writes there on its own is not seen (section 1).
- In `dev` and `agent`, `git push` asks and the `--force` spelling of a force
  push is denied (so is `--force-with-lease`, which the pattern also
  matches). Patterns match words, not Git semantics: `git push -f`, a `+`
  refspec, and `git -C dir push --force` do not match the deny. The first two
  still ask; the last matches no rule. A team that wants those covered adds a
  pattern per spelling.
- `ci` is `workspace` (network on, so cargo can fetch crates) with a
  catch-all shell `deny` followed by `allow` carve-outs, so only the listed
  cargo commands run. Every other shell command is denied, including a
  compound command with one unlisted segment and a command Euler cannot
  resolve. The carve-outs only exempt the listed commands from the
  catch-all; `workspace` already runs commands inside its boundary.
  Structured file tools keep the `workspace` behavior. CI selects it with
  `--profile ci`. The same rules on `locked` would deny every shell command
  under headless `exec`: `approvals: always` applies the auto-approve tier,
  whose default sets shell commands to `always-deny` before any rule
  (question 17). `workspace` has `approvals: escalation-only`, so no tier
  applies.
- A team with a policy extension adds `"reviewers": ["policy-check.review"]`
  to `agent`; every machine that runs the profile then needs that extension
  installed (question 16).

### 11. Compatibility

A configuration without `rules`, `ask_timeout`, and `reviewers` behaves as
today, decision for decision, and records the same `execution_profile`
digest (question 14), except that the guardian deadline (question 18) turns
a guardian review that runs past it into a deny. Older logs fold unchanged.
An older Euler reading a `profiles.json` with rules fails at startup with an
unknown-field error naming the field, rather than running looser than
written. An older Euler resuming a session whose record carries these fields
fails in `from_record` on the unknown key; that is intended, for the same
reason.

## Alternatives rejected

- **An extension-owned permission system.** Extensions are out-of-process
  code that can fail; a permission layer there would not cover core tools
  consistently, could not fail closed against its own crash, and would split
  provenance. The deny-only seam gives extensions the useful part.
- **Rules as more grant kinds.** Grants are allow-only and are installed by
  answers to prompts; rules need deny and ask, carve-outs, and a shareable
  file.
- **Object maps keyed by pattern.** Readable, but JSON object order is not
  guaranteed and Euler's own writer sorts keys (fact 3), which would
  silently change last-match-wins results.
- **Allowing reviewers to allow.** An allow from extension code would be a
  second permission authority. The guardian, which runs with an empty
  capability set and thresholds enforced in core, stays the only automated
  allow.

## Rollout

Each step is one PR with tests that fail without it and updates its owning
contract (`capabilities.md`, `events.md`, `extension-sdk.md`, `ui.md`,
`docs/guides/headless.md`). The steps do not depend on #285; only the
examples assume its headless default.

1. **Parse and record.** `rules`, `ask_timeout`, and `reviewers` parsing and
   validation, empty-to-unset normalization, the layered `ProfileFields`
   record with the omit-when-unset digest rule, and the resume rule, with
   record, `from_record`, resume, and child round trips over conflicting
   profile, overlay, and child rules. A profile that sets them is refused as
   not enforced in this build, as unshipped presets were.
2. **`shell` and `tools` rules** in the gate (section 3): the cause carried
   with the mode and read by the static-safe and grant-coverage checks in
   `tool_dispatch.rs`, which key on the bare mode today; a
   `permission.decision` for step 7 allows, which today emit none; `rule`
   provenance; the deny tool error; and a route test matrix (root, child,
   batch, companion, extension, zero-capability tool).
   Tests include an `ask` rule on a statically safe command under `full`,
   an `allow` rule with a sensitive operand under `escalation-only`, a
   dangerous escalated command, and the section 10 force-push spellings.
3. **`path` and `outside_workspace` rules** for structured tools and
   `git_*`, then `run_shell` operands (question 3), with per-target tests
   (two destinations, source and destination, read and write, a known deny
   or a known allow beside an unresolved operand) and the ordinary writers
   of question 3.
4. **Layers.** Overlay rules and child composition (sections 6 and 7).
5. **`ask_timeout` and the guardian deadline** in the TUI decider and for
   child asks answered by the parent, with tests of a timeout racing an
   answer, a cancellation, and another child's ask, and of a guardian that
   streams output without ending.
6. **Extension reviewers.** The SDK reviewer descriptor, the host protocol,
   `reviewers`, `decision_source: "reviewer"`, `reviewers_degraded`, and a
   reviewer whose host call raises an ask (question 8).
7. **Guide.** A shareable team `profiles.json` in the headless guide.

## Consequences

- A user or team can set hard stops and review points per profile without
  leaving `full`, and unattended runs settle every ask in bounded time.
- Every rule decision is attributable in provenance, and rules join the
  profile digest.
- Rules are a guard on shell and extension file access, not a boundary; the
  sandbox profiles stay the boundary.
- New prompts: an `ask` rule prompts under every profile, including `full`,
  and its prompt offers no grant. New blocks: `deny` rules, timed-out asks,
  guardian deadline expiry, and reviewer denials; a malformed rule or a
  missing named reviewer is a startup error.
- `profiles.json` stays a privileged Euler file: a structured write asks a
  human under every profile.

## Open questions (proposed answers)

1. **Rule list shape.** Proposed: each surface is an ordered array of rule
   objects, not an object keyed by pattern, because object order is lost on
   rewrite (fact 3) and the object form gives room for `access` and
   `reason`.
2. **Pattern syntax.** Proposed: `shell` patterns are words matched against
   each segment after the danger walk's wrapper unwrapping, anchored at both
   ends; `*` matches any run of characters including spaces, and a trailing
   ` *` also matches nothing (`git push *` matches `git push`). `path`
   patterns are globs (`*` within a component, `**` zero or more
   components, so `~/.euler/**` matches `~/.euler` itself); `~/` expands to
   `HOME`. Anchoring means `git push * main` misses
   `git push origin main --force`; end a pattern with `*` to cover trailing
   flags. `outside_workspace` patterns are directory prefixes matched
   component by component (`/tmp` does not match `/tmpx`). Paths are
   matched in literal and canonical form (symlinks resolved up to the
   longest existing ancestor, as sensitive-path checks do), with patterns
   canonicalized the same way at load (so `/tmp` also covers `/private/tmp`
   on macOS); the state is the most restrictive of the two matches.
3. **Shell path operands and direction.** Proposed: path operands come from
   the danger walk's parse. Writes are redirection targets and the write
   positions the danger walk already recognizes (`cp`, `install`, `rsync`,
   and `ln` destinations, both the source and the destination of `mv`,
   `tee`, `touch`, `mkdir`, `rm`, `mktemp`, and the file edited in place by
   `sed -i`, `perl -i`, `ruby -i`, `awk -i inplace`, `patch`, and `ed`),
   plus `dd of=`, `curl -o`, and `wget -O`. Reads are the other operands of
   those commands and the operands of a short list of readers (`cat`, `ls`,
   `head`, `tail`, `grep`, `rg`, `wc`, `stat`, `diff`). Operands of any other
   command have unknown direction and match `read`, `write`, and `any` rules
   alike, rather than counting as reads. So `cp a b dir/` reads `a` and `b`
   and writes `dir/`, and `ln -s target link` writes `link`. This is not a
   shell effects interpreter: a program that writes a path it was not given
   in a recognized position is not seen. A command whose operands cannot be
   resolved statically (substitution, dynamic command names, parse errors)
   counts as `ask` when the surface has any `ask` or `deny` rule, and when
   that surface has a `deny` rule the ask is human-only, since the guardian
   cannot tell whether it is the denied case. With only `allow` rules it has
   no rule state and keeps the profile's mode. An `allow` never applies to
   it.
4. **May an `allow` rule silence a sensitive-path ask?** Proposed: no.
   Grants still cover sensitive-path asks, and `approvals: never` drops
   them; a rule does not, so a broad `allow` cannot hide `.env` access under
   `workspace`.
5. **May an `allow` rule lift the profile's `approvals` ask?** Proposed:
   yes, inside the boundary and once-scoped, so an interactive `locked`
   profile can pre-approve its build commands. `approvals` is the default
   for requests no rule decides, not part of the floor. Under headless
   `exec` with `approvals: always` the auto-approve tier decides first
   (question 17).
6. **Grants and rules.** Proposed: a grant never overrides a `deny` rule and
   never satisfies a rule-caused ask, and the prompt for a rule-caused ask
   offers allow once and deny, not a session or durable grant. Today's shell
   grants are scoped to the first token or unscoped, and an unscoped session
   grant sets the capability to `session-allow` (`install_grant`), so one
   "allow for session" on any `git` prompt would otherwise silence a
   `git push *` review point for the rest of the session, and a durable
   grant made before the rule existed would skip it from the start. The
   alternative is to let a grant satisfy a rule ask only when its scope is
   at least as narrow as the rule's pattern, which needs a
   pattern-containment test.
7. **`ask_timeout` default and outcome.** Proposed: absent means wait
   interactively as today and `0` under a never-prompt decider (headless
   `exec`), so headless denies at once when no reviewer decides. No built-in
   profile sets it. A timeout deny, and an undecided ask under
   `ask_timeout: 0`, records `decision_source: "timeout"` and behaves as a
   human deny, including today's denial of the capability for the rest of
   the turn, so one timed-out `git push` review stops shell work for that
   turn; narrowing that to the denied request is a separate change. Under a
   never-prompt decider the deny is recorded as that decider records it
   today.
8. **Reviewer protocol.** Proposed: a managed-process command invocation
   with a reviewer descriptor, a host budget of 10 seconds per reviewer and
   30 seconds per ask, input redacted under the secrets contract and bounded
   like guardian input, verdict `{verdict: deny|defer, reason}`; failure,
   timeout, missing authority, or a malformed verdict is `defer` and is
   recorded in `reviewers_degraded`. A reviewer runs in a no-review context:
   an ask raised by its host calls or by agents it spawns is denied without
   review, and the per-ask budget covers that work and cancels it on
   expiry. A reviewer that must run is a separate decision, not implied
   here.
9. **Allow rules in children.** Proposed: with `subagent: inherit` the
   child has the parent's rules, `allow` included. With a named subagent
   profile no `allow` rule applies, the parent's or the child's, so a child
   never runs looser than its parent (ADR 0026 resolved question 9) or than
   the profile chosen for it, without comparing rule sets. A named child
   profile's `reviewers` are not used, as its `reviewer` is not today
   (fact 9).
10. **Resume with changed rules.** Proposed: the recorded rule layers stay.
    The resolved profile's and the current overlay's `ask` and `deny` rules
    join as further layers, so a rule added since the session started
    applies, and a layer identical to a recorded one is not added again. A
    lower resolved `ask_timeout` and added `reviewers` apply too, and the
    recorded `reviewers` stay. Every other change (a removed or reordered
    rule, an added `allow`, a higher or removed `ask_timeout`, a removed
    reviewer) takes effect only with `--profile`, since joined layers can
    only tighten; the startup summary names it. The resulting layers are
    recorded on `session.resumed`, so the next resume verifies the digest
    that applied. This extends `ResumeProfile::choose`, which today
    applies the resolved profile when `first_looser_field` finds no looser
    field and has no tighter-or-looser test for rule sets.
11. **Evaluation cost.** Proposed: patterns compile at load; evaluation is
    a linear last-match scan per surface; at most 256 rules per surface per
    layer and 512 bytes per pattern. The cost is small next to a model turn,
    and the rollout adds a test pinning the bound.
12. **May the overlay name reviewers?** Proposed: no. Adding a deny-only
    reviewer only tightens, but it would let repository content choose
    extension code to run on every ask.
13. **A human-only `ask`?** Proposed: not in this ADR. A profile that wants
    every ask answered by a person sets `reviewer: user`.
14. **Migration.** Proposed: none needed. No rules means today's behavior
    and today's digest (section 11), apart from the guardian deadline
    (question 18).
15. **Field name.** `reviewers` sits next to the existing `reviewer`.
    Proposed: keep `reviewers`, since both name who reviews an ask and the
    contract documents them together; `extension_reviewers` is the
    alternative if review finds the pair confusing.
16. **A named reviewer that is missing.** Proposed: a startup error, not a
    warning. A missing deny-only reviewer cannot loosen a single decision,
    but the profile would run with less review than written, which ADR 0026
    resolved question 21 rejects. The error names the reviewer and the
    fixes: install or enable the extension, pass `--profile`, or change
    `defaults.headless`.
17. **Rules and the headless auto-approve tier.** Under `approvals: always`,
    headless `exec` applies the `--auto-approve` tier: the default
    `read-only` sets `fs-write` and `shell-exec` to `always-deny`, and
    `trusted-local` sets them to `session-allow`. Proposed: rules do not
    change the tier, and an `allow` rule never overrides a configured
    `always-deny`. A headless allowlist is a catch-all `deny` with `allow`
    carve-outs on a profile that already runs commands (section 10 `ci`),
    which needs no ADR 0026 change. The alternative, letting a profile with
    rules replace the implicit tier while keeping explicitly configured
    denies, would amend ADR 0026.
18. **Guardian deadline.** The guardian has one turn and no tools but no
    wall-clock limit, and provider liveness sets no total limit while output
    keeps arriving, so a guardian could hold an ask open before the human
    timer starts. Proposed: the host gives each guardian review a 60-second
    deadline covering retries; expiry is a guardian failure and so a deny
    (fact 7), never a defer, and it cancels the in-flight review. The
    decision records `decision_source: "guardian"` with
    `guardian_expired: true`, so an operator can tell it from a guardian's
    own deny. This applies to every guardian review, with or without rules,
    so it is the one behavior change for a configuration without the new
    fields (section 11); the alternative is to apply it only to profiles
    that set `rules` or `ask_timeout`.
19. **A `tools` rule naming an extension that is not installed.** A typo
    such as `code_swarm.*` for `code-swarm` matches nothing. Proposed: a
    startup notice that names the rule and lists the installed extension
    ids, not an error, because a shared profile may deny an extension that
    some machines (such as CI) never install. The alternative is a startup
    error, which would make every such machine install the extension it
    denies.
