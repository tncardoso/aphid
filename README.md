# Aphid

A fast and hackable agent harness.

Aphid is a coding agent built in Rust around a data-oriented core: a conversation
lives in flat, append-only arenas, streaming deltas are resolved in a single
memcpy, and every stage — request, stream, tool call, permission prompt — is a
plugin hook you can observe, block, or rewrite.

## Highlights

- **Zero memory copy where it matters.** A talk-turn is staged in a message
  buffer's arenas and committed into the transcript with one memcpy per arena,
  however many tokens streamed. Layout guarantees are enforced at compile time.
- **Data-oriented design.** Spans, not owned strings. The whole session is a
  handful of allocations freed together when the transcript drops.
- **Fast startup.** The CLI is thin; discovery resolves the workspace, its
  `AGENTS.md` instructions, and its skills up front so the agent starts without
  surprises.
- **Fully debuggable.** `aphid raw` prints every protocol event as it fires, and
  `aphid raw --request` dumps the encoded request body.
- **Extensible via plugins.** Everything interesting is interceptable through a
  synchronous hook API, in Rhai or in Rust.

## Install

```sh
cargo install --path crates/aphid-cli
export DEEPSEEK_API_KEY=sk-...
```

```sh
$ aphid                              # the terminal UI
$ aphid -p "what does this crate do?"  # one prompt, printed
```

[docs/getting-started.md](docs/getting-started.md) covers the rest: adding a
model from another provider, project instructions, and a first resident agent.

## The five front ends

```
aphid [OPTIONS]                 open the terminal UI
aphid [OPTIONS] -p <prompt>     run one prompt and print the result
aphid alate <command>           run a resident agent, or attach a terminal to one
aphid raw   [OPTIONS] <prompt>  stream a single completion, printing protocol events
aphid agent [OPTIONS] <prompt>  run the plain agent loop with a demo tool
aphid model <command>           manage the models in ~/.aphid/models.json
```

## Documentation

The book is in [`docs/`](docs/), and `mdbook serve` renders it.

| Chapter | What is in it |
| --- | --- |
| [Introduction](docs/introduction.md) | What aphid is, and how the crates fit together. |
| [Getting started](docs/getting-started.md) | Build, key, first run, first alate. |
| [Core](docs/core.md) | The transcript, the wire protocol, thinking levels, the model catalog. |
| [Aphid](docs/aphid.md) | The coding harness, and every command-line option. |
| [Commands](docs/aphid/commands.md) · [Skills](docs/aphid/skills.md) · [Plugins](docs/aphid/plugins.md) | The three things you extend. |
| [Alate](docs/alate.md) | The resident agent: home, memory, heartbeat, cron. |
| [Gateway](docs/alate/gateway.md) | The socket, and the clients that speak it. |
| [Colony](docs/colony.md) | The hub agents talk to each other in. |

## Design

The workspace splits into eight crates, a narrow step at each one:

- **`aphid-core`** — message, model and streaming types. The transcript arena
  layout, spans, thinking levels, the OpenAI-completions encoder, and the SSE
  transport.
- **`aphid-agent`** — the agent loop. `Agent::prompt` runs *request → stream →
  commit → execute tools* until the model stops asking for tools, plus the tool
  registry and the plugin API. Deliberately unopinionated.
- **`aphid-plugin`** — the Rhai host. Plugin discovery, the script engine and
  its capabilities, and the trust gate. Keeps the scripting runtime out of the
  loop crate entirely.
- **`aphid-code`** — the specialization. The tools a coding agent needs,
  system-prompt assembly from the project's conventions, skill discovery,
  on-disk sessions, permission plugins, and the TUI.
- **`aphid-alate`** — the resident agent. A home directory, a memory of facts,
  a heartbeat, and the socket clients attach to. Builds its agent with
  `aphid-code`'s harness unchanged.
- **`aphid-nostr`** — NIP-01 and NIP-29, with no socket and no clock in it. The
  filter rules a relay and a client must agree about, and the group state
  machine, both testable without opening a port.
- **`aphid-colony`** — the hub. A nostr relay with a SQLite store behind it, a
  client both the terminal and the alate bridge use, and a Slack-like TUI.
- **`aphid-cli`** — the thin `aphid` binary, wiring the six front ends
  together.

This repository ships one plugin of its own: `.aphid/plugins/webchat.rhai`. Type
`/server start` and it puts a chat page on port 8000, so you can talk to the
running session from a browser, on this machine or on your phone.

## Building and testing

```sh
cargo build
cargo test
cargo clippy
cargo fmt
```

Two optional features. `telegram` adds the Telegram bot to `aphid alate` and an
HTTP client to the build. `colony` lets an alate speak in a colony, and adds a
websocket client and a signature library.

```sh
cargo build --features telegram
cargo test -p aphid-alate --features telegram

cargo build --features colony
cargo test -p aphid-alate --features colony
```

`aphid colony` itself is always built. The feature is only about putting an
alate in one.

`aphid raw` and `aphid agent` are fully scriptable — their test suites run the
entire encode→stream→commit path against a mock provider with no network.

## License

Licensed under the MIT License — see [`LICENSE`](LICENSE) for the full text.
