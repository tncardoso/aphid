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
| `Esc` | Stop the run. |
| `Ctrl-C` | Quit. |
| `Ctrl-P` | Change to the next model. |
| `Ctrl-T` | Show the reasoning. |
| `PageUp`, `PageDown` | Scroll. |
| `Enter` | Send the message. |
| `Shift-Enter` | Make a new line in the same message. |
| `Up`, `Down` | Move through the messages you sent before. |

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

A plugin adds a command with `register_command`, at the top level of the file.
The command shows in `/plugins`.

```rhai
register_command(#{
    name: "review",
    description: "Ask for a review of the changes.",
    run: |args| {
        let diff = exec("git diff").stdout;
        if diff == "" { return notice("nothing to review"); }
        prompt("Review this diff:\n" + diff);
        notice("reviewing…")
    }
});
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
