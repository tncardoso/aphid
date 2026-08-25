# Composition

A plugin does not decide when it runs. It says what it needs, and the runtime
decides.

That is the whole of the model, and it costs one paragraph to learn and one
afternoon to stop fighting. This page is that afternoon.

## The two things a component gets

**It waits.** A plugin that declares `inject = ["shell"]` does not run until
something provides `shell`. If nothing ever does, it never runs — and it is not
an error, it is *waiting*. If the provider goes away later, the plugin unloads
again, and comes back when the provider does.

**It is undone.** Everything a plugin registers — a tool, a listener, a service,
a command, a panel — leaves when the plugin does. Not because the author
remembered to remove it, but because registering it produced its own removal.

Those two are the same idea from two sides: you can add a component to a running
system, and you can take it back out.

## The states

| State | What it means |
| --- | --- |
| `pending` | A service it declared has never been available. |
| `loading` | `apply` is running. |
| `active` | Loaded, and everything it registered is in place. |
| `unloading` | Coming down. It has already stopped providing. |
| `failed` | `apply` raised, or its configuration was refused. |
| `inactive` | Was loaded, is not now. |

`/plugins` shows these, and shows which key a waiting plugin is short of. That
line is the answer to almost every *"why is my plugin doing nothing?"*, because
`pending` is a legitimate state and therefore a silent one.

## Declaring

Three constants, read out of your file **before any of it runs** — which is
necessary, because the body must not run until `inject` is satisfied.

```rhai
const inject   = ["shell"];           // wait for these
const provides = ["todos"];           // offer these
const emits    = ["todos/changed"];   // announce these
```

A plugin that declares nothing is trivially satisfied and loads immediately. So
"a plugin with no dependencies behaves as it always did" is not a compatibility
case; it falls out of the model.

## `apply`

Everything a plugin contributes happens in `apply`, and only there. That is not
a style rule: `apply` is the one call the runtime can attach your registrations
to, so that unloading you can take them back. A `provide` from inside a listener
has no owner, and is refused with that sentence.

```rhai
fn apply(ctx) {
    on("agent/turn-start", |cx| {
        cx.note("Today is a Tuesday.");
    });

    provide("todos", #{
        list: || state().items,
        add:  |text| { let s = state(); s.items.push(text); save_state(s); },
    });

    effect(
        || { log("acquired something"); },
        || { log("and released it"); },
    );
}
```

### `on(event, |…| { … })`

Subscribe. See [Events](#events) for the names and what each hands you.

Subscribing is a decision your plugin makes, so it can be conditional:

```rhai
fn apply(ctx) {
    if config().verbose == true {
        on("agent/event", |event| { log(event.kind); });
    }
}
```

A name nothing announces is reported when you subscribe, rather than never
firing.

### `tool(#{ … })`, `command(#{ … })`, `surface(#{ … })`

Contribute something. Declare the matching service in `inject` first:

```rhai
const inject = ["tools", "commands", "surfaces"];

fn apply(ctx) {
    tool(#{ name: "wordcount", description: "…", parameters: #{ … }, execute: |args| { … } });
    command(#{ name: "review", description: "…", run: |args| { … } });
    surface(#{ name: "todos", placement: #{ kind: "side", side: "right" }, view: |s| { … } });
}
```

These are contributions, not declarations, and the difference is the whole
point: what you contribute is offered while your plugin is loaded and taken back
when it is not. A plugin waiting on a service it never gets has no `/command`
listed, because it never ran to offer one.

It also means you can decide:

```rhai
fn apply(ctx) {
    if config().experimental == true {
        command(#{ name: "wip", description: "…", run: |args| { … } });
    }
}
```

### `provide(name, #{ … })`

Offer a service: a map of names to functions. Another plugin reaches it with
`invoke`, and one that declared it in `inject` is guaranteed it exists.

### `invoke(service, method, [args])`

Call a service. Not `call` — Rhai already has one on function pointers, and a
second would shadow it.

A service nothing provides raises, which is why you usually declare it in
`inject` instead: then you never run at all until it is there.

### `effect(setup, teardown)`

For anything the runtime does not already track — a timer, a connection, a file
you wrote. `setup` runs now; `teardown` runs when your plugin unloads.

You never call `teardown` yourself.

## Events

| Name | Handed | Can change |
| --- | --- | --- |
| `agent/prompt` | `draft` | the text, or `reject("why")` |
| `agent/run-start` | `cx` | notes on `cx` |
| `agent/turn-start` | `cx` | notes on `cx` |
| `agent/message` | `cx`, `message` | notes on `cx` |
| `agent/tool-call` | `tool` | `block("why")`, or `#{ arguments: … }` |
| `agent/tool-progress` | `id`, `tool`, `chunk` | nothing |
| `agent/tool-result` | `result` | `#{ content: … }` |
| `agent/turn-end` | `cx`, `turn` | `stop()` |
| `agent/run-end` | `cx`, `outcome` | nothing |
| `agent/event` | `event` | nothing — one call per token |

And these from the coding harness, which are this crate's ideas rather than the
loop's — announced on the same bus, subscribed to the same way:

| Name | Mode | What it is |
| --- | --- | --- |
| `code/system-prompt` | waterfall | The prompt, before anything sees it |
| `code/session-start` | emit | A session opened |
| `code/session-end` | emit | A session is closing |
| `code/permission` | bail | A tool needs permission |
| `code/file-change` | emit | `write` or `edit` changed a file |
| `code/notice` | emit | Something was shown to the user |
| `code/tick` | emit | Time passed |

Every listener runs, even after one has refused a tool call: an observer still
wants to see a call somebody else blocked. The first refusal is the one that
stands.

`agent/event` fires once per token. Subscribing to it is a choice with a cost,
and `/plugins` says who made it.

## Services

A service is a capability one plugin offers and others consume **by name**, so a
composition can choose an implementation without the consumers knowing.

The harness offers three of its own, and they are ordinary services — the same
`inject`, the same waiting, the same isolation:

| Service | What it holds |
| --- | --- |
| `tools` | What the model may call |
| `commands` | What a person may type |
| `surfaces` | What a person may look at |

So `ctx.isolate("commands")` gives a subtree its own command set, and a
component that offers a tool waits for `tools` the same way it would wait for
anything else.

```rhai
// todos.rhai
const provides = ["todos"];

fn apply(ctx) {
    provide("todos", #{ add: |text| { … } });
}
```

```rhai
// nag.rhai
const inject = ["todos"];

fn apply(ctx) {
    on("agent/run-end", |cx, outcome| {
        invoke("todos", "add", ["review what just happened"]);
    });
}
```

Neither file mentions the other, and the order they load in does not matter.

Service names live in one flat namespace. Prefix your own.

## Two plugins that need each other

They cannot both load: neither's requirement can ever be true. That is
predictable from the declarations alone, so it is **reported when they mount**
rather than left as two plugins quietly doing nothing.

The fix is almost always to split the shared thing out. Two components that each
want something from the other are usually three: two that offer, and one that
joins them.

## The composition file

`.aphid/plugins.json` is where you override what discovery found. It does not
replace it — dropping a `.rhai` into `.aphid/plugins` still loads it, and this
file is for saying something different about one of them.

```json
[
  { "id": "webchat", "disabled": true },
  { "id": "budget",  "config": { "max_tool_calls": 100 } },
  { "id": "sandbox", "isolate": { "shell": true } },
  { "id": "shared",  "url": "/opt/team-plugins/shared.rhai" }
]
```

| Field | What it does |
| --- | --- |
| `id` | Which plugin. The name discovery gave the file. |
| `disabled` | Keep it, do not run it. |
| `config` | What `config()` returns. |
| `url` | Load it from somewhere else. Also how you name a plugin outside `.aphid/plugins`. |
| `isolate` | `true` for a private realm, a string for one shared by name. See below. |

`/reload` brings the composition back in step with the files: a new one loads, a
deleted one unloads, an edited one reloads. `/reload <name>` forces one down and
back up even if nothing changed — which is the case you are in while you are
writing it.

## Isolation

Two plugins can each have their own `shell`, or their own `sink`, without either
knowing.

A key does not name a binding directly. It names a **realm**, and the realm
names the binding. `"isolate": { "shell": true }` gives that entry a realm of its
own, so what it provides under `shell` nobody else sees, and what it injects
comes from its own scope. `"isolate": { "shell": "sandbox" }` gives it a realm
shared with every entry naming `sandbox`.

Moving an entry between realms does not rebuild it. What its own scope provided
travels with it; what it merely shared stays behind.

## What cannot be undone

Unloading reverses what this process controls. That line runs through the middle
of most interesting operations, and it is worth knowing where:

- **Reversible.** A tool, a command, a surface, a listener, a service, a child
  plugin, a process started with `exec` — the runtime holds the record, and
  dropping the record really is the inverse.
- **Not.** Bytes already written to a socket. A request `http_post` already
  sent. A line already in the transcript, which only ever grows.

For those, a teardown can only **compensate**: delete the file it created, kill
the process it started, post the correction. That composes the same way, and the
runtime treats it the same. What it cannot do is promise the two were
equivalent.

`webchat.rhai` is the plugin that lives on this line, which is why it is the one
worth reading: it starts a server, and `/reload` has to stop it and free the
port.
