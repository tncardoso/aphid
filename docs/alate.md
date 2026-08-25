# Alate — the live agent

An alate is the winged form of an aphid. It is the form that leaves the plant
and lives away from it.

The coding agent starts in a repository, does the work you ask for, and forgets
everything when you close the terminal. An alate is different in five ways:

- It has a **home directory** that it owns. The home is also its workspace.
- It has a **memory**. What it learns in one session, it knows in the next.
- It has a **heartbeat**. It wakes on a clock and looks at what it has.
- It has a **crontab**. It can schedule a prompt to run at a time, in a
  conversation of its own.
- It has a **[gateway](alate/gateway.md)**. You attach a terminal to it, and you
  detach again. The agent continues either way.

The agent itself is the same agent. The tools, the instruction files, the
sessions and the plugins all work as they do in the [coding
agent](aphid.md).

```console
$ aphid alate run --name work        # one terminal
$ aphid alate attach --name work     # another, whenever you want it
```

[CLI](alate/gateway/cli.md) gives the commands that start an alate and the
terminal that attaches to one. This chapter gives what an alate *is*: its home,
its configuration, its memory, its clock and its gate.

## Sessions

An alate has more than one conversation at a time. Each is a **session**: one
context, one transcript, one file in `~/.aphid/sessions`. Sessions run at the
same time, so a job that starts at nine does not wait for you to stop typing.

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
chat on the Telegram bot is listed as `telegram: <chat id>`, and a channel in a
colony as `colony: #general`, so a list of conversations tells you where each
one is being had.

A **cron** session starts empty each time. It cannot see what you are saying,
and you cannot see it in your own window — but the memory is shared, so a job
can write a fact that you recall an hour later.

What sessions share is everything that is *the alate* and not a conversation:
the memory, the crontab, the plugins, the model and the permission gate.

A session that ended still has its transcript. Ending a session loses the
context, never the record. `/session <id>` opens any of them, including the ones
that finished last week.

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
- `AGENTS.md`, `.aphid/skills`, `.agents/skills` and `.aphid/plugins` are found
  in the usual way, because they are in the usual place.

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
  "gateway": { "socket": null, "telegram": null, "colony": null }
}
```

| Field | Effect |
| --- | --- |
| `model` | The model, by the name `aphid model list` shows. The first configured model when absent. An alate with no configured model fails and says to run `aphid models add`. |
| `thinking` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh` or `max`. |
| `workspace` | Where the agent works. The home when absent. |
| `permissions` | `ask`, `allow` or `deny`. See [Permissions](#permissions). |
| `heartbeat.every` | The time between wakes: `30s`, `15m`, `2h`, `1d`. Use `off` for none. |
| `heartbeat.prompt` | What to say on a wake. See [The heartbeat](#the-heartbeat). |
| `memory.recall` | The quantity of facts offered for each prompt. Use `0` for none. |
| `gateway.socket` | The socket file. `gateway.sock` in the home when absent. |
| `gateway.telegram` | A Telegram bot on the gateway. No bot when absent. See [Telegram](alate/gateway/telegram.md). |
| `gateway.colony` | A colony on the gateway. No colony when absent. See [Colony](alate/gateway/colony.md). |

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
terminal uses. A plugin listening for `code/tick` runs four times each second. See
[Plugins](aphid/plugins.md).

Skills in `<home>/.aphid/skills` and `<home>/.agents/skills` work as they do in
the coding agent. See [Skills](aphid/skills.md).

## Logs

There are two, and they are not the same thing.

`alate.log` in the home is the **frames**: one line for each thing the gateway
sent, as JSON. Read it with `jq`. Refer to [The
log](alate/gateway.md#the-log).

The daemon also writes a log of the program to standard error, which says when a
session opened, when a client connected, when the socket was bound, and what
Telegram did. `RUST_LOG` controls it, and it shows messages of level `info` and
higher when the variable is absent.

```console
$ RUST_LOG=debug aphid alate run --name work
$ RUST_LOG=aphid_alate::telegram=debug aphid alate run --name work
$ aphid alate run --name work 2> ~/.aphid/alate/work/daemon.log
```

The terminal that runs the alate is the terminal that gets this. A daemon that
you start with `systemd` or `nohup` sends it where you told that tool to send
it.

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
| `APHID_COLONY_KEY` | The key this agent speaks with in a colony. `gateway.colony.key_env` can name a different variable. |
| `RUST_LOG` | Which messages the daemon writes to standard error. `info` when absent. |
