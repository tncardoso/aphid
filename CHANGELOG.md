# Changelog

## [Unreleased]

## [0.3.0] - 2026-09-14

### Added

- **`@` opens a list of the workspace's files in the aphid terminal.** Type `@`
  at the start of a word and a list of every file in the workspace opens over
  the screen. Type any part of a path to cut it down — the letters do not have
  to be next to each other, and a mistyped one is forgiven. Move with the arrows
  or Ctrl-P and Ctrl-N, and press Enter to write the path into the box where the
  `@` was. Esc, a space, or a Backspace with nothing left to take back closes
  the list and leaves what you typed. The list is powered by `fff`, which reads
  the tree one time and then follows it, so it stays correct while you work and
  a file you have just made is in it. The tree is read the first time you press
  `@`, so a session that never uses it costs nothing.

- **A file can be sent with a message, and an image can be looked at.**
  Choosing a file with `@` now asks a question: **Cite** writes its path into the
  message, as before, and **Attach** sends the file with it. Enter takes the
  marked answer and Cite is marked, so `@` then Enter then Enter still writes a
  path. An attachment is written as `@path`, which is ordinary text: delete one
  letter and the file is not sent, and Backspace on the marker removes the whole
  of it in one keystroke. An image is sent directly after the words that name
  it, so `compare @before.png with @after.png` arrives with each picture beside
  its own mention. PNG, JPEG, GIF and WebP up to 10 MB, read when you attach the
  file, so what you attached is what the model gets. A model that cannot look at
  an image says so when you attach it, and again if you point the session at one
  before sending — your line comes back into the box, nothing is lost. An
  attached text file is sent as its content, capped like the `read` tool at 1000
  lines or 64 KiB. Refer to the Commands page.

- **A scheduled job can answer in the conversation that scheduled it.** A job
  ran in a session of its own, said what it found, and nobody heard it: nothing
  was watching that session, and the job had no way to reach anything that was.
  A job now records the conversation it was written in and gets a
  `send_message` tool that says one thing there — a Telegram chat, a colony
  channel, a terminal, the resident conversation. The conversation is found by
  its id while it is open and by its name after that, so a chat that reconnects
  and an alate that was restarted are both still reachable. A message that has
  nowhere to go is reported to the job rather than swallowed. Refer to the
  Cron section of the Alate page.

- **The Telegram bot attaches its chats when the alate starts.** Every chat on
  the allow list has a conversation from the moment the alate is up, instead of
  only after somebody writes to it. That is what gives a job at three in the
  morning a chat to report into. A conversation for each allowed chat now shows
  in `/sessions` from the start.

- **The Telegram bot can receive files from the agent.** When you ask an
  alate to send a file in a Telegram chat, it can attach a file from an allowed
  workspace path as a document. The permission policy still controls each send.

- **The Telegram bot listens.** Send it a voice message and the alate makes
  text of it, shows you the text, and gives it to the agent as if you had typed
  it. Music files, round videos and files that say they are audio work as well.
  The speech is read on the machine of the alate and goes nowhere else: the
  model is Parakeet TDT 0.6b v3, it reads 25 languages, and the alate gets it
  one time into the cache of the machine. This is behind a build feature,
  `voice`, and a `voice` block in `alate.json`. Refer to the Telegram page.

- **A session tells Herdr what it is doing.** [Herdr](https://herdr.dev) is a
  terminal workspace manager that shows the state of the agents in its panes in
  a sidebar. Aphid is not one of the agents Herdr knows, so a plugin reports the
  state in its place: the pane reads `working` while a run goes on, `blocked`
  with the question that waits for your answer, and `idle` when the session is
  ready for you. The plugin is
  `crates/aphid-code/examples/plugins/herdr.rhai`. Copy it to
  `~/.aphid/plugins/herdr.rhai`, or to `.aphid/plugins/herdr.rhai` in a
  workspace. It does nothing when aphid runs outside Herdr. Refer to the Plugins
  page.

- **`aphid alate gui` opens a window on a running alate.** It is a console that
  drops from the top of the screen, or a column against its right edge, and it
  shows what the agent is doing between prompts. It is a gateway client and
  nothing more: closing the window does not stop the alate.
  - The two are different windows and not two sizes of one. The **companion**
    is the log, the alate in a band at its foot, and the text box. The
    **console** has no log: expanded, it is the alate and a balloon with the
    last thing it said, which is what you glance at while you are doing
    something else.
  - `aphid alate gui toggle`, `show`, `mode` and `quit` are a remote control
    for the window that is open, to bind to a key in a window manager. There is
    one window for the machine: a second `aphid alate gui` brings the first
    forward, and `--name` points it at another alate.
  - Without a daemon, the window opens anyway, says the alate is asleep, and
    offers to start it. That is the one place where aphid puts an agent in the
    background for you.
  - If the connection breaks it comes back, waiting a little longer each time
    up to half a minute. The daemon opens a session for each connection, so the
    window says the conversation is a new one instead of pretending otherwise.
  - Where the window sits is best effort. Under X11 it is moved into place and
    asked to stay above the others; on macOS it floats and follows you between
    spaces. Wayland lets no program place its own windows, so there it needs a
    rule in the compositor, matching the `com.embornal.aphid.alate` app id. The
    documentation gives one for Hyprland and one for Sway.
  - **The alate is drawn in the window, and what it does follows the run.** It
    thinks while a turn is going, talks while text arrives, is pleased when a
    run ends well, startled when a tool asks permission, and asleep when the
    connection is gone. There are two of them to choose between, `sap` and
    `drift`. On a machine with nothing to draw on, the window opens anyway and
    says why there is no creature. What it last said stands in a balloon over
    its head until it says something else; `Escape` puts that away.
  - An icon in the tray carries the same commands as the socket does: show,
    expand, switch mode, choose a familiar, point the window at another alate,
    and quit. Both tray protocols are spoken, since neither can be asked to do
    the other's job: StatusNotifierItem over the bus, which is what KDE, a
    GNOME with the extension and waybar listen for, and XEmbed, which is what
    i3 with polybar, xfce4-panel and everything of that generation provide. On
    an XEmbed panel there is no menu on the panel's side, so the right button
    opens it in the window. A desktop with neither gets no icon, and the window
    says so once rather than leaving you to wonder.
- Alate now runs shell commands and plugin commands in a Bubblewrap sandbox.
  By default they can change only the Alate workspace. The user can grant
  extra paths and selected host environment variables in a policy outside the
  agent workspace.
- `aphid gui` opens a GPUI desktop interface on Linux and macOS. It has a
  collapsible session drawer, streamed agent messages, tool cards, permission
  prompts, Markdown, and Rhai side panels. Remote Markdown images load only
  after you select them.
- **You can now select text in the terminal transcript and copy it.** Hold the
  left mouse button and move the pointer over what you want. The text is shown
  in reverse video while you hold the button, and it goes to the clipboard when
  you release it. The status line says how many lines it took, and `Esc` clears
  the selection. Aphid uses OSC 52, so this also works over SSH and in tmux.
- A plugin surface gets its defaults from `init`, and a value already stored
  wins over its default. A panel no longer has to write
  `if "open" in s { s.open } else { false }` for each of its keys.

### Changed

- **`/sessions` in the alate terminal opens a list you can filter.** Type any
  part of an id, a kind or a date and the list cuts down to what matches; the
  characters do not have to be next to each other. Move with the arrows and
  press Enter to look at the conversation under the cursor. `/session <id>`
  still works as before.

- **A session list holds the 20 most recent stored conversations.** Every
  conversation that runs now is still named. An older one on disk is still
  there, and `/session <id>` opens it.

- The graphical interface is now a build feature, `gui`, on by default. A
  build that turns it off keeps the whole terminal agent and stops compiling
  the window library, which is most of the build. `aphid gui` stays in
  `--help` either way: without the feature it says which build has a window
  instead of answering that the command does not exist.
- In the graphical interface, the buttons, the session list, the model list
  and the process list are drawn by the component library. They take the
  keyboard as well as the pointer, and a session row now refuses the click
  while a run is going instead of dimming and taking it anyway.
- **The graphical interfaces are rebuilt on a component library.** The text
  box now composes: a dead key makes `á` where it used to type `´a`, and so do
  the input methods of the system. Markdown, code highlighting and the
  transcript are drawn by that library as well, and the transcript now measures
  and draws only the messages on screen instead of the whole conversation on
  every frame.
- **Remote images in the graphical interface load straight away.** They used to
  wait for you to select them. The Markdown view that replaces the old one has
  no way to hold them back.
- A plugin surface's `mouse` message now carries `host`, which is `"terminal"`
  or `"gui"`. A row and a column are cells of a terminal, so a window sends
  zero for them; a panel that lines a table up by counting characters can read
  `host` and draw the other way.
- Selecting a model in the graphical interface no longer goes through a key
  that nobody pressed. It names the model.
- **Plugin surfaces are now written as three parts.** A surface declares
  `init` for what its model starts as, `update(state, msg)` for how a message
  changes it, and `view(state)` for what it looks like. The old `render` and
  `on_event` are refused at load, with a message saying what to rename. A
  surface can also ask for the background tick with `tick: true`, send itself a
  message with `send`, and send text to the model with `prompt_with`.
- A surface keeps its own model, reachable from the rest of the plugin with
  `surface_state(name)`. A tool that fills a panel no longer shares one map
  with every other part of the plugin.
- In the terminal, a permission question that nobody answers refuses the tool
  call after five minutes, as it already did for an alate. A run left waiting
  on a prompt no longer holds for the rest of the day.
- Quitting the terminal while a permission question is on screen refuses the
  call immediately, instead of leaving it to time out.
- Sessions are now stored in one place shared by every project:
  `~/.aphid/sessions` (or `$APHID_HOME/sessions`), as files named
  `<project>-<id>.jsonl`. Sessions saved under a workspace's own `.aphid`
  are no longer listed or resumed.

### Fixed

- **A scheduled job waits for its time.** A job you gave the alate ran as soon
  as you wrote it, if the time it names had already gone past since the alate
  started — a job written at eight in the evening for nine in the morning ran
  immediately. A new job now counts from the moment you write it, which is what
  the `cron` tool said it would do all along.

- **An alate's sessions no longer see one another's runs.** Each session
  mounted its components on the one composition the daemon shares, and a
  bus announcement reached every listener whatever session made it — so the
  resident session's heartbeat replies arrived in the Telegram chat, and each
  session's transcript file held every conversation. Announcements now carry
  the session that made them, and the per-session components listen only to
  their own.
- **A plugin's hooks no longer run once per conversation.** The scripts, the
  crontab tool and the colony tools were mounted in every session, so a hook
  fired N times for one event and the system prompt carried each plugin's
  instructions N times. They are now mounted once, for the daemon — and a
  script's `session_start` hook, which used to miss the announcement, now
  fires.
- Plugin commands now update their graphical side panels immediately instead
  of waiting for the next periodic refresh.
- The graphical interface can now start an agent reply, a shell command, or a
  plugin reload. These actions no longer exit with a missing Tokio reactor.
- `/clear` and `/new` now empty the screen. The transcript was dropped, but the
  lines already drawn stayed where they were, so a new conversation started
  under the old one.
- A plugin's tick and its panel no longer run at the same time. They could
  both read the plugin's state, change it and write it back, and one of the
  two changes was lost.
- A slash command typed while the model is replying now runs. It used to be
  sent to the model as the text of the command, so `/tools` mid-reply asked
  the model about the word `/tools`.
- `/model` and `/think` typed while the model is replying now take effect when
  the reply ends. The change used to be dropped without a word.
- A colony no longer forgets who joined its channels when it is restarted.
  Anybody who had joined `#general` was quietly put out of it at the next
  start, and their next message came back as `restricted: join general before
  you talk in it`. Re-joining is no longer necessary.
- A colony restarted in a later second no longer re-signs its channels. The
  channels the configuration names were made from the clock of the moment, so
  every start produced a new group metadata, admin list, member list and role
  list for each of them. They are now made from what the colony signed before,
  and a quiet restart writes nothing.

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
[Unreleased]: https://github.com/tncardoso/aphid/compare/v0.3.0...main
[0.3.0]: https://github.com/tncardoso/aphid/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/tncardoso/aphid/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/tncardoso/aphid/releases/tag/v0.1.0
