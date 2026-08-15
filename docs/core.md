# Core — the AI layer

`aphid-core` is the layer below every front end. It holds the message types, the
model catalogue and the streaming code, and it knows nothing about tools,
plugins or terminals.

This chapter tells you what the layer does and which files you can edit. For the
Rust API, use `cargo doc -p aphid-core --open`.

## The transcript

A conversation is a **transcript**: a flat list of messages over two
append-only arenas, one for text and one for binary data.

A content block holds a **span** — a range of bytes in an arena — and not an
owned string. Thus a full session is a small quantity of allocations, which are
released together when the transcript is released. No lifetime goes out into the
code that uses the crate: a transcript is one owned value that you can move
between threads.

Spans stay inside the crate. Everything is read through views, which resolve a
range against the arena and give back a plain string.

The system prompt is not special. It is a message with the system role. The map
from that to the wire format is the work of an encoder.

### Why it is arranged like this

Streaming is where the layout is of use. A provider collects a reply in a
**message buffer**, which has arenas of its own. Each delta is added to the tail
of an arena one time, and the event that reports it carries only the span of the
bytes that were written. To commit the turn, aphid moves the finished buffer
across with one memory copy for each arena — whatever quantity of tokens
arrived.

The rules of the layout are applied when aphid is compiled: a span is 8 bytes, a
content block is not more than 24, an event is not more than 16, and a message
header is not more than 32. A change that makes one of these larger does not
build.

The transcript only grows. A plugin adds to it, and cannot rewrite it.

## The wire

Aphid speaks the OpenAI chat-completions protocol, and no other. The stream is
server-sent events.

This has one result that you see: a provider that speaks a different protocol
cannot be added to the catalogue. `aphid model add` refuses such a model, and
says so.

Almost every provider says that it is "OpenAI-compatible", and each one is
compatible in a slightly different way. Aphid states these differences as a
**compatibility profile** on the model, and not as a guess made from the address
at the time of the request.

| Profile | Use |
| --- | --- |
| `compatible` | A different company's OpenAI-compatible server. The default. |
| `openai` | OpenAI and Azure. |
| `deepseek` | DeepSeek. |
| `none` | No behaviour table. |

A profile holds the answers to questions that models.dev cannot answer, because
they are about the server and not about the model: which field limits the length
of the answer, whether the endpoint accepts `reasoning_effort`, whether a tool
result must repeat the name of the tool, whether a user message can come
directly after a tool result, and approximately twelve more.

A model gives the name of a profile, and then each behaviour that is different
from that profile. Thus a correction is usually one line. Refer to
[The catalogue](#the-catalogue).

## Thinking levels

Aphid has one ladder of levels for each model that can reason:

```
off  minimal  low  medium  high  xhigh  max
```

`off` is not a level. It removes the reasoning fields from the request.

Each model supplies a different set of levels. If you ask for a level that the
model does not supply, aphid decreases it to the nearest level that the model
does supply, and prints a note. If the model cannot reason at all, aphid ignores
the level and prints a note.

The coding agent starts at `medium`. `--think` and the `/think` command change
it, and `thinking` in `alate.json` sets it for a resident agent.

## The catalogue

The coding agent catalogue is the models in `~/.aphid/models.json`. It ships no
default models, so a fresh install has an empty catalogue until you add one.

`aphid models add` writes this file for you, from the description on
[models.dev](https://models.dev). Refer to [`model`](aphid.md#model) for the
commands. When the coding agent starts with no models, it prints how to add
one and exits.

The `raw` and `agent` front ends still use the built-in DeepSeek models. They
do not read `~/.aphid/models.json`.

### models.dev

Aphid keeps a copy of the models.dev document in `~/.aphid/models.dev.json`, and
it uses the copy while the copy is less than 24 hours old. `aphid model update`
gets the document again.

If aphid cannot get the document, and a local copy exists, aphid uses the local
copy and tells you that the data is old. An old price is more useful than an
error.

### The file

`~/.aphid/models.json` is a file that you can edit. Each model looks like this:

```json
{
  "version": 1,
  "models": [
    {
      "id": "glm-5",
      "name": "GLM-5",
      "provider": "zhipuai",
      "api": "openai-completions",
      "base_url": "https://open.bigmodel.cn/api/paas/v4",
      "api_key_env": "ZHIPU_API_KEY",
      "reasoning": true,
      "input": ["text"],
      "context_window": 204800,
      "max_tokens": 131072,
      "cost": { "input": 1.0, "output": 3.2, "cache_read": 0.2, "cache_write": 0.0 },
      "compat": { "profile": "compatible", "supports_reasoning_effort": false }
    }
  ]
}
```

A model needs an `id`, a `base_url`, a `context_window` and a `max_tokens`. All
the other fields have defaults.

In the example above, the endpoint is a usual OpenAI-compatible server, but it
refuses the `reasoning_effort` field. That is the whole of the correction.

`thinking_levels` gives the value to send for each level. A text value is the
value to send. `false` means that the model refuses the level. If a level is not
in the file, aphid sends the name of the level.

```json
"thinking_levels": { "off": "disabled", "minimal": "low", "max": "max", "xhigh": false }
```

If aphid cannot read the file, it prints the problem and continues with an
empty catalogue. The coding agent then exits with the no-models message. A
mistake in this file cannot make aphid use a model you did not configure.

## Looking at the protocol

`aphid raw` and `aphid agent` print what this layer does, in place of the text:

```console
$ aphid raw --request "hello"                # the encoded request body, with no key
$ aphid raw --events --tool "what is the weather in Lisbon?"
```

`--events` prints each delta event with its span, which is the layout of this
chapter made visible. Refer to [`raw` and `agent`](aphid.md#raw-and-agent).
