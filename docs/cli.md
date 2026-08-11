# The aphid command line

This document tells you how to use the `aphid` command. It is written in
Simplified Technical English.

## Contents

- [Invocation](#invocation)
- [The coding agent](#the-coding-agent)
- [`raw` and `agent`](#raw-and-agent)
- [`model`](#model)
- [Files and environment variables](#files-and-environment-variables)
- [Exit codes](#exit-codes)

## Invocation

```
aphid [OPTIONS] [PROMPT]...    the coding agent
aphid raw   [OPTIONS] <PROMPT>...   stream one completion, and print each protocol event
aphid agent [OPTIONS] <PROMPT>...   run the agent loop with a demo tool
aphid model <COMMAND>               manage the models in ~/.aphid/models.json
```

The coding agent is the default. If the first word is `raw`, `agent` or `model`,
aphid runs that subcommand. If the first word is something different, aphid uses
the full command line as a prompt for the coding agent.

Give a prompt to run the agent one time. Give no prompt to open the terminal
user interface.

```console
$ aphid                              # opens the terminal user interface
$ aphid -p "fix the failing test"    # runs one time, and prints the result
$ aphid "fix the failing test"       # the same, with no -p
```

`-p` and the bare words do the same thing. Only an empty prompt opens the
terminal user interface.

## The coding agent

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

`--model` accepts the full identifier, the last part of it, or a prefix. aphid
tries these three forms in that sequence. If two or more models match, aphid
refuses the name and prints the models that matched.

```console
$ aphid --model deepseek-v4-pro -p "hello"   # the full identifier
$ aphid --model pro -p "hello"               # the last part
```

If you give no `--model`, aphid uses the first model in the catalog.

`--models` prints the catalog. The catalog contains the models that aphid
supplies and the models in `~/.aphid/models.json`. To add a model, refer to
[`model`](#model).

### Set the quantity of reasoning

`--think` accepts these levels: `off`, `minimal`, `low`, `medium`, `high`,
`xhigh` and `max`. `off` is the default.

Each model supplies a different set of levels. aphid decreases the level to the
nearest level that the model supplies, and prints a note. If the model cannot
reason, aphid ignores the option and prints a note.

### Continue a session

aphid records each session, and it records the headless runs also.

```console
$ aphid --sessions                       # print the saved sessions
$ aphid --resume                         # continue the most recent session here
$ aphid --resume 20260810T012035-0000    # continue the session with this identifier
```

The identifier is optional. If you give no identifier, aphid continues the most
recent session for the current directory.

### See what is running

In a session, type `/ps`. The list shows each command that runs now, and the
last four commands that stopped. Each line gives the number of the command, its
system process identifier, the source (`bash`, or the name of a plugin), the
time, and, for a command that stopped, the result and the quantity of output.

Press the arrow keys to select a command that runs now, and press `k` to stop
it. This stops the command and each command that it started. Press `Esc` to
close the list.

The list opens while the agent runs also, which is when there is most to see.

### Control the tools

`--confirm` makes aphid ask you before it runs a command that changes the
workspace. A headless run has no terminal for a question. Thus `--confirm` and
`-p` together refuse each such command.

`--no-context` prevents aphid from reading the `AGENTS.md` files and the skills.

The plugin options control the Rhai plugins in `.aphid/plugins`. A plugin in your
home directory always loads. A plugin that comes with a workspace needs your
agreement the first time; aphid asks before the terminal user interface starts,
and keeps the answer in `~/.aphid/trust.json`. A headless run has no terminal for
a question, and thus does not load the plugins of the workspace unless you give
`--trust-plugins`. `--plugin` names a file directly and does not ask.
Read [plugins.md](plugins.md) to write one.
Use it to make the start of a run fully predictable.

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
the same thing.

The model descriptions come from [models.dev](https://models.dev). aphid keeps a
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

Some model identifiers contain a slash. aphid reads the full name as a model
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
| `--compat <PROFILE>` | Set the endpoint behaviour. Refer to the table that follows. |
| `--force` | Replace a model that is already in the catalog. |
| `--refresh` | Get the models.dev document again, even if the copy is new. |
| `--offline` | Use the local copy only. Fail if there is no copy. |

aphid speaks the OpenAI chat-completions protocol only. If the provider speaks a
different protocol, aphid refuses the model. `--api openai-completions` makes
aphid add the model regardless.

`--compat` accepts these profiles:

| Profile | Use |
| --- | --- |
| `compatible` | A different company's OpenAI-compatible server. The default. |
| `openai` | OpenAI and Azure. |
| `deepseek` | DeepSeek. |
| `none` | No behaviour table. |

models.dev describes models. It does not describe which request fields a server
refuses. Thus aphid selects the profile from the provider, and you can correct
it. Refer to
[Correct a model by hand](#correct-a-model-by-hand).

### `model remove`

```
aphid model remove <NAME>
```

`<NAME>` accepts the same three forms as `--model`. This command removes a model
from `~/.aphid/models.json` only. It cannot remove a model that aphid supplies.

```console
$ aphid model remove glm-5
removed glm-5 from /home/you/.aphid/models.json
```

### `model list`

```
aphid model list [--all]
```

This command prints the models in `~/.aphid/models.json`. `--all` prints the
models that aphid supplies also, and gives the source of each model.

### `model search`

```
aphid model search [OPTIONS] <QUERY>
```

This command finds models on models.dev, but it adds no model. aphid compares
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

If aphid cannot get the document, and a local copy exists, aphid uses the local
copy and tells you that the data is old. An old price is more useful than an
error.

### Correct a model by hand

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

`compat` gives the name of a profile, and then each behaviour that is different
from that profile. In the example above, the endpoint is a usual OpenAI-
compatible server, but it refuses the `reasoning_effort` field.

`thinking_levels` gives the value to send for each level. A text value is the
value to send. `false` means that the model refuses the level. If a level is not
in the file, aphid sends the name of the level.

```json
"thinking_levels": { "off": "disabled", "minimal": "low", "max": "max", "xhigh": false }
```

If aphid cannot read the file, it prints the problem and continues with the
models that it supplies. A mistake in this file cannot prevent a start.

## Files and environment variables

| Path | Content |
| --- | --- |
| `~/.aphid/models.json` | Your models. |
| `~/.aphid/models.dev.json` | The local copy of the models.dev document. |
| `~/.aphid/AGENTS.md` | Instructions for each workspace. |
| `<workspace>/AGENTS.md` | Instructions for one workspace. |
| `<workspace>/.aphid/sessions/` | The saved sessions. |
| `<workspace>/.aphid/plugins/` | The plugins of this workspace. |
| `~/.aphid/plugins/` | Your plugins, for each workspace. |
| `~/.aphid/trust.json` | The workspaces whose plugins you agreed to. |

aphid reads each `AGENTS.md` file from the workspace root down to the current
directory. The most specific file is last, and it has the final word.

| Variable | Effect |
| --- | --- |
| `APHID_HOME` | Replaces `~/.aphid`. Use it to keep a separate configuration. |
| `DEEPSEEK_API_KEY` | The key for the models that aphid supplies. |

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
