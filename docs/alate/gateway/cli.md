# CLI

`aphid alate attach` opens a terminal on an alate that runs. It is a client of
the [gateway](../gateway.md), in the same manner as the Telegram bot.

An alate is two processes. One runs the agent. The other is a terminal that
looks at it.

```
aphid alate run    [--name NAME]    run the alate in this terminal
aphid alate attach [--name NAME]    open a terminal on a running alate
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
`/session` moves either of them to a different one.

`aphid alate run` holds the terminal. To put it in the background, use the tools
of your system — `nohup`, `systemd`, or a terminal multiplexer. The agent does
not do this for you.

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
| `/sessions` | Show the conversations, running and stored. |
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

Each other line goes to the agent.

There is no model selector here. The model is a property of the alate, and not
of a terminal. Set `model` in [`alate.json`](../../alate.md#alatejson).

## Moving between sessions

`/sessions` lists the conversations that run now and the ones on disk.

```
  20260811T091500-0000  resident      2026-08-11 09:15  running
* 20260811T142200-0000  attached      2026-08-11 14:22
  20260811T143000-0000  telegram: 42  2026-08-11 14:30
  20260811T090000-0000  cron: news    2026-08-11 09:00
```

`/session <id>` looks at one. The daemon reads the transcript and sends it back,
so a session that ended last week draws exactly like one running now. Only the
terminal changes; the agent does not know that it is being watched.

[Alate](../../alate.md#sessions) describes the three kinds of session and what
each of them shares.
