# Aphid

A fast and hackable agent harness.

Aphid is a coding agent built in Rust around a data-oriented core: a conversation
lives in flat, append-only arenas, streaming deltas are resolved in a single
memcpy, and every stage — request, stream, tool call, permission prompt — is a
plugin hook you can observe, block, or rewrite.

## Highlights

- **Zero memory copy where it matters.** A talk-turn is staged in a
  [`MessageBuffer`]'s arenas and committed into the [`Transcript`] with one
  memcpy per arena, however many tokens streamed. Layout guarantees are enforced
  at compile time.
- **Data-oriented design.** Spans, not owned strings. The whole session is a
  handful of allocations freed together when the transcript drops, and a
  `Transcript` is a single owned, `Send` value.
- **Fast startup.** The CLI is thin; discovery resolves the workspace, its
  `AGENTS.md` instructions, and its skills up front so the agent starts without
  surprises.
- **Fully debuggable.** `aphid raw` prints every protocol event as it fires, and
  `aphid agent --request` dumps the encoded request body.
- **Extensible via plugins.** Everything interesting is interceptable through a
  synchronous hook API — see [Plugins](#plugins).

## What it looks like

```
$ aphid
```

opens the terminal UI. A prompt runs one time and prints the result:

```
$ aphid -p "what does this crate do?"
$ aphid "what does this crate do?"
```

## The four front ends

```
aphid [OPTIONS]                 open the terminal UI
aphid [OPTIONS] -p <prompt>     run one prompt and print the result
aphid raw   [OPTIONS] <prompt>  stream a single completion, printing protocol events
aphid agent [OPTIONS] <prompt>  run the plain agent loop with a demo tool
aphid model <command>           manage the models in ~/.aphid/models.json
```

[docs/cli.md](docs/cli.md) gives each option and each file.

### Coding agent (`aphid`)

The default. Interactive TUI or one-shot `-p`, sessions that survive a restart,
a permissions gate, and a prompt assembled from the project's own conventions.

```
OPTIONS:
    -p, --print <prompt>  run headless: stream to stdout and exit
    --model <name>        model id, or a unique part of one
    --models              list the known models and exit
    --think <level>       off | minimal | low | medium | high | xhigh | max
    --system <text>       replace the built-in instructions
    --append-system <t>   add to the instructions
    --resume [id]         continue the newest session here, or one named by id
    --sessions            list saved sessions for this workspace and exit
    --confirm             ask before running anything that changes the workspace
    --no-context          skip AGENTS.md and skills
    --max-turns <n>       stop a run after this many provider requests
    --quiet               headless: drop the line-by-line output of running tools
    -h, --help            show this help
```

### Models (`aphid model`)

The catalog is the models aphid supplies, and then your own
`~/.aphid/models.json`. A model in the file with the same id as a built-in
replaces it, so aphid works with no configuration at all.

```
aphid model add <provider/model>   add a model, described by models.dev
aphid model remove <name>          remove one of your models
aphid model list [--all]           list your models, or the whole catalog
aphid model search <query>         find a model on models.dev
aphid model update                 refresh the cached models.dev document
```

`add` reads [models.dev](https://models.dev) rather than making you write out a
context window and a price by hand, and records which environment variable holds
that provider's key — so a model you add authenticates against its own provider,
not against DeepSeek. The document is cached at `~/.aphid/models.dev.json` and
reused for a day, so only the first command in a while pays for the download.

```
$ aphid model add zhipuai/glm-5
added glm-5 in /home/you/.aphid/models.json
  provider  zhipuai
  endpoint  https://open.bigmodel.cn/api/paas/v4
  limits    204800 context · 131072 output
  price     $1.00 in · $3.20 out per M tokens
  key       $ZHIPU_API_KEY

$ aphid --model glm-5 -p "what does this crate do?"
```

The file is meant to be edited. Everything models.dev cannot know — which
request fields an endpoint actually rejects — is a named compatibility profile
plus the flags that differ from it, so a correction is usually one line.

### Protocol debugging (`aphid raw` and `aphid agent`)

Tiny tools that exercise the whole path — encode a request, stream it, resolve
each delta span, commit the turn — and print the events rather than the text.

```
OPTIONS:
    --pro                 use deepseek-v4-pro (default: deepseek-v4-flash)
    --system <text>       prepend a system message
    --think <level>       minimal | low | medium | high | xhigh | max
    --max-tokens <n>      cap the response length
    --temperature <f>     sampling temperature
    --tool                offer a demo `get_weather` tool, to see tool-call deltas
    --events              print every Delta event with its span, instead of the text
    --request             print the encoded request body and exit (single-shot only)
    -h, --help            show this help
```

## Environment

- `DEEPSEEK_API_KEY` — required by the models aphid ships, and by `raw` and
  `agent`. The `--request` flag can inspect an encoded request without one.
- Models added from models.dev name their own key variable, and the coding agent
  reads the one belonging to the model you selected.
- `APHID_HOME` — replaces `~/.aphid`, for a sandboxed configuration.

## Design

The workspace splits into five crates, a narrow step at each one:

- **[`aphid-core`]** — message, model and streaming types. The `Transcript`
  arena layout, spans, thinking levels, the OpenAI-completions encoder, and the
  SSE transport.
- **[`aphid-agent`]** — the agent loop. `Agent::prompt` runs *request → stream →
  commit → execute tools* until the model stops asking for tools, plus the tool
  registry and the plugin API. Deliberately unopinionated.
- **[`aphid-plugin`]** — the Rhai host. Plugin discovery, the script engine and
  its capabilities, and the trust gate. Keeps the scripting runtime out of the
  loop crate entirely.
- **[`aphid-code`]** — the specialization. The tools a coding agent needs,
  system-prompt assembly from the project's conventions, skill discovery,
  on-disk sessions, permission plugins, and the TUI.
- **[`aphid-cli`]** — the thin `aphid` binary, wiring the four front ends
  together.

### The memory model

A conversation lives in a `Transcript`: a flat list of messages over two
append-only arenas, one for text and one for binary payloads. Content blocks
hold byte ranges rather than owned strings. Streaming appends each delta to the
arena tail exactly once, and an [`Event`] carries only the [`Span`] of the bytes
just written; `Transcript::commit` then moves the finished turn across in one
memcpy per arena.

The system prompt is not special — it is a message with `Role::System`, mapped
to the wire format by a provider encoder.

## Plugins

Plugins contribute tools, add context before a request, watch every protocol
event, block or rewrite a tool call, patch a tool result, and stop the run. Hooks
are **synchronous** — the only per-token hook is `Plugin::on_event`, and boxing a
future for each token would undo the point of the arena layout. Anything that
must await belongs in a `ToolHandler`. Plugins declare an `Interest` set, so a
hook nobody wants costs an empty-slice check.

Example — a plugin that vetoes one city:

```rust
use aphid_agent::{Guard, Plugin, PendingCall};

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

### Rhai plugins

You can also write a plugin as one file of [Rhai](https://rhai.rs), with no need
to compile aphid again. Put the file in the workspace or in your home directory:

```text
.aphid/plugins/<name>.rhai
.aphid/plugins/<name>/main.rhai
```

A plugin declares a hook when it declares a function with the correct name.
Aphid reads the names from the compiled script, and subscribes to only those
hooks.

```rhai
//! Keeps the model away from the changelog.

fn on_tool_call(tool) {
    if tool.name == "write" && tool.arguments.contains("CHANGELOG") {
        return block("the changelog is written by hand");
    }
}

fn on_turn_end(cx, turn) {
    if turn.stop_reason == "length" { cx.note("the last answer was cut short"); }
}
```

A plugin can also add a tool with `register_tool` and a command with
`register_command`, keep settings in `<name>.json`, and remember things with
`save_state`.

A plugin in your home directory is yours and always loads. A plugin that comes
with a checkout is different: aphid asks you the first time, and keeps the answer
in `~/.aphid/trust.json`.

This repository has one such plugin: `.aphid/plugins/webchat.rhai`. Type
`/server start` and it puts a chat page on port 8000, so you can talk to the
running session from a browser, on this machine or on your phone.

Read [docs/plugins.md](docs/plugins.md) for the hooks, the return values, the
capabilities and the limits. WebAssembly plugins are still to come.

## Skills

Skills are instruction files the model opens on demand. Only each skill's name,
description and path go into the system prompt; the model reads the body with the
`read` tool when a task matches (progressive disclosure). Layout, searched in the
workspace and then under `~/.aphid`:

```text
.aphid/skills/<name>/SKILL.md
.aphid/skills/<name>.md
```

## Sessions

A session is one JSONL file per conversation, appended to as messages are
committed. Nothing is ever rewritten, so a crash costs at most the turn that was
in flight, and `--resume` is a replay of the file. Headless runs are recorded
too, so `--sessions` and `--resume` see them the same way they see interactive
ones.

## Building and testing

```sh
cargo build
cargo test
cargo clippy
cargo fmt
```

`aphid raw` and `aphid agent` are fully scriptable — their test suites run the
entire encode→stream→commit path against a mock provider with no network.

## License

Licensed under the MIT License — see [`LICENSE`](LICENSE) for the full text.
