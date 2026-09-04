# Telegram

A Telegram bot can speak to the alate. You send a message, the agent answers,
and you can permit or refuse a tool from the chat.

The bot is a client of the [gateway](../gateway.md), and not a second door. Each
chat attaches to the same socket and gets its own conversation, in the same
manner as a terminal. So two chats do not see each other, and `aphid alate
attach` shows what a chat said and what the agent answered.

This is behind a build feature, because it adds an HTTP client that a build
without a bot does not need:

```console
$ cargo build --release --features telegram
```

## Make a bot

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

## In a chat

| What you send | Effect |
| --- | --- |
| A voice message | The words in it, for the agent. Refer to [Recordings](#recordings). |
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

## Files from the agent

When you explicitly ask the agent to send a file, it can call `send_attachment`
and Telegram receives the file as a document in the same chat. It can send a
file from an allowed workspace read path. The default limit is 20 MiB. Set
`gateway.attachment_limit` to change it, or to `0` to turn this feature off.

With `permissions: ask`, the chat shows the file path, name, size, hash and
caption before it is sent. The confirmation is only for the chat that receives
the file. A group chat on the allow list can receive a file as well.

## Recordings

The bot can listen. A voice message becomes text on the machine of the alate,
the chat shows the text, and the agent is given it as if you had typed it.

Nothing is sent to a different company to do this. The model is
[Parakeet TDT 0.6b v3][parakeet], it runs on the CPU of the alate, and it reads
25 languages. This is behind a second build feature, because it adds a machine
learning runtime that a build with no ears does not need:

```console
$ cargo build --release --features telegram,voice
```

Then put a `voice` block in `alate.json`:

```json
{ "voice": {} }
```

The block is at the top of the file and not inside `gateway`, because the ears
belong to the alate and not to the bot.

| Field | Effect |
| --- | --- |
| `model` | The directory the model is in. The cache of the machine when absent. |
| `download` | Get the model when it is not there. `true` when absent. |
| `longest` | The longest recording to accept. `off` when absent, which accepts all of them. |
| `idle` | How long the model stays in memory with no work. `10m` when absent, and `off` keeps it. |

### The model

The model is 670 MB in four files. When it is not on the machine, the alate
gets it at start and puts it in
`$XDG_CACHE_HOME/aphid/models/parakeet-tdt-0.6b-v3-int8`. This occurs one time,
in the background, and the alate does all its other work while it goes on.
Every file is measured against a checksum before it is used.

The cache is of the machine and not of the instance, so three alates on one
computer share one model.

The model is read into memory at the first recording and is put out of memory
again after `idle`. This keeps 670 MB out of a daemon that stays awake for
weeks and gets a recording each day. To read it once and keep it, set `idle` to
`off`.

### What you can send

A voice message, a music file, a round video, and a file that says it is audio.
Telegram gives a bot files up to 20 MB.

A recording is cut into pieces of approximately 30 seconds before it is read,
at the most quiet point near each boundary, and the texts are joined. So a
recording of ten minutes is read correctly, and slowly.

Voice messages, mp3 and wav are read correctly. A round video and an `.m4a`
file are AAC, and the AAC decoder in this build is not as good: the words come
out with mistakes in them. This is a limit of the decoder and not of the speech
model.

### What you see

The chat shows 🎤 and the text before the agent answers, because speech
recognition makes mistakes and you must be able to see the sentence the agent
was given. A recording with no speech in it is said to have none, and the agent
is not given a turn.

Speech is never read as a command. If the recognition writes `/new`, it is
words for the agent and it does not throw the conversation away.

A recording is read in a task of its own, so the bot answers all the other
chats while it goes on. Two effects follow. The order in one chat is not
promised: a recording and then a typed line can reach the agent the other way
round. And `/cancel` sent while a recording is being read does not stop it,
because it is not yet a run — the words arrive, and the next `/cancel` stops
what they start.

[parakeet]: https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3

## Permission from a chat

A permission question comes to the chat with three buttons: **Allow**, **Allow
always** and **Deny**. The question goes only to a chat with a run in flight. A
question that belongs to a terminal or to a job is left for the terminal to
answer.

Note that a chat that has spoken stays attached until the daemon stops. So an
alate with a bot **is attended**, and a tool that asks permission is asked in
the chat instead of being refused. Before a chat speaks for the first time, no
connection exists, and an unattended alate behaves as it does with no bot. Refer
to [Permissions](../../alate.md#permissions).

## When Telegram does not answer

If the bot cannot be reached, the daemon says so one time and tries again, and
waits longer after each failure up to one minute. It says so again when Telegram
answers.

The bot is not necessary for the alate to start. A token that is absent, a
`poll` that is not a length of time, and a Telegram that does not answer are all
reported and passed over.

The ears are not necessary either. A `voice` block in a build with no `voice`
feature, a model that cannot be fetched, and a `longest` that is not a length of
time are all reported and passed over. An alate that cannot listen says so to a
chat that sends a recording, one time.
