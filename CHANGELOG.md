# Changelog

## [Unreleased]

### Changed

- In the terminal, a permission question that nobody answers refuses the tool
  call after five minutes, as it already did for an alate. A run left waiting
  on a prompt no longer holds for the rest of the day.
- Quitting the terminal while a permission question is on screen refuses the
  call immediately, instead of leaving it to time out.

### Fixed

- A plugin's tick and its panel no longer run at the same time. They could
  both read the plugin's state, change it and write it back, and one of the
  two changes was lost.
- A slash command typed while the model is replying now runs. It used to be
  sent to the model as the text of the command, so `/tools` mid-reply asked
  the model about the word `/tools`.
- `/model` and `/think` typed while the model is replying now take effect when
  the reply ends. The change used to be dropped without a word.

## [0.2.0] - 2026-08-16

### Added

- The todo plugin gains a `todo_clear` tool that removes every pending and
  done task from the list.
- The `write` tool reports the file size it is about to write and the bytes
  written, as it runs, so a write to a large or slow file is no longer a silent
  card.
- The status line shows the live download speed, in KB/s, while the model's
  reply streams in.
- In the terminal input, a line starting with `!` runs a shell command and
  prints its output into the content area. The input border turns red while
  the line is a command.
- Skills are read from `.agents/skills` as well as `.aphid/skills`, in the
  workspace and in your home directory. Thus one skill directory can serve aphid
  and the other agents together. A name in `.aphid/skills` still wins.
- Pasting into the terminal input brings the text in whole, newlines and all,
  and a paste does not send the message: press `Enter` when it is complete.
- A message keeps the line breaks it was written with, in the input and in the
  chat.
- The aphid mascot on the site splats into green droplets when you click on it,
  then the splat fades away.
- When the coding agent starts with no configured models, it prints how to add
  one with `aphid models add`.
- Rhai plugins can register interactive side panels in the terminal UI. A panel
  can show text, lists, inputs and buttons, and receives focus with `F6` or a
  click.

### Changed

- The mouse wheel now scrolls the transcript instead of the input box in the
  terminal UI.
- The coding agent reads its model catalogue only from `~/.aphid/models.json`;
  it no longer supplies built-in DeepSeek models.

### Fixed

- In the terminal input, `Down` on the line you are writing no longer wipes the
  draft, and the draft survives a trip `Up` and back `Down`.
- The terminal UI keeps only the latest 300 transcript entries on screen and
  redraws only the message blocks that changed, so a long session no longer gets
  slower as it grows.
- Long lines inside assistant code blocks wrap to the width of the transcript
  pane instead of running off the edge.
- When you scroll up, new output no longer pushes the viewport off the text you
  were reading.

## [0.1.0] - 2026-08-13

### Fixed

- Stopping a command sends the signal straight through the syscall instead of
  spawning the `kill` binary, so a busy machine can no longer stretch a stop
  past the grace period behind a queued fork+exec.

### Added

- Aphid, the coding agent: a terminal user interface, tools, project context
  and sessions.
- A core with a data-oriented design: a conversation in flat, append-only
  arenas, and streaming deltas resolved in one memcpy.
- Plugins in Rhai, with hooks on the request, the stream, the tool calls and
  the permission prompts.
- Alate, the resident agent: a home directory, a memory that continues between
  sessions, a heartbeat, cron, and the CLI, Telegram and colony gateways.
- Colony, the hub agents speak in: a nostr relay for NIP-29 groups over
  websockets, a store in SQLite, and a terminal that attaches to it.
- Packaged binaries for Linux and macOS, and a shell installer that gets the
  last release.
- The crates on crates.io: `aphid-ai`, which installs the `aphid` binary, and
  the seven libraries behind it.
- A site at <https://aphid.embornal.com>, with the book at `/docs/`.

[kac]: https://keepachangelog.com/en/1.1.0/
[semver]: https://semver.org/spec/v2.0.0.html
[releasing]: https://aphid.embornal.com/docs/releasing.html
[Unreleased]: https://github.com/tncardoso/aphid/compare/v0.2.0...main
[0.2.0]: https://github.com/tncardoso/aphid/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/tncardoso/aphid/releases/tag/v0.1.0
