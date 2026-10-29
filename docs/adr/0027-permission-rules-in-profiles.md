# ADR 0027: Permission rules in profiles

## Status

Proposed (2026-10-29). Adds permission rules, an ask timeout, and a deny-only
extension reviewer seam to the execution profiles of ADR 0026. Builds on ADR
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
untrusted out-of-process code (ADR 0015, ADR 0018, ADR 0022). So rules are
native to core, cover core and extension tools alike, fail closed, and land in
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
the dangerous-command list (ADR 0026 section 2). Extensions run outside any
sandbox, so `path` rules see only the file access they request through the
host. The sandbox profiles remain the boundary.

### 2. States

- `deny`: refuse without asking anyone. The tool error names the rule and
  its `reason` and tells the model not to work around it.
- `ask`: take the ask path (section 5), even under `approvals: never`.
  ADR 0026's list of what asks under `never` is what Euler itself asks; an
  `ask` rule is the user's own addition to it.
- `allow`: satisfy an ask that comes from the profile's `approvals` or the
  capability's configured mode (question 5). It never satisfies a floor ask
  or a sensitive-path ask (question 4).

### 3. Composition and precedence

Within one surface the last matching rule wins, so a later carve-out
overrides an earlier broad rule. Across surfaces the most restrictive match
wins (`deny` > `ask` > `allow`); a request no rule matches has no rule
state.

A shell command splits into segments on `&&`, `||`, `;`, `|`, and newlines,
and each command substitution is a segment of its own; each segment is
matched in its literal and wrapper-unwrapped form. The command is `deny` if
any segment is, else `ask` if any segment is, else `allow` only if every
segment is, else it has no rule state. So `cargo build*` allowed does not
cover `cargo build && curl ...`, matching how shell grants require every
segment (fact 6). An `allow` never applies to a command or operand Euler
cannot resolve (question 3).

Each layer is evaluated on its own, last match wins within its own lists,
and the layers' states combine by most restrictive: the user profile's
rules, the project overlay's, and in a child the parent's and the child
profile's (section 6). Every layer below the user profile contributes only
`ask` and `deny`, except the parent's rules in a child, which are the
parent session's own rules. So no lower layer loosens one above it.

The gate evaluates a request in this order, extending `effective_mode`
(fact 4):

1. Configured `always-deny`: deny.
2. A `deny` rule: mode `always-deny`, recorded with the rule. This includes
   the escalated form of a denied command: the user has already answered,
   so the escalation never reaches the reviewer.
3. Escalation: ask when the profile allows escalation, else deny
   (unchanged).
4. The floor: a request marked dangerous asks a human and evaluation stops
   (unchanged, fact 5), so steps 5 to 7 never see it.
5. An `ask` rule: ask.
6. Today's `profile_mode_override` and sensitive-path logic produce a mode.
7. If that mode is `ask` for a reason other than a sensitive path and the
   rule state is `allow`: allow, recorded with the rule. The allow installs
   no grant; the rule decides again on the next request.

The floor is fixed and no rule, grant, or approval setting loosens it: the
dangerous-command list, structured writes to interpreter-honored
configuration and to privileged Euler files (privileged secret and config
edits), extension install and launch consent, the project-tier extension
config write started by a model tool (ADR 0022), and the escalation ask.
All but the escalation ask are human-only: they never reach the guardian.

**Grants.** Grants are consulted, as today, only when the resulting mode is
`ask`, so they may satisfy an ask from step 5 or 6 (question 6) and never a
floor ask. A grant never overrides a `deny` rule: step 2 returns
`always-deny` and grants never apply to it (fact 6).

**Guardian.** An `ask` rule is an ordinary ask. With `reviewer: guardian`
the guardian reviews it under its existing contract and may allow it once.

### 4. `ask_timeout`

`ask_timeout` is a whole number of seconds a human prompt may stay open
(question 7). When it passes, the ask is denied, the prompt closes, and the
decision records `decision_source: "timeout"`. A timeout is a deny, not a
cancellation, so the model gets a tool error and the run continues. It never
hangs a run whose user walked away.

- Absent: an interactive prompt waits as today.
- `0`: no human is asked. An ask that the reviewers (section 5) do not
  decide is denied at once.
- A never-prompt decider (headless `exec`, lifecycle hooks) has no human to
  wait for, so an undecided ask is denied at once whatever `ask_timeout`
  says.
- The timeout bounds only the human wait, and each pending ask has its own
  timer. Reviewers have their own budgets.

With `full` as the headless default, `rules` plus `ask_timeout` is how a user
states requirements for unattended agents: deny what must never happen, ask
for what needs review, and let the guardian or a timeout settle each ask.

### 5. The ask path and the reviewer seam

An uncovered ask goes, in order:

1. To each extension reviewer named in the profile's `reviewers`, in order,
   whatever `reviewer` says. Each returns `deny` with a reason or `defer`.
   It can never allow.
2. To the guardian when `reviewer: guardian`, unless the request is
   human-only (section 3). The guardian may allow, deny, or abstain per its
   contract.
3. To the human through the session's decider, bounded by `ask_timeout`.
4. Otherwise deny.

The first `deny` ends the path. A reviewer that fails, times out, or returns
a malformed verdict defers (question 8). Deferring is never an allow: some
later step must still allow, so a broken reviewer cannot loosen a decision.

An extension reviewer is an extension command with a reviewer descriptor,
named `<extension-id>.<command>` in `reviewers` (for example
`["policy-check.review"]`). It runs through the managed-process protocol
with standing authority only, as idle contributors do (ADR 0018): it never
opens a prompt, and missing authority defers. Its input is the redacted
request (capability, tool, command or path, matched rule, reason, profile
name), bounded like guardian input. Floor requests reach reviewers too,
since a deny there is always safe; this shows a reviewer the redacted text
of requests the guardian never sees, which the user accepted by naming it.

The native guardian stays the built-in reviewer that may allow. Extension
reviewers are deny-only because they are untrusted code (ADR 0022).

### 6. Agents

A child agent runs under `defaults.subagent`, tightened against the parent
(fact 9). Its rules are the parent's rules plus the child profile's `ask` and
`deny` rules; the child profile's `allow` rules do not apply in a child
(question 9). Those `allow` rules are still parsed and validated, since the
same profile may serve another context. The parent's own `allow` rules do
apply in the child: they are the user's statement for the parent session, and
a child under them is still no looser than its parent. A child keeps the
parent's `reviewer` and `reviewers`, and its asks go to the parent session's
ask path, so an interactive parent's human answers them. The timeout for a
child's ask is the lower of the parent's and the child's `ask_timeout`, an
absent value counting as no limit. The child's `execution_profile` on
`agent.spawn` records the composed rules (the parent's and its own `ask` and
`deny` rules), so its digest binds the rules that decide its requests.

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
- A `tools` pattern without a `.` must match at least one core tool name. An
  extension pattern is checked for form only; when that extension is not
  enabled the rule matches nothing, because the tool does not exist.
- `reviewers` entries must be `<extension-id>.<command>`; a named reviewer
  that is not enabled or has no reviewer descriptor is a startup error, so a
  profile never runs without a reviewer it names (question 16). A team that
  shares such a profile shares the extension install with it.
- `ask_timeout` is an integer from 0 to 86400 (one day).
- At most 256 rules per surface per layer and 512 bytes per pattern
  (question 11).

### 9. Provenance

Every `permission.decision` a rule, reviewer, or timeout decides says so:

- `decision_source`: `"rule"`, `"reviewer"`, or `"timeout"`, beside today's
  `"guardian"` and the omitted user value.
- `rule`: `{surface, index, pattern, state, layer}` where `layer` is
  `profile`, `overlay`, or `parent` and `index` is the rule's position in
  that layer's array for that surface. Present on every decision a rule
  decided or capped, including an allow under step 7. An ask caused by
  operands Euler could not resolve records `{surface, unresolved: true}`.
- `reviewer`: the `<extension-id>.<command>` that denied, and its bounded
  reason.
- `builtin`: on floor decisions, which floor item applied:
  `dangerous-command`, `interpreter-config`, `privileged-file`,
  `extension-config-write`, or `escalation`. Extension install and launch
  consent keep their own records.

`rules`, `ask_timeout`, and `reviewers` join `ProfileFields`, so they are part
of `execution_profile.fields` and `fields_sha256`. Each is omitted from the
JSON when unset, and an empty `rules` object, empty surface, or empty
`reviewers` list counts as unset, so the digest of a profile without them is
unchanged and older logs still verify (fact 1). On resume, any difference in
these fields keeps the recorded ones unless `--profile` is passed (question
10).

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
          { "pattern": "terraform apply*", "state": "deny" }
        ],
        "path": [
          { "pattern": "~/.euler/**", "access": "write", "state": "deny" }
        ]
      }
    },
    "agent": {
      "extends": "full",
      "reviewer": "guardian",
      "reviewers": ["policy-check.review"],
      "ask_timeout": 300,
      "rules": {
        "shell": [
          { "pattern": "git push *", "state": "ask" },
          { "pattern": "git push *--force*", "state": "deny" },
          { "pattern": "gh pr merge*", "state": "deny" },
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
      "extends": "locked",
      "ask_timeout": 0,
      "rules": {
        "shell": [
          { "pattern": "cargo build*", "state": "allow" },
          { "pattern": "cargo test*", "state": "allow" },
          { "pattern": "cargo clippy*", "state": "allow" },
          { "pattern": "cargo publish*", "state": "deny" }
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
  and its rules. An ask goes to the team's policy extension (which may deny),
  then the guardian (which may allow low-risk pushes once), then a human if
  one is attached (a TUI run left unattended, or a parent session), and is
  denied after 300 seconds otherwise. Plain `euler exec` has a never-prompt
  decider, which behaves as `ask_timeout: 0` (section 4), so an ask the
  reviewers do not settle is denied at once. `kubectl` is denied except
  `kubectl get`, and `git push` asks except a force push, which is denied,
  because the last match wins. Writing outside the workspace asks, except
  under `/tmp` and `~/.cache`. Writes under `~/.euler` are denied, so an agent
  cannot edit its own rules through a structured tool or a recognized shell
  write.
- `ci` is `locked` (no network, every command asks) with the build and test
  commands allowed, so CI runs them unprompted and denies everything else.
  CI selects it with `--profile ci`.

### 11. Compatibility

A configuration without `rules`, `ask_timeout`, and `reviewers` behaves as
today, decision for decision, and records the same `execution_profile`
digest (question 14). Older logs fold unchanged. An older Euler reading a
`profiles.json` with rules fails at startup with an unknown-field error
naming the field, rather than running looser than written.

## Alternatives rejected

- **An extension-owned permission system.** Extensions are untrusted,
  out-of-process, and can fail; a permission layer there would not cover
  core tools consistently, could not fail closed against its own crash, and
  would split provenance. The deny-only seam gives extensions the useful
  part.
- **Rules as more grant kinds.** Grants are allow-only and are installed by
  answers to prompts; rules need deny and ask, carve-outs, and a shareable
  file.
- **Object maps keyed by pattern.** Readable, but JSON object order is not
  guaranteed and Euler's own writer sorts keys (fact 3), which would
  silently change last-match-wins results.
- **Allowing reviewers to allow.** An allow from untrusted code would be a
  second permission authority. The guardian, which runs with an empty
  capability set and thresholds enforced in core, stays the only automated
  allow.

## Rollout

Each step is one PR with tests that fail without it and updates its owning
contract (`capabilities.md`, `events.md`, `extension-sdk.md`, `ui.md`,
`docs/guides/headless.md`). The steps do not depend on #285; only the
examples assume its headless default.

1. **Parse and record.** `rules`, `ask_timeout`, and `reviewers` parsing and
   validation, `ProfileFields` with the omit-when-unset digest rule, and the
   resume rule. A profile that sets them is refused as not enforced in this
   build, as unshipped presets were.
2. **`shell` and `tools` rules** in the gate (section 3) with `rule`
   provenance and the deny tool error.
3. **`path` and `outside_workspace` rules** for structured tools and
   `git_*`, then `run_shell` operands (question 3).
4. **Layers.** Overlay rules and child composition (sections 6 and 7).
5. **`ask_timeout`** in the TUI decider and for child asks answered by the
   parent.
6. **Extension reviewers.** The SDK reviewer descriptor, the host protocol,
   `reviewers`, and `decision_source: "reviewer"`.
7. **Guide.** A shareable team `profiles.json` in the headless guide.

## Consequences

- A user or team can set hard stops and review points per profile without
  leaving `full`, and unattended runs settle every ask without hanging.
- Every rule decision is attributable in provenance, and rules join the
  profile digest.
- Rules are a guard on shell and extension file access, not a boundary; the
  sandbox profiles stay the boundary.
- New prompts: an `ask` rule prompts under every profile, including `full`.
  New blocks: `deny` rules, timed-out asks, and reviewer denials; a malformed
  rule or a missing named reviewer is a startup error.
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
   patterns are globs (`*` within a component, `**` across components);
   `~/` expands to `HOME`. Anchoring means `git push * main` misses
   `git push origin main --force`; end a pattern with `*` to cover trailing
   flags. `outside_workspace` patterns are directory
   prefixes. Paths are matched in literal and canonical form (symlinks
   resolved up to the longest existing ancestor, as sensitive-path checks
   do), with patterns canonicalized the same way at load (so `/tmp` also
   covers `/private/tmp` on macOS); the state is the most restrictive of the
   two matches.
3. **Shell path operands and direction.** Proposed: path operands come from
   the danger walk's parse; redirection targets and the operands of a short
   fixed list of writers (`cp` and `mv` destinations, `tee`, `touch`,
   `mkdir`, `rm`, `ln`) are writes, everything else is a read. A command
   whose operands cannot be resolved statically (substitution, dynamic
   command names, parse errors) counts as `ask` when the surface has any
   `ask` or `deny` rule; with only `allow` rules it has no rule state and
   keeps the profile's mode. An `allow` never applies to it.
4. **May an `allow` rule silence a sensitive-path ask?** Proposed: no.
   Grants still cover sensitive-path asks, and `approvals: never` drops
   them; a rule does not, so a broad `allow` cannot hide `.env` access under
   `workspace`.
5. **May an `allow` rule lift the profile's `approvals` ask?** Proposed:
   yes, inside the boundary and once-scoped, so a `locked` CI profile can
   pre-approve its build commands. `approvals` is the default for requests
   no rule decides, not part of the floor.
6. **Grants and rules.** Proposed: a grant never overrides a `deny` rule and
   may satisfy an `ask` rule, including a durable user grant made before
   the rule existed; the startup summary names any durable grant that
   covers an `ask` rule, so a team's review point is not silently skipped.
7. **`ask_timeout` default.** Proposed: absent means wait interactively as
   today and `0` under a never-prompt decider (headless `exec`), so
   headless denies at once when no reviewer decides. No built-in profile
   sets it.
8. **Reviewer protocol.** Proposed: a managed-process command invocation
   with a reviewer descriptor, a host budget of 10 seconds per reviewer and
   30 seconds per ask, input redacted and bounded like guardian input,
   verdict `{verdict: deny|defer, reason}`; failure, timeout, or a malformed
   verdict is `defer`.
9. **Allow rules in children.** Proposed: a child's own `allow` rules do not
   apply, so a child never runs looser than its parent (ADR 0026 resolved
   question 9) without comparing rule sets.
10. **Resume with changed rules.** Proposed: any difference in `rules`,
    `ask_timeout`, or `reviewers` keeps the recorded values unless
    `--profile` is passed, since rule sets have no cheap tighter-or-looser
    test; the startup summary names the difference.
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
    and today's digest (section 11).
15. **Field name.** `reviewers` sits next to the existing `reviewer`.
    Proposed: keep `reviewers`, since both name who reviews an ask and the
    contract documents them together; `extension_reviewers` is the
    alternative if review finds the pair confusing.
16. **A named reviewer that is missing.** Proposed: a startup error, not a
    warning. A missing deny-only reviewer cannot loosen a single decision,
    but the profile would run with less review than written, which ADR 0026
    resolved question 21 rejects.
