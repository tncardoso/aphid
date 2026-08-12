# Skills

A skill is an instruction file that the model opens when it needs it.

Only the name, the description and the path of each skill go into the system
prompt. The model reads the body with the `read` tool when a task agrees with
the description. This is progressive disclosure, and it is what keeps twelve
skills from costing twelve skills' worth of context on each request.

Use an `AGENTS.md` file for what is true always. Use a skill for what is true
sometimes: how to make a release, how to add a migration, how to write a
particular kind of test.

## Where skills go

Aphid looks in the workspace first, and then in your home directory. Two layouts
are correct:

```
.aphid/skills/<name>/SKILL.md
.aphid/skills/<name>.md
```

Use the directory when the skill has files of its own — a script, a template, an
example. The model can read them, because you give it the path.

A skill in the workspace hides a skill in your home directory with the same
name. Thus a project can replace a skill that you carry everywhere.

## Writing one

A skill needs frontmatter: a `---` block at the top of the file, with flat
`key: value` lines.

```markdown
---
name: release
description: How to cut a release of this crate. Use when the user asks to release, tag or publish.
---

# Release

1. Make sure that `cargo test` passes on `main`.
2. Change the version in `Cargo.toml`.
...
```

| Field | Effect |
| --- | --- |
| `description` | **Necessary.** What the skill is for, and when to use it. |
| `name` | The name of the skill. Optional. |

The `description` is the whole of what the model sees before it opens the file.
Write it to say *when* to use the skill, and not only what it is. A description
of more than 1024 characters is refused, and the skill is reported.

If there is no `name`, aphid uses the name of the directory for a `SKILL.md`, or
the name of the file for a loose `.md`.

Aphid reads only the two keys above. Frontmatter with more in it is accepted, and
the rest is passed over.

## Looking at them

Type `/skills` in a session. Each line gives the name of the skill, its
description, and whether the skill comes from the workspace (`project`) or from
your home directory (`global`).

A line that starts with `!` is a skill file that aphid could not use, and the
reason: no description, a description that is too long, or a file that could not
be read. A skill file with a mistake in it is reported, and the session
continues.

`--no-context` stops aphid from reading the skills and the `AGENTS.md` files.

## In a resident agent

An alate reads the skills in `<home>/.aphid/skills`, in the same manner. The
home of the alate is its workspace, so this is the workspace layout and not a
special one. Refer to [Alate](../alate.md).
