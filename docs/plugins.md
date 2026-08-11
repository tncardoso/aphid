# Plugins

This document tells you how to write a plugin for aphid. It is written in
Simplified Technical English.

A plugin is one file of [Rhai](https://rhai.rs) code. It can look at a run, stop
a tool, change a prompt, add a tool, and add a command. You do not compile
aphid again to add one.

## Contents

- [Where plugins go](#where-plugins-go)
- [Trust](#trust)
- [Hooks](#hooks)
- [How a hook changes a run](#how-a-hook-changes-a-run)
- [The run context](#the-run-context)
- [Capabilities](#capabilities)
- [Settings and memory](#settings-and-memory)
- [Tools](#tools)
- [Commands](#commands)
- [When a plugin fails](#when-a-plugin-fails)
- [Limits](#limits)
- [Command-line options](#command-line-options)

## Where plugins go

Aphid looks in the workspace first, then in your home directory. Two layouts are
correct:

```
.aphid/plugins/<name>.rhai
.aphid/plugins/<name>/main.rhai
```

The name of the plugin is the name of the file, or the name of the directory.
A plugin in the workspace hides a plugin in the home directory with the same
name.

Write the description of the plugin in `//!` comment lines at the top of the
file. The `/plugins` command and `aphid --list-plugins` show this text.

```rhai
//! Keeps the model away from the changelog.

fn on_tool_call(tool) {
    if tool.name == "write" && tool.arguments.contains("CHANGELOG") {
        return block("the changelog is written by hand");
    }
}
```

**Important:** `call` is a reserved word in Rhai. Do not use it as the name of a
parameter.

## Trust

A plugin in your home directory always loads. It is yours.

A plugin in a workspace comes with the checkout, so aphid asks you before it
loads one for the first time. Aphid keeps your answer in `~/.aphid/trust.json`
and does not ask again for that workspace.

Aphid asks the question before the terminal user interface starts. In headless
mode aphid does not ask, and does not load the plugins of the workspace. Use
`--trust-plugins` to agree without a question.

This controls which plugins load. It does not control what a plugin that loaded
can do. A plugin that you agreed to can do all that you can do.

## Hooks

To add a hook, write a function with the correct name. Aphid reads the names when
it loads the file. A plugin pays only for the hooks that it has.

These hooks come from the agent loop:

| Function | When it runs |
| --- | --- |
| `on_prompt(draft)` | Before aphid puts your prompt in the transcript |
| `on_run_start(cx)` | The run starts |
| `on_turn_start(cx)` | Before each request to the model |
| `on_event(event)` | For each protocol event. This is the fast path |
| `on_message(cx, message)` | After the answer of the model is in the transcript |
| `on_tool_call(tool)` | A tool call is asked for, but did not run |
| `on_tool_progress(id, tool, chunk)` | A tool sent partial output |
| `on_tool_result(result)` | A tool completed |
| `on_turn_end(cx, turn)` | A turn is complete |
| `on_run_end(cx, outcome)` | The run stopped |
| `on_request(body)` | Before aphid sends the request body |

These hooks come from the coding agent:

| Function | When it runs |
| --- | --- |
| `on_system_prompt(text)` | Aphid made the system prompt |
| `on_session_start(session)` | A session opened |
| `on_session_end(session)` | A session is closing |
| `on_permission(request)` | A tool needs permission |
| `on_file_change(change)` | `write` or `edit` changed a file |
| `on_notify(text)` | Aphid showed a message to the user |
| `on_tick()` | Every 250 milliseconds, in the terminal UI |

`on_tick` is the only hook that the agent does not cause. Use it to look at
something outside the session: a file, a queue, a clock. Keep it short. It runs
while the user is at the prompt, and `exec` and the http functions stop it until
they are complete. Aphid does not start a tick while the last one runs. There
are no ticks in headless mode.

Each hook gets a map. These are the fields:

- `on_prompt`: `text`
- `on_tool_call`: `id`, `name`, `arguments`, `known`, `blocked`
- `on_tool_result`: `id`, `name`, `arguments`, `turn`, `content`, `is_error`,
  `details`
- `on_message`: `text`, `thinking`, `tool_calls`
- `on_event`: `kind`, `turn`, and then `index`, `block`, `text` or `stop`
- `on_turn_end`: `stop_reason`, `tool_calls`, `input`, `output`, `error`
- `on_run_end`: `stop`, `turns`, `input`, `output`, `error`
- `on_session_start` and `on_session_end`: `id`, `path`, `reason`, `restored`
- `on_permission`: `tool`, `summary`, `risk`
- `on_file_change`: `path`, `kind`, `before`, `after`

## How a hook changes a run

Rhai sends the arguments of a function by value. Thus a hook cannot change the
map that it receives. A hook changes the run with the value that it **returns**.

Return nothing to change nothing.

| Return value | Result |
| --- | --- |
| `block("why")` | The tool does not run. The model reads the reason |
| `block_and_stop("why")` | The same, and the run stops after this batch |
| `reject("why")` | From `on_prompt`: the prompt does not go to the model |
| `stop()` | From `on_turn_end`: the run stops cleanly |
| `#{ text: "…" }` | From `on_prompt`: use this text in place of the prompt |
| `#{ arguments: "…" }` | From `on_tool_call`: use these arguments |
| `#{ content: "…" }` | From `on_tool_result`: use this result |
| `#{ append: "…" }` | From `on_system_prompt`: add this to the prompt |
| `#{ replace: "…" }` | From `on_system_prompt`: use this prompt |
| `"allow"`, `"deny"` | From `on_permission` |

`on_permission` also accepts `"allow_always"` and `"ask"`. Use `"ask"` when the
plugin has no opinion. Aphid then asks the user.

## The run context

The hooks that receive `cx` are different. `cx` holds a handle, not a copy, and
thus its methods do change the run.

```rhai
fn on_turn_start(cx) {
    cx.note("Today is a Tuesday.");   // adds a system message
}
```

| Member | Result |
| --- | --- |
| `cx.note(text)` | Adds a system message at the end of the transcript |
| `cx.push_user(text)` | Adds a user message at the end of the transcript |
| `cx.cancel()` | Stops the run at the next safe point |
| `cx.model` | The identifier of the model |
| `cx.turn` | The number of the turn, from zero |
| `cx.input_tokens`, `cx.output_tokens` | The tokens of the run until now |

The transcript only grows. A hook adds to it, and cannot rewrite it.

## Capabilities

A Rhai script can only calculate. Aphid gives it these functions:

| Function | Result |
| --- | --- |
| `notify(text)` | Shows text to the user |
| `prompt(text)` | Sends text to the model, as if the user typed it |
| `log(text)` | Writes text to standard error |
| `fs_read(path)` | Reads a file, and returns the text |
| `fs_write(path, text)` | Writes a file |
| `fs_exists(path)` | Returns `true` if the path is there |
| `fs_list(path)` | Returns the names in a directory |
| `exec(command)` | Runs a shell command |
| `http_get(url)` | Makes a GET request |
| `http_post(url, body, headers)` | Makes a POST request |

`prompt` is a call, not a value that a hook returns. A hook, a tool and a
command all use it the same way. The text goes in the queue that a typed line
goes in, and the terminal UI shows it as a message from the user. Only the
terminal UI has this queue: in headless mode, `prompt` does nothing.

A relative path in `fs_read` and the other file functions starts at the
workspace. In a coding session the path can go out of the workspace, because the
same plugin has `exec`, and a shell reads and writes anywhere. An embedder that
makes its own capabilities keeps the file functions in the workspace.

`exec` returns `#{ status, stdout, stderr }`. The http functions return
`#{ status, body, headers }`.

`exec` and the http functions run on a different thread, and they stop after 30
seconds.

`exec` runs the command with `bash`. It uses the same code as the `bash` tool of
the coding agent. Thus the runtime records each command that a plugin starts.
In a session, type `/ps` to see these commands. The list gives the name of your
plugin as the source of its commands. You can stop a command from that list; the
`exec` that started it then gives an error, and the script can continue.

`exec` reads the output while the command runs. Thus a command that writes many
lines continues correctly.

## Settings and memory

`config()` returns the settings of the plugin. Write them here:

```
.aphid/plugins/<name>.json          # in the workspace
~/.aphid/plugins/<name>.json        # in your home directory
```

The workspace file wins. The settings are read-only: a plugin cannot change what
you wrote.

`state()` returns what the plugin remembers, and `save_state(map)` keeps it.
Aphid writes the state to `.aphid/plugins/state/<name>.json` at the end of each
run and at the end of the session. A plugin that does not call `save_state` does
not write a file.

```rhai
fn on_session_start(session) {
    let s = state();
    s.runs = if "runs" in s { s.runs + 1 } else { 1 };
    save_state(s);
    notify("session number " + s.runs);
}
```

## Tools

Call `register_tool` at the top level of the file. Aphid runs the top level one
time, when it loads the plugin.

```rhai
register_tool(#{
    name: "wordcount",
    description: "Count the words in a file.",
    parameters: #{
        type: "object",
        properties: #{ path: #{ type: "string" } },
        required: ["path"]
    },
    execute: |args| { fs_read(args.path).split(' ').len() }
});
```

Write the `parameters` schema by hand, as a JSON Schema. Aphid sends it to the
model without a change.

The tool returns text. To say more, return a map with `content`, and then
`is_error` or `details` if you need them.

A tool with the name of a standard tool replaces that tool.

The body of a tool runs on a different thread. Thus a tool can be slow, and can
use `exec` and the http functions. Add `sequential: true` to stop aphid from
running it at the same time as other tools.

## Commands

Call `register_command` at the top level. The command shows in `/plugins`.

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

A standard command always wins. If two plugins use one name, aphid keeps both:
the second becomes `/review:2`.

## When a plugin fails

A plugin that does not compile becomes a message, and aphid continues. The other
plugins still load.

If a hook fails while it runs, aphid shows the error and continues without that
hook. Two hooks are different:

- `on_tool_call` stops the tool.
- `on_permission` refuses the permission.

These two are the hooks that people write to be safe. A guard that failed did not
agree to anything, and thus aphid does not continue as if it did.

A tool that fails becomes an error result. The model reads it and can correct
itself.

## Limits

Each hook can do 5 000 000 operations. Strings can be 8 MB. Arrays and maps can
hold 100 000 items. A hook that goes past a limit stops with an error.

## Command-line options

| Option | Result |
| --- | --- |
| `--list-plugins` | Shows the plugins that would load, and stops |
| `--no-plugins` | Loads no plugin from `.aphid/plugins` |
| `--plugin PATH` | Loads one plugin from a path. No trust question |
| `--trust-plugins` | Agrees to the plugins of this workspace |

In the terminal user interface, `/plugins` shows what loaded, the commands that
plugins added, and the files that did not load.

## Examples

The `crates/aphid-plugin/examples/plugins` directory holds plugins that work:

| File | What it does |
| --- | --- |
| `guard.rhai` | Stops the model from writing to protected files |
| `trace.rhai` | Reports each tool call and the cost of the run |
| `branch.rhai` | Tells the model the name of the git branch |
| `redact.rhai` | Keeps keys out of the transcript |
| `budget.rhai` | Stops a run that asks for too many tools |
| `wordcount.rhai` | Adds a `wordcount` tool |
| `review.rhai` | Adds a `/review` command |

## The web chat

This repository has one plugin of its own, in `.aphid/plugins/webchat.rhai`. It
puts a chat page on port 8000, and you talk to the session from a browser.

| Command | Result |
| --- | --- |
| `/server start` | Opens the chat, and shows the address to use |
| `/server stop` | Closes the chat |
| `/server` | Says if the chat is open, and on what address |

The address holds a token, and the page does not open without it. Keep the
address private: a person who has it can tell the agent what to do.

What you write in the browser shows in the terminal like a line that you type,
and the answer of the model goes to the browser while it writes it. What you
type in the terminal also shows in the browser.

The plugin writes a small Python server to `/tmp/aphid-webchat/<project>`, and
starts it with `exec`. Python 3 must be on the machine. `on_tick` reads what the
browser sends, and each hook sends the answer of the model back. The workspace
stays clean, because the plugin writes nothing in it.

Settings go in `.aphid/plugins/webchat.json`:

```json
{ "host": "0.0.0.0", "port": 8000 }
```

`host` is `0.0.0.0`, and thus another machine on the same network can open the
chat. Use `127.0.0.1` to keep the chat on this machine only.
