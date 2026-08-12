# Aphid

A fast and hackable agent harness.

Aphid is a coding agent written in Rust around a data-oriented core. A
conversation lives in flat, append-only arenas. Streaming deltas are resolved
with one memory copy. Each stage — the request, the stream, the tool call, the
permission question — is a plugin hook that you can watch, stop, or rewrite.

This book is written in Simplified Technical English.

## Highlights

- **Almost no memory copies.** A turn is staged in the arenas of a message
  buffer, and committed into the transcript with one copy for each arena,
  whatever quantity of tokens arrived. The layout rules are applied when aphid
  is compiled. See [Core](core.md).
- **Data-oriented design.** Spans, and not owned strings. A full session is a
  small quantity of allocations, released together.
- **A fast start.** The command-line tool is thin. Discovery finds the
  workspace, its `AGENTS.md` instructions and its skills before the agent
  starts.
- **Fully debuggable.** `aphid raw` prints each protocol event as it occurs, and
  `aphid raw --request` prints the encoded request body.
- **Extensible with plugins.** Each interesting point is a synchronous hook. See
  [Plugins](aphid/plugins.md).

## What it looks like

```console
$ aphid
```

This opens the terminal user interface. A prompt runs one time and prints the
result:

```console
$ aphid -p "what does this crate do?"
$ aphid "what does this crate do?"
```

[Getting started](getting-started.md) tells you how to install aphid and how to
give it a key.

## The five front ends

```
aphid [OPTIONS]                 open the terminal user interface
aphid [OPTIONS] -p <prompt>     run one prompt, and print the result
aphid alate <command>           run a resident agent, or attach a terminal to one
aphid raw   [OPTIONS] <prompt>  stream one completion, and print each protocol event
aphid agent [OPTIONS] <prompt>  run the agent loop with a demo tool
aphid model <command>           manage the models in ~/.aphid/models.json
```

The first two are the coding agent, which [Aphid](aphid.md) describes with each
of its options. `alate` is the resident agent, which [Alate](alate.md)
describes. `raw`, `agent` and `model` are also in the
[Aphid](aphid.md) chapter.

## How the code is arranged

The workspace is six crates, and each one is a narrow step above the one before
it:

| Crate | What it holds |
| --- | --- |
| `aphid-core` | The message, model and streaming types. See [Core](core.md). |
| `aphid-agent` | The agent loop, the tool registry and the plugin API. |
| `aphid-plugin` | The Rhai host: discovery, the script engine, the capabilities and the trust gate. |
| `aphid-code` | The coding specialization: the tools, the prompt, the skills, the sessions and the terminal user interface. See [Aphid](aphid.md). |
| `aphid-alate` | The resident agent: a home, a memory, a heartbeat and a gateway. See [Alate](alate.md). |
| `aphid-cli` | The thin `aphid` binary, which connects the five front ends. |

`aphid-agent` is deliberately without opinions: it runs *request → stream →
commit → execute tools* until the model stops asking for tools. Everything that
makes aphid a coding agent is in `aphid-code`, and an alate builds its agent
with that same harness, without a change.

For the Rust API of any crate, use `cargo doc --open`.

## Licence

Aphid is licensed under the MIT Licence. The `LICENSE` file in the repository
gives the full text.
