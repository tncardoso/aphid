# Commands

A command is a line that starts with `/`. The terminal user interface reads it
and acts on it. A command never goes to the model, unless a plugin decides to
send something to the model itself.

Type `/help` to see the list in the terminal.

## The standard commands

| Command | Effect |
| --- | --- |
| `/model [name]` | Change the model, or open the picker when you give no name. |
| `/think <level>` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh` or `max`. |
| `/clear`, `/new` | Start a new conversation. The system prompt stays. |
| `/tools` | List the tools that are registered. |
| `/ps` | Show what the runtime runs now, and what it ran before. |
| `/session` | Show where this session is written. |
| `/plugins` | List the plugins that loaded, and the commands they added. |
| `/skills` | List the skills that the model can open. |
| `/help` | Print the list. |
| `/quit` | Exit. `/q` and `/exit` do the same. |

| Key | Effect |
| --- | --- |
| `Esc` | Clear the selection, or stop the run. |
| `Ctrl-C` | Quit. |
| `Ctrl-P` | Change to the next model. |
| `Ctrl-T` | Show the reasoning. |
| `PageUp`, `PageDown` | Scroll. |
| `Enter` | Send the message. |
| `Shift-Enter` | Make a new line in the same message. |
| `Up`, `Down` | Move through the messages you sent before. |
| Mouse wheel | Scroll. |
| Mouse drag | Select text in the transcript. Release to copy it. |

To copy text out of the transcript, hold the left mouse button and move the
pointer over it. The text under the pointer is shown in reverse video. Release
the button, and the text goes to the clipboard. The status line says how many
lines it took. `Esc` clears the selection.

A drag takes the characters that are on the screen, the prompt markers
included: a selection that starts at the left edge of a message you sent takes
the `> ` with it. To leave the marker out, start the drag after it.

Aphid sends the text to your terminal with OSC 52, so a copy also works over
SSH and in tmux. Some terminals keep OSC 52 off until you turn it on.

Text that you paste goes into the editor as it is, on as many lines as it has.
A paste does not send the message: press `Enter` when the message is complete.

`Up` on the first line shows the message you sent before. `Down` comes back to
what you were writing, which is kept while you look.

## Shell commands

A line that starts with `!` is a shell command, not a message. The terminal
user interface runs the text after the `!` in the workspace, and prints the
output into the content area.

The input border turns red while the line is a command. The command never
goes to the model. It runs through the same engine as the `bash` tool, so
`/ps` shows it while it runs, and `k` stops it. A bang line is kept in the
input history, so `Up` recalls it and `Enter` runs it again.

`/model` with no name opens a list of the catalogue. `/model <name>` accepts the
same three forms as `--model`: the full identifier, the last part of it, or a
prefix.

`/clear` and `/new` are the same command. The conversation is dropped and the
system prompt is kept, so the agent still knows the project.

## The file list

Type `@` at the start of a word to open a list of every file in the workspace.
Type any part of a path to cut the list down. The letters do not have to be
next to each other, and a letter that is wrong is forgiven, so `@tuiapp` finds
`crates/aphid-code/src/tui/app.rs`.

| Key | Result |
| --- | --- |
| Arrow keys, or `Ctrl-P` and `Ctrl-N` | Move the cursor |
| `Enter` | Choose the file, and open the question below |
| `Esc`, or `Ctrl-C` | Close the list, and keep the `@` |
| `Backspace` | Take back one letter of the query, or close the list when there is none |
| Space | Close the list, and type the space |

The path that the list writes is relative to the workspace root, which is the
path that the tools of the agent accept.

An `@` inside a word is only a character: `you@example.com` opens no list.

The file tree is read the first time that you press `@`, and a watcher keeps
it correct after that. A file that you make while the terminal is open is in
the list. A session that never presses `@` does not read the tree.

### Cite or Attach

`Enter` on a file opens a question with two answers:

| Answer | Result |
| --- | --- |
| Cite | Write the path into the message. This is text, and nothing more. |
| Attach | Send the file with the message. The path is written with an `@` in front of it. |

| Key | Result |
| --- | --- |
| `Enter` | Take the answer that is marked. Cite is marked when the question opens. |
| `c` | Cite. |
| `a` | Attach. |
| Arrow keys | Move the mark. |
| `Esc` | Close the question, and keep the `@` in the box. |

Press `Enter` two times to write a path, which is the quickest way: the first
`Enter` chooses the file, and the second one cites it.

### Attachments

A marker is an `@` and a path, and it is ordinary text. `@src/main.rs` is a
marker; `src/main.rs` is a citation. Both can be in the same message, and a
marker can be in the middle of a sentence:

```
compare @shots/before.png with @shots/after.png
```

The words of a marker become the reference of the file. The image is sent
directly after the words that name it.

An attached text file is sent as its content, wrapped in `<file path="…">`.
The cap is the cap of the `read` tool: 1000 lines or 64 KiB, whichever comes
first. The text says when the file is longer than that.

An attached image is sent as an image. Aphid reads PNG, JPEG, GIF and WebP, and
refuses a file above 10 MB. The bytes decide the format, not the file name. The
model must accept images: aphid refuses an image for a model that cannot look at
one, and the message says which model to choose with `/model`.

A marker breaks when its text changes. Take one letter away and the file is not
sent with the message. `Backspace` and `Delete` remove the whole marker at one
keystroke when the cursor is on it or next to it, so a broken marker is rare.
Type the marker again and the file is attached again, as long as the message is
not sent yet. Thus an edit does not lose the work of reading the file.

`Esc` on a line that is not running clears the line and the files it named.

A file is read when you attach it, not when the message goes out. What you saw
in the message is what the model receives, even if the file changes after that.

A file is read when you attach it, not when the message goes out. What you saw
in the message is what the model receives, even if the file changes after that.

## `/ps`

The list shows each command that runs now, and the last four commands that
stopped. Each line gives the number of the command, its system process
identifier, the source (`bash`, or the name of a plugin), the time, and, for a
command that stopped, the result and the quantity of output.

Press the arrow keys to select a command that runs now, and press `k` to stop
it. This stops the command and each command that it started. Press `Esc` to
close the list.

The list opens while the agent runs also, which is when there is most to see.
The other commands wait for the run, because they speak to the agent; this one
does not.

## Commands from plugins

A plugin adds a command with `command`, from its `apply`, with `commands` in its
`inject`. The command shows in `/plugins`, and it is on offer for as long as the
plugin is loaded — no longer.

```rhai
const inject = ["commands"];

fn apply(ctx) {
    command(#{
        name: "review",
        description: "Ask for a review of the changes.",
        run: |args| {
            let diff = exec("git diff").stdout;
            if diff == "" { return notice("nothing to review"); }
            prompt("Review this diff:\n" + diff);
            notice("reviewing…")
        }
    });
}
```

`args` is the text after the name of the command.

Return `notice(text)`, a text, or an array of them to show text to the user. To
send text to the model, call `prompt(text)`. Aphid shows the notices first, and
then the prompt, whatever the order in the command.

A standard command always wins, and thus a plugin cannot take `/quit` away. If
two plugins use one name, aphid keeps both: the second becomes `/review:2`.

A name with a space in it is refused. A leading `/` is removed, so `review` and
`/review` give the same command.

Refer to [Plugins](plugins.md) for the rest of what a plugin can do.

## The resident agent

The terminal that attaches to an alate has a different, smaller set of commands.
Refer to [CLI](../alate/gateway/cli.md).
