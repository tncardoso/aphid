# The resident agent

This document tells you how to use `aphid alate`. It is written in Simplified
Technical English.

## Contents

- [What an alate is](#what-an-alate-is)
- [Start and attach](#start-and-attach)
- [Sessions](#sessions)
- [The home directory](#the-home-directory)
- [`alate.json`](#alatejson)
- [The memory](#the-memory)
- [The heartbeat](#the-heartbeat)
- [Cron](#cron)
- [The gateway](#the-gateway)
- [Telegram](#telegram)
- [Permissions](#permissions)
- [Plugins and skills](#plugins-and-skills)
- [Files and environment variables](#files-and-environment-variables)

## What an alate is

An alate is the winged form of an aphid. It is the form that leaves the plant
and lives away from it.

The coding agent starts in a repository, does the work you ask for, and forgets
everything when you close the terminal. An alate is different in four ways:

- It has a **home directory** that it owns. The home is also its workspace.
- It has a **memory**. What it learns in one session, it knows in the next.
- It has a **heartbeat**. It wakes on a clock and looks at what it has.
- It has a **crontab**. It can schedule a prompt to run at a time, in a
  conversation of its own.
- It has a **gateway**. You attach a terminal to it, and you detach again. The
  agent continues either way.

The agent itself is the same agent. The tools, the instruction files, the
sessions and the plugins all work as they do in the coding agent.

## Start and attach

An alate is two processes. One runs the agent. The other is a terminal that
looks at it.

```
aphid alate run    [--name NAME]    run the alate in this terminal
aphid alate attach [--name NAME]    open a terminal on a running alate
aphid alate list                    show the alates on this machine
```

`--name` selects the instance. The default name is `default`.

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

### Commands in the attached terminal

| Command | Effect |
| --- | --- |
| `/sessions` | Show the conversations, running and stored. |
| `/session <id>` | Look at one of them. A shortened id is enough. |
| `/new` | Start another conversation in this terminal. |
| `/log` | Show or hide notices, heartbeats and jobs. |
| `/clear` | Clear the screen. The memory does not change. |
| `/help` | Print this list. |
| `/quit` | Detach. The alate continues to run. |

Each other line goes to the agent. The model is a property of the alate, so
there is no model selector here. Set the model in `alate.json`.

## Sessions

An alate has more than one conversation at a time. Each is a **session**: one
context, one transcript, one file in `.aphid/sessions`. Sessions run at the same
time, so a job that starts at nine does not wait for you to stop typing.

Three things make a session, and each ends differently:

| Kind | Made when | Ends when |
| --- | --- | --- |
| resident | The alate starts. | Never. It stops with the alate. |
| attached | A client attaches. | That client detaches. |
| cron | A job comes due. | Its run ends. |

The **resident** session is where the heartbeat wakes. It keeps its context all
day, which is what makes an alate resident and not new every quarter of an hour.
Give it the work that must continue after you close the terminal.

An **attached** session is yours, and it ends with your terminal. A run still in
progress is stopped. This is deliberate: it keeps a day of attaching and
detaching from filling the alate with conversations nobody returns to.

A terminal is not the only client that can attach. A client can say what it is
when it attaches, and the session list then shows that in place of `attached`. A
chat on the Telegram bot is listed as `telegram: <chat id>`, so a list of
conversations tells you where each one is being had:

```
  20260811T091500-0000  resident      2026-08-11 09:15  running
* 20260811T142200-0000  attached      2026-08-11 14:22
  20260811T143000-0000  telegram: 42  2026-08-11 14:30
  20260811T090000-0000  cron: news    2026-08-11 09:00
```

A **cron** session starts empty each time. It cannot see what you are saying,
and you cannot see it in your own window — but the memory is shared, so a job
can write a fact that you recall an hour later.

What sessions share is everything that is *the alate* and not a conversation:
the memory, the crontab, the plugins, the model and the permission gate.

### Moving between them

`/sessions` lists the conversations that run now and the ones on disk.
`/session <id>` looks at one. The daemon reads the transcript and sends it back,
so a session that ended last week draws exactly like one running now. Only the
terminal changes; the agent does not know it is being watched.

A session that ended still has its transcript. Ending a session loses the
context, never the record.

## The home directory

Each instance has one directory:

```
~/.aphid/alate/<name>/
  alate.json      the configuration
  AGENTS.md       the instructions this alate always carries
  HEARTBEAT.md    what to say when it wakes itself
  memory/         the facts, as markdown
  cron.json       the jobs it has scheduled
  state.json      when the heartbeat last woke
  gateway.sock    the socket that clients attach to
  alate.log       each frame the gateway sent
  .aphid/
    skills/       skills for this alate
    plugins/      Rhai plugins for this alate
    sessions/     the transcripts
```

The directory is made when you first run the instance.

The home is also the workspace of the agent. Two results follow:

- `read`, `write` and `edit` can touch only this directory. To let the agent
  work somewhere different, set `workspace` in `alate.json`.
- `AGENTS.md`, `.aphid/skills` and `.aphid/plugins` are found in the usual way,
  because they are in the usual place.

The `bash` tool is not limited to the home. This is true of the coding agent
also.

A name can hold letters, digits, dot, dash and underscore. It cannot start with
a dot, and it cannot hold a path separator. These rules keep `--name` inside the
root directory.

## `alate.json`

Each field has a default. An absent file, and an empty file, give the defaults.

```json
{
  "version": 1,
  "model": null,
  "thinking": "medium",
  "workspace": null,
  "permissions": "ask",
  "heartbeat": { "every": "15m", "prompt": null },
  "memory": { "recall": 5 },
  "gateway": { "socket": null, "telegram": null }
}
```

| Field | Effect |
| --- | --- |
| `model` | The model, by the name `aphid model list` shows. The first model of the catalogue when absent. |
| `thinking` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh` or `max`. |
| `workspace` | Where the agent works. The home when absent. |
| `permissions` | `ask`, `allow` or `deny`. See [Permissions](#permissions). |
| `heartbeat.every` | The time between wakes: `30s`, `15m`, `2h`, `1d`. Use `off` for none. |
| `heartbeat.prompt` | What to say on a wake. See [The heartbeat](#the-heartbeat). |
| `memory.recall` | The quantity of facts offered for each prompt. Use `0` for none. |
| `gateway.socket` | The socket file. `gateway.sock` in the home when absent. |
| `gateway.telegram` | A Telegram bot on the gateway. No bot when absent. See [Telegram](#telegram). |

A file with a higher `version` than this build understands is refused by name.
This prevents a new file from being read as an old one.

## The memory

The memory is a set of facts. A fact is one short sentence. Each fact belongs to
a path, such as `/projects/aphid` or `/people/thiago`.

The facts are markdown files in the home. The path `/projects/aphid` is the file
`memory/projects/aphid.md`:

```markdown
# /projects/aphid

- 2026-08-11 — The plugin API stays as small as it can be.
- 2026-08-11 — Docs are written in ASD-STE100.
```

You can read these files with `cat`, search them with `grep`, and change them
with an editor. The agent can also read and change them with its own file tools,
because they are in its workspace. A memory that only the agent can open is a
memory that nobody can check.

### The two tools

| Tool | Effect |
| --- | --- |
| `remember` | Write one fact under one path. A path is made the first time it is used. |
| `recall` | Search the memory. With no query, it gives the newest facts. |

### Recall that you do not ask for

Before each prompt, the alate searches its memory with the words of the prompt.
It puts the best `memory.recall` facts in front of the model as a system note.
The facts are never put in the message of the person who spoke. The model can
always see which words came from the memory and which came from you.

Recall gives more weight to a word that is rare in the memory than to a word
that is common in it. Facts that answer equally well come back newest first.

The paths, but not the facts, are in the system prompt. The agent sees which
subjects exist, and calls `recall` for what is in them.

### Size

There is no index. The memory reads all of its files for each search. For the
hundreds of facts that one agent writes, this takes a fraction of a millisecond.
A memory of tens of thousands of facts needs a database, and this is not one.

## The heartbeat

The heartbeat is a pulse at a fixed interval. `heartbeat.every` sets it: `15m`,
`2h`, `30s`, or `off` for none. The first wake comes one interval after the
alate starts.

It wakes in the **resident** session, so the alate comes back to a conversation
that remembers this morning. A wake does not happen while that session is
already running, and missed wakes do not collect.

What the alate hears is, in order:

1. `heartbeat.prompt` from `alate.json`;
2. `HEARTBEAT.md` in the home;
3. a standard line, which tells it to look at its memory and either act or stop.

Every attached terminal sees the wake, whichever conversation it is looking at.

Use the heartbeat for "look around and see". Use [cron](#cron) for anything that
must happen at a particular time.

## Cron

The alate schedules its own work with the `cron` tool. Each job has a name, a
schedule and a prompt.

| Argument | Effect |
| --- | --- |
| `name` | Which job. A name that exists is replaced. |
| `schedule` | Five fields, in local time. Use `off` to remove the job. |
| `prompt` | What to do. |

A job runs in a session of its own, which starts empty. The prompt must
therefore hold everything the job needs: the session that runs it does not
remember the conversation that scheduled it.

The jobs are in `cron.json` in the home. You can edit that file yourself.

```json
{
  "version": 1,
  "entries": [
    {
      "name": "morning-review",
      "schedule": "0 9 * * *",
      "prompt": "Read yesterday's notes and tell me what is still open.",
      "last": "2026-08-11T09:00:00-03:00"
    }
  ]
}
```

### The schedule

Five fields, as in Vixie cron: minute, hour, day of month, month, day of week.
Seconds are not accepted; a pattern with six fields is refused, and the message
says so.

```
0 9 * * *          every day at 09:00
*/15 * * * *       every 15 minutes
0 9 * * MON-FRI    at 09:00 on the days of work
0 3 1 * *          at 03:00 on the first day of each month
```

**The times are local.** `0 9 * * *` is nine in the morning where the machine
is, not nine UTC.

A job that goes past while the alate is stopped runs **one time** when the alate
comes back. A daily job and a week of stopped time make one run, not seven.

The names of the jobs, their schedules and their prompts are in the system
prompt, so the alate knows what it already told itself to do.

## The gateway

The gateway is a Unix socket in the home. The daemon listens on it. Each
terminal that attaches is a client, and so is the [Telegram](#telegram) bot.

The protocol is one JSON object for each line, in both directions. You can read
it with `nc`, and you can write another client for it.

Each line the daemon sends holds a `kind`, and a `session` when the line belongs
to a conversation. A line with no `session` is the daemon speaking for itself:
the greeting, a heartbeat, a session list, a permission question.

A client sends `{"kind":"attach"}` first. The daemon then opens a session for it
and answers with `hello`. A program that only wants to know whether an alate is
awake connects and closes without sending anything, and no conversation is made
for it.

A client can also say what it is: `{"kind":"attach","channel":"telegram: 42"}`.
The name is what `/sessions` shows for that conversation. It is cut to 32
characters, and line ends are removed, because it is printed in a list. The
field can be absent, and a client that does not send it is listed as `attached`.

A client sees the frames of the session it watches, and the daemon's own. To
change what it watches, it sends `{"kind":"watch","id":"..."}`; the daemon then
replays that session between `history_start` and `history_end`, whether the
session runs now or ended long ago.

There is no store of recent frames. What a client missed is in the transcript,
which is what `watch` reads — so what it gets back cannot disagree with what
happened.

Each line is also written to `alate.log`. Read the hours when nobody watched
with `jq`:

```console
$ jq -r 'select(.kind == "heartbeat") | .at + "  " + .note' alate.log
$ jq -r 'select(.session == "20260811T090000-0000") | .text // empty' alate.log
```

The socket permits only its owner to read and write it. Anything that can
connect can make the agent run commands, so the permissions of the file are the
whole of the access control.

The gateway needs a Unix socket, so `aphid alate` does not work on Windows.

A socket file that no daemon is behind is removed and made again. Two daemons
cannot serve one alate: the second one stops and says so.

## Telegram

A Telegram bot can speak to the alate. You send a message, the agent answers,
and you can permit or refuse a tool from the chat.

The bot is a client of the gateway, and not a second door. Each chat attaches to
the same socket and gets its own conversation, in the same manner as a terminal.
So two chats do not see each other, and `aphid alate attach` shows what a chat
said and what the agent answered.

This is behind a build feature, because it adds an HTTP client that a build
without a bot does not need:

```console
$ cargo build --release --features telegram
```

To make a bot:

1. Speak to `@BotFather` in Telegram and send `/newbot`. It gives you a token.
2. Put the token in the environment of the daemon:
   ```console
   $ export TELEGRAM_BOT_TOKEN=123456:AA...
   ```
3. Put a `telegram` block in `alate.json`:
   ```json
   { "gateway": { "telegram": { "chats": [], "tools": true } } }
   ```
4. Start the alate, and send a message to the bot. The bot refuses, and the
   refusal holds the id of your chat.
5. Put that id in `chats`, and start the alate again.

| Field | Effect |
| --- | --- |
| `token_env` | The variable that holds the bot token. `TELEGRAM_BOT_TOKEN` when absent. |
| `chats` | The chats that can speak to this alate, by id. An empty list permits nobody. |
| `poll` | How long one request waits for a message: `25s` when absent. |
| `tools` | Show one line for each tool call. `false` when absent. |
| `api` | The address of the Bot API. The Telegram one when absent. |

The token is never in `alate.json`, only the name of the variable that holds it.
This is the rule the model keys follow, and for the same cause: a configuration
file is copied and shared, and a token in it goes with it.

`chats` is an allow list, and an empty one permits nobody. Anything that can
speak to the bot can make the agent run commands, so a bot that anybody found
would be a bot that anybody could use. A chat that is refused is told its id one
time.

In a chat:

| What you send | Effect |
| --- | --- |
| Anything else | Words for the agent. |
| `/new` | Start a new conversation. The one before it stays on disk. |
| `/cancel` | Stop the run in flight. |
| `/start`, `/help` | Show these commands. |

The agent's answer comes in one message for each turn, and not one for each
word. Telegram permits approximately one message each second for a chat, and a
message for each part of an answer would be held back. A long answer is cut into
messages of 4096 characters, at a line end where there is one.

The chat shows the text of the answer, and the errors. It does not show the
thinking, the tool arguments or the tool results. Use `aphid alate attach` to
read those. With `tools` set to `true`, each tool call also gives one short
line, which makes a long run legible from a telephone.

In `/sessions`, a chat is listed as `telegram: <chat id>` and not as `attached`,
so you can tell a conversation in a chat from one in a terminal.

A permission question comes to the chat with three buttons: **Allow**, **Allow
always** and **Deny**. The question goes only to a chat with a run in flight. A
question that belongs to a terminal or to a job is left for the terminal to
answer.

Note that a chat that has spoken stays attached until the daemon stops. So an
alate with a bot **is attended**, and a tool that asks permission is asked in
the chat instead of being refused. Before a chat speaks for the first time, no
connection exists, and an unattended alate behaves as it does with no bot.

If the bot cannot be reached, the daemon says so one time and tries again, and
waits longer after each failure up to one minute. It says so again when Telegram
answers.

The bot is not necessary for the alate to start. A token that is absent, a
`poll` that is not a length of time, and a Telegram that does not answer are all
reported and passed over.

## Permissions

`permissions` in `alate.json` controls the `bash`, `write` and `edit` tools.

| Value | Effect |
| --- | --- |
| `ask` | Ask each attached client. The first answer decides. |
| `allow` | Permit each call. |
| `deny` | Refuse each call. |

With `ask` and no terminal attached, there is nobody to ask, and the call is
refused. An unattended agent that permitted instead could agree with itself all
night.

A question waits five minutes for an answer. After that it is refused.

## Plugins and skills

Rhai plugins in `<home>/.aphid/plugins` load when the alate starts. They are not
gated by a trust question: there is no terminal to ask at, and the home is a
directory that you made for this agent.

A plugin that calls `prompt` puts words to the agent in the same queue that a
terminal uses. A plugin with an `on_tick` hook runs four times each second. See
[Plugins](plugins.md).

Skills in `<home>/.aphid/skills` work as they do in the coding agent. See the
README.

## Files and environment variables

| Path | Content |
| --- | --- |
| `~/.aphid/alate/<name>/` | One instance. `$APHID_HOME` moves the parent of this. |
| `~/.aphid/models.json` | The model catalogue, shared with the other front ends. |

| Variable | Effect |
| --- | --- |
| `APHID_HOME` | Move `~/.aphid`. The alates move with it. |
| `DEEPSEEK_API_KEY` | The key for the standard models. A model in the catalogue can name a different variable. |
| `TELEGRAM_BOT_TOKEN` | The token of the Telegram bot. `gateway.telegram.token_env` can name a different variable. |
