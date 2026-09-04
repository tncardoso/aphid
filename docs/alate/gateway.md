# Gateway

The gateway is a Unix socket in the home of the alate. The daemon listens on it.
Each terminal that attaches is a client, and so is the
[Telegram](gateway/telegram.md) bot and the [colony](gateway/colony.md) bridge.

The gateway is the only door. Nothing that speaks to an alate has a way in that
is not this socket, which is why a new kind of client — a chat, a browser, a
program of your own — changes nothing in the daemon.

## The protocol

One JSON object for each line, in both directions. You can read it with `nc`,
and you can write another client for it.

Each line that the daemon sends holds a `kind`, and a `session` when the line
belongs to a conversation. A line with no `session` is the daemon speaking for
itself: the greeting, a heartbeat, a session list, a permission question.

A client sends `{"kind":"attach"}` first. The daemon then opens a session for it
and answers with `hello`. A program that only wants to know whether an alate is
awake connects and closes without sending anything, and no conversation is made
for it.

A client can also say what it is: `{"kind":"attach","channel":"telegram: 42"}`.
The name is what `/sessions` shows for that conversation. It is cut to 32
characters, and line ends are removed, because it is printed in a list. The
field can be absent, and a client that does not send it is listed as `attached`.

## What a client sends

| Kind | Fields | Effect |
| --- | --- | --- |
| `attach` | `channel` (optional) | Say that this is a client, and open a session for it. |
| `attach` | `attachments` (optional) | Set this to `true` when the client can receive file attachments from the agent. |
| `prompt` | `text` | Say this to the agent, as if it were typed. |
| `cancel` | | Stop the run in flight. |
| `answer` | `id`, `decision` | Answer a `confirm`. `allow`, `allow_always` or `deny`. |
| `attachment_result` | `id`, `error` (optional) | Confirm an attachment, or report why the gateway could not send it. |
| `watch` | `id` | Look at a different session, and replay it. |
| `sessions` | | Ask what sessions there are. |
| `new` | | Open another session on this connection. |

A request needs no session on it. A connection has one session that it watches,
and each request is about that one. `watch` is what changes it.

## What the daemon sends

| Kind | Fields | Meaning |
| --- | --- | --- |
| `hello` | `instance`, `model`, `context_window`, `thinking` | The first frame. What this alate is. |
| `session_opened` | `info` | A session started. Sent to everybody. |
| `session_closed` | `id` | A session ended, and sends nothing more. |
| `sessions` | `live`, `stored` | The answer to `sessions`, to the connection that asked. |
| `history_start` | `id` | A replay starts. What is drawn for this session is old. |
| `history_end` | `id` | The replay is complete. What comes now is live. |
| `turn_started` | | A turn started. |
| `text` | `text` | Text from the model. |
| `thinking` | `text` | Reasoning from the model. |
| `tool_stream_start` | `block`, `name` | A tool call opened, and its arguments still arrive. |
| `tool_stream_delta` | `block`, `bytes` | More of those arguments arrived. |
| `tool_call` | `id`, `name`, `arguments` | A tool call, complete and committed. |
| `tool_progress` | `id`, `chunk` | Partial output of a tool. |
| `tool_result` | `id`, `name`, `text`, `is_error`, `details` | A tool completed. |
| `attachment` | `id`, `name`, `data`, `caption` | A Base64 file for an attachment-capable client. This goes only to that client. |
| `turn_ended` | `usage`, `stop`, `error` | A turn is complete. |
| `run_ended` | `stop`, `turns`, `error` | The run stopped. |
| `notice` | `text` | Something a plugin wants seen. |
| `prompt` | `text` | A prompt went to the agent. Echoed to everybody in that session. |
| `heartbeat` | `at`, `note` | The alate woke on its own. |
| `confirm` | `id`, `tool`, `summary`, `risk` | A tool waits for permission. The first answer decides. |

A client sees the frames of the session it watches, and the frames of the daemon
itself. Two terminals on two sessions thus do not draw each other's replies.

## Watching a different session

To change what it watches, a client sends `{"kind":"watch","id":"..."}`. The
daemon replays that session between `history_start` and `history_end`, whether
the session runs now or ended long ago.

There is no store of recent frames. What a client missed is in the transcript,
which is what `watch` reads — so what it gets back cannot disagree with what
happened.

Five kinds are not replayed: `confirm`, `hello`, `sessions`, `history_start` and
`history_end`. A question that was answered an hour ago must not open a window
over the new client, and the other four are addressed to one connection and not
to a conversation.

## The log

Each line is also written to `alate.log` in the home. Read the hours when nobody
watched with `jq`:

```console
$ jq -r 'select(.kind == "heartbeat") | .at + "  " + .note' alate.log
$ jq -r 'select(.session == "20260811T090000-0000") | .text // empty' alate.log
```

This file is the frames, and not the log of the program. For the log of the
program, refer to [Logs](../alate.md#logs).

## The socket

The socket permits only its owner to read and write it. Anything that can
connect can make the agent run commands, so the permissions of the file are the
whole of the access control.

The gateway needs a Unix socket, so `aphid alate` does not work on Windows.

A socket file that no daemon is behind is removed and made again. Two daemons
cannot serve one alate: the second one stops and says so.

`gateway.socket` in `alate.json` moves the file. It is `gateway.sock` in the
home when absent.

## The clients

| Client | What it is |
| --- | --- |
| [CLI](gateway/cli.md) | `aphid alate attach`. A terminal on the alate. |
| [Window](gateway/gui.md) | `aphid alate gui`. A window on the alate, on the desktop. |
| [Telegram](gateway/telegram.md) | A bot. Each chat is a conversation. |
| [Colony](gateway/colony.md) | Not written yet. |
