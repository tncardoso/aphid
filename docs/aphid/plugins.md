# Plugins

A plugin is one file of [Rhai](https://rhai.rs) code. It can look at a run, stop
a tool, change a prompt, add a tool, add a command, and add an interactive
terminal surface. You do not compile aphid again to add one.

A plugin can also be written in Rust, and compiled in. Refer to
[Plugins in Rust](#plugins-in-rust).

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

One more hook is not a hook of the loop:

| Function | When it runs |
| --- | --- |
| `on_request(body)` | Before aphid sends the encoded request body |

The loop hands the transcript to a backend, and never sees a request body: the
body is made inside the transport. Thus `on_request` **replaces** the transport
rather than watching it. Return a map to send that body in place of the one you
were given, and return nothing to send it unchanged. A script that fails here
leaves the body as it was.

Because it owns the transport, `on_request` cannot be joined with a backend that
the program that embeds aphid supplied itself. The coding agent has no such
backend, so this affects an embedder only.

`on_tick` is the only hook that the agent does not cause. Use it to look at
something outside the session: a file, a queue, a clock. Keep it short. It runs
while the user is at the prompt, and `exec` and the http functions stop it until
they are complete. Aphid does not start a tick while the last one runs. There
are no ticks in headless mode.

Every call into a plugin — a hook, a tick, a command, a panel — runs on one
thread, one at a time. So a change a tick makes to the state is what the next
panel render reads, and two calls can never both read the state, change it, and
write it back over each other.

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

`state()` returns what the plugin remembers. `state(map)` replaces the
in-memory state and does not write a file. `save_state(map)` replaces the
in-memory state and marks it for writing. Aphid writes saved state to
`.aphid/plugins/state/<name>.json` at the end of each run and at the end of the
session. A plugin that does not call `save_state` does not write a file.

```rhai
fn on_session_start(session) {
    let s = state();
    s.runs = if "runs" in s { s.runs + 1 } else { 1 };
    save_state(s);
    notify("session number " + s.runs);
}
```

Use `state(map)` for memory-only data. That data lives for the session and is
never written to disk.

A surface keeps its own model beside the plugin's, under a `surfaces` key.
`surface_state(name)` reads it and `surface_state(name, map)` replaces it, in
memory. A tool or a hook uses those to reach what a panel is showing; the panel
itself is given the model and returns the new one, and never calls either.

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

A plugin adds a slash command with `register_command`, at the top level of the
file. Refer to [Commands](commands.md#commands-from-plugins).

## Surfaces and widgets

A plugin adds an interactive terminal surface with `register_surface`. A surface
is a named region that the plugin fills with a declarative widget tree. The
first cut renders side panels on the right and the left of the transcript.

A surface is a small app of its own, with three parts: a model, a function that
changes it, and a function that draws it.

```rhai
register_surface(#{
    name: "todos",
    placement: #{ kind: "side", side: "right" },

    init: || #{ items: [], selected: 0, open: false },

    update: |s, msg| {
        if msg.kind == "key" && msg.code == "down" {
            s.selected = (s.selected + 1) % s.items.len();
        }
        s
    },

    view: |s| {
        if !s.open { return (); }
        #{ type: "list", items: s.items, selected: s.selected }
    }
});
```

### The model

`init` runs once, when the plugin loads, and its keys are the defaults. A value
that is already in the surface's state wins over its default, so `init` says
what a key means and not what it is. Nothing has to write `if "open" in s`.

The model is the surface's own, under the plugin's state. A hook, a tool or a
command reaches it with `surface_state(name)`, and replaces it with
`surface_state(name, map)`. That is how a tool writes what its panel draws: the
todo plugin's `todo_add` tool adds to the very list the todo panel shows.

Like `state(map)`, this is in memory for the session. Use `save_state` to keep
something across sessions.

### The update

`update` takes the model and a message and returns the new model. It is called
with one message at a time and its answer is stored before anything is drawn,
so what it returns is what `view` sees.

A message is a map with a `kind`:

| `kind` | Fields |
| --- | --- |
| `key` | `code`, `modifiers` |
| `mouse` | `button`, `row`, `column`, `target` |
| `paste` | `text` |
| `tick` | none, and only with `tick: true` |
| `msg` | `name`, `payload` |

Return the new model, or a map of `#{ state: …, cmd: [ … ] }` to change the
model and ask the host for something as well. To ask for something without
changing the model, return the ask alone.

The asks are:

| Ask | What it does |
| --- | --- |
| `"consume"` | The message was handled |
| `"release_focus"` | Return focus to the input box |
| `notice("text")` | Show a notice |
| `prompt_with("text")` | Send text to the model, as a typed line |
| `send("name")`, `send("name", payload)` | Send the surface a message of its own |

`send` is how a surface asks for its next step: the update says what should
happen and returns, rather than doing it in the middle of working out the new
model. The message comes back as `kind: "msg"`.

Add `tick: true` to hear the background tick as a `kind: "tick"` message.

### The view

`view` takes the model and returns `()` to close the surface, or a widget tree
to open it. It changes nothing. The first cut knows these widgets:

| Type | Fields |
| --- | --- |
| `rows` | `children` |
| `cols` | `children` |
| `text` | `text` |
| `list` | `id`, `items`, `selected` |
| `input` | `id`, `text`, `placeholder` |
| `button` | `id`, `label` |
| `spacer` | none |

`id` is for the widgets a click can hit. A mouse message carries `target` with
that id.

In the terminal UI, `F6` gives focus to an open panel. `Esc` returns focus to
the input box. Clicking a panel also focuses it. While a panel has focus, its
`update` receives the keys, mouse messages and pastes. `F6`, `Esc` and `Ctrl-C`
stay with the app and are not sent to a plugin.

Render and event callbacks run on the same thread as every other script call,
which is not the thread that draws the screen. Keep them short: a slow one
delays the other plugins, but it does not hold the terminal.

### Moving a surface written for the older shape

A surface used to have `render(state)` and `on_event(event)`, and kept its
state in the plugin's own map. Aphid refuses such a surface at load, and says
what to rename.

| Before | Now |
| --- | --- |
| `render: \|s\| …` | `view: \|s\| …` |
| `on_event: \|event\| …` | `update: \|s, msg\| …`, returning the new model |
| `state()` inside a surface | the model `update` and `view` are given |
| `state(map)` inside a surface | return the new model from `update` |
| `state()` in a tool, for the panel | `surface_state("name")` |
| defaults with `if "x" in s` | `init: \|\| #{ x: … }` |

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

## Plugins in Rust

A program that embeds aphid can supply a plugin as a Rust type. The hooks are
the same hooks, with the same names.

```rust
use aphid_agent::{Guard, PendingCall, Plugin};

struct NoCityName;

impl Plugin for NoCityName {
    fn name(&self) -> &str { "no-lisbon" }

    fn on_tool_call(&self, call: &mut PendingCall<'_>) -> Guard {
        if call.arguments().contains("CityName") {
            return Guard::block("CityName is off limits.");
        }
        Guard::Allow
    }
}
```

The hooks are **synchronous**. The only hook that runs for each token is
`on_event`, and to box a future for each token would remove the point of the
memory layout that [Core](../core.md) describes. Anything that must wait belongs
in a tool, because a tool is the one part of this surface that is asynchronous.

A plugin declares an `Interest` set, and thus a hook that no plugin wants costs
the check of an empty list.

Use `cargo doc -p aphid-agent --open` for the full trait.

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
| `panel.rhai` | Adds an interactive right-hand side panel |

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
