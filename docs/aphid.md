# Aphid — the AI harness

The coding agent is what `aphid` runs when you give it no subcommand. It is the
agent loop of `aphid-agent`, with everything a coding agent needs put around it:
the tools, a system prompt made from the conventions of the project, the skills,
the sessions and the permission gate.

This chapter tells you what the harness does, and gives each option of the
command. The [Commands](aphid/commands.md), [Skills](aphid/skills.md) and
[Plugins](aphid/plugins.md) chapters describe the three parts that you control.

## The workspace

Aphid finds the workspace when it starts. This is the root of the repository, or
the directory that you are in when there is no repository.

The `read`, `write` and `edit` tools can touch only this directory. The `bash`
tool is not limited in this manner, because a shell reads and writes anywhere.

| Tool | Effect |
| --- | --- |
| `bash` | Runs a command. Not limited to the workspace. |
| `read` | Reads a file, or a part of one. |
| `write` | Writes a full file. |
| `edit` | Replaces text in a file. |

The output of a tool is cut when it is very long, and the full output is kept in
a file that the message gives the name of.

## The instructions

Aphid reads each `AGENTS.md` file from the root of the workspace down to the
current directory. The most specific file is last, and it has the final word. A
file at `~/.aphid/AGENTS.md` is read before all of them, and thus is applied in
each workspace.

Put the conventions of the project in these files: how to run the tests, how to
write a commit message, what not to touch.

For instructions that are only necessary sometimes, write a
[skill](aphid/skills.md). A skill costs almost nothing until the model opens it.

`--no-context` stops aphid from reading the `AGENTS.md` files and the skills.

## Sessions

Aphid records each session as one file of JSON lines in `~/.aphid/sessions`,
shared by every project on the machine, and adds to it as each message is
committed. The filename is `<project>-<id>`, where `<project>` is the
workspace's directory name — cosmetic only, so listing and resuming a session
still work correctly even if two projects share a name. Nothing is written a
second time. Thus a failure costs the turn that was in flight and no more, and
`--resume` is a replay of the file.

Headless runs are recorded also. `--sessions`, `--resume`, and the graphical
session drawer see them in the same manner as the terminal.

```console
$ aphid --sessions                       # print the saved sessions
$ aphid --resume                         # continue the most recent session here
$ aphid --resume 20260810T012035-0000    # continue the session with this identifier
```

The identifier is optional. If you give no identifier, aphid continues the most
recent session for the current directory.

## Permissions

`--confirm` makes aphid ask you before it runs a command that changes the
workspace. A headless run has no terminal for a question. Thus `--confirm` and
`-p` together refuse each such command, and do not permit it quietly.

A plugin can answer these questions in place of you, with the `on_permission`
announcement a plugin can subscribe to. Refer to [Plugins](aphid/plugins.md)
and [Composition](aphid/composition.md).

## Invocation

```
aphid [OPTIONS] [PROMPT]...    the coding agent
aphid gui [OPTIONS]                 open the graphical coding agent
aphid alate <COMMAND>               run a resident agent, or attach to one
aphid raw   [OPTIONS] <PROMPT>...   stream one completion, and print each protocol event
aphid agent [OPTIONS] <PROMPT>...   run the agent loop with a demo tool
aphid model <COMMAND>               manage the models in ~/.aphid/models.json
```

The coding agent is the default. If the first word is `alate`, `raw`, `agent` or
`model`, aphid runs that subcommand. If the first word is something different,
aphid uses the full command line as a prompt for the coding agent.

Give a prompt to run the agent one time. Give no prompt to open the terminal
user interface.

```console
$ aphid                              # opens the terminal user interface
$ aphid gui                          # opens the graphical user interface
$ aphid -p "fix the failing test"    # runs one time, and prints the result
$ aphid "fix the failing test"       # the same, with no -p
```

`-p` and the bare words do the same thing. Only an empty prompt opens the
terminal user interface.

`aphid gui` uses the same model, context, tools, sessions, permission gate,
slash commands, and Rhai plugins as the terminal. The main area shows streamed
text, reasoning, tool calls, tool output, and run state. Markdown includes
tables, nested lists, code highlighting, links, and images. A remote image is
not fetched until you select its load control.

## The options

| Option | Effect |
| --- | --- |
| `-p`, `--print <PROMPT>` | Run one time. Stream the result to stdout, and exit. |
| `--model <NAME>` | Select a model. Give the identifier, or a unique part of it. |
| `--models` | Print the known models, and exit. |
| `--think <LEVEL>` | Set the quantity of reasoning. |
| `--system <TEXT>` | Replace the standard instructions. |
| `--append-system <TEXT>` | Add text to the instructions. |
| `--resume [<ID>]` | Continue a saved session. |
| `--sessions` | Print the saved sessions for this workspace, and exit. |
| `--confirm` | Ask before each command that changes the workspace. |
| `--no-context` | Do not read `AGENTS.md` files or skills. |
| `--list-plugins` | Print the plugins that would load, and exit. |
| `--no-plugins` | Do not load any plugin from `.aphid/plugins`. |
| `--plugin <PATH>` | Load one plugin from a path. |
| `--trust-plugins` | Agree to the plugins of this workspace. |
| `--max-turns <N>` | Stop the run after this quantity of requests. |
| `--quiet` | Do not print the output of each tool. |

### Select a model

`--model` accepts the full identifier, the last part of it, or a prefix. Aphid
tries these three forms in that sequence. If two or more models match, aphid
refuses the name and prints the models that matched.

```console
$ aphid --model deepseek-v4-pro -p "hello"   # the full identifier
$ aphid --model pro -p "hello"               # the last part
```

If you give no `--model`, aphid uses the first model in the catalogue. If the
catalogue is empty, aphid prints how to add a model and exits.

`--models` prints the catalogue. The catalogue is the models in
`~/.aphid/models.json`. To add a model, refer to [`model`](#model).

### Set the quantity of reasoning

`--think` accepts these levels: `off`, `minimal`, `low`, `medium`, `high`,
`xhigh` and `max`. `medium` is the default.

Each model supplies a different set of levels. Aphid decreases the level to the
nearest level that the model supplies, and prints a note. If the model cannot
reason, aphid ignores the option and prints a note. Refer to
[Thinking levels](core.md#thinking-levels).

### Control the plugins

The plugin options control the Rhai plugins in `.aphid/plugins`. A plugin in your
home directory always loads. A plugin that comes with a workspace needs your
agreement the first time; aphid asks before the terminal user interface starts,
and keeps the answer in `~/.aphid/trust.json`. A headless run has no terminal for
a question, and thus does not load the plugins of the workspace unless you give
`--trust-plugins`. `--plugin` names a file directly and does not ask.

Use `--no-plugins` to make the start of a run fully predictable. Read
[Plugins](aphid/plugins.md) to write one.

## `alate`

`aphid alate` runs a resident agent. An alate has a home directory of its own, a
memory that continues between sessions, a clock that wakes it, and a socket that
a terminal attaches to.

```
aphid alate run    [--name NAME]    run the alate in this terminal
aphid alate attach [--name NAME]    open a terminal on a running alate
aphid alate list                    show the alates on this machine
```

| Option | Effect |
| --- | --- |
| `-n`, `--name <NAME>` | Select the instance. The default is `default`. |

`run` holds the terminal until you stop it. `attach` opens a terminal on an
alate that already runs; close it, and the alate continues.

[Alate](alate.md) gives the home directory, each field of the configuration, the
memory, the heartbeat and the crontab. [CLI](alate/gateway/cli.md) gives the
terminal that attaches.

`aphid alate` needs a Unix socket, so it does not work on Windows.

## `raw` and `agent`

These two subcommands are the debug tools. `raw` sends one request. `agent`
loops until the model stops to call tools. Both accept the same options.

| Option | Effect |
| --- | --- |
| `--pro` | Use `deepseek-v4-pro`. The default is `deepseek-v4-flash`. |
| `--system <TEXT>` | Put a system message before the prompt. |
| `--think <LEVEL>` | Set the quantity of reasoning. |
| `--max-tokens <N>` | Limit the length of the response. |
| `--temperature <F>` | Set the sampling temperature. |
| `--tool` | Supply a demo `get_weather` tool, to show tool-call deltas. |
| `--events` | Print each delta event with its span, in place of the text. |
| `--request` | Print the encoded request body, and exit. |

`--request` does not send a request. Thus you can use it with no API key.

```console
$ aphid raw --request "hello"        # print the request body
$ aphid raw --events --tool "what is the weather in Lisbon?"
```

These two subcommands always use a DeepSeek model, and they always read
`DEEPSEEK_API_KEY`. To use a different model, use the coding agent.

## `model`

`aphid model` manages `~/.aphid/models.json`. The command `aphid models` does
the same thing. [Core](core.md#the-catalogue) describes the catalogue and the
format of the file.

The model descriptions come from [models.dev](https://models.dev). Aphid keeps a
copy of that document in `~/.aphid/models.dev.json`, and it uses the copy while
the copy is less than 24 hours old.

### `model add`

```
aphid model add [OPTIONS] <NAME>
```

`<NAME>` is `provider/model`, or a model identifier that only one provider
supplies.

```console
$ aphid model add zhipuai/glm-5
added glm-5 in /home/you/.aphid/models.json
  provider  zhipuai
  endpoint  https://open.bigmodel.cn/api/paas/v4
  limits    204800 context · 131072 output
  price     $1.00 in · $3.20 out per M tokens
  key       $ZHIPU_API_KEY
(cached 3h ago; `aphid model update` to refresh)
```

Many providers supply a model with the same identifier. If the name is
ambiguous, aphid prints each provider that supplies that model:

```console
$ aphid model add deepseek-v4-pro
aphid: `deepseek-v4-pro` is served by 23 providers:
    alibaba-cn/deepseek-v4-pro
    azure/deepseek-v4-pro
    ...
Name one of them, or pass --provider <id>.
```

Some model identifiers contain a slash. Aphid reads the full name as a model
identifier first, and as `provider/model` second. Thus both of these commands
find the same model:

```console
$ aphid model add openai/gpt-oss-120b
$ aphid model add wandb/openai/gpt-oss-120b
```

| Option | Effect |
| --- | --- |
| `--provider <ID>` | Use only this provider. Use it when a name is ambiguous. |
| `--base-url <URL>` | Give the endpoint URL. models.dev does not list one for each provider. |
| `--api <API>` | Set the wire protocol. |
| `--api-key-env <VAR>` | Set the environment variable that holds the API key. |
| `--compat <PROFILE>` | Set the endpoint behaviour. Refer to [Core](core.md#the-wire). |
| `--force` | Replace a model that is already in the catalogue. |
| `--refresh` | Get the models.dev document again, even if the copy is new. |
| `--offline` | Use the local copy only. Fail if there is no copy. |

Aphid speaks the OpenAI chat-completions protocol only. If the provider speaks a
different protocol, aphid refuses the model. `--api openai-completions` makes
aphid add the model regardless.

### `model remove`

```
aphid model remove <NAME>
```

`<NAME>` accepts the same three forms as `--model`. This command removes a model
from `~/.aphid/models.json`.

```console
$ aphid model remove glm-5
removed glm-5 from /home/you/.aphid/models.json
```

### `model list`

```
aphid model list
```

This command prints the models in `~/.aphid/models.json`.

### `model search`

```
aphid model search [OPTIONS] <QUERY>
```

This command finds models on models.dev, but it adds no model. Aphid compares
the query with the provider identifier, the model identifier and the model name.

```console
$ aphid model search glm --limit 3
302ai/glm-4.5      131072 ctx  $  0.29/$1.14    GLM-4.5
zhipuai/glm-5      204800 ctx  $  1.00/$3.20    GLM-5
...
(cached 3h ago; `aphid model update` to refresh)
```

| Option | Effect |
| --- | --- |
| `--limit <N>` | Print at most this many results. The default is to print them all. |
| `--refresh` | Get the models.dev document again. |
| `--offline` | Use the local copy only. |

Each name in the first column is a name that `aphid model add` accepts.

### `model update`

```
aphid model update
```

This command gets the models.dev document again, and writes it to
`~/.aphid/models.dev.json`. Then it prints the quantity of providers and models,
and the models that models.dev added or removed after the previous copy.

```console
$ aphid model update
/home/you/.aphid/models.dev.json · 182 providers · 6243 models · 3.5 MB
3 added:
    deepseek/deepseek-v4-pro
    ...
```

This command changes the local copy only. It does not change
`~/.aphid/models.json`.

`add` and `search` also get the document if the local copy is more than 24 hours
old. Use `model update` to get the document immediately.

To correct a model by hand, refer to [The file](core.md#the-file).

## Files and environment variables

| Path | Content |
| --- | --- |
| `~/.aphid/models.json` | Your models. |
| `~/.aphid/models.dev.json` | The local copy of the models.dev document. |
| `~/.aphid/AGENTS.md` | Instructions for each workspace. |
| `<workspace>/AGENTS.md` | Instructions for one workspace. |
| `<workspace>/.aphid/skills/` | The skills of this workspace. |
| `<workspace>/.agents/skills/` | The skills of this workspace that the other agents read too. |
| `<workspace>/.aphid/plugins/` | The plugins of this workspace. |
| `~/.aphid/sessions/` | The saved sessions of every project, named `<project>-<id>.jsonl`. |
| `~/.aphid/skills/` | Your skills, for each workspace. |
| `~/.agents/skills/` | Your skills that the other agents read too. |
| `~/.aphid/plugins/` | Your plugins, for each workspace. |
| `~/.aphid/trust.json` | The workspaces whose plugins you agreed to. |
| `~/.aphid/alate/<name>/` | One resident agent. See [Alate](alate.md). |

| Variable | Effect |
| --- | --- |
| `APHID_HOME` | Replaces `~/.aphid`. Use it to keep a separate configuration. |
| `DEEPSEEK_API_KEY` | The key for the built-in models that `raw` and `agent` use. |

`APHID_HOME` moves the model catalogue, the trust file and the alates. It does
not move `AGENTS.md`, the skills or the plugins of your home directory: those
follow `HOME`, so that a separate catalogue does not take your instructions away
with it.

Each model gives the name of the variable that holds its key. The coding agent
reads the variable of the model that you selected. Thus a model from a different
provider reads a different variable:

```console
$ aphid --model glm-5 -p "hello"
aphid: ZHIPU_API_KEY is not set, and glm-5 needs it
```

If you change the model in the terminal user interface, aphid reads the key of
the new model.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | The run failed, or aphid could not read or write a file or the network. |
| `2` | The command line was wrong. |
