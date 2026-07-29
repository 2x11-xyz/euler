# Create and use an Euler skill

An Euler skill is a named Markdown procedure that the model can load when it
is useful. Euler advertises accepted skills in a compact catalog and reads the
full instructions only when the model calls `skill_read`.

## Choose a scope

- User skills apply across projects. Put them in
  `${EULER_HOME}/skills/<name>/SKILL.md`. `EULER_HOME` defaults to
  `~/.euler`.
- Project skills apply to one repository. Put them in
  `.euler/skills/<name>/SKILL.md` at the project root.

User skills remain available when project context is off. Project skills use
the same acknowledgment boundary as `EULER.md`.

## Create the skill

This example creates a user skill using the default Euler home:

```sh
mkdir -p ~/.euler/skills/commit-writing
```

Create `~/.euler/skills/commit-writing/SKILL.md`:

```markdown
---
name: commit-writing
description: Prepare small, reviewable Git commits with clear messages.
---

Use this skill when preparing a Git commit.

1. Inspect the staged diff.
2. Keep the commit limited to one logical change.
3. Use a concise, imperative subject.
4. Report the checks that support the commit.

Do not push unless the user asks.
```

The directory name and frontmatter `name` must match exactly. Names may contain
lowercase ASCII letters, digits, and hyphens. A hyphen cannot be first, last,
or repeated. `description` must be present, and the file must contain valid
UTF-8 Markdown with YAML frontmatter.

To make the same skill project-specific, use
`.euler/skills/commit-writing/SKILL.md` instead. A user skill and project skill
cannot share a name. Euler excludes both when names are ambiguous.

## Load and use it

Start a new Euler session after creating or editing a skill. In the TUI, use
`/new`. Euler freezes accepted skill content for the session, so an existing or
resumed session does not pick up later file changes.

Euler can select a skill from its catalog when the description matches the
task. You can also ask directly:

```text
Use the commit-writing skill for this change.
```

Skills provide guidance only. They do not grant tool permissions or bypass
Euler policy. Only `SKILL.md` enters the skill snapshot. Supporting files
remain subject to normal filesystem tools and permissions.

## If the skill does not appear

Check the path, exact `SKILL.md` capitalization, frontmatter delimiters,
directory and skill name match, duplicate names, and whether the session was
created after the latest edit.
