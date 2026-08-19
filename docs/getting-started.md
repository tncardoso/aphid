# Getting started

This chapter tells you how to build aphid, how to give it a key, and how to run
it for the first time.

## What you need

- A Rust toolchain of the 2024 edition or later. Aphid is built with
  `rustc` 1.94.
- An API key for a model. The coding agent has no models until you add one
  with `aphid models add <provider/model>`. The command records which
  environment variable holds the key. Refer to [Add a model](#add-a-model).
- A system with Unix sockets, if you want the resident agent. `aphid alate` does
  not work on Windows. The coding agent does.

## Install

The installer gets the binary of the last release and puts it in
`~/.local/bin`. It is the fastest way, because it compiles nothing:

```console
$ curl --proto '=https' --tlsv1.2 -LsSf https://github.com/tncardoso/aphid/releases/latest/download/aphid-ai-installer.sh | sh
```

The releases hold binaries for Linux and macOS. On other systems, and on a
different processor, cargo compiles it from the registry:

```console
$ cargo install aphid-ai
```

## Build from the source

```console
$ git clone https://github.com/tncardoso/aphid
$ cd aphid
$ cargo build --release
```

The binary is then `target/release/aphid`. To put it on your path:

```console
$ cargo install --path crates/aphid-cli
```

There is one optional feature, `telegram`, which adds a Telegram bot to the
resident agent and an HTTP client to the build. It is not on by default, because
a build with no bot does not need the HTTP client.

```console
$ cargo install --path crates/aphid-cli --features telegram
```

## Give it a key

```console
$ export DEEPSEEK_API_KEY=sk-...
```

Put this line in the file that your shell reads at start, so that each terminal
has it.

Each model gives the name of the variable that holds its key, and aphid reads
the variable of the model that you selected. Thus a model from a different
provider reads a different variable, and a key that is absent is reported by
name:

```console
$ aphid --model glm-5 -p "hello"
aphid: ZHIPU_API_KEY is not set, and glm-5 needs it
```

## The first run

Go to a repository and start the terminal user interface:

```console
$ cd ~/projects/my-project
$ aphid
```

Type a question and press `Enter`. Type `/help` to see the
[commands](aphid/commands.md).

To run one prompt and print the result, give the prompt on the command line:

```console
$ aphid -p "what does this crate do?"
```

Aphid records each session, and it records the headless runs also. `aphid
--sessions` lists them, and `aphid --resume` continues the most recent one.

## Add a model

The catalogue is the models in `~/.aphid/models.json`. The descriptions come
from [models.dev](https://models.dev), so you do not write out a context
window and a price by hand.

```console
$ aphid model search glm --limit 3
$ aphid model add zhipuai/glm-5
$ aphid --model glm-5 -p "hello"
```

[Aphid](aphid.md#model) describes each `model` subcommand, and
[Core](core.md#the-catalogue) describes the file that they write.

## Tell it about your project

Aphid reads each `AGENTS.md` file from the root of the workspace down to the
current directory, and the most specific file has the final word. Put the
conventions of the project in one:

```markdown
# AGENTS.md

- Run `cargo clippy` and `cargo fmt` after each change.
- The tests are in `tests/`, and each one is a file.
```

A file at `~/.aphid/AGENTS.md` is applied in each workspace.

For instructions that are only needed sometimes, write a
[skill](aphid/skills.md) instead. A skill costs almost nothing until the model
opens it.

## Start a resident agent

The coding agent starts in a repository and forgets everything when you close
the terminal. An alate has a home of its own, a memory, and a clock that wakes
it.

```console
$ aphid alate run --name work
aphid: work is awake in /home/you/.aphid/alate/work
aphid: attach with `aphid alate attach --name work`
```

Attach a terminal to it from somewhere else, and detach again with `Ctrl-C`. The
alate continues to run. [Alate](alate.md) describes the home, the memory, the
heartbeat and the crontab.

## Where things are kept

| Path | Content |
| --- | --- |
| `~/.aphid/models.json` | Your models. |
| `~/.aphid/AGENTS.md` | Instructions for each workspace. |
| `~/.aphid/skills/` | Your skills, for each workspace. |
| `~/.agents/skills/` | Your skills that the other agents read too. |
| `~/.aphid/plugins/` | Your plugins, for each workspace. |
| `~/.aphid/alate/<name>/` | One resident agent. |
| `~/.aphid/sessions/` | The saved sessions of every project. |
| `<workspace>/AGENTS.md` | Instructions for one workspace. |
| `<workspace>/.aphid/skills/` | The skills of this workspace. |
| `<workspace>/.agents/skills/` | The skills of this workspace that the other agents read too. |
| `<workspace>/.aphid/plugins/` | The plugins of this workspace. |

`APHID_HOME` replaces `~/.aphid`. Use it to keep a separate configuration.

## Build and test the source

```console
$ cargo build
$ cargo test
$ cargo clippy
$ cargo fmt
```

```console
$ cargo build --features telegram
$ cargo test -p aphid-alate --features telegram
```

`aphid raw` and `aphid agent` can be fully scripted. Their tests run the full
encode, stream and commit path against a model that is not on the network.
