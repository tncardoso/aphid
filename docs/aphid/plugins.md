# Plugins

A plugin is one file of [Rhai](https://rhai.rs) code. It can look at a run, stop
a tool, change a prompt, add a tool, add a command, add an interactive terminal
surface, and offer a service to other plugins. You do not compile aphid again to
add one.

A plugin can also be written in Rust, and compiled in. Refer to
[Plugins in Rust](#plugins-in-rust).

This page is the reference. [Composition](composition.md) is the model behind
it, and worth reading first — a plugin here declares what it needs and the
runtime decides when it runs, which is a different bargain from the one most
plugin systems offer.

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

fn apply(ctx) {
    on("agent/tool-call", |tool| {
        if tool.name == "write" && tool.arguments.contains("CHANGELOG") {
            return block("the changelog is written by hand");
        }
    });
}
```

`.aphid/plugins.json` overrides what was found: switch one off, configure it,
isolate a service for it, or name a file that lives elsewhere. See
[the composition file](composition.md#the-composition-file).

**Important:** `call` is a reserved word in Rhai. Do not use it as the name of a
parameter, and use `invoke` to reach a service.

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

## `apply`

Everything a plugin contributes happens in `apply`. It runs once, when the
plugin loads — which is not necessarily at startup: a plugin that declared
`inject` waits until what it declared is there.

```rhai
const inject   = ["shell"];
const provides = ["todos"];
const emits    = ["todos/changed"];

fn apply(ctx) {
    on("agent/turn-start", |cx| { cx.note("…"); });
    provide("todos", #{ add: |text| { … } });
    effect(|| { … }, || { … });
}
```

| In `apply` | What it does | Needs in `inject` |
| --- | --- | --- |
| `on(event, closure)` | Subscribe to something announced | — |
| `tool(map)` | Contribute a tool the model may call | `tools` |
| `command(map)` | Contribute a slash command | `commands` |
| `surface(map)` | Contribute a panel | `surfaces` |
| `provide(name, map)` | Offer a service, as a map of functions | — |
| `invoke(name, method, args)` | Call a service | the service |
| `effect(setup, teardown)` | Take something, give it back on unload | — |

These work **only inside `apply`**. Outside it there is no component for the
runtime to attach the registration to, so nothing could undo it when your plugin
unloads — the call is refused, and says so.

`tools`, `commands` and `surfaces` are ordinary services: a plugin that
contributes one waits for the registry the same way it waits for anything else,
and what it contributed leaves when it does. See
[Composition](composition.md#services).

## Events

Subscribe with `on`. These come from the agent loop:

| Event | When it fires |
| --- | --- |
| `agent/prompt` | Before aphid puts your prompt in the transcript |
| `agent/run-start` | The run starts |
| `agent/turn-start` | Before each request to the model |
| `agent/request` | After `agent/turn-start`. Changes what the request sends |
| `agent/event` | For each protocol event. This is the fast path |
| `agent/message` | After the answer of the model is in the transcript |
| `agent/tool-call` | A tool call is asked for, but did not run |
| `agent/tool-progress` | A tool sent partial output |
| `agent/tool-result` | A tool completed |
| `agent/turn-end` | A turn is complete |
| `agent/run-end` | The run stopped |

Subscribing to a name nothing announces is reported when you subscribe, rather
than silently never firing.

These come from the coding harness — the things the loop has no word for,
because a permission or a file change is this harness's idea rather than the
loop's. They are announced on the same bus and subscribed to the same way:

| Event | When it fires |
| --- | --- |
| `code/system-prompt` | Aphid made the system prompt |
| `code/session-start` | A session opened |
| `code/session-end` | A session is closing |
| `code/permission` | A tool needs permission |
| `code/file-change` | `write` or `edit` changed a file |
| `code/notice` | Aphid showed a message to the user |
| `code/tick` | Every 250 milliseconds, in the terminal UI |

`code/system-prompt` is a **waterfall**: each listener receives the prompt as it
stands and returns what the next should see, so appending and replacing are the
same operation from two ends. It fires while the harness is being built, which
makes it the only announcement made before an agent exists.

`code/permission` is a **bail**: the first listener with an opinion decides and
the rest do not run, because a second opinion on a settled question is a second
question for the user.

`code/tick` is the only one the agent does not cause. Use it to look at
something outside the session: a file, a queue, a clock. Keep it short. It runs
while the user is at the prompt, and `exec` and the http functions stop it until
they are complete. A tick still being handled is not announced again, so a slow
listener costs its own time rather than a queue behind it. There are no ticks in
headless mode.

`code/notice` is not reentrant either, and for the same kind of reason: a
listener that shows the user something would announce itself.

Calls into a plugin come from different threads. A listener of the agent runs
on the thread of the agent, a tool on a thread of its own, and a tick, a command
or a panel on the plugin thread. But aphid lets only one call into a plugin at a
time. A call waits while another call into the same plugin runs. Thus a change
that a tick makes is what the next panel render reads, and two calls can never
both read the state, change it, and write it back over each other. This is also
true for a map that the closures of `apply` capture.

A call that waits for another call into the same plugin cannot continue until
that call ends. Thus keep each call short. A call into a different plugin does
not wait.

## What each listener is handed

- `agent/prompt`: `text`
- `agent/tool-call`: `id`, `name`, `arguments`, `known`, `blocked`
- `agent/tool-result`: `id`, `name`, `arguments`, `turn`, `content`, `is_error`,
  `details`
- `agent/message`: `cx`, then `text`, `thinking`, `tool_calls`
- `agent/event`: `kind`, `turn`, and then `index`, `block`, `text` or `stop`
- `agent/request`: `cx`, then `turn`, `run_start`, `length`
- `agent/turn-end`: `cx`, then `stop_reason`, `tool_calls`, `input`, `output`,
  `error`
- `agent/run-end`: `cx`, then `stop`, `turns`, `input`, `output`, `error`
- `code/session-start` and `code/session-end`: `id`, `path`, `reason`, `restored`.
  `reason` is `new`, `resume` or `end`, or `switch` when the user moves to a
  different session file. A jump or a fork in the same file sends no event.
- `code/permission`: `tool`, `summary`, `risk`
- `code/file-change`: `path`, `kind`, `before`, `after`
- `code/system-prompt`: the prompt as text
- `code/notice`: the text shown
- `code/tick`: nothing

## How a listener changes a run

Rhai sends the arguments of a function by value. Thus a listener cannot change
the map that it receives. It changes the run with the value that it **returns**.

Return nothing to change nothing.

| Return value | Result |
| --- | --- |
| `block("why")` | The tool does not run. The model reads the reason |
| `block_and_stop("why")` | The same, and the run stops after this batch |
| `reject("why")` | From `agent/prompt`: the prompt does not go to the model |
| `stop()` | From `agent/turn-end`: the run stops cleanly |
| `#{ text: "…" }` | From `agent/prompt`: use this text in place of the prompt |
| `#{ arguments: "…" }` | From `agent/tool-call`: use these arguments |
| `#{ content: "…" }` | From `agent/tool-result`: use this result |
| `#{ append: "…" }` | From `code/system-prompt`: add this to the prompt |
| `#{ replace: "…" }` | From `code/system-prompt`: use this prompt |
| `#{ history: …, system: …, … }` | From `agent/request`: send a different request. Refer to [The request](#the-request) |
| `"allow"`, `"deny"` | From `code/permission` |

`code/permission` also accepts `"allow_always"` and `"ask"`. Use `"ask"` when
the plugin has no opinion; the next listener, and finally the user, then
decides.

Every listener runs, even after one has refused a tool call — an observer still
wants to see a call somebody else blocked. The first refusal is the one that
stands.

## The run context

The listeners that receive `cx` are different. `cx` holds a handle, not a copy,
and thus its methods do change the run — whatever Rhai did with the value on the
way in, and from wherever the listener happens to run.

```rhai
fn apply(ctx) {
    on("agent/turn-start", |cx| {
        cx.note("Today is a Tuesday.");   // adds a system message
    });
}
```

| Member | Result |
| --- | --- |
| `cx.note(text)` | Adds a system message at the end of the transcript |
| `cx.push_user(text)` | Adds a user message at the end of the transcript |
| `cx.cancel()` | Stops the run at the next safe point |
| `cx.cancelled` | `true` if something stopped the run |
| `cx.hold()` | From `agent/request`: keeps the request back. Refer to [The request](#the-request) |
| `cx.model` | The identifier of the model |
| `cx.turn` | The number of the turn, from zero |
| `cx.input_tokens`, `cx.output_tokens` | The tokens of the run until now |

The transcript only grows. A listener adds to it, and cannot rewrite it.

## The request

`agent/request` changes what one request sends to the model. It does not change
the transcript. The transcript, and the session file, keep all the messages.
`/tree` and `--resume` thus show the conversation as the user had it.

Aphid announces `agent/request` before each request, after `agent/turn-start`.
The listener gets `cx` and a map with `turn`, `run_start` and `length`.
`run_start` is the position of the prompt of this run in the transcript.
`length` is the number of messages in the transcript.

The listener returns a map. Each field is optional:

| Field | Result |
| --- | --- |
| `history` | `"run"` sends only the messages of this run. `"all"` sends all the messages. The default is `"all"` |
| `system` | Sends this text as the system prompt. The text replaces the full prompt of aphid |
| `prefix` | An array of `#{ role, text }`. Aphid sends these messages after the system prompt. `role` is `"system"` or `"user"` |
| `prompt_prefix` | Aphid puts this text before the first user message, in the same message, with an empty line between |
| `exclude_tools` | An array of tool names. Aphid does not offer these tools in this request |

When more than one listener returns a map, a field from a later listener
replaces the same field from an earlier one. `prefix` and `exclude_tools` add to
what is there.

```rhai
fn apply(ctx) {
    on("agent/request", |cx, request| {
        #{ history: "run", prompt_prefix: "Today is a Tuesday." }
    });
}
```

### Hold a request

`cx.hold()` keeps the request back. It returns a handle. Keep the handle, and
call `release()` on it when the request can go. Aphid then announces
`agent/request` again, and the listener can shape the request from what it
waited for. A tick, a command, or the reply of a model can release the handle.

```rhai
fn apply(ctx) {
    let mem = #{ ready: false, hold: () };
    on("agent/request", |cx, request| {
        if !mem.ready { mem.hold = cx.hold(); return; }
        #{ prompt_prefix: "ready" }
    });
    on("code/tick", || {
        mem.ready = true;
        if type_of(mem.hold) == "Hold" { mem.hold.release(); }
    });
}
```

While the request waits, the listener does not run and nothing is blocked. The
user can push `Esc`. The run then stops and aphid sends nothing. Do not keep a
request back without a plan to release it: aphid waits until the release or
`Esc`.

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
| `fs_append(path, text)` | Adds text at the end of a file, and writes it to the disk before it returns |
| `fs_lock(path)` | Takes a lock on a file. Returns `false` if another process, or another plugin, has the lock |
| `fs_unlock(path)` | Releases a lock that `fs_lock` took |
| `time_now()` | The time, as `#{ unix_ms, iso, day }`. `day` is the local date, such as `2026-10-07` |
| `aphid_home()` | The directory of aphid, usually `~/.aphid` |
| `try_parse_json(text)` | The value in a JSON text, or `()` if the text is not correct JSON |
| `exec(command)` | Runs a shell command |
| `http_get(url)` | Makes a GET request |
| `http_post(url, body, headers)` | Makes a POST request |

These functions give the parts that aphid made its system prompt from. Use them
when `agent/request` replaces the system prompt:

| Function | Result |
| --- | --- |
| `system_prompt()` | The system prompt of aphid, as aphid sends it |
| `agents_md()` | The `AGENTS.md` files, as an array of `#{ path, text }`. The global file is first |
| `skills()` | The skills, as an array of `#{ name, description, path, project }` |
| `tool_list()` | The tools of this session, as an array of `#{ name, description, snippet }`. `snippet` is the short guideline of a built-in tool, and `""` for other tools |

`tool_list()` reads the tools when you call it. Thus it also gives a tool that a
plugin added later. Before the session starts, and in an alate, these functions
give empty values.

`prompt` is a call, not a value that a listener returns. A listener, a tool
and a command all use it the same way. The text goes in the queue that a typed line
goes in, and the terminal UI shows it as a message from the user. Only the
terminal UI has this queue: in headless mode, `prompt` does nothing.

A relative path in `fs_read` and the other file functions starts at the
workspace. In a coding session the path can go out of the workspace, because the
same plugin has `exec`, and a shell reads and writes anywhere. An embedder that
makes its own capabilities keeps the file functions in the workspace.

Use `try_parse_json` for a file that a crash can cut. The `parse_json` of Rhai
raises an error that `try` cannot catch.

`fs_append` and `fs_write` make the directories that are not there.
`fs_append` writes the text with one write, then makes sure the disk has it. A
crash does not lose a line that `fs_append` returned for.

A lock from `fs_lock` stays until `fs_unlock`, until the plugin unloads, or until
aphid stops. The system releases it when the process stops, so a crash does not
leave a lock behind.

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

`exec` returns 250 ms after the shell stops, also when a process that the
command started in the background keeps the output. `exec` does not return the
output of that process. To keep it, send it to a file:
`exec("server > server.log 2>&1 &")`. `/ps` shows the process with `↻ bg`.

## Models

A plugin can send a request to a model of `~/.aphid/models.json`, without the
agent and without the transcript. Use it for work in the background, for
example to make a summary.

| Function | Result |
| --- | --- |
| `model_ask(request, reply)` | Sends the request and returns its number at once. Aphid calls `reply` with the result when the model answers |
| `model_busy()` | The number of requests of this plugin that did not get an answer yet |
| `model_list()` | The models, as an array of `#{ id, name, provider, input_cost, output_cost, reasoning }` |

The request is a map:

| Field | Result |
| --- | --- |
| `model` | The model. Aphid finds it as `/model` does: the full id, the last part of the id, or the start of the id |
| `system` | Optional. The system prompt |
| `messages` | An array of `#{ role, text }`. `role` is `"user"` or `"assistant"` |
| `thinking` | Optional. `"off"`, or a level such as `"low"` or `"medium"`. The default is `"off"` |
| `max_tokens` | Optional. The most tokens of the answer |
| `timeout_ms` | Optional. Aphid stops the request after this time. The default is 5 minutes |

`reply` gets a map: `id`, `ok`, `text`, `error`, `stop`, `input`, `output`,
`cache_read` and `cost`. `text` is the text of the answer, without the thinking.
When `ok` is `false`, `error` tells why.

```rhai
model_ask(#{ model: "flash", messages: [#{ role: "user", text: "Say hi." }] }, |reply| {
    if reply.ok { notify(reply.text); } else { notify("failed: " + reply.error); }
});
```

Many requests can wait for an answer at the same time. A `reply` is a call into
the plugin like a listener, so only one call into the plugin runs at a time.
Aphid reads the key of the model from the variable that `api_key_env` names.
The tokens of these requests are not in the cost of the session.

`model_ask` raises an error at once when it does not know the model, or when the
request is not correct. In an alate, `model_ask` is not available.

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
fn apply(ctx) {
    on("code/session-start", |session| {
        let s = state();
        s.runs = if "runs" in s { s.runs + 1 } else { 1 };
        save_state(s);
        notify("session number " + s.runs);
    });
}
```

Use `state(map)` for memory-only data. That data lives for the session and is
never written to disk.

A surface keeps its own model beside the plugin's, under a `surfaces` key.
`surface_state(name)` reads it and `surface_state(name, map)` replaces it, in
memory. A tool or a listener uses those to reach what a panel is showing; the panel
itself is given the model and returns the new one, and never calls either.

## Tools

Call `tool` from `apply`, with `tools` in your `inject`.

```rhai
const inject = ["tools"];

fn apply(ctx) {
    tool(#{
        name: "wordcount",
        description: "Count the words in a file.",
        parameters: #{
            type: "object",
            properties: #{ path: #{ type: "string" } },
            required: ["path"]
        },
        execute: |args| { fs_read(args.path).split(' ').len() }
    });
}
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

A plugin adds a slash command with `command`, from `apply`, with `commands` in
its `inject`. Refer to [Commands](commands.md#commands-from-plugins).

## Surfaces and widgets

A plugin adds an interactive terminal surface with `surface`, from `apply`, with
`surfaces` in its `inject`. A surface is a named region that the plugin fills
with a declarative widget tree. The first cut renders side panels on the right
and the left of the transcript.

A surface is a small app of its own, with three parts: a model, a function that
changes it, and a function that draws it.

```rhai
const inject = ["surfaces"];

fn apply(ctx) {
    surface(#{
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
}
```

### The model

`init` runs once, when the plugin loads, and its keys are the defaults. A value
that is already in the surface's state wins over its default, so `init` says
what a key means and not what it is. Nothing has to write `if "open" in s`.

The model is the surface's own, under the plugin's state. A listener, a tool or a
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
| `mouse` | `button`, `row`, `column`, `target`, `host` |
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

A mouse message also carries `host`, which is `"terminal"` or `"gui"`. A row and
a column are cells of a terminal, so the graphical interface has no true value
for them and sends zero; `target` is what it knows. Read `host` before you read
`row` and `column`, and keep it in your model if your `view` must draw
differently in each — a `view` gets no message and cannot ask.

In the terminal UI, `F6` gives focus to an open panel. `Esc` returns focus to
the input box. Clicking a panel also focuses it. While a panel has focus, its
`update` receives the keys, mouse messages and pastes. `F6`, `Esc` and `Ctrl-C`
stay with the app and are not sent to a plugin.

Render and event callbacks run on the plugin thread, which is not the thread
that draws the screen. Keep them short: a slow one delays the other plugins, but
it does not hold the terminal.

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
plugins still load, and so does the rest of the session.

A plugin whose `apply` raises goes to `failed` and stays down. Its own
registrations are taken back off — whatever it managed to put in place before it
raised does not survive it. `/plugins` shows the state and the reason.

A plugin that is waiting on a service nobody provides is **not** failed. It is
`pending`, which is a legitimate state and therefore a silent one; `/plugins`
names the key it is short of.

If a listener fails while it runs, aphid shows the error and continues without
that listener. Two are different:

- `agent/tool-call` stops the tool.
- `code/permission` refuses the permission.

These two are the ones people write to be safe. A guard that failed did not
agree to anything, and thus aphid does not continue as if it did.

A tool that fails becomes an error result. The model reads it and can correct
itself.

## Limits

Each call into a plugin can do 5 000 000 operations. Strings can be 8 MB. Arrays and maps can
hold 100 000 items. A call that goes past a limit stops with an error.

A plugin can change these limits for itself, with a `const` at the top of the
file. `0` removes the limit:

```rhai
const max_operations = 0;          // operations in one call
const max_string_size = 67108864;  // bytes in one string
const max_array_size = 0;          // items in one array
const max_map_size = 0;            // items in one map
```

Use this only when the plugin keeps a large memory. When there is a limit on the
size of an array or a map, aphid measures it again at each change, and a large
array then becomes slow. A limit of `0` does not have this cost.

## Command-line options

| Option | Result |
| --- | --- |
| `--list-plugins` | Shows the plugins that would load, and stops |
| `--no-plugins` | Loads no plugin from `.aphid/plugins` |
| `--plugin PATH` | Loads one plugin from a path. No trust question |
| `--trust-plugins` | Agrees to the plugins of this workspace |

In the terminal user interface, `/plugins` shows what loaded, what state each
one is in, the commands they added, and the files that did not load. `/reload`
brings the set back in step with the files on disk.

## Plugins in Rust

A program that embeds aphid supplies a plugin as a `Component`. It is the same
model a `.rhai` file follows, in Rust: declare, subscribe in `apply`, and
everything registered is taken back when it unloads.

```rust
use std::sync::Arc;

use aphid_agent::rt::{Component, Composition, Context};
use aphid_agent::{Blocked, ToolRequest};

struct NoCityName {
    composition: Composition,
}

impl Component for NoCityName {
    fn name(&self) -> &str {
        "no-lisbon"
    }

    fn apply(&self, ctx: &Context) -> Result<(), String> {
        self.composition.bus.on::<ToolRequest>(ctx.uid(), |request| {
            if request.arguments.contains("CityName") {
                request.refuse(Blocked::new("CityName is off limits."));
            }
        });
        Ok(())
    }
}
```

A component declares what it needs with `inject`, offers services with
`provide`, and contributes tools through `Composition::tools`. Mount it with
`Composition::plug`, and hand the composition to the agent with
`AgentBuilder::compose` — components that mounted first are already subscribed
when the loop starts announcing.

Listeners are **synchronous** and their payloads own their data, so one may be
kept, moved to another thread, or answered from a task. The exception is the
per-token stream, which hands out a borrow into the response arena: copying it
out would undo the memory layout that [Core](../core.md) describes, so it has a
list of its own. Anything that must await belongs in a tool, which is the one
asynchronous part of this surface.

Use `cargo doc -p aphid-agent --open` for the full API, and
[Composition](composition.md) for the model.

## Examples

The `crates/aphid-code/examples/plugins` directory holds plugins that work:

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
| `herdr.rhai` | Reports the session to [Herdr](#herdr), so its sidebar shows the pane as working, blocked or idle |
| `optchat.rhai` | One chat that does not end, and that the agent remembers. Refer to [OptChat](#optchat) |

## Herdr

[Herdr](https://herdr.dev) is a terminal multiplexer for coding agents. It
reads the state of the agents it knows and shows it in a sidebar. Aphid is not
one of them, so `crates/aphid-code/examples/plugins/herdr.rhai` reports the
state itself, with `herdr pane report-agent`. Refer to
[Herdr integrations](https://herdr.dev/docs/integrations/) for the contract this
follows.

Copy the file to `~/.aphid/plugins/herdr.rhai`, or to
`.aphid/plugins/herdr.rhai` in the workspace you use. The plugin reports
nothing when aphid does not run inside herdr.

| State | When |
| --- | --- |
| idle | The session is open, and a run has finished |
| working | A run is in progress. The line under it names the tool that is running |
| blocked | A tool needs permission. The line under it is the question |

A plugin in your home directory also reports from a headless session. The
report is taken back when the session ends, and a herdr row for a session that
has ended does not stay behind.

Settings go in `.aphid/plugins/herdr.json`, or in your home directory.

| Setting | Default | Result |
| --- | --- | --- |
| `enabled` | `true` | `false` makes the plugin do nothing |
| `pane` | the pane herdr started | Report about another pane |
| `bin` | `HERDR_BIN_PATH`, then `herdr` | Use a different herdr binary |
| `source` | `custom:aphid` | The authority the report is filed under. Keep it stable: a report is taken back by the source that made it |
| `agent` | `aphid` | The label in the sidebar |
| `message` | `true` | Send the line under the state |
| `tools` | `true` | Name the tool while working |
| `heartbeat` | `60` | Seconds between repeats of the last report. This covers a herdr server that restarts and forgets. `0` switches it off |
| `notify` | `true` | Show a herdr notice when a run ends |
| `verbose` | `false` | Log every report and every failure |

One report costs one `herdr` call, and the plugin makes a call only when the
state or the line under it changes. A run of twenty tool calls is a handful of
calls, not twenty.

A report is display only. It does not make the pane an agent that other panes
can prompt: `herdr agent prompt aphid` does not find it.

## OptChat

`crates/aphid-code/examples/plugins/optchat.rhai` makes one chat that does not
end. The agent remembers all of it, and the size of what it reads stays the
same. The design is [OptChat](https://gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449),
by Victor Taelin.

- Aphid keeps each message — your prompts, the answers, the tool calls and
  their results — in a log, and never changes it.
- In the background, a cheap model writes a summary of one line for each
  message. Then it merges two lines into one line, and two of those into one,
  and so on. This makes a tree of summaries.
- Each prompt starts the model with no history. The model gets a "view" of all
  the chat: recent messages one line each, older messages more for each line.
  Then it gets your prompt.
- When a line does not tell enough, the model calls `zoom` to open the line
  into the two lines below it, down to the full message. `date` gives the time
  of a message.

Copy the file to `~/.aphid/plugins/optchat.rhai`.

| Command | Result |
| --- | --- |
| `/optchat on` | Starts a new session in OptChat mode |
| `/optchat off` | Stops OptChat mode, and starts a new session |
| `/optchat status` | Shows the size of the log, the tree and the view, and what the compactor used |
| `/optchat browse` | Writes the view, the log and the tree to `browse.html`, and opens it |
| `/optchat` | Turns the mode on, or off |

The mode is only for the session that `/optchat on` started. `/new`,
`/sessions` and `--resume` stop it. The session file keeps the messages of the
session as usual. Only the request to the model is different.

A prompt waits until each line of the view is a summary. This takes some
seconds after a long answer. Push `Esc` to stop the wait: your prompt stays in
the log without an answer.

The chat is in `~/.aphid/optchat`: `main/` holds the log and `tree/` holds the
summaries, one file for each day. `view.json` makes the start fast, and aphid
can make it again. Only one aphid at a time can use the chat.

Settings go in `optchat.json`, in `.aphid/plugins` or `~/.aphid/plugins`:

| Setting | Default | Result |
| --- | --- | --- |
| `dir` | `~/.aphid/optchat` | Where the chat is |
| `compactor_model` | the cheapest model of the provider of the session | The model that writes the summaries |
| `compactor_thinking` | `"medium"` | The thinking level of the compactor. Aphid sets it to `"off"` if the model refuses it |
| `node` | `512` | The size of a line of the tree, in bytes |
| `view` | `128000` | The size of the view, in bytes |
| `jobs` | `8` | The most compactor requests at the same time |
| `tries` | `5` | How many times the compactor tries to make a line short enough |
| `retry_ms` | `10000` | How long to wait before a failed line is tried again |
| `cap` | `30000` | The most characters of a tool result. Aphid keeps the start and the end |
| `browser` | `"xdg-open"` | The command that opens `browse.html` |

Differences from the specification:

- A message you send while the agent works becomes a new prompt. It does not
  go to the agent between two tool calls.
- Aphid does not send cache breakpoints. A provider that caches the start of a
  request, such as DeepSeek, still reads most of the view from its cache.
- There are no subagents.

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
starts it with `exec`. Python 3 must be on the machine. A `code/tick` listener
reads what the browser sends, and the other listeners send the answer of the
model back. The workspace stays clean, because the plugin writes nothing in it.

Settings go in `.aphid/plugins/webchat.json`:

```json
{ "host": "0.0.0.0", "port": 8000 }
```

`host` is `0.0.0.0`, and thus another machine on the same network can open the
chat. Use `127.0.0.1` to keep the chat on this machine only.
