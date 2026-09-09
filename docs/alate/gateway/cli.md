# CLI

`aphid alate attach` opens a terminal on an alate that runs. It is a client of
the [gateway](../gateway.md), in the same manner as the Telegram bot.

An alate is two processes. One runs the agent. The other is a terminal that
looks at it.

```
aphid alate run    [--name NAME]    run the alate in this terminal
aphid alate attach [--name NAME]    open a terminal on a running alate
aphid alate gui    [--name NAME]    open a window on a running alate
aphid alate list                    show the alates on this machine
```

`--name` selects the instance. The default name is `default`.

## Start and attach

Start one in the first terminal:

```console
$ aphid alate run --name work
aphid: work is awake in /home/you/.aphid/alate/work
aphid: attach with `aphid alate attach --name work`
```

Attach in a second terminal:

```console
$ aphid alate attach --name work
```

Attaching gives you a conversation of your own. Type to speak to the agent.
Press `Esc` to stop the run in it. Press `Ctrl-C`, or type `/quit`, to detach.
The alate continues to run.

Two terminals can attach at the same time. Each gets its own conversation, and
`/session` moves either of them to a different one. [The window](gui.md) is a
third, and works the same way.

`aphid alate list` shows an attached window as `gui`, where a terminal shows as
`attached`.

`aphid alate run` holds the terminal. To put it in the background, use the tools
of your system — `nohup`, `systemd`, or a terminal multiplexer. The agent does
not do this for you.

There is one exception, and it is [the window](gui.md#waking-an-alate-from-the-window):
opened on an alate that is asleep, it offers to start one. The window is already
a program with a long life, so adopting a daemon costs it nothing.

Stop an alate with `Ctrl-C` in the terminal that runs it, or send it `SIGTERM`.

## What is on this machine

```console
$ aphid alate list
work                 awake
notes                asleep
```

`awake` means that a daemon answers on the socket of that instance. `list`
connects and closes, and thus it leaves no conversation behind it.

## The commands

| Command | Effect |
| --- | --- |
| `/sessions` | Open the list of conversations and pick one. |
| `/session <id>` | Look at one of them. A shortened id is enough. |
| `/new` | Start another conversation in this terminal. |
| `/log` | Show or hide notices, heartbeats and jobs. |
| `/clear` | Clear the screen. The memory does not change. |
| `/help` | Print this list. |
| `/quit` | Detach. The alate continues to run. `exit` and `detach` do the same. |

| Key | Effect |
| --- | --- |
| `Esc` | Stop the run in this session. |
| `Ctrl-C` | Detach. |

In the `/sessions` list the keys are different:

| Key | Effect |
| --- | --- |
| Any character | Add it to the filter. |
| `Backspace` | Remove the last character of the filter. |
| `↑` `↓` | Move the cursor. `Ctrl-P` and `Ctrl-N` do the same. |
| `Enter` | Look at the conversation under the cursor. |
| `Esc` | Close the list. Nothing changes. `Ctrl-C` does the same. |

Each other line goes to the agent.

There is no model selector here. The model is a property of the alate, and not
of a terminal. Set `model` in [`alate.json`](../../alate.md#alatejson).

## Moving between sessions

`/sessions` opens a list of the conversations that run now and the ones on
disk. Type to cut the list down: the filter reads the id, the kind and the date,
and the characters do not have to be next to each other. `telegram` finds the
chats, `cron` finds the jobs, and the first digits of a date find that day.

```
┌ sessions — type to filter, ↑↓ to move, Enter to open, Esc to close ┐
│ > cron                                                             │
│ ▸ 20260811T143000-0000  cron: news  2026-08-11 14:30  running      │
│   20260810T090000-0000  cron: news  2026-08-10 09:00               │
└────────────────────────────────────────────────────────────────────┘
```

The conversations that run now are first, and a `*` marks the one this terminal
is looking at. `Enter` looks at the one under the cursor.

The list names the conversations that run now, and the 20 most recent of the
ones on disk. An older one is still there: `/session <id>` opens it, because the
daemon looks for the id among every session there has ever been.

`/session <id>` looks at one. The daemon reads the transcript and sends it back,
so a session that ended last week draws exactly like one running now. Only the
terminal changes; the agent does not know that it is being watched.

[Alate](../../alate.md#sessions) describes the three kinds of session and what
each of them shares.
